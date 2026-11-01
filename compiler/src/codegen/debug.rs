//! DWARF debug information (docs/DEBUGGER.md, stage 1) through LLVM's
//! `DIBuilder` (the C API in `llvm_sys::debuginfo`; inkwell's wrapper lacks
//! enumerations and replaceable types).
//!
//! * One compile unit per source file (`DW_LANG_C11`, so lldb/gdb use their
//!   C type systems and expression parsers), DWARF 5.
//! * One `DISubprogram` per HIR function, named by its full name
//!   (`app.Max<int>`, `app.Point.Length`, `app.main$lambda1`) with the
//!   symbol as linkage name. Stdlib functions (no source) and helpers
//!   (thunks, rc helpers, C `main`) get none.
//! * Parameters and named locals are described with `llvm.dbg.declare` on
//!   their entry allocas (cells: `DW_OP_deref, DW_OP_plus_uconst 16`);
//!   nested blocks are `DILexicalBlock`s; every statement, call and `await`
//!   sets the builder's location. Coroutine frames: CoroSplit rewrites the
//!   declares of allocas it moves into the frame, so locals stay visible in
//!   the resume functions after an `await`.
//! * Types follow the value representation of [`super`] exactly; names are
//!   the source spellings ([`crate::hir::Program::type_name`]), so the lldb
//!   and gdb formatters in `tools/` match them by name. Pointer-represented
//!   runtime values are *handles*: an 8-byte struct `{T *ptr}`, so that
//!   debuggers display just the formatter's summary.
//!
//! | Zell | DWARF |
//! |---|---|
//! | `int`, `uint8`, ... | base types (`char` is `DW_ATE_UTF`, `bool` `DW_ATE_boolean`) |
//! | `string` | handle `string {zell_string *ptr}`, `zell_string {refcount, type, len, data[]}` |
//! | `[]T` | struct `[]T {T *data; int len; int cap; void *backing}` |
//! | `[N]T` | array |
//! | `*T` | pointer to `T` (the payload) |
//! | `weak *T` | handle -> `zell_weak<T> {hdr, zell_obj<T> *target}` |
//! | `map[K]V` | handle -> `zell_map[K]V` (runtime table, ABI.md §6, with `slots` typed as `zell_map_entry[K]V {key, value}`) |
//! | `error` | handle `error {zell_error *ptr}` (ABI.md §7.1) |
//! | `T!` | struct `T! {T value; error err}`; `void!` is a typedef of `error` |
//! | tuple | struct `(A, B) {_0, _1}` |
//! | plain enum | enumeration |
//! | data enum | struct `E {E.Tag tag; union E.payload {struct E.V V; ...}}` |
//! | interface | struct `I {void *data; zell_vtable *vtable}`; `any` uses `zell_type_info *type` |
//! | `func(..)` | struct `func(..) {fnptr fn; void *env}` |
//! | `Task<T>` | handle -> the opaque `zell.task` (formatters read it through `zell_debug_task_layout`, ABI.md §20) |

use super::CodeGen;
use crate::hir::{self, LocalId, LocalKind};
use crate::span::Span;
use crate::types::{Type, TypeId};
use inkwell::types::{BasicTypeEnum, StructType};
use inkwell::values::{AsValueRef, BasicValue, FunctionValue};
use llvm_sys::core as llc;
use llvm_sys::debuginfo as di;
use llvm_sys::prelude::{LLVMDIBuilderRef, LLVMMetadataRef};
use std::collections::HashMap;

type Meta = LLVMMetadataRef;

const DW_ATE_BOOLEAN: u32 = 0x02;
const DW_ATE_FLOAT: u32 = 0x04;
const DW_ATE_SIGNED: u32 = 0x05;
const DW_ATE_SIGNED_CHAR: u32 = 0x06;
const DW_ATE_UNSIGNED: u32 = 0x07;
const DW_ATE_UTF: u32 = 0x10;
const DW_TAG_STRUCTURE_TYPE: u32 = 0x13;
const DW_OP_DEREF: u64 = 0x06;
const DW_OP_PLUS_UCONST: u64 = 0x23;
/// DWARF version written into the module flags.
pub(crate) const DWARF_VERSION: u64 = 5;

/// One compile unit.
struct Unit {
    b: LLVMDIBuilderRef,
    file: Meta,
}

/// Debug state of the function being emitted.
struct FnDbg {
    /// `None` for helpers and functions without source.
    sp: Option<Meta>,
    unit: usize,
    /// Lexical scopes, innermost last (the subprogram first).
    scopes: Vec<Meta>,
    declared: Vec<bool>,
    /// The builder's location before this function started.
    saved_loc: Meta,
}

/// Module-wide debug-info state ([`CodeGen::dbg`]).
pub(crate) struct DebugInfo {
    units: Vec<Unit>,
    types: HashMap<TypeId, Meta>,
    named: HashMap<&'static str, Meta>,
    stack: Vec<FnDbg>,
    /// Full debug info; `false`: line tables only ([`super::Options::line_tables`]).
    full: bool,
}

fn s(x: &str) -> (*const libc_char, usize) {
    (x.as_ptr() as *const libc_char, x.len())
}

#[allow(non_camel_case_types)]
type libc_char = std::os::raw::c_char;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Creates the compile units (one per source file) when debug info is
    /// requested and the program has sources.
    pub(crate) fn init_debug(&mut self) {
        if !(self.opts.debug_info || self.opts.line_tables) || self.prog.sources.is_empty() {
            return;
        }
        let m = self.module.as_mut_ptr();
        let mut units = Vec::new();
        let producer = concat!("zell ", env!("CARGO_PKG_VERSION"));
        for src in &self.prog.sources {
            let p = std::path::Path::new(&src.path);
            let abs = crate::paths::canonical_or_absolute(p);
            let name = abs.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| src.path.clone());
            let dir = abs.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
            unsafe {
                let b = di::LLVMCreateDIBuilder(m);
                let (np, nl) = s(&name);
                let (dp, dl) = s(&dir);
                let file = di::LLVMDIBuilderCreateFile(b, np, nl, dp, dl);
                let (pp, pl) = s(producer);
                di::LLVMDIBuilderCreateCompileUnit(
                    b,
                    di::LLVMDWARFSourceLanguage::LLVMDWARFSourceLanguageC11,
                    file,
                    pp,
                    pl,
                    (self.opts.optimized) as i32,
                    std::ptr::null(),
                    0,
                    0,
                    std::ptr::null(),
                    0,
                    if self.opts.debug_info { di::LLVMDWARFEmissionKind::LLVMDWARFEmissionKindFull } else { di::LLVMDWARFEmissionKind::LLVMDWARFEmissionKindLineTablesOnly },
                    0,
                    0,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                );
                units.push(Unit { b, file });
            }
        }
        unsafe {
            let i32t = self.ctx.i32_type();
            let ver = llc::LLVMValueAsMetadata(i32t.const_int(di::LLVMDebugMetadataVersion() as u64, false).as_value_ref());
            let k = "Debug Info Version";
            llc::LLVMAddModuleFlag(m, llvm_sys::LLVMModuleFlagBehavior::LLVMModuleFlagBehaviorWarning, k.as_ptr() as *const _, k.len(), ver);
            if crate::target::current().is_msvc() {
                // MSVC ABI: CodeView (PDB via lld-link /debug), for Visual Studio and WinDbg.
                let one = llc::LLVMValueAsMetadata(i32t.const_int(1, false).as_value_ref());
                let k = "CodeView";
                llc::LLVMAddModuleFlag(m, llvm_sys::LLVMModuleFlagBehavior::LLVMModuleFlagBehaviorWarning, k.as_ptr() as *const _, k.len(), one);
            } else {
                // DWARF everywhere else, MinGW included (gdb, lldb).
                let dv = llc::LLVMValueAsMetadata(i32t.const_int(DWARF_VERSION, false).as_value_ref());
                let k = "Dwarf Version";
                llc::LLVMAddModuleFlag(m, llvm_sys::LLVMModuleFlagBehavior::LLVMModuleFlagBehaviorWarning, k.as_ptr() as *const _, k.len(), dv);
            }
        }
        self.dbg = Some(Box::new(DebugInfo { units, types: HashMap::new(), named: HashMap::new(), stack: Vec::new(), full: self.opts.debug_info }));
    }

    /// Describes the globals and finalises every `DIBuilder`.
    pub(crate) fn finish_debug(&mut self) {
        if self.dbg.is_none() {
            return;
        }
        let full = self.dbg.as_ref().unwrap().full;
        for (i, g) in self.prog.globals.iter().enumerate().filter(|_| full) {
            if crate::sema::stdlib::is_std_namespace(&g.full_name) || g.file >= self.prog.sources.len() {
                continue;
            }
            let ty = self.di_type(g.ty);
            let (line, _) = self.line_col(g.file, g.span.start);
            let gv = self.globals[i];
            let d = self.dbg.as_ref().unwrap();
            let u = &d.units[g.file];
            unsafe {
                let (np, nl) = s(&g.full_name);
                let (lp, ll) = s(&g.symbol);
                let expr = di::LLVMDIBuilderCreateExpression(u.b, std::ptr::null_mut(), 0);
                let gve = di::LLVMDIBuilderCreateGlobalVariableExpression(u.b, u.file, np, nl, lp, ll, u.file, line, ty, 1, expr, std::ptr::null_mut(), 0);
                llvm_sys::core::LLVMGlobalSetMetadata(gv.as_value_ref(), llc::LLVMGetMDKindIDInContext(self.ctx.raw(), c"dbg".as_ptr(), 3), gve);
            }
        }
        // Calls emitted without a location (prologues, helpers' callers)
        // get line 0 in their function: the verifier requires a location on
        // every call to a function with a subprogram, which helpers have.
        unsafe {
            let mut f = llc::LLVMGetFirstFunction(self.module.as_mut_ptr());
            while !f.is_null() {
                let sp = di::LLVMGetSubprogram(f);
                if !sp.is_null() {
                    let loc = di::LLVMDIBuilderCreateDebugLocation(self.ctx.raw(), 0, 0, sp, std::ptr::null_mut());
                    let mut bb = llc::LLVMGetFirstBasicBlock(f);
                    while !bb.is_null() {
                        let mut i = llc::LLVMGetFirstInstruction(bb);
                        while !i.is_null() {
                            if (!llc::LLVMIsACallInst(i).is_null() || !llc::LLVMIsAInvokeInst(i).is_null()) && di::LLVMInstructionGetDebugLoc(i).is_null() {
                                di::LLVMInstructionSetDebugLoc(i, loc);
                            }
                            i = llc::LLVMGetNextInstruction(i);
                        }
                        bb = llc::LLVMGetNextBasicBlock(bb);
                    }
                }
                f = llc::LLVMGetNextFunction(f);
            }
        }
        let d = self.dbg.take().unwrap();
        for u in &d.units {
            unsafe {
                di::LLVMDIBuilderFinalize(u.b);
                di::LLVMDisposeDIBuilder(u.b);
            }
        }
    }

    /// 1-based line and column of byte `offset` in file `file`.
    fn line_col(&self, file: usize, offset: u32) -> (u32, u32) {
        match self.prog.sources.get(file) {
            Some(src) => {
                let line = src.line_of(offset);
                let start = src.line_starts.get(line as usize - 1).copied().unwrap_or(0);
                (line, offset.saturating_sub(start) + 1)
            }
            None => (0, 0),
        }
    }

    /// Whether HIR function `f` gets a subprogram.
    fn has_source(&self, f: &hir::Function) -> bool {
        !crate::sema::stdlib::is_std_namespace(&f.full_name) && f.file < self.prog.sources.len() && f.span != Span::DUMMY
    }

    /// Hook of [`CodeGen::begin_fn`]: starts the subprogram of `func`.
    pub(crate) fn dbg_begin_fn(&mut self, func: FunctionValue<'ctx>, hir: Option<&'a hir::Function>) {
        if self.dbg.is_none() {
            return;
        }
        let bref = self.b.as_mut_ptr();
        let saved_loc = unsafe { llc::LLVMGetCurrentDebugLocation2(bref) };
        let f = hir.filter(|f| self.has_source(f));
        let Some(f) = f else {
            // Helpers and stdlib functions get an artificial subprogram at
            // line 0, so that Zell code inlined into them keeps its
            // locations (LLVM drops the locations of code inlined into a
            // function without a subprogram), e.g. `main` into C `main`.
            let name = func.get_name().to_string_lossy().into_owned();
            let (unit, line) = (0, 0);
            let d = self.dbg.as_ref().unwrap();
            let u = &d.units[unit];
            let sp = unsafe {
                let st = di::LLVMDIBuilderCreateSubroutineType(u.b, u.file, std::ptr::null_mut(), 0, di::LLVMDIFlagArtificial);
                let (np, nl) = s(&name);
                let sp = di::LLVMDIBuilderCreateFunction(u.b, u.file, np, nl, np, nl, u.file, line, st, 1, 1, line, di::LLVMDIFlagArtificial, (self.opts.optimized) as i32);
                di::LLVMSetSubprogram(func.as_value_ref(), sp);
                let loc = di::LLVMDIBuilderCreateDebugLocation(self.ctx.raw(), line, 0, sp, std::ptr::null_mut());
                llc::LLVMSetCurrentDebugLocation2(bref, loc);
                sp
            };
            self.dbg.as_mut().unwrap().stack.push(FnDbg { sp: None, unit, scopes: vec![sp], declared: Vec::new(), saved_loc });
            return;
        };
        let mut tys = Vec::new();
        if self.dbg.as_ref().unwrap().full {
            tys.push(if f.is_async { self.di_named_task(f.ret) } else { self.di_ret(f.ret) });
            for p in &f.params {
                tys.push(self.di_type(f.locals[p.0 as usize].ty));
            }
        }
        let (line, _) = self.line_col(f.file, f.span.start);
        let d = self.dbg.as_ref().unwrap();
        let u = &d.units[f.file];
        let sp = unsafe {
            let st = di::LLVMDIBuilderCreateSubroutineType(u.b, u.file, tys.as_mut_ptr(), tys.len() as u32, di::LLVMDIFlagPrototyped);
            let (np, nl) = s(&f.full_name);
            let sp = di::LLVMDIBuilderCreateFunction(u.b, u.file, np, nl, std::ptr::null(), 0, u.file, line, st, 1, 1, line, di::LLVMDIFlagPrototyped, (self.opts.optimized) as i32);
            di::LLVMSetSubprogram(func.as_value_ref(), sp);
            sp
        };
        self.dbg.as_mut().unwrap().stack.push(FnDbg { sp: Some(sp), unit: f.file, scopes: vec![sp], declared: vec![false; f.locals.len()], saved_loc });
        // Like clang, the prologue (task creation, parameter stores) has no
        // location, so `prologue_end` and function breakpoints land on the
        // first statement, after the parameters are stored.
        unsafe { llc::LLVMSetCurrentDebugLocation2(bref, std::ptr::null_mut()) };
    }

    /// Hook of [`CodeGen::end_fn`]: restores the enclosing location.
    pub(crate) fn dbg_end_fn(&mut self) {
        let Some(d) = self.dbg.as_mut() else { return };
        if let Some(fd) = d.stack.pop() {
            unsafe { llc::LLVMSetCurrentDebugLocation2(self.b.as_mut_ptr(), fd.saved_loc) };
        }
    }

    fn cur_fn(&self) -> Option<(&FnDbg, &DebugInfo)> {
        let d = self.dbg.as_deref()?;
        let fd = d.stack.last()?;
        fd.sp?;
        Some((fd, d))
    }

    /// Sets the builder's location to byte `offset` of the current
    /// function's file (no-op without debug info or for dummy spans).
    pub(crate) fn dbg_loc(&mut self, offset: u32) {
        let Some((fd, _)) = self.cur_fn() else { return };
        let (unit, scope) = (fd.unit, *fd.scopes.last().unwrap());
        let (line, col) = self.line_col(unit, offset);
        unsafe {
            let loc = di::LLVMDIBuilderCreateDebugLocation(self.ctx.raw(), line, col, scope, std::ptr::null_mut());
            llc::LLVMSetCurrentDebugLocation2(self.b.as_mut_ptr(), loc);
        }
    }

    /// [`Self::dbg_loc`] for a span (ignores dummy spans).
    pub(crate) fn dbg_span(&mut self, sp: Span) {
        if sp != Span::DUMMY {
            self.dbg_loc(sp.start);
        }
    }

    /// Opens a lexical block starting at `sp`.
    pub(crate) fn dbg_push_block(&mut self, sp: Span) {
        self.dbga_push_block(sp);
        let Some((fd, d)) = self.cur_fn() else { return };
        let (unit, parent) = (fd.unit, *fd.scopes.last().unwrap());
        let u = &d.units[unit];
        let (b, file) = (u.b, u.file);
        let (line, col) = self.line_col(unit, sp.start);
        let blk = unsafe { di::LLVMDIBuilderCreateLexicalBlock(b, parent, file, line, col) };
        self.dbg.as_mut().unwrap().stack.last_mut().unwrap().scopes.push(blk);
    }

    /// Closes the innermost lexical block.
    pub(crate) fn dbg_pop_block(&mut self) {
        self.dbga_pop_block();
        if self.cur_fn().is_none() {
            return;
        }
        let fd = self.dbg.as_mut().unwrap().stack.last_mut().unwrap();
        if fd.scopes.len() > 1 {
            fd.scopes.pop();
        }
    }

    /// Declares the parameters of the current function (after the slots exist).
    pub(crate) fn dbg_params(&mut self, f: &hir::Function) {
        if self.cur_fn().is_none_or(|(_, d)| !d.full) {
            return;
        }
        for (k, p) in f.params.iter().enumerate() {
            self.dbg_declare_as(*p, Some(k as u32 + 1));
        }
        // The result slot, named like Delve's unnamed results: shows the
        // value being returned while stopped at the closing brace.
        if let Some((slot, ty)) = self.fs().ret_slot {
            let dt = self.di_type(ty);
            let (fd, d) = self.cur_fn().unwrap();
            let u = &d.units[fd.unit];
            let (b, file, scope) = (u.b, u.file, fd.scopes[0]);
            let (line, col) = self.line_col(fd.unit, f.span.start);
            let Some(bb) = self.b.get_insert_block() else { return };
            unsafe {
                let (np, nl) = s("~r0");
                let var = di::LLVMDIBuilderCreateAutoVariable(b, scope, np, nl, file, line, dt, 1, 0, 0);
                let expr = di::LLVMDIBuilderCreateExpression(b, std::ptr::null_mut(), 0);
                let loc = di::LLVMDIBuilderCreateDebugLocation(self.ctx.raw(), line, col, scope, std::ptr::null_mut());
                di::LLVMDIBuilderInsertDeclareRecordAtEnd(b, slot.as_value_ref(), var, expr, loc, bb.as_mut_ptr());
            }
        }
    }

    /// In an unoptimised coroutine, reads every described local in the
    /// exit block (volatile), so CoroSplit keeps them in the frame and they
    /// stay readable in the resume function after any `await` (a local last
    /// used before a suspension would otherwise stay on the ramp's stack).
    pub(crate) fn dbg_keep_coro_locals(&mut self) {
        if self.opts.optimized || self.fs().coro.is_none() {
            return;
        }
        let Some((fd, _)) = self.cur_fn() else { return };
        let declared = fd.declared.clone();
        let Some(f) = self.fs().hir else { return };
        for (i, d) in declared.iter().enumerate() {
            let Some(slot) = self.fs().locals.get(i).copied().flatten() else { continue };
            if !d || slot.cell {
                continue;
            }
            let ty = self.llty(f.locals[i].ty);
            let v = self.b.build_load(ty, slot.ptr, "dbg.keep").unwrap();
            if let Some(inst) = v.as_instruction_value() {
                let _ = inst.set_volatile(true);
            }
        }
    }

    /// Declares local `l` in the current lexical scope (once).
    pub(crate) fn dbg_declare(&mut self, l: LocalId) {
        self.dbga_declare(l);
        self.dbg_declare_as(l, None);
    }

    fn dbg_declare_as(&mut self, l: LocalId, arg: Option<u32>) {
        let Some((fd, d)) = self.cur_fn() else { return };
        if !d.full {
            return;
        }
        if fd.declared.get(l.0 as usize).copied().unwrap_or(true) {
            return;
        }
        let Some(f) = self.fs().hir else { return };
        let info = &f.locals[l.0 as usize];
        if info.kind == LocalKind::Temp || info.name.is_empty() || info.name == "_" || info.name.contains(['$', '#', '.']) {
            return;
        }
        let Some(slot) = self.fs().locals.get(l.0 as usize).copied().flatten() else { return };
        let Some(bb) = self.b.get_insert_block() else { return };
        let ty = self.di_type(info.ty);
        let (fd, d) = self.cur_fn().unwrap();
        let (unit, scope) = (fd.unit, *fd.scopes.last().unwrap());
        let u = &d.units[unit];
        let (b, file) = (u.b, u.file);
        let at = if info.span == Span::DUMMY { f.span.start } else { info.span.start };
        let (line, col) = self.line_col(unit, at);
        unsafe {
            let (np, nl) = s(&info.name);
            let var = match arg {
                Some(n) => di::LLVMDIBuilderCreateParameterVariable(b, scope, np, nl, n, file, line, ty, 1, 0),
                None => di::LLVMDIBuilderCreateAutoVariable(b, scope, np, nl, file, line, ty, 1, 0, 0),
            };
            let mut ops: Vec<u64> = if slot.cell { vec![DW_OP_DEREF, DW_OP_PLUS_UCONST, 16] } else { Vec::new() };
            let expr = di::LLVMDIBuilderCreateExpression(b, ops.as_mut_ptr(), ops.len());
            let loc = di::LLVMDIBuilderCreateDebugLocation(self.ctx.raw(), line, col, scope, std::ptr::null_mut());
            di::LLVMDIBuilderInsertDeclareRecordAtEnd(b, slot.ptr.as_value_ref(), var, expr, loc, bb.as_mut_ptr());
        }
        self.dbg.as_mut().unwrap().stack.last_mut().unwrap().declared[l.0 as usize] = true;
    }

    // ------------------------------------------------------------------
    // Types
    // ------------------------------------------------------------------

    fn tb(&self) -> LLVMDIBuilderRef {
        self.dbg.as_ref().unwrap().units[0].b
    }

    fn tfile(&self) -> Meta {
        self.dbg.as_ref().unwrap().units[0].file
    }

    fn di_ret(&mut self, t: TypeId) -> Meta {
        if matches!(self.ty(t), Type::Void) {
            std::ptr::null_mut()
        } else {
            self.di_type(t)
        }
    }

    fn di_basic(&self, name: &str, bits: u64, enc: u32) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateBasicType(self.tb(), np, nl, bits, enc, 0) }
    }

    fn di_ptr(&self, to: Meta, name: &str) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreatePointerType(self.tb(), to, 64, 0, 0, np, nl) }
    }

    fn di_typedef(&self, to: Meta, name: &str) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateTypedef(self.tb(), to, np, nl, self.tfile(), 0, std::ptr::null_mut(), 0) }
    }

    /// A runtime handle: struct `name { T *ptr }`. A struct rather than a
    /// typedef of the pointer, so debuggers show only the formatter's
    /// summary (lldb-dap prints a pointer's address before it). Same size,
    /// alignment and calling convention as the pointer.
    fn di_handle(&self, ptr: Meta, name: &str) -> Meta {
        let m = self.di_member("ptr", ptr, 64, 0);
        self.di_struct(name, 8, 8, vec![m])
    }

    fn di_member(&self, name: &str, ty: Meta, bits: u64, off_bytes: u64) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateMemberType(self.tb(), std::ptr::null_mut(), np, nl, self.tfile(), 0, bits, 0, off_bytes * 8, 0, ty) }
    }

    fn di_struct(&self, name: &str, size: u64, align: u32, mut members: Vec<Meta>) -> Meta {
        let (np, nl) = s(name);
        unsafe {
            di::LLVMDIBuilderCreateStructType(
                self.tb(),
                std::ptr::null_mut(),
                np,
                nl,
                self.tfile(),
                0,
                size * 8,
                align * 8,
                0,
                std::ptr::null_mut(),
                members.as_mut_ptr(),
                members.len() as u32,
                0,
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
            )
        }
    }

    fn di_union(&self, name: &str, size: u64, align: u32, mut members: Vec<Meta>) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateUnionType(self.tb(), std::ptr::null_mut(), np, nl, self.tfile(), 0, size * 8, align * 8, 0, members.as_mut_ptr(), members.len() as u32, 0, std::ptr::null(), 0) }
    }

    fn di_array(&self, elem: Meta, elem_bits: u64, align: u32, count: i64) -> Meta {
        unsafe {
            let mut sub = [di::LLVMDIBuilderGetOrCreateSubrange(self.tb(), 0, count)];
            di::LLVMDIBuilderCreateArrayType(self.tb(), elem_bits * count.max(0) as u64, align * 8, elem, sub.as_mut_ptr(), 1)
        }
    }

    fn di_temp(&self, name: &str) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateReplaceableCompositeType(self.tb(), DW_TAG_STRUCTURE_TYPE, np, nl, std::ptr::null_mut(), self.tfile(), 0, 0, 0, 0, di::LLVMDIFlagFwdDecl, std::ptr::null(), 0) }
    }

    fn di_fwd(&self, name: &str) -> Meta {
        let (np, nl) = s(name);
        unsafe { di::LLVMDIBuilderCreateForwardDecl(self.tb(), DW_TAG_STRUCTURE_TYPE, np, nl, std::ptr::null_mut(), self.tfile(), 0, 0, 0, 0, std::ptr::null(), 0) }
    }

    fn size_align(&self, t: BasicTypeEnum<'ctx>) -> (u64, u32) {
        (self.td.get_abi_size(&t), self.td.get_abi_alignment(&t))
    }

    fn field_off(&self, st: StructType<'ctx>, i: u32) -> u64 {
        self.td.offset_of_element(&st, i).unwrap_or(0)
    }

    /// Memoised runtime-side helper types.
    fn di_rt(&mut self, which: &'static str) -> Meta {
        if let Some(m) = self.dbg.as_ref().unwrap().named.get(which) {
            return *m;
        }
        let i64t = self.di_basic("int64", 64, DW_ATE_SIGNED);
        let void_ptr = self.di_ptr(std::ptr::null_mut(), "");
        let m = match which {
            "void*" => void_ptr,
            "int64" => i64t,
            "byte" => self.di_basic("uint8", 8, DW_ATE_UNSIGNED),
            "cchar" => self.di_basic("char", 8, DW_ATE_SIGNED_CHAR),
            "zell_type_info" => {
                let cc = self.di_rt("cchar");
                let cstr = self.di_ptr(cc, "");
                let ms = vec![self.di_member("name", cstr, 64, 0), self.di_member("size", i64t, 64, 8), self.di_member("drop", void_ptr, 64, 16)];
                self.di_struct("zell_type_info", 24, 8, ms)
            }
            "zell_type_info*" => {
                let t = self.di_rt("zell_type_info");
                self.di_ptr(t, "")
            }
            "zell_string" => {
                let ti = self.di_rt("zell_type_info*");
                let cc = self.di_rt("cchar");
                let data = self.di_ptr(cc, "");
                let ms = vec![self.di_member("refcount", i64t, 64, 0), self.di_member("type", ti, 64, 8), self.di_member("len", i64t, 64, 16), self.di_member("data", data, 64, 24)];
                self.di_struct("zell_string", 32, 8, ms)
            }
            "string" => {
                let st = self.di_rt("zell_string");
                let p = self.di_ptr(st, "");
                self.di_handle(p, "string")
            }
            "zell_error" => {
                let tmp = self.di_temp("zell_error");
                self.dbg.as_mut().unwrap().named.insert("zell_error", tmp);
                let ti = self.di_rt("zell_type_info*");
                let str_t = self.di_rt("string");
                let errp = self.di_ptr(tmp, "");
                let ms = vec![
                    self.di_member("refcount", i64t, 64, 0),
                    self.di_member("type", ti, 64, 8),
                    self.di_member("message", str_t, 64, 16),
                    self.di_member("inner", errp, 64, 24),
                    self.di_member("user_type", ti, 64, 32),
                    self.di_member("payload", void_ptr, 64, 40),
                    self.di_member("message_fn", void_ptr, 64, 48),
                    self.di_member("origin", errp, 64, 56),
                    self.di_member("created", void_ptr, 64, 64),
                    self.di_member("nframes", i64t, 64, 72),
                ];
                let real = self.di_struct("zell_error", 224, 8, ms);
                unsafe { di::LLVMMetadataReplaceAllUsesWith(tmp, real) };
                real
            }
            "error" => {
                let st = self.di_rt("zell_error");
                let p = self.di_ptr(st, "");
                self.di_handle(p, "error")
            }
            "zell_vtable" => {
                let ti = self.di_rt("zell_type_info*");
                let fns = self.di_array(void_ptr, 64, 8, -1);
                let ms = vec![self.di_member("type", ti, 64, 0), self.di_member("boxed", i64t, 64, 8), self.di_member("methods", fns, 0, 16)];
                self.di_struct("zell_vtable", 16, 8, ms)
            }
            "zell_object" => {
                let ti = self.di_rt("zell_type_info*");
                let ms = vec![self.di_member("refcount", i64t, 64, 0), self.di_member("type", ti, 64, 8)];
                self.di_struct("zell_object", 16, 8, ms)
            }
            "zell_task*" => {
                let t = self.di_fwd("zell.task");
                self.di_ptr(t, "")
            }
            _ => unreachable!("debug: unknown runtime type {which}"),
        };
        self.dbg.as_mut().unwrap().named.insert(which, m);
        m
    }

    /// `Task<T>` (async function results).
    fn di_named_task(&mut self, t: TypeId) -> Meta {
        let p = self.di_rt("zell_task*");
        let name = format!("Task<{}>", self.type_name(t));
        self.di_handle(p, &name)
    }

    /// The DWARF type of Zell type `t` (memoised).
    pub(crate) fn di_type(&mut self, t: TypeId) -> Meta {
        if let Some(m) = self.dbg.as_ref().unwrap().types.get(&t) {
            return *m;
        }
        let m = self.di_type_new(t);
        self.dbg.as_mut().unwrap().types.insert(t, m);
        m
    }

    /// Builds a composite type that may refer to itself: a replaceable
    /// placeholder stands for `t` while `build` runs.
    fn di_recursive(&mut self, t: TypeId, name: &str, build: impl FnOnce(&mut Self) -> Meta) -> Meta {
        let tmp = self.di_temp(name);
        self.dbg.as_mut().unwrap().types.insert(t, tmp);
        let real = build(self);
        unsafe { di::LLVMMetadataReplaceAllUsesWith(tmp, real) };
        real
    }

    fn di_type_new(&mut self, t: TypeId) -> Meta {
        let name = self.type_name(t);
        let ty = self.ty(t).clone();
        match ty {
            Type::Void | Type::Nil | Type::Infer(_) | Type::Param(_) | Type::Invalid => self.di_rt("void*"),
            Type::Bool => self.di_basic("bool", 8, DW_ATE_BOOLEAN),
            Type::Int(k) => {
                // A typedef over a C-sized base type: lldb maps base types
                // by their C name and size (a 64-bit `int` would show as `long`).
                let (c, enc) = match (k.bits(), k.signed()) {
                    (8, true) => ("int8_t", DW_ATE_SIGNED),
                    (16, true) => ("int16_t", DW_ATE_SIGNED),
                    (32, true) => ("int32_t", DW_ATE_SIGNED),
                    (_, true) => ("int64_t", DW_ATE_SIGNED),
                    (8, false) => ("uint8_t", DW_ATE_UNSIGNED),
                    (16, false) => ("uint16_t", DW_ATE_UNSIGNED),
                    (32, false) => ("uint32_t", DW_ATE_UNSIGNED),
                    (_, false) => ("uint64_t", DW_ATE_UNSIGNED),
                };
                let base = self.di_basic(c, k.bits() as u64, enc);
                self.di_typedef(base, &name)
            }
            Type::Float32 => {
                let base = self.di_basic("float", 32, DW_ATE_FLOAT);
                self.di_typedef(base, &name)
            }
            Type::Float64 => {
                let base = self.di_basic("double", 64, DW_ATE_FLOAT);
                self.di_typedef(base, &name)
            }
            Type::Char => {
                let base = self.di_basic("char32_t", 32, DW_ATE_UTF);
                self.di_typedef(base, "char")
            }
            Type::String => self.di_rt("string"),
            Type::Error => self.di_rt("error"),
            Type::Pointer(inner) => {
                // Placeholder first: `*T` inside `T` refers back to it.
                let tmp = self.di_temp(&name);
                self.dbg.as_mut().unwrap().types.insert(t, tmp);
                let it = self.di_type(inner);
                let real = self.di_ptr(it, "");
                unsafe { di::LLVMMetadataReplaceAllUsesWith(tmp, real) };
                real
            }
            Type::Weak(inner) => self.di_recursive(t, &name, |cg| {
                let it = cg.di_type(inner);
                let lt = cg.llty(inner);
                let (sz, al) = cg.size_align(lt);
                let hdr = cg.di_rt("zell_object");
                let objname = format!("zell_obj<{}>", cg.type_name(inner));
                let ms = vec![cg.di_member("hdr", hdr, 128, 0), cg.di_member("value", it, sz * 8, 16)];
                let obj = cg.di_struct(&objname, 16 + sz, al.max(8), ms);
                let objp = cg.di_ptr(obj, "");
                let boxname = format!("zell_weak<{}>", cg.type_name(inner));
                let ms = vec![cg.di_member("hdr", hdr, 128, 0), cg.di_member("target", objp, 64, 16)];
                let bx = cg.di_struct(&boxname, 24, 8, ms);
                let p = cg.di_ptr(bx, "");
                cg.di_handle(p, &name)
            }),
            Type::Slice(elem) => self.di_recursive(t, &name, |cg| {
                let et = cg.di_type(elem);
                let ep = cg.di_ptr(et, "");
                let i64t = cg.di_type(cg.prog.types.int);
                let vp = cg.di_rt("void*");
                let ms = vec![cg.di_member("data", ep, 64, 0), cg.di_member("len", i64t, 64, 8), cg.di_member("cap", i64t, 64, 16), cg.di_member("backing", vp, 64, 24)];
                cg.di_struct(&name, 32, 8, ms)
            }),
            Type::Array(elem, n) => {
                let et = self.di_type(elem);
                let lt = self.llty(elem);
                let (sz, al) = self.size_align(lt);
                self.di_array(et, sz * 8, al, n as i64)
            }
            Type::Map(k, v) => self.di_recursive(t, &name, |cg| cg.di_map(&name, k, v)),
            Type::Tuple(ts) => self.di_recursive(t, &name, |cg| {
                let st = cg.llty(t).into_struct_type();
                let mut ms = Vec::new();
                for (i, et) in ts.iter().enumerate() {
                    let m = cg.di_type(*et);
                    let lt = cg.llty(*et);
                    let off = cg.field_off(st, i as u32);
                    ms.push(cg.di_member(&format!("_{i}"), m, cg.td.get_abi_size(&lt) * 8, off));
                }
                let (sz, al) = cg.size_align(st.into());
                cg.di_struct(&name, sz, al, ms)
            }),
            Type::Fallible(inner) => {
                if matches!(self.ty(inner), Type::Void) {
                    let e = self.di_rt("error");
                    return self.di_typedef(e, &name);
                }
                self.di_recursive(t, &name, |cg| {
                    let st = cg.llty(t).into_struct_type();
                    let it = cg.di_type(inner);
                    let lt = cg.llty(inner);
                    let e = cg.di_rt("error");
                    let off = cg.field_off(st, 1);
                    let ms = vec![cg.di_member("value", it, cg.td.get_abi_size(&lt) * 8, 0), cg.di_member("err", e, 64, off)];
                    let (sz, al) = cg.size_align(st.into());
                    cg.di_struct(&name, sz, al, ms)
                })
            }
            Type::Task(_) => {
                let p = self.di_rt("zell_task*");
                self.di_handle(p, &name)
            }
            Type::Any => {
                let vp = self.di_rt("void*");
                let ti = self.di_rt("zell_type_info*");
                let ms = vec![self.di_member("data", vp, 64, 0), self.di_member("type", ti, 64, 8)];
                self.di_struct("any", 16, 8, ms)
            }
            Type::Interface(_) => {
                let vp = self.di_rt("void*");
                let vt = self.di_rt("zell_vtable");
                let vtp = self.di_ptr(vt, "");
                let ms = vec![self.di_member("data", vp, 64, 0), self.di_member("vtable", vtp, 64, 8)];
                self.di_struct(&name, 16, 8, ms)
            }
            Type::Func(sig) => self.di_recursive(t, &name, |cg| {
                let mut tys = vec![cg.di_ret(sig.ret), cg.di_rt("void*")];
                for p in &sig.params {
                    tys.push(cg.di_type(*p));
                }
                let st = unsafe { di::LLVMDIBuilderCreateSubroutineType(cg.tb(), cg.tfile(), tys.as_mut_ptr(), tys.len() as u32, di::LLVMDIFlagPrototyped) };
                let fp = cg.di_ptr(st, "");
                let vp = cg.di_rt("void*");
                let ms = vec![cg.di_member("fn", fp, 64, 0), cg.di_member("env", vp, 64, 8)];
                cg.di_struct(&name, 16, 8, ms)
            }),
            Type::Struct(d) => self.di_recursive(t, &name, |cg| {
                let st = cg.struct_llty(d.0);
                let fields: Vec<(String, TypeId)> = cg.prog.structs[d.0 as usize].fields.iter().map(|f| (f.name.clone(), f.ty)).collect();
                let mut ms = Vec::new();
                for (i, (fname, fty)) in fields.into_iter().enumerate() {
                    let m = cg.di_type(fty);
                    let lt = cg.llty(fty);
                    let off = cg.field_off(st, i as u32);
                    ms.push(cg.di_member(&fname, m, cg.td.get_abi_size(&lt) * 8, off));
                }
                let (sz, al) = cg.size_align(st.into());
                cg.di_struct(&name, sz, al, ms)
            }),
            Type::Enum(d) => {
                if self.prog.enums[d.0 as usize].is_plain {
                    return self.di_enumeration(d.0, &name);
                }
                self.di_recursive(t, &name, |cg| cg.di_data_enum(d.0, &name))
            }
        }
    }

    /// `enum` tags: an enumeration of the variant names (`i32`).
    fn di_enumeration(&mut self, eid: u32, name: &str) -> Meta {
        let variants: Vec<String> = self.prog.enums[eid as usize].variants.iter().map(|v| v.name.clone()).collect();
        let base = self.di_basic("int32", 32, DW_ATE_SIGNED);
        unsafe {
            let mut es: Vec<Meta> = variants
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let (np, nl) = s(v);
                    di::LLVMDIBuilderCreateEnumerator(self.tb(), np, nl, i as i64, 0)
                })
                .collect();
            let (np, nl) = s(name);
            di::LLVMDIBuilderCreateEnumerationType(self.tb(), std::ptr::null_mut(), np, nl, self.tfile(), 0, 32, 32, es.as_mut_ptr(), es.len() as u32, base)
        }
    }

    /// A data enum: `{tag, union payload {struct per variant}}`.
    fn di_data_enum(&mut self, eid: u32, name: &str) -> Meta {
        let st = self.enum_llty(eid);
        let tag = self.di_enumeration(eid, &format!("{name}.Tag"));
        let poff = self.field_off(st, 1);
        let (sz, al) = self.size_align(st.into());
        let nvar = self.prog.enums[eid as usize].variants.len();
        let mut vms = Vec::new();
        for v in 0..nvar {
            let vst = self.variant_llty(eid, v as u32);
            let var = &self.prog.enums[eid as usize].variants[v];
            let (vname, fields) = (var.name.clone(), var.fields.clone());
            let mut ms = Vec::new();
            for (i, (fname, fty)) in fields.into_iter().enumerate() {
                let m = self.di_type(fty);
                let lt = self.llty(fty);
                let off = self.field_off(vst, i as u32);
                ms.push(self.di_member(&fname, m, self.td.get_abi_size(&lt) * 8, off));
            }
            let (vs, va) = self.size_align(vst.into());
            let vt = self.di_struct(&format!("{name}.{vname}"), vs, va, ms);
            vms.push(self.di_member(&vname, vt, vs * 8, 0));
        }
        let usz = sz - poff;
        let un = self.di_union(&format!("{name}.payload"), usz, 8, vms);
        let ms = vec![self.di_member("tag", tag, 32, 0), self.di_member("payload", un, usz * 8, poff)];
        self.di_struct(name, sz, al, ms)
    }

    /// `map[K]V`: pointer to the runtime table (runtime/src/map.c layout).
    fn di_map(&mut self, name: &str, k: TypeId, v: TypeId) -> Meta {
        let kt = self.di_type(k);
        let vt = self.di_type(v);
        let ks = self.size_of(k);
        let vs = self.size_of(v);
        let r8 = |n: u64| n.div_ceil(8) * 8;
        let suffix = name.strip_prefix("map").unwrap_or(name);
        let ems = vec![self.di_member("key", kt, ks * 8, 0), self.di_member("value", vt, vs * 8, r8(ks))];
        let entry = self.di_struct(&format!("zell_map_entry{suffix}"), r8(ks) + r8(vs), 8, ems);
        let entryp = self.di_ptr(entry, "");
        let i64t = self.di_rt("int64");
        let ti = self.di_rt("zell_type_info*");
        let byte = self.di_rt("byte");
        let bytep = self.di_ptr(byte, "");
        let desc = self.di_array(byte, 8, 1, 48);
        let i32t = self.di_basic("int32", 32, DW_ATE_SIGNED);
        let ms = vec![
            self.di_member("refcount", i64t, 64, 0),
            self.di_member("type", ti, 64, 8),
            self.di_member("desc", desc, 48 * 8, 16),
            self.di_member("count", i64t, 64, 64),
            self.di_member("cap", i64t, 64, 72),
            self.di_member("growth_left", i64t, 64, 80),
            self.di_member("slot_size", i64t, 64, 88),
            self.di_member("val_off", i64t, 64, 96),
            self.di_member("kind", i32t, 32, 104),
            self.di_member("ctrl", bytep, 64, 112),
            self.di_member("slots", entryp, 64, 120),
        ];
        let table = self.di_struct(&format!("zell_map{suffix}"), 128, 8, ms);
        let p = self.di_ptr(table, "");
        self.di_handle(p, name)
    }
}

/// Removes duplicate `llvm.dbg.declare`s of one variable, keeping the last.
///
/// CoroSplit (LLVM 18) also attaches a local's declare to the frame slot of
/// every spilled SSA load of it (a task pointer kept across an `await`),
/// which holds a stale copy; the declare it rewrites for the local's own
/// alloca comes last. Our code declares each variable once, so outside
/// coroutines this is a no-op. Run after the optimisation pipeline.
pub fn dedup_declares(module: &inkwell::module::Module) {
    use std::collections::HashSet;
    if unsafe { di::LLVMGetModuleDebugMetadataVersion(module.as_mut_ptr()) } == 0 {
        return;
    }
    for f in module.get_functions() {
        let mut declares = Vec::new();
        for bb in f.get_basic_blocks() {
            let mut i = bb.get_first_instruction();
            while let Some(inst) = i {
                i = inst.get_next_instruction();
                if inst.get_opcode() != inkwell::values::InstructionOpcode::Call {
                    continue;
                }
                unsafe {
                    let callee = llc::LLVMGetCalledValue(inst.as_value_ref());
                    if callee.is_null() {
                        continue;
                    }
                    let mut len = 0usize;
                    let name = llc::LLVMGetValueName2(callee, &mut len);
                    let name = std::slice::from_raw_parts(name as *const u8, len);
                    if name == b"llvm.dbg.declare" {
                        declares.push((inst, llc::LLVMGetOperand(inst.as_value_ref(), 1)));
                    }
                }
            }
        }
        if declares.len() < 2 {
            continue;
        }
        let mut seen = HashSet::new();
        for (inst, var) in declares.into_iter().rev() {
            if !seen.insert(var as usize) {
                inst.erase_from_basic_block();
            }
        }
    }
}
