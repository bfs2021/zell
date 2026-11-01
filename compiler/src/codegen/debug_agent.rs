//! Hooks and tables of the in-process debug agent (docs/DEBUGGER.md stage
//! 2, runtime/ABI.md §22), emitted with [`super::Options::debug_agent`].
//!
//! Every function with source gets a 32-byte frame record `F`
//! (`zell_dbg_frame`, not zeroed: `zell_dbg_enter` initialises it) and
//! loads the hidden runtime flag `zell_dbg_on` once at entry. Then, each
//! behind `if (on)` (a register test, predicted not taken):
//!
//! * after the parameters are stored: `zell_dbg_enter(&F, &func, locals, entry_stmt)`,
//!   where `locals` (unoptimised builds only) is a `[n x ptr]` frame array
//!   filled in that branch with the address of every local's slot;
//! * at every statement: `zell_dbg_stmt(&F, id)`;
//! * at the start of the exit block: `zell_dbg_leave(&F)`;
//! * around an `await` that may suspend: `zell_dbg_suspend(&F)` before
//!   the continuation is registered, `zell_dbg_resume(&F)` once it is
//!   ready again (a no-op unless the frame was suspended).
//!
//! The static tables (statements, functions, locals, files, breakpoint
//! bytes) are described in ABI.md §22.1; `zell_dbg_boot(&tables, entry)`
//! in the C `main` starts the agent (`debug.AgentMain`, wrapped as a C
//! function) when `ZELL_DEBUG` is set. Functions of the standard library
//! (including the agent itself) get no hooks.

use super::CodeGen;
use crate::hir::{self, LocalId, LocalKind};
use crate::span::Span;
use inkwell::module::Linkage;
use inkwell::types::StructType;
use inkwell::values::{BasicValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue};
use inkwell::IntPredicate;

/// Statement kinds (`zell_dbg_stmt.kind`).
const KIND_STMT: i32 = 0;
const KIND_ENTRY: i32 = 1;
/// `zell_dbg_func.flags`.
const FUNC_ASYNC: i32 = 1;
/// `zell_dbg_local.flags`: the slot holds a heap cell (value at cell + 16).
const LOCAL_CELL: i32 = 1;
/// Version of the table layout (`zell_dbg_tables.version`).
const TABLES_VERSION: u64 = 1;

/// One `zell_dbg_local` record before emission.
struct LocalRec {
    name: String,
    ty: crate::types::TypeId,
    slot: i32,
    flags: i32,
    line: i32,
    end_line: i32,
}

/// The function being emitted.
struct CurFn<'ctx> {
    func: FunctionValue<'ctx>,
    index: i32,
    file: usize,
    frame: PointerValue<'ctx>,
    on: IntValue<'ctx>,
    /// End lines of the enclosing blocks, innermost last.
    block_ends: Vec<i32>,
    /// Per local: its record index once declared.
    declared: Vec<Option<usize>>,
    locals: Vec<LocalRec>,
}

/// A `zell_dbg_func` record before emission.
struct FuncRec<'ctx> {
    name: String,
    short: String,
    file: i32,
    line: i32,
    first: i32,
    last: i32,
    flags: i32,
    locals: Vec<LocalRec>,
    addr: PointerValue<'ctx>,
}

/// Module-wide agent state ([`CodeGen::dbga`]).
pub(crate) struct Agent<'ctx> {
    /// `{file, line, col, func, scope, kind}` per statement id.
    stmts: Vec<[i32; 6]>,
    funcs: Vec<FuncRec<'ctx>>,
    cur: Option<CurFn<'ctx>>,
    tables: GlobalValue<'ctx>,
    on_flag: GlobalValue<'ctx>,
    /// Release builds emit hooked functions twice (see
    /// [`CodeGen::dbga_emit_cloned`]): 1 while the hooked copy is emitted,
    /// 2 while the plain one is.
    mode: u8,
    /// The hooked copy the plain function being emitted dispatches to.
    dispatch: Option<FunctionValue<'ctx>>,
    /// (plain function, hooked copy, the dispatch branch) per cloned function.
    clones: Vec<(FunctionValue<'ctx>, FunctionValue<'ctx>, inkwell::values::InstructionValue<'ctx>)>,
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Creates the agent state when hooks are requested.
    pub(crate) fn init_agent(&mut self) {
        if !self.opts.debug_agent || self.prog.sources.is_empty() || self.prog.entry.is_none() {
            return;
        }
        let tables_ty = self.dbga_tables_ty();
        let tables = self.module.add_global(tables_ty, None, "zell.dbg.tables");
        tables.set_linkage(Linkage::Internal);
        tables.set_alignment(8);
        let on_flag = self.module.add_global(self.ctx.i8_type(), None, "zell_dbg_on");
        on_flag.set_linkage(Linkage::External);
        self.dbga = Some(Box::new(Agent { stmts: Vec::new(), funcs: Vec::new(), cur: None, tables, on_flag, mode: 0, dispatch: None, clones: Vec::new() }));
    }

    fn dbga_tables_ty(&self) -> StructType<'ctx> {
        let (i64t, p) = (self.i64_ty().into(), self.ptr_ty().into());
        self.ctx.struct_type(&[i64t, i64t, p, i64t, p, i64t, p, p], false)
    }

    fn agent_line_col(&self, file: usize, offset: u32) -> (i32, i32) {
        match self.prog.sources.get(file) {
            Some(src) => {
                let line = src.line_of(offset);
                let start = src.line_starts.get(line as usize - 1).copied().unwrap_or(0);
                (line as i32, (offset.saturating_sub(start) + 1) as i32)
            }
            None => (0, 0),
        }
    }

    /// Whether `f` gets hooks: user code with a source span.
    fn agent_wants(&self, f: &hir::Function) -> bool {
        !crate::sema::stdlib::is_std_namespace(&f.full_name) && f.file < self.prog.sources.len() && f.span != Span::DUMMY
    }

    fn agent_cur(&self) -> Option<&CurFn<'ctx>> {
        let a = self.dbga.as_deref()?;
        if a.mode == 2 {
            return None;
        }
        let c = a.cur.as_ref()?;
        let st = self.f.as_ref()?;
        (st.func == c.func).then_some(c)
    }

    /// Branches on `on` (expected false) into a fresh block where `body`
    /// emits the hook call; continues after it.
    fn agent_guard(&mut self, on: IntValue<'ctx>, body: impl FnOnce(&mut Self)) {
        let hook = self.block("dbg.hook");
        let cont = self.block("dbg.cont");
        let expect = inkwell::intrinsics::Intrinsic::find("llvm.expect").and_then(|i| i.get_declaration(&self.module, &[self.ctx.bool_type().into()]));
        let c = match expect {
            Some(f) => self.b.build_call(f, &[on.into(), self.ctx.bool_type().const_zero().into()], "").unwrap().try_as_basic_value().basic().unwrap().into_int_value(),
            None => on,
        };
        self.b.build_conditional_branch(c, hook, cont).unwrap();
        self.b.position_at_end(hook);
        body(self);
        self.b.build_unconditional_branch(cont).unwrap();
        self.b.position_at_end(cont);
    }

    fn agent_stmt_id(&mut self, file: usize, at: u32, func: i32, kind: i32) -> i32 {
        let (line, col) = self.agent_line_col(file, at);
        let a = self.dbga.as_mut().unwrap();
        a.stmts.push([file as i32, line, col, func, 0, kind]);
        (a.stmts.len() - 1) as i32
    }

    /// Function entry (after the parameters are stored): the frame record,
    /// the flag load and the `enter` hook.
    pub(crate) fn dbga_enter(&mut self, f: &'a hir::Function) {
        if self.dbga.as_ref().is_none_or(|a| a.mode == 2) || !self.agent_wants(f) {
            return;
        }
        let fv = self.fs().func;
        let index = self.dbga.as_ref().unwrap().funcs.len() as i32;
        let frame_ty = self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.ptr_ty().into(), self.ctx.i32_type().into(), self.ctx.i32_type().into()], false);
        let frame = self.entry_alloca(frame_ty.into(), "dbg.frame", false);
        let flag = self.dbga.as_ref().unwrap().on_flag.as_pointer_value();
        let on = {
            let st = self.fs();
            let v = st.init_b.build_load(self.ctx.i8_type(), flag, "dbg.on").unwrap().into_int_value();
            st.init_b.build_int_compare(IntPredicate::NE, v, self.ctx.i8_type().const_zero(), "").unwrap()
        };
        let (line, _) = self.agent_line_col(f.file, f.span.start);
        let (end_line, _) = self.agent_line_col(f.file, f.span.end.saturating_sub(1));
        let entry = self.agent_stmt_id(f.file, f.span.start, index, KIND_ENTRY);
        let mut cur = CurFn { func: fv, index, file: f.file, frame, on, block_ends: vec![end_line], declared: vec![None; f.locals.len()], locals: Vec::new() };
        // Parameters (and `this`) are visible in the whole body.
        for p in &f.params {
            Self::agent_record(&mut cur, f, *p, line, end_line, self.fs().locals.get(p.0 as usize).copied().flatten().is_some_and(|s| s.cell));
        }
        // A lambda's captured variables, too: slot `locals + i` of the
        // frame array is the address of the env's cell pointer `i`.
        let env = self.fs().env;
        let ncap = if env.is_some() { f.captures.len() } else { 0 };
        for (i, c) in f.captures.iter().enumerate().take(ncap) {
            if c.name.is_empty() || c.name == "_" || c.name.contains(['$', '#', '.']) || cur.locals.iter().any(|l| l.name == c.name) {
                continue;
            }
            cur.locals.push(LocalRec { name: c.name.clone(), ty: c.ty, slot: (f.locals.len() + i) as i32, flags: LOCAL_CELL, line, end_line });
        }
        let name = f.full_name.clone();
        let short = f.name.clone();
        let flags = if f.is_async { FUNC_ASYNC } else { 0 };
        let addr = fv.as_global_value().as_pointer_value();
        let a = self.dbga.as_mut().unwrap();
        a.funcs.push(FuncRec { name, short, file: f.file as i32, line, first: entry, last: entry, flags, locals: Vec::new(), addr });
        a.cur = Some(cur);
        let func_rec = self.dbga_func_ptr(index);
        let with_locals = !self.opts.optimized && (!f.locals.is_empty() || ncap > 0);
        let nl = f.locals.len() as u32;
        let n = nl + ncap as u32;
        let arr = with_locals.then(|| self.entry_alloca(self.ptr_ty().array_type(n).into(), "dbg.locals", false));
        self.agent_guard(on, |cg| {
            let lp = match arr {
                Some(arr) => {
                    let at = |cg: &mut Self, i: u32| unsafe { cg.b.build_in_bounds_gep(cg.ptr_ty().array_type(n), arr, &[cg.i64_ty().const_zero(), cg.i64_ty().const_int(i as u64, false)], "") }.unwrap();
                    for i in 0..nl {
                        let Some(slot) = cg.fs().locals.get(i as usize).copied().flatten() else { continue };
                        let p = at(cg, i);
                        cg.b.build_store(p, slot.ptr).unwrap();
                    }
                    if let Some(env) = env {
                        for i in 0..ncap as u32 {
                            let cellp = cg.byte_gep(env, 24 + 8 * i as u64);
                            let p = at(cg, nl + i);
                            cg.b.build_store(p, cellp).unwrap();
                        }
                    }
                    arr
                }
                None => cg.ptr_ty().const_null(),
            };
            let e = cg.ctx.i32_type().const_int(entry as u64, false);
            cg.call_rt("zell_dbg_enter", &[frame.into(), func_rec.into(), lp.into(), e.into()]);
        });
    }

    /// Optimised builds: a hooked function is emitted twice, so that with
    /// the agent off the code is the code of a build without the agent
    /// (tail calls, accumulator recursion and frame elision survive):
    /// `sym.dbg` with the hooks, and `sym` without them, which starts with
    /// `if (zell_dbg_on) return sym.dbg(args)`. Async functions keep one
    /// (hooked) body. Returns `true` when both copies were emitted.
    pub(crate) fn dbga_emit_cloned(&mut self, fid: hir::FuncId) -> super::R<bool> {
        let f: &'a hir::Function = &self.prog.funcs[fid.0 as usize];
        let Some(a) = self.dbga.as_ref() else { return Ok(false) };
        if a.mode != 0 || !self.opts.optimized || f.is_async || !self.agent_wants(f) {
            return Ok(false);
        }
        let orig = self.funcs[fid.0 as usize];
        let clone = self.module.add_function(&format!("{}.dbg", f.symbol), orig.get_type(), Some(Linkage::Internal));
        self.funcs[fid.0 as usize] = clone;
        self.dbga.as_mut().unwrap().mode = 1;
        let r = self.emit_function(fid);
        self.funcs[fid.0 as usize] = orig;
        let a = self.dbga.as_mut().unwrap();
        a.mode = 2;
        a.dispatch = Some(clone);
        let r2 = r.and_then(|_| self.emit_function(fid));
        let a = self.dbga.as_mut().unwrap();
        a.mode = 0;
        a.dispatch = None;
        r2.map(|_| true)
    }

    /// Start of the plain copy's body: the dispatch to the hooked copy.
    pub(crate) fn dbga_dispatch(&mut self) {
        let Some(clone) = self.dbga.as_mut().and_then(|a| a.dispatch.take()) else { return };
        let flag = self.dbga.as_ref().unwrap().on_flag.as_pointer_value();
        let fv = self.fs().func;
        let v = self.b.build_load(self.ctx.i8_type(), flag, "dbg.on").unwrap().into_int_value();
        let on = self.b.build_int_compare(IntPredicate::NE, v, self.ctx.i8_type().const_zero(), "").unwrap();
        let hook = self.block("dbg.dispatch");
        let cont = self.block("dbg.plain");
        let br = self.b.build_conditional_branch(on, hook, cont).unwrap();
        self.dbga.as_mut().unwrap().clones.push((fv, clone, br));
        self.b.position_at_end(hook);
        if let Some(f) = self.fs().hir {
            self.dbg_loc(f.span.start);
        }
        let args: Vec<inkwell::values::BasicMetadataValueEnum> = fv.get_params().iter().map(|p| (*p).into()).collect();
        let call = self.b.build_call(clone, &args, "").unwrap();
        call.set_tail_call(true);
        match call.try_as_basic_value().basic() {
            Some(r) => self.b.build_return(Some(&r)).unwrap(),
            None => self.b.build_return(None).unwrap(),
        };
        self.b.position_at_end(cont);
    }

    fn agent_record(cur: &mut CurFn<'ctx>, f: &hir::Function, l: LocalId, line: i32, end_line: i32, cell: bool) {
        let info = &f.locals[l.0 as usize];
        if cur.declared[l.0 as usize].is_some() || info.kind == LocalKind::Temp || info.name.is_empty() || info.name == "_" || info.name.contains(['$', '#', '.']) {
            return;
        }
        cur.declared[l.0 as usize] = Some(cur.locals.len());
        cur.locals.push(LocalRec { name: info.name.clone(), ty: info.ty, slot: l.0 as i32, flags: if cell { LOCAL_CELL } else { 0 }, line, end_line });
    }

    /// A local's declaration (first fresh assignment): visible from the
    /// next line to the end of the enclosing block.
    pub(crate) fn dbga_declare(&mut self, l: LocalId) {
        let Some(c) = self.agent_cur() else { return };
        let (file, end) = (c.file, *c.block_ends.last().unwrap());
        let Some(f) = self.fs().hir else { return };
        let info = &f.locals[l.0 as usize];
        let at = if info.span == Span::DUMMY { f.span.start } else { info.span.start };
        let (line, _) = self.agent_line_col(file, at);
        let cell = self.fs().locals.get(l.0 as usize).copied().flatten().is_some_and(|s| s.cell);
        let cur = self.dbga.as_mut().unwrap().cur.as_mut().unwrap();
        Self::agent_record(cur, f, l, line, end, cell);
    }

    /// A nested block (its end bounds the locals declared inside).
    pub(crate) fn dbga_push_block(&mut self, sp: Span) {
        let Some(c) = self.agent_cur() else { return };
        let file = c.file;
        let end = if sp == Span::DUMMY { *c.block_ends.last().unwrap() } else { self.agent_line_col(file, sp.end.saturating_sub(1)).0 };
        self.dbga.as_mut().unwrap().cur.as_mut().unwrap().block_ends.push(end);
    }

    pub(crate) fn dbga_pop_block(&mut self) {
        if self.agent_cur().is_none() {
            return;
        }
        let ends = &mut self.dbga.as_mut().unwrap().cur.as_mut().unwrap().block_ends;
        if ends.len() > 1 {
            ends.pop();
        }
    }

    /// The statement hook.
    pub(crate) fn dbga_stmt(&mut self, sp: Span) {
        let Some(c) = self.agent_cur() else { return };
        if sp == Span::DUMMY {
            return;
        }
        let (file, index, frame, on) = (c.file, c.index, c.frame, c.on);
        let id = self.agent_stmt_id(file, sp.start, index, KIND_STMT);
        self.dbga.as_mut().unwrap().funcs[index as usize].last = id;
        self.agent_guard(on, |cg| {
            let v = cg.ctx.i32_type().const_int(id as u64, false);
            cg.call_rt("zell_dbg_stmt", &[frame.into(), v.into()]);
        });
    }

    /// Start of the exit block: a statement at the closing brace (where
    /// the locals and the result can still be seen), then the frame
    /// leaves the chain.
    pub(crate) fn dbga_leave(&mut self) {
        let Some(c) = self.agent_cur() else { return };
        let (file, index, frame, on) = (c.file, c.index, c.frame, c.on);
        let Some(f) = self.fs().hir else { return };
        let id = self.agent_stmt_id(file, f.span.end.saturating_sub(1), index, KIND_STMT);
        self.dbga.as_mut().unwrap().funcs[index as usize].last = id;
        self.agent_guard(on, |cg| {
            let v = cg.ctx.i32_type().const_int(id as u64, false);
            cg.call_rt("zell_dbg_stmt", &[frame.into(), v.into()]);
            match cg.fs().coro {
                // A frame-linked coroutine (ABI.md 10.3) ends without its
                // task ending: it has none, its root's goes on.
                Some(co) => {
                    let ts = cg.b.build_load(cg.ptr_ty(), co.task_slot, "").unwrap().into_pointer_value();
                    let bits = cg.b.build_ptr_to_int(ts, cg.i64_ty(), "").unwrap();
                    let tag = cg.b.build_and(bits, cg.i64_ty().const_int(1, false), "").unwrap();
                    let linked = cg.b.build_int_compare(inkwell::IntPredicate::NE, tag, cg.i64_ty().const_zero(), "").unwrap();
                    let (lb, ob, jb) = (cg.block("dbg.leave.linked"), cg.block("dbg.leave"), cg.block("dbg.left"));
                    cg.b.build_conditional_branch(linked, lb, ob).unwrap();
                    cg.b.position_at_end(lb);
                    cg.call_rt("zell_dbg_leave_linked", &[frame.into()]);
                    cg.b.build_unconditional_branch(jb).unwrap();
                    cg.b.position_at_end(ob);
                    cg.call_rt("zell_dbg_leave", &[frame.into()]);
                    cg.b.build_unconditional_branch(jb).unwrap();
                    cg.b.position_at_end(jb);
                }
                None => {
                    cg.call_rt("zell_dbg_leave", &[frame.into()]);
                }
            }
        });
    }

    /// Before an `await` registers its continuation.
    pub(crate) fn dbga_suspend(&mut self) {
        self.agent_simple("zell_dbg_suspend");
    }

    /// Once an awaited task is ready (no-op unless the frame was suspended).
    pub(crate) fn dbga_resume(&mut self) {
        self.agent_simple("zell_dbg_resume");
    }

    fn agent_simple(&mut self, hook: &str) {
        let Some(c) = self.agent_cur() else { return };
        let (frame, on) = (c.frame, c.on);
        self.agent_guard(on, |cg| {
            cg.call_rt(hook, &[frame.into()]);
        });
    }

    /// End of the function's emission: keeps its locals table.
    pub(crate) fn dbga_end(&mut self) {
        let Some(a) = self.dbga.as_mut() else { return };
        if let Some(c) = a.cur.take() {
            a.funcs[c.index as usize].locals = c.locals;
        }
    }

    /// Address of function record `i` inside the tables' function array
    /// (the array is emitted at the end, so this is a constant GEP into a
    /// placeholder that `finish_agent` replaces).
    fn dbga_func_ptr(&mut self, i: i32) -> PointerValue<'ctx> {
        let fty = self.dbga_func_ty();
        let g = match self.module.get_global("zell.dbg.funcs") {
            Some(g) => g,
            None => {
                // An opaque array; its real type and initializer come later.
                let g = self.module.add_global(fty.array_type(0), None, "zell.dbg.funcs");
                g.set_linkage(Linkage::Internal);
                g
            }
        };
        unsafe { g.as_pointer_value().const_gep(fty, &[self.i64_ty().const_int(i as u64, false)]) }
    }

    fn dbga_func_ty(&self) -> StructType<'ctx> {
        let (p, i32t) = (self.ptr_ty().into(), self.ctx.i32_type().into());
        self.ctx.struct_type(&[p, p, i32t, i32t, i32t, i32t, i32t, i32t, p, p], false)
    }

    /// In the C `main`, after `zell_runtime_init`: `zell_dbg_boot`.
    pub(crate) fn dbga_boot(&mut self) {
        let Some(a) = self.dbga.as_ref() else { return };
        let tables = a.tables.as_pointer_value();
        let entry = self.prog.funcs.iter().position(|f| (f.full_name == "debug.AgentMain" || f.full_name == "debug.AgentMain<>") && f.params.is_empty());
        let wrapper = match entry {
            Some(i) => {
                let target = self.funcs[i];
                let w = self.module.add_function("zell.dbg.agent", self.ctx.void_type().fn_type(&[], false), Some(Linkage::Internal));
                let _ = self.with_helper(w, |cg| {
                    cg.b.build_call(target, &[], "").unwrap();
                    cg.b.build_return(None).unwrap();
                    Ok(())
                });
                w.as_global_value().as_pointer_value()
            }
            None => self.ptr_ty().const_null(),
        };
        self.call_rt("zell_dbg_boot", &[tables.into(), wrapper.into()]);
    }

    /// In the C `main`, before it returns `code`: `zell_dbg_exit`.
    pub(crate) fn dbga_exit(&mut self, code: IntValue<'ctx>) {
        if self.dbga.is_some() {
            self.call_rt("zell_dbg_exit", &[code.into()]);
        }
    }

    /// Optimised builds, after every function is emitted: direct calls in
    /// hooked copies go to the hooked copies of their callees, and a plain
    /// function whose only uses are direct calls from plain copies loses
    /// its dispatch check (with the agent on, plain code is only entered
    /// through a check: from the C `main`, function values, vtables or
    /// code without copies), so it is exactly the code of a build without
    /// the agent.
    fn agent_link_clones(&mut self, clones: &[(FunctionValue<'ctx>, FunctionValue<'ctx>, inkwell::values::InstructionValue<'ctx>)]) {
        use inkwell::values::AsValueRef;
        use llvm_sys::core as llc;
        use llvm_sys::prelude::LLVMValueRef;
        let plain: std::collections::HashMap<LLVMValueRef, LLVMValueRef> = clones.iter().map(|(p, c, _)| (p.as_value_ref(), c.as_value_ref())).collect();
        let hooked: std::collections::HashSet<LLVMValueRef> = clones.iter().map(|(_, c, _)| c.as_value_ref()).collect();
        let plain_set: std::collections::HashSet<LLVMValueRef> = plain.keys().copied().collect();
        unsafe {
            // (the call, the function containing it, the callee) for every use.
            let uses_of = |f: LLVMValueRef| {
                let mut out = Vec::new();
                let mut u = llc::LLVMGetFirstUse(f);
                while !u.is_null() {
                    let user = llc::LLVMGetUser(u);
                    let is_call = !llvm_sys::core::LLVMIsACallInst(user).is_null();
                    let callee = if is_call { llc::LLVMGetCalledValue(user) } else { std::ptr::null_mut() };
                    let parent = if is_call { llc::LLVMGetBasicBlockParent(llc::LLVMGetInstructionParent(user)) } else { std::ptr::null_mut() };
                    out.push((user, is_call && callee == f, parent));
                    u = llc::LLVMGetNextUse(u);
                }
                out
            };
            for (&p, &c) in &plain {
                for (user, direct, parent) in uses_of(p) {
                    if direct && hooked.contains(&parent) {
                        let n = llc::LLVMGetNumOperands(user);
                        llc::LLVMSetOperand(user, (n - 1) as u32, c);
                    }
                }
            }
            for (p, _, br) in clones {
                let only_plain = uses_of(p.as_value_ref()).iter().all(|(_, direct, parent)| *direct && plain_set.contains(parent));
                if only_plain {
                    let b = br.as_value_ref();
                    let plain_bb = llc::LLVMGetSuccessor(b, 1);
                    let builder = llc::LLVMCreateBuilderInContext(self.ctx.raw());
                    llc::LLVMPositionBuilderBefore(builder, b);
                    llc::LLVMBuildBr(builder, plain_bb);
                    llc::LLVMDisposeBuilder(builder);
                    llc::LLVMInstructionEraseFromParent(b);
                }
            }
        }
    }

    /// Emits the tables.
    pub(crate) fn finish_agent(&mut self) {
        let Some(mut a) = self.dbga.take() else { return };
        let clones = std::mem::take(&mut a.clones);
        self.agent_link_clones(&clones);
        let i32t = self.ctx.i32_type();
        let i64t = self.i64_ty();
        let p = self.ptr_ty();
        // Statements.
        let srec = self.ctx.struct_type(&[i32t.into(); 6], false);
        let svals: Vec<_> = a.stmts.iter().map(|r| srec.const_named_struct(&r.iter().map(|x| i32t.const_int(*x as u64, true).into()).collect::<Vec<BasicValueEnum>>())).collect();
        let stmts = self.agent_array(srec.const_array(&svals), "zell.dbg.stmts", true);
        // Locals and functions.
        let lrec = self.ctx.struct_type(&[p.into(), p.into(), i32t.into(), i32t.into(), i32t.into(), i32t.into()], false);
        let fty = self.dbga_func_ty();
        let funcs = std::mem::take(&mut a.funcs);
        let mut fvals = Vec::new();
        for fr in &funcs {
            let mut lvals = Vec::new();
            for l in &fr.locals {
                let nm = self.str_lit(&l.name);
                let ti = self.type_info(l.ty);
                lvals.push(lrec.const_named_struct(&[
                    nm.into(),
                    ti.into(),
                    i32t.const_int(l.slot as u64, true).into(),
                    i32t.const_int(l.flags as u64, true).into(),
                    i32t.const_int(l.line as u64, true).into(),
                    i32t.const_int(l.end_line as u64, true).into(),
                ]));
            }
            let locals = if lvals.is_empty() { p.const_null() } else { self.agent_array(lrec.const_array(&lvals), "zell.dbg.locals", true) };
            let nm = self.str_lit(&fr.name);
            let sh = self.str_lit(&fr.short);
            fvals.push(fty.const_named_struct(&[
                nm.into(),
                sh.into(),
                i32t.const_int(fr.file as u64, true).into(),
                i32t.const_int(fr.line as u64, true).into(),
                i32t.const_int(fr.first as u64, true).into(),
                i32t.const_int(fr.last as u64, true).into(),
                i32t.const_int(lvals.len() as u64, true).into(),
                i32t.const_int(fr.flags as u64, true).into(),
                locals.into(),
                fr.addr.into(),
            ]));
        }
        let farr = fty.const_array(&fvals);
        let funcs_g = self.module.add_global(farr.get_type(), None, "zell.dbg.funcs.def");
        funcs_g.set_linkage(Linkage::Internal);
        funcs_g.set_constant(true);
        funcs_g.set_alignment(8);
        funcs_g.set_initializer(&farr);
        if let Some(ph) = self.module.get_global("zell.dbg.funcs") {
            ph.as_pointer_value().replace_all_uses_with(funcs_g.as_pointer_value());
            unsafe { ph.delete() };
        }
        funcs_g.as_pointer_value().set_name("zell.dbg.funcs");
        // Files: absolute paths, like the DWARF compile units.
        let mut fps = Vec::new();
        for src in &self.prog.sources {
            let pth = std::path::Path::new(&src.path);
            let abs = crate::paths::canonical_or_absolute(pth);
            fps.push(self.str_lit(&abs.to_string_lossy()));
        }
        let files = self.agent_array(p.const_array(&fps), "zell.dbg.files", true);
        // Breakpoint bytes (written by the agent).
        let n = a.stmts.len() as u32;
        let bp_ty = self.ctx.i8_type().array_type(n.max(1));
        let bp = self.module.add_global(bp_ty, None, "zell.dbg.bp");
        bp.set_linkage(Linkage::Internal);
        bp.set_initializer(&bp_ty.const_zero());
        let tables_ty = self.dbga_tables_ty();
        a.tables.set_initializer(&tables_ty.const_named_struct(&[
            i64t.const_int(TABLES_VERSION, false).into(),
            i64t.const_int(a.stmts.len() as u64, false).into(),
            stmts.into(),
            i64t.const_int(funcs.len() as u64, false).into(),
            funcs_g.as_pointer_value().into(),
            i64t.const_int(fps.len() as u64, false).into(),
            files.into(),
            bp.as_pointer_value().into(),
        ]));
    }

    fn agent_array(&mut self, v: inkwell::values::ArrayValue<'ctx>, name: &str, constant: bool) -> PointerValue<'ctx> {
        let g = self.module.add_global(v.get_type(), None, name);
        g.set_linkage(Linkage::Internal);
        g.set_constant(constant);
        g.set_alignment(8);
        g.set_initializer(&v);
        g.as_pointer_value()
    }
}
