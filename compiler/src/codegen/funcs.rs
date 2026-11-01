//! Function bodies: locals, parameters, the single exit block, `defer`,
//! closures and function-value thunks.

use super::{CodeGen, LocalSlot, Val, R};
use crate::hir::{self, CaptureSource, FuncId, FuncKind, LocalId};
use crate::types::TypeId;
use inkwell::values::{AsValueRef, BasicMetadataValueEnum, BasicValueEnum, FunctionValue, PointerValue};
use inkwell::IntPredicate;

/// Maximum number of pending deferred calls per function activation.
pub(crate) const MAX_DEFERS: u32 = 64;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Emits the body of HIR function `fid`.
    pub(crate) fn emit_function(&mut self, fid: FuncId) -> R<()> {
        if self.dbga_emit_cloned(fid)? {
            return Ok(());
        }
        let f: &'a hir::Function = &self.prog.funcs[fid.0 as usize];
        let fv = self.funcs_borrow.get(&fid.0).copied().unwrap_or(self.funcs[fid.0 as usize]);
        let saved = self.begin_fn(fv, Some(f));
        self.fs_mut().borrow_ret = self.funcs_borrow.contains_key(&fid.0);
        self.dbga_dispatch();
        let off = if matches!(f.kind, FuncKind::Lambda { .. }) { 1 } else { 0 };
        if off == 1 {
            self.fs_mut().env = Some(fv.get_nth_param(0).unwrap().into_pointer_value());
        }
        if f.is_async && self.is_plain_async(fid.0 as usize) {
            let neutral = self.plain_async.ctx_neutral.get(fid.0 as usize).copied().unwrap_or(false);
            self.plain_async_prologue(!neutral);
        } else if f.is_async {
            self.coro_prologue(f)?;
        }
        // Local slots (zeroed at entry). Stack values with a destructor get
        // a drop flag.
        let mut slots = Vec::with_capacity(f.locals.len());
        let mut flags = Vec::with_capacity(f.locals.len());
        // Escape analysis: address-taken locals and `new` objects that do
        // not escape live on the stack, in allocas with an immortal header.
        let stack = crate::hir::ownership::stack_locals(self.prog, f, &self.strict_facts);
        for (i, l) in f.locals.iter().enumerate() {
            let cell = (l.captured || l.address_taken) && stack[i] != crate::hir::ownership::StackKind::Cell;
            let (p, stack_obj) = match stack[i] {
                crate::hir::ownership::StackKind::Cell => (self.stack_object(l.ty, &l.name), None),
                crate::hir::ownership::StackKind::New => {
                    let crate::types::Type::Pointer(t) = self.ty(l.ty).clone() else { unreachable!("stack new of a non-pointer") };
                    let obj = self.stack_object(t, &format!("{}.obj", l.name));
                    (self.entry_alloca(self.ptr_ty().into(), &l.name, true), Some(obj))
                }
                crate::hir::ownership::StackKind::Normal => {
                    let ty = if cell { self.ptr_ty().into() } else { self.llty(l.ty) };
                    (self.entry_alloca(ty, &l.name, true), None)
                }
            };
            slots.push(Some(LocalSlot { ptr: p, cell, stack_obj }));
            flags.push(if !cell && self.has_dtor(l.ty) { Some(self.entry_alloca(self.ctx.bool_type().into(), "dflag", true)) } else { None });
        }
        self.fs_mut().locals = slots;
        self.fs_mut().flags = flags;
        // Borrowed locals (hir::borrow): never retained or released.
        let bl = crate::hir::borrow::borrowed_locals(self.prog, fid, f, &self.pure_fns, &stack, &|t| self.cycles.holds_cyclic(t));
        self.fs_mut().borrowed = bl.clone();
        self.fs_mut().borrow_local = bl;
        let const_only: Vec<bool> = f.locals.iter().map(|l| !(l.captured || l.address_taken) && matches!(self.rc_kind(l.ty), super::rc::RcKind::Ref | super::rc::RcKind::Ptr)).collect();
        let n = const_only.len();
        self.fs_mut().const_only = const_only;
        self.fs_mut().const_depth = vec![usize::MAX; n];
        self.setup_interior(fid, f);
        if self.fs().coro.is_some() {
            // Destroy path (ABI.md 8.1): held temporaries live in slots, and
            // a personality of its own keeps landing pads out (unwind.rs).
            self.fs_mut().spill_temps = true;
            let p = self.personality_fn(super::unwind::PERSONALITY_CORO);
            unsafe { inkwell::llvm_sys::core::LLVMSetPersonalityFn(fv.as_value_ref(), inkwell::values::AsValueRef::as_value_ref(&p)) };
        }
        if f.ret != self.prog.types.void {
            let t = self.llty(f.ret);
            let p = self.entry_alloca(t, "ret", true);
            self.fs_mut().ret_slot = Some((p, f.ret));
        }
        self.dbg_params(f);
        let exit = self.block("exit");
        self.fs_mut().exit = Some(exit);
        for (k, p) in f.params.iter().enumerate() {
            let v = fv.get_nth_param((k + off) as u32).unwrap();
            if self.facts.moved(fid, k) {
                // Values with a destructor are moved in.
                self.assign_local(*p, Val::owned(v), true)?;
            } else if self.facts.owned(fid, k) || self.fs().locals[p.0 as usize].unwrap().cell || !self.needs_rc(f.locals[p.0 as usize].ty) {
                self.assign_local(*p, Val::borrowed(v), true)?;
            } else {
                // Borrowed: the caller keeps the value alive for the call.
                let slot = self.fs().locals[p.0 as usize].unwrap();
                self.b.build_store(slot.ptr, v).unwrap();
                self.fs_mut().borrowed[p.0 as usize] = true;
                self.clear_const_only(*p);
            }
        }
        if self.fs().coro.is_none() {
            // After the parameters: borrowed ones own nothing.
            self.setup_lpads(f);
        }
        self.dbga_enter(f);
        self.block_stmts(&f.body)?;
        self.br(exit);
        self.b.position_at_end(exit);
        // The epilogue (defers, releases) belongs to the closing brace.
        self.dbg_loc(f.span.end.saturating_sub(1));
        self.dbga_leave();
        self.dbg_keep_coro_locals();
        self.emit_exit()?;
        self.end_fn(saved, None);
        self.dbga_end();
        self.emit_rc_wrapper(fid)
    }

    /// For a borrow-returning function (`CodeGen::funcs_borrow`): defines
    /// `funcs[fid]`, the `.rc` wrapper that calls the body and retains the
    /// result, so that indirect callers get an owned value as usual.
    fn emit_rc_wrapper(&mut self, fid: FuncId) -> R<()> {
        let Some(body) = self.funcs_borrow.get(&fid.0).copied() else { return Ok(()) };
        let w = self.funcs[fid.0 as usize];
        let ret = self.prog.funcs[fid.0 as usize].ret;
        self.with_helper(w, |cg| {
            let args: Vec<BasicMetadataValueEnum> = w.get_params().iter().map(|p| BasicMetadataValueEnum::from(*p)).collect();
            let r = cg.b.build_call(body, &args, "").unwrap().try_as_basic_value().basic().unwrap();
            cg.retain_val(ret, r);
            cg.b.build_return(Some(&r)).unwrap();
            Ok(())
        })
    }

    /// The exit block: deferred calls, release of locals, return.
    fn emit_exit(&mut self) -> R<()> {
        let open = self.fs().open_defers.clone();
        // A deferred call that panics unwinds the rest (its landing pads);
        // the release of the locals does not (a `Drop` panicking there
        // must not release them again).
        self.run_defers(false)?;
        self.without_invokes(|cg| cg.release_locals());
        if self.fs().plain.is_some() {
            self.without_invokes(|cg| cg.plain_async_return());
            return self.emit_unwind_block(open);
        }
        if self.fs().coro.is_some() {
            self.coro_return()?;
            // The unwind path of a panic (ABI.md 8.1): the same defers,
            // each under its armed flag (cleared when it runs).
            let unwind = self.fs().coro.unwrap().unwind;
            self.b.position_at_end(unwind);
            let f = self.fs().hir.expect("coroutine without a body");
            let owns = f.locals.iter().any(|l| self.has_dtor(l.ty) || self.needs_rc(l.ty));
            // The destroy path must see the frame as it is now, not as of
            // the last suspension: the flags, the defer array and the locals
            // stay in memory (escaped at entry).
            let full = !open.is_empty() || self.fs().defers.is_some() || owns || !self.fs().temp_slots.is_empty();
            self.coro_escape_state(&open, full);
            if full {
                self.release_temp_slots();
                self.fs_mut().open_defers = open;
                self.run_defers(true)?;
                self.release_locals();
            }
            self.coro_unwind_exit();
            return Ok(());
        }
        match self.fs().ret_slot {
            Some((p, t)) => {
                let ty = self.llty(t);
                let v = self.b.build_load(ty, p, "ret").unwrap();
                self.b.build_return(Some(&v)).unwrap();
            }
            None => {
                self.b.build_return(None).unwrap();
            }
        }
        self.emit_unwind_block(open)
    }

    /// Releases every scoped temporary (last created first), then every
    /// local slot (last declared first) of the function. Temporaries go
    /// first: they hold copies of values computed from earlier locals (the
    /// sequence of a `for`), and an owner such as an arena must outlive
    /// them, since releasing a slice of arena pointers reads their headers.
    pub(crate) fn release_locals(&mut self) {
        let Some(f) = self.fs().hir else { return };
        let scoped = self.fs().scoped.clone();
        for (p, t) in scoped.into_iter().rev() {
            let ty = self.llty(t);
            let v = self.b.build_load(ty, p, "").unwrap();
            self.release_val(t, v);
        }
        for i in (0..f.locals.len()).rev() {
            self.release_local_final(LocalId(i as u32), false);
        }
    }

    /// Releases the value of local `l` (no-op for zeroed slots, cleared
    /// drop flags and borrowed parameters); with `zero` the slot is reset
    /// so that a later release is a no-op.
    pub(crate) fn release_local(&mut self, l: LocalId, zero: bool) {
        let (info, slot) = self.local_info(l);
        if self.is_interior_local(l) {
            self.release_pins(l);
            return;
        }
        if self.fs().borrowed.get(l.0 as usize).copied().unwrap_or(false) {
            return;
        }
        if let (Some(_), crate::types::Type::Pointer(t)) = (slot.stack_obj, self.ty(info.ty).clone()) {
            // A stack `new` object: release its fields (running `Drop`), not
            // the object; NULL when not initialised (or already released).
            let p = self.b.build_load(self.ptr_ty(), slot.ptr, "sobj").unwrap().into_pointer_value();
            if self.needs_rc(t) {
                let rel = self.block("sobj.drop");
                let done = self.block("sobj.done");
                let nn = self.b.build_is_not_null(p, "").unwrap();
                self.b.build_conditional_branch(nn, rel, done).unwrap();
                self.b.position_at_end(rel);
                self.rc_slot(t, p, false);
                self.b.build_unconditional_branch(done).unwrap();
                self.b.position_at_end(done);
            }
            if zero {
                self.b.build_store(slot.ptr, self.ptr_ty().const_null()).unwrap();
            }
        } else if slot.cell {
            let c = self.b.build_load(self.ptr_ty(), slot.ptr, "cell").unwrap();
            self.call_rt("zell_release", &[c.into()]);
            if zero {
                self.b.build_store(slot.ptr, self.ptr_ty().const_null()).unwrap();
            }
        } else if let Some(flag) = self.fs().flags.get(l.0 as usize).copied().flatten() {
            let live = self.b.build_load(self.ctx.bool_type(), flag, "live").unwrap().into_int_value();
            let rel = self.block("drop.local");
            let done = self.block("drop.done");
            self.b.build_conditional_branch(live, rel, done).unwrap();
            self.b.position_at_end(rel);
            self.b.build_store(flag, self.ctx.bool_type().const_zero()).unwrap();
            self.rc_slot(info.ty, slot.ptr, false);
            self.b.build_unconditional_branch(done).unwrap();
            self.b.position_at_end(done);
        } else if self.needs_rc(info.ty) {
            let ty = self.llty(info.ty);
            let v = self.b.build_load(ty, slot.ptr, "").unwrap();
            self.release_val(info.ty, v);
            if zero {
                self.b.build_store(slot.ptr, ty.const_zero()).unwrap();
            }
        }
    }

    /// [`release_local`](Self::release_local) at a point where every store
    /// into `l` has been emitted (the end of its block, the function exit):
    /// nothing to do when only constants were ever stored
    /// ([`FnState::const_only`]). Early exits (`return`, `break`, `?`) and
    /// a `Let` executed again by a loop come before later stores in the
    /// emission order, so they always release.
    pub(crate) fn release_local_final(&mut self, l: LocalId, zero: bool) {
        let (_, slot) = self.local_info(l);
        if !slot.cell && slot.stack_obj.is_none() && self.fs().const_only.get(l.0 as usize).copied().unwrap_or(false) && !self.is_interior_local(l) {
            if zero {
                let (info, _) = self.local_info(l);
                let ty = self.llty(info.ty);
                self.b.build_store(slot.ptr, ty.const_zero()).unwrap();
            }
            return;
        }
        self.release_local(l, zero);
    }

    /// True if `v`, of a type held as one object pointer (string, `*T`,
    /// map, error ...), is an LLVM constant: NULL or an immortal global
    /// (a literal, a reflection table, an attribute value), which no
    /// retain or release ever touches.
    pub(crate) fn is_const_ref(&self, t: TypeId, v: Val<'ctx>) -> bool {
        matches!(self.rc_kind(t), super::rc::RcKind::Ref | super::rc::RcKind::Ptr) && v.v.is_some_and(|x| x.is_pointer_value() && x.into_pointer_value().is_const())
    }

    /// Notes a store into local `l` that is not a constant pointer (see
    /// [`FnState::const_only`]).
    pub(crate) fn clear_const_only(&mut self, l: LocalId) {
        if let Some(c) = self.fs_mut().const_only.get_mut(l.0 as usize) {
            *c = false;
        }
    }

    /// Runs the deferred calls, last first: the open-coded ones (a function
    /// has either kind, never both) each under its armed flag, then the
    /// heap array. `unwinding` (a panic's unwind path) skips the deferred
    /// awaits (`defer await`): a frame being destroyed cannot suspend.
    pub(crate) fn run_defers(&mut self, unwinding: bool) -> R<()> {
        let open = std::mem::take(&mut self.fs_mut().open_defers);
        for (i, (armed, call)) in open.iter().enumerate().rev() {
            // SAFETY: the pointer was taken from `fs().hir` (a `&'a
            // hir::Function`) by `emit_defer` in this same function body.
            let call: &hir::Expr = unsafe { &**call };
            if unwinding && matches!(call.kind, hir::ExprKind::Await(_)) {
                continue;
            }
            let run = self.block(&format!("defer{i}.run"));
            let next = self.block(&format!("defer{i}.next"));
            let a = self.b.build_load(self.ctx.bool_type(), *armed, "armed").unwrap().into_int_value();
            self.b.build_conditional_branch(a, run, next).unwrap();
            self.b.position_at_end(run);
            // Disarmed first: a panic inside it must not run it again.
            self.b.build_store(*armed, self.ctx.bool_type().const_zero()).unwrap();
            let d = self.depth();
            self.dbg_span(call.span);
            self.expr_discard(call)?;
            self.release_temps_from(d);
            self.br(next);
            self.b.position_at_end(next);
        }
        let Some((arr, n)) = self.fs().defers else { return Ok(()) };
        let check = self.block("defer.check");
        let run = self.block("defer.run");
        let done = self.block("defer.done");
        self.b.build_unconditional_branch(check).unwrap();
        self.b.position_at_end(check);
        let cnt = self.b.build_load(self.i64_ty(), n, "n").unwrap().into_int_value();
        let empty = self.b.build_int_compare(IntPredicate::EQ, cnt, self.i64_ty().const_zero(), "").unwrap();
        self.b.build_conditional_branch(empty, done, run).unwrap();
        self.b.position_at_end(run);
        let n1 = self.b.build_int_sub(cnt, self.i64_ty().const_int(1, false), "").unwrap();
        self.b.build_store(n, n1).unwrap();
        let pair = self.pair_ty();
        let slot = unsafe { self.b.build_in_bounds_gep(pair.array_type(MAX_DEFERS), arr, &[self.i64_ty().const_zero(), n1], "") }.unwrap();
        let c = self.b.build_load(pair, slot, "").unwrap().into_struct_value();
        let fnp = self.b.build_extract_value(c, 0, "").unwrap().into_pointer_value();
        let env = self.b.build_extract_value(c, 1, "").unwrap();
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        self.call_site(fty, inkwell::values::AsValueRef::as_value_ref(&fnp), &[env.into()], true);
        self.call_rt("zell_release", &[env.into()]);
        self.b.build_unconditional_branch(check).unwrap();
        self.b.position_at_end(done);
        Ok(())
    }

    /// `defer`: open-coded (the operand is the call) arms the statement's
    /// flag; otherwise the closure is pushed on the defer array.
    pub(crate) fn emit_defer(&mut self, e: &hir::Expr) -> R<()> {
        if self.fs().hir.is_none() {
            return super::unsupported("defer outside a function body");
        }
        if !matches!(e.kind, hir::ExprKind::Closure { .. }) {
            let armed = self.entry_alloca(self.ctx.bool_type().into(), "defer.armed", true);
            self.b.build_store(armed, self.ctx.bool_type().const_int(1, false)).unwrap();
            self.fs_mut().open_defers.push((armed, e as *const hir::Expr));
            return Ok(());
        }
        let (arr, n) = match self.fs().defers {
            Some(d) => d,
            None => {
                // Allocated on first use; the exit block is emitted last.
                let pair = self.pair_ty();
                let arr = self.entry_alloca(pair.array_type(MAX_DEFERS).into(), "defers", false);
                let n = self.entry_alloca(self.i64_ty().into(), "ndefers", true);
                self.fs_mut().defers = Some((arr, n));
                (arr, n)
            }
        };
        let c = self.expr_owned(e)?;
        let cnt = self.b.build_load(self.i64_ty(), n, "n").unwrap().into_int_value();
        let full = self.b.build_int_compare(IntPredicate::UGE, cnt, self.i64_ty().const_int(MAX_DEFERS as u64, false), "").unwrap();
        let bad = self.block("defer.full");
        let ok = self.block("defer.ok");
        self.b.build_conditional_branch(full, bad, ok).unwrap();
        self.b.position_at_end(bad);
        let msg = self.cstr("too many pending deferred calls (limit 64)");
        self.call_noreturn("zell_panic", &[msg.into()]);
        self.br(ok);
        self.b.position_at_end(ok);
        let pair = self.pair_ty();
        let slot = unsafe { self.b.build_in_bounds_gep(pair.array_type(MAX_DEFERS), arr, &[self.i64_ty().const_zero(), cnt], "") }.unwrap();
        self.b.build_store(slot, c).unwrap();
        let n1 = self.b.build_int_add(cnt, self.i64_ty().const_int(1, false), "").unwrap();
        self.b.build_store(n, n1).unwrap();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Locals and cells
    // ------------------------------------------------------------------

    pub(crate) fn local_info(&self, l: LocalId) -> (&'a hir::Local, LocalSlot<'ctx>) {
        let f = self.fs().hir.expect("codegen: local outside a function");
        (&f.locals[l.0 as usize], self.fs().locals[l.0 as usize].expect("codegen: local without slot"))
    }

    /// A stack object `{i64 -1, ptr null, T}` (immortal header, zeroed
    /// payload) in the entry block; returns the payload address.
    pub(crate) fn stack_object(&mut self, t: TypeId, name: &str) -> PointerValue<'ctx> {
        let st = self.ctx.struct_type(&[self.i64_ty().into(), self.ptr_ty().into(), self.llty(t)], false);
        let obj = self.entry_alloca(st.into(), name, true);
        // The header is written once, at function entry, like the zeroing.
        let ib = &self.fs().init_b;
        ib.build_store(obj, self.i64_ty().const_all_ones()).unwrap();
        unsafe { ib.build_in_bounds_gep(self.ctx.i8_type(), obj, &[self.i64_ty().const_int(16, false)], "") }.unwrap()
    }

    /// Address of the value of local `l`.
    pub(crate) fn local_addr(&mut self, l: LocalId) -> PointerValue<'ctx> {
        let (_, slot) = self.local_info(l);
        if slot.cell {
            let c = self.b.build_load(self.ptr_ty(), slot.ptr, "cell").unwrap().into_pointer_value();
            self.payload(c)
        } else {
            slot.ptr
        }
    }

    /// Address of upvalue `i` of the current lambda.
    pub(crate) fn upvalue_addr(&mut self, i: u32) -> PointerValue<'ctx> {
        if let Some(t) = self.fs().hir.and_then(|f| f.value_env) {
            // Element `i` of the tuple in the box.
            let env = self.fs().env.expect("codegen: upvalue outside a lambda");
            let st = self.llty(t).into_struct_type();
            let off = self.td.offset_of_element(&st, i).unwrap_or(0);
            return self.byte_gep(env, 16 + off);
        }
        let cell = self.upvalue_cell(i);
        self.payload(cell)
    }

    pub(crate) fn upvalue_cell_ptr(&mut self, i: u32) -> PointerValue<'ctx> {
        self.upvalue_cell(i)
    }

    fn upvalue_cell(&mut self, i: u32) -> PointerValue<'ctx> {
        let env = self.fs().env.expect("codegen: upvalue outside a lambda");
        // A value environment (expression trees): the box itself owns the
        // values, so it stands in for every cell.
        if self.fs().hir.is_some_and(|f| f.value_env.is_some()) {
            return env;
        }
        let p = self.byte_gep(env, 24 + 8 * i as u64);
        self.b.build_load(self.ptr_ty(), p, "upcell").unwrap().into_pointer_value()
    }

    /// `obj + 16`: the payload of a heap object.
    pub(crate) fn payload(&mut self, obj: PointerValue<'ctx>) -> PointerValue<'ctx> {
        self.byte_gep(obj, 16)
    }

    pub(crate) fn byte_gep(&mut self, p: PointerValue<'ctx>, off: u64) -> PointerValue<'ctx> {
        unsafe { self.b.build_in_bounds_gep(self.ctx.i8_type(), p, &[self.i64_ty().const_int(off, false)], "") }.unwrap()
    }

    /// Stores `v` into local `l`. With `fresh` (declaration, loop variable,
    /// binding, parameter) the previous value of the slot (from an earlier
    /// execution) is released and a captured local gets a new cell.
    pub(crate) fn assign_local(&mut self, l: LocalId, v: Val<'ctx>, fresh: bool) -> R<()> {
        let (info, slot) = self.local_info(l);
        let const_ref = self.is_const_ref(info.ty, v);
        let depth = self.fs().loops.len();
        let old_const = self.fs().const_only.get(l.0 as usize).copied().unwrap_or(false);
        let same_depth = self.fs().const_depth.get(l.0 as usize).copied() == Some(depth);
        if !const_ref {
            self.clear_const_only(l);
        } else if fresh {
            if let Some(d) = self.fs_mut().const_depth.get_mut(l.0 as usize) {
                *d = depth;
            }
        }
        if !fresh && old_const && same_depth && !slot.cell && !self.is_interior_local(l) {
            // The slot holds the constant of its Let (no loop in between):
            // store without releasing it.
            let x = self.to_owned(info.ty, v);
            self.b.build_store(slot.ptr, x).unwrap();
            return Ok(());
        }
        if fresh && self.fs().borrow_local.get(l.0 as usize).copied().unwrap_or(false) {
            // A borrowed local: the value is kept alive by the place it was
            // read from. An owned value (not expected from a path) is kept
            // until the function exits instead.
            self.dbg_declare(l);
            let x = if v.owned {
                let p = self.scoped_slot_owned(info.ty, v.get());
                let lt = self.llty(info.ty);
                self.b.build_load(lt, p, "").unwrap()
            } else {
                v.get()
            };
            self.b.build_store(slot.ptr, x).unwrap();
            return Ok(());
        }
        if fresh {
            self.dbg_declare(l);
            let owned = self.to_owned(info.ty, v);
            self.release_local(l, false);
            if slot.cell {
                let cell = self.new_box(info.ty, owned);
                self.b.build_store(slot.ptr, cell).unwrap();
            } else {
                self.b.build_store(slot.ptr, owned).unwrap();
                if let Some(flag) = self.fs().flags.get(l.0 as usize).copied().flatten() {
                    self.b.build_store(flag, self.ctx.bool_type().const_int(1, false)).unwrap();
                }
            }
        } else {
            let p = self.local_addr(l);
            self.store_assign(p, info.ty, v);
        }
        Ok(())
    }

    /// Resets local `l` to its zero value (`var x T` executed again).
    pub(crate) fn zero_local(&mut self, l: LocalId) -> R<()> {
        let (info, _) = self.local_info(l);
        let z = self.llty(info.ty).const_zero();
        self.assign_local(l, Val::borrowed(z), true)
    }

    // ------------------------------------------------------------------
    // Closures
    // ------------------------------------------------------------------

    /// Shared type info of closure environments: `{hdr, i64 n, ptr cells[n]}`.
    fn env_type_info(&mut self) -> PointerValue<'ctx> {
        if let Some(g) = self.env_ti {
            return g.as_pointer_value();
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let drop = self.module.add_function("zell.env.drop", fty, Some(inkwell::module::Linkage::Internal));
        let _ = self.with_helper(drop, |cg| {
            let obj = drop.get_nth_param(0).unwrap().into_pointer_value();
            let np = cg.byte_gep(obj, 16);
            let n = cg.b.build_load(cg.i64_ty(), np, "n").unwrap().into_int_value();
            let cells = cg.byte_gep(obj, 24);
            let i = cg.entry_alloca(cg.i64_ty().into(), "i", true);
            let head = cg.block("head");
            let body = cg.block("body");
            let done = cg.block("done");
            cg.b.build_unconditional_branch(head).unwrap();
            cg.b.position_at_end(head);
            let iv = cg.b.build_load(cg.i64_ty(), i, "").unwrap().into_int_value();
            let c = cg.b.build_int_compare(IntPredicate::ULT, iv, n, "").unwrap();
            cg.b.build_conditional_branch(c, body, done).unwrap();
            cg.b.position_at_end(body);
            let cp = unsafe { cg.b.build_in_bounds_gep(cg.ptr_ty(), cells, &[iv], "") }.unwrap();
            let cell = cg.b.build_load(cg.ptr_ty(), cp, "").unwrap();
            cg.call_rt("zell_release", &[cell.into()]);
            let i1 = cg.b.build_int_add(iv, cg.i64_ty().const_int(1, false), "").unwrap();
            cg.b.build_store(i, i1).unwrap();
            cg.b.build_unconditional_branch(head).unwrap();
            cg.b.position_at_end(done);
            cg.b.build_return(None).unwrap();
            Ok(())
        });
        let name = self.cstr("closure env");
        let tity = self.type_info_ty();
        let init = tity.const_named_struct(&[name.into(), self.i64_ty().const_zero().into(), drop.as_global_value().as_pointer_value().into()]);
        let g = self.module.add_global(tity, None, "ti.env");
        g.set_linkage(inkwell::module::Linkage::Internal);
        g.set_constant(true);
        g.set_initializer(&init);
        self.env_ti = Some(g);
        g.as_pointer_value()
    }

    /// `closure func [captures]`: an owned `{fn, env}`.
    pub(crate) fn emit_closure(&mut self, func: FuncId, captures: &[CaptureSource]) -> R<Val<'ctx>> {
        let fv = self.closure_entry(func)?;
        let env = if captures.is_empty() {
            self.ptr_ty().const_null()
        } else {
            let shared = self.env_type_info();
            // Environments holding cells of cyclic types are flagged for
            // the cycle collector and carry their own type info (codegen/cycles.rs).
            let size = self.i64_ty().const_int(24 + 8 * captures.len() as u64, false);
            let obj = match self.cyc_env_entry(func) {
                Some(e) => self.call_rt_v("zell_cycles_new_entry", &[e.into(), size.into()]),
                None => self.call_rt_v("zell_object_new", &[shared.into(), size.into()]),
            }
            .into_pointer_value();
            let np = self.byte_gep(obj, 16);
            self.b.build_store(np, self.i64_ty().const_int(captures.len() as u64, false)).unwrap();
            for (i, c) in captures.iter().enumerate() {
                let cell = match c {
                    CaptureSource::Local(l) => {
                        let (_, slot) = self.local_info(*l);
                        if !slot.cell {
                            return super::unsupported("capture of a local without a cell");
                        }
                        self.b.build_load(self.ptr_ty(), slot.ptr, "").unwrap().into_pointer_value()
                    }
                    CaptureSource::Upvalue(u) => self.upvalue_cell(*u),
                };
                self.call_rt("zell_retain", &[cell.into()]);
                let p = self.byte_gep(obj, 24 + 8 * i as u64);
                self.b.build_store(p, cell).unwrap();
            }
            obj
        };
        let v = self.make_pair(fv.as_global_value().as_pointer_value(), env);
        Ok(Val::owned(v))
    }

    /// [`hir::ExprKind::EnvClosures`]: the closures of `funcs` (a tuple of
    /// type `ty`) over one box holding the value of `env`, or over NULL.
    pub(crate) fn emit_env_closures(&mut self, ty: TypeId, funcs: &[FuncId], env: Option<&hir::Expr>) -> R<Val<'ctx>> {
        let envp = match env {
            None => self.ptr_ty().const_null(),
            Some(x) => {
                let v = self.expr_owned(x)?;
                self.new_box(x.ty, v)
            }
        };
        let tt = self.llty(ty).into_struct_type();
        let mut agg = tt.get_undef();
        for (i, f) in funcs.iter().enumerate() {
            if i > 0 && env.is_some() {
                self.call_rt("zell_retain", &[envp.into()]);
            }
            let fv = self.closure_entry(*f)?;
            let pair = self.make_pair(fv.as_global_value().as_pointer_value(), envp);
            agg = self.b.build_insert_value(agg, pair, i as u32, "").unwrap().into_struct_value();
        }
        Ok(Val::owned(agg.into()))
    }

    pub(crate) fn make_pair(&mut self, a: PointerValue<'ctx>, b: PointerValue<'ctx>) -> BasicValueEnum<'ctx> {
        let pt = self.pair_ty();
        let s = self.b.build_insert_value(pt.get_undef(), a, 0, "").unwrap();
        self.b.build_insert_value(s, b, 1, "").unwrap().into_struct_value().into()
    }

    /// A named function used as a value: `{thunk, null}`.
    pub(crate) fn func_ref(&mut self, fid: FuncId, fty: TypeId) -> R<Val<'ctx>> {
        let thunk = self.func_thunk(fid, fty)?;
        let v = self.make_pair(thunk.as_global_value().as_pointer_value(), self.ptr_ty().const_null());
        Ok(Val::borrowed(v))
    }

    fn func_thunk(&mut self, fid: FuncId, fty: TypeId) -> R<FunctionValue<'ctx>> {
        if let Some(t) = self.thunks.get(&fid) {
            return Ok(*t);
        }
        let f = &self.prog.funcs[fid.0 as usize];
        if matches!(f.kind, FuncKind::Lambda { .. }) {
            return super::unsupported("a lambda referenced by name");
        }
        let lty = self.closure_fn_llty(fty);
        let thunk = self.module.add_function(&format!("{}$thunk", f.symbol), lty, Some(inkwell::module::Linkage::Internal));
        self.thunks.insert(fid, thunk);
        let target = self.funcs[fid.0 as usize];
        let (is_async, ret) = (f.is_async, f.ret);
        self.with_helper(thunk, |cg| {
            let args: Vec<BasicMetadataValueEnum> = thunk.get_params().iter().skip(1).map(|p| (*p).into()).collect();
            if is_async {
                let t = cg.call_async_thunk(target, ret, args);
                cg.b.build_return(Some(&t)).unwrap();
                return Ok(());
            }
            let r = cg.b.build_call(target, &args, "").unwrap().try_as_basic_value().basic();
            match r {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })?;
        Ok(thunk)
    }

    /// Calls async function `target` (result type `ret`) with `args` and
    /// returns its `Task<T>` (+1), materialising a completed one when the
    /// call never suspended: the body of thunks that must return a task.
    pub(crate) fn call_async_thunk(&mut self, target: FunctionValue<'ctx>, ret: TypeId, mut args: Vec<BasicMetadataValueEnum<'ctx>>) -> PointerValue<'ctx> {
        let rty = self.coro_res_ty(ret);
        let res = self.entry_alloca(rty.into(), "res", false);
        self.coro_res_init(ret, res, None);
        args.push(res.into());
        self.call_fn(target, &args);
        self.coro_res_task(ret, res)
    }

    /// The function pointer stored in a closure value of lambda `func`:
    /// the lambda itself, or for an async lambda a thunk `(env, params)
    /// -> Task<T>` over its ramp (which returns through a result slot).
    pub(crate) fn closure_entry(&mut self, func: FuncId) -> R<FunctionValue<'ctx>> {
        let f = &self.prog.funcs[func.0 as usize];
        if !f.is_async {
            return Ok(self.funcs[func.0 as usize]);
        }
        if let Some(t) = self.thunks.get(&func) {
            return Ok(*t);
        }
        let (symbol, ret) = (f.symbol.clone(), f.ret);
        let target = self.funcs[func.0 as usize];
        let mut ps: Vec<inkwell::types::BasicMetadataTypeEnum> = Vec::new();
        for p in &f.params {
            let t = f.locals[p.0 as usize].ty;
            ps.push(self.llty(t).into());
        }
        ps.insert(0, self.ptr_ty().into());
        let lty = self.ptr_ty().fn_type(&ps, false);
        let thunk = self.module.add_function(&format!("{symbol}$thunk"), lty, Some(inkwell::module::Linkage::Internal));
        self.thunks.insert(func, thunk);
        self.with_helper(thunk, |cg| {
            let args: Vec<BasicMetadataValueEnum> = thunk.get_params().iter().map(|p| (*p).into()).collect();
            let t = cg.call_async_thunk(target, ret, args);
            cg.b.build_return(Some(&t)).unwrap();
            Ok(())
        })?;
        Ok(thunk)
    }

    /// Emits helper function `fv` with `body`, saving and restoring the
    /// current function state and insertion point.
    pub(crate) fn with_helper<T>(&mut self, fv: FunctionValue<'ctx>, body: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let pos = self.b.get_insert_block();
        let saved = self.begin_fn(fv, None);
        let r = body(self);
        self.end_fn(saved, pos);
        r
    }

    /// Calls closure value `c` (of Zell type `fty`) with `args`.
    pub(crate) fn call_closure(&mut self, fty: TypeId, c: BasicValueEnum<'ctx>, args: &[BasicValueEnum<'ctx>]) -> R<Option<BasicValueEnum<'ctx>>> {
        let cs = c.into_struct_value();
        let fnp = self.b.build_extract_value(cs, 0, "fn").unwrap().into_pointer_value();
        let env = self.b.build_extract_value(cs, 1, "env").unwrap();
        // Calling a nil func value panics.
        self.nil_check(fnp);
        let lty = self.closure_fn_llty(fty);
        let mut all: Vec<BasicMetadataValueEnum> = vec![env.into()];
        all.extend(args.iter().map(|a| BasicMetadataValueEnum::from(*a)));
        Ok(self.call_site(lty, inkwell::values::AsValueRef::as_value_ref(&fnp), &all, true).try_as_basic_value().basic())
    }

    /// Panics with `zell_panic_nil` if `p` is NULL.
    pub(crate) fn nil_check(&mut self, p: PointerValue<'ctx>) {
        let isnull = self.b.build_is_null(p, "isnil").unwrap();
        let bad = self.block("nil");
        let ok = self.block("notnil");
        self.b.build_conditional_branch(isnull, bad, ok).unwrap();
        self.b.position_at_end(bad);
        self.call_rt("zell_panic_nil", &[]);
        self.b.build_unreachable().unwrap();
        self.b.position_at_end(ok);
    }

}
