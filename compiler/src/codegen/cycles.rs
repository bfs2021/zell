//! Cycle-collector support (DESIGN.md "Циклы", runtime/ABI.md §3.8).
//!
//! [`crate::sema::cycles`] marks the types that lie on a cycle of the type
//! reference graph. For them this module emits:
//!
//! * allocation through `zell_cycles_new_entry(entry, size)` (it sets
//!   `ZELL_RC_CYCLIC` in the header while the collector is on) for boxes
//!   of a cyclic type and for closure environments of lambdas that capture
//!   one ([`CodeGen::cyc_alloc`]); `entry` is a per-type registry entry
//!   (`cycles.entry.N`) the runtime registers on first use, so only types
//!   the program really allocates are registered and dead code stays dead;
//! * *visitors* `void visit(ptr slot, ptr cb, ptr ctx)` that call
//!   `cb(ctx, obj, tid)` for every strong reference in a value to an object
//!   that may be flagged: `tid` is NULL when the static type says so, or the
//!   dynamic type id (interface/`any` type word, closure function pointer)
//!   that the runtime looks up in its registry first;
//! * a third `visit` word in every compiled `zell_value_ops` (non-NULL for
//!   element types that hold such references, so the runtime flags the
//!   slice backings and maps made with them);
//! * the entries `{key, visit, fin, clear, alias, state}` (ABI.md 3.8):
//!   the type info, the payload visitor, for types containing destructors
//!   a destructor-only and a release-only helper (`clear` NULL means the
//!   type info's drop), and the dynamic type id that names objects of the
//!   type in interfaces (`ti.<*S>`) or closures (the lambda's function).

use super::CodeGen;
use crate::hir::FuncId;
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::types::FunctionType;
use inkwell::values::{BasicValue, FunctionValue, GlobalValue, PointerValue};
use inkwell::IntPredicate;
use std::collections::HashMap;

/// Per-module state of the cycle-collector support.
#[derive(Default)]
pub(crate) struct CycState<'ctx> {
    /// Slot visitors by type.
    visit: HashMap<TypeId, FunctionValue<'ctx>>,
    /// Destructor-only and release-only object helpers by type.
    fin: HashMap<TypeId, FunctionValue<'ctx>>,
    clear: HashMap<TypeId, FunctionValue<'ctx>>,
    /// Environments of lambdas capturing cyclic cells: (registry entry,
    /// type info, visitor).
    envs: HashMap<FuncId, (GlobalValue<'ctx>, GlobalValue<'ctx>, FunctionValue<'ctx>)>,
    /// Registry entries by boxed type; initializers emitted last.
    entries: HashMap<TypeId, GlobalValue<'ctx>>,
}

/// What a helper does to the destructors and references of a value.
#[derive(Clone, Copy, PartialEq)]
enum Pass {
    /// Run the `Drop` methods only (parent first, then fields in order).
    Fin,
    /// Release the references without running `Drop`.
    Clear,
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// `void(ptr ctx, ptr obj, ptr tid)`.
    fn cyc_cb_ty(&self) -> FunctionType<'ctx> {
        self.ctx.void_type().fn_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.ptr_ty().into()], false)
    }

    /// `{ptr key, ptr visit, ptr fin, ptr clear, ptr alias, i64 state}`.
    fn cyc_entry_ty(&self) -> inkwell::types::StructType<'ctx> {
        let p = self.ptr_ty();
        self.ctx.struct_type(&[p.into(), p.into(), p.into(), p.into(), p.into(), self.i64_ty().into()], false)
    }

    fn cyc_entry_global(&mut self, name: &str) -> GlobalValue<'ctx> {
        let ty = self.cyc_entry_ty();
        let n = self.fresh();
        let g = self.module.add_global(ty, None, &format!("{name}.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_alignment(8);
        g
    }

    /// Allocates a heap box (header included, `size` bytes) holding a `t`
    /// with type info `ti`: through its registry entry when `t` is cyclic.
    pub(crate) fn cyc_alloc(&mut self, t: TypeId, ti: PointerValue<'ctx>, size: inkwell::values::IntValue<'ctx>) -> PointerValue<'ctx> {
        if !self.cycles.is_cyclic(t) {
            return self.call_rt_v("zell_object_new", &[ti.into(), size.into()]).into_pointer_value();
        }
        let g = match self.cyc.entries.get(&t) {
            Some(g) => *g,
            None => {
                let g = self.cyc_entry_global("cycles.entry");
                self.cyc.entries.insert(t, g);
                g
            }
        };
        self.call_rt_v("zell_cycles_new_entry", &[g.as_pointer_value().into(), size.into()]).into_pointer_value()
    }

    /// The `visit` word of a `zell_value_ops` for elements of `t`.
    pub(crate) fn ops_visit(&mut self, t: TypeId) -> PointerValue<'ctx> {
        if self.cycles.holds_cyclic(t) {
            self.cyc_visit_helper(t).as_global_value().as_pointer_value()
        } else {
            self.ptr_ty().const_null()
        }
    }

    /// `void visit(ptr slot, ptr cb, ptr ctx)` for values of `t`.
    fn cyc_visit_helper(&mut self, t: TypeId) -> FunctionValue<'ctx> {
        if let Some(f) = self.cyc.visit.get(&t) {
            return *f;
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.ptr_ty().into()], false);
        let n = self.fresh();
        let f = self.module.add_function(&format!("cyc.visit.{n}"), fty, Some(Linkage::Internal));
        self.cyc.visit.insert(t, f);
        let _ = self.with_helper(f, |cg| {
            let p = f.get_nth_param(0).unwrap().into_pointer_value();
            let cb = f.get_nth_param(1).unwrap().into_pointer_value();
            let cx = f.get_nth_param(2).unwrap().into_pointer_value();
            cg.cyc_visit_slot(t, p, cb, cx);
            cg.b.build_return(None).unwrap();
            Ok(())
        });
        f
    }

    /// `cb(ctx, obj, tid)` unless `obj` is NULL.
    fn cyc_call(&mut self, cb: PointerValue<'ctx>, cx: PointerValue<'ctx>, obj: PointerValue<'ctx>, tid: PointerValue<'ctx>, test: PointerValue<'ctx>) {
        let call = self.block("cyc.ref");
        let done = self.block("cyc.next");
        let nn = self.b.build_is_not_null(test, "").unwrap();
        self.b.build_conditional_branch(nn, call, done).unwrap();
        self.b.position_at_end(call);
        let fty = self.cyc_cb_ty();
        self.b.build_indirect_call(fty, cb, &[cx.into(), obj.into(), tid.into()], "").unwrap();
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
    }

    /// Emits the visit of the references held in slot `p` of type `t`.
    fn cyc_visit_slot(&mut self, t: TypeId, p: PointerValue<'ctx>, cb: PointerValue<'ctx>, cx: PointerValue<'ctx>) {
        if !self.cycles.holds_cyclic(t) {
            return;
        }
        let null = self.ptr_ty().const_null();
        match self.ty(t).clone() {
            Type::Pointer(_) => {
                let v = self.b.build_load(self.ptr_ty(), p, "").unwrap().into_pointer_value();
                let obj = self.byte_gep(v, (-16i64) as u64);
                self.cyc_call(cb, cx, obj, null, v);
            }
            Type::Slice(_) => {
                let bp = self.byte_gep(p, 24);
                let v = self.b.build_load(self.ptr_ty(), bp, "").unwrap().into_pointer_value();
                self.cyc_call(cb, cx, v, null, v);
            }
            Type::Map(..) => {
                let v = self.b.build_load(self.ptr_ty(), p, "").unwrap().into_pointer_value();
                self.cyc_call(cb, cx, v, null, v);
            }
            Type::Func(_) => {
                let fnp = self.b.build_load(self.ptr_ty(), p, "fn").unwrap().into_pointer_value();
                let ep = self.byte_gep(p, 8);
                let env = self.b.build_load(self.ptr_ty(), ep, "env").unwrap().into_pointer_value();
                self.cyc_call(cb, cx, env, fnp, env);
            }
            Type::Interface(_) | Type::Any => {
                let data = self.b.build_load(self.ptr_ty(), p, "data").unwrap().into_pointer_value();
                let wp = self.byte_gep(p, 8);
                let w = self.b.build_load(self.ptr_ty(), wp, "w").unwrap().into_pointer_value();
                let has = self.block("cyc.has");
                let done = self.block("cyc.done");
                let nn = self.b.build_is_not_null(w, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let tid = if matches!(self.ty(t), Type::Any) {
                    let f = self.any_inline_fn();
                    let inl = self.b.build_call(f, &[w.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_int_value();
                    let boxed = self.block("cyc.boxed");
                    self.b.build_conditional_branch(inl, done, boxed).unwrap();
                    self.b.position_at_end(boxed);
                    w
                } else {
                    // vtable[0] is the dynamic type id.
                    self.b.build_load(self.ptr_ty(), w, "tid").unwrap().into_pointer_value()
                };
                let obj = self.byte_gep(data, (-16i64) as u64);
                self.cyc_call(cb, cx, obj, tid, data);
                self.b.build_unconditional_branch(done).unwrap();
                self.b.position_at_end(done);
            }
            Type::Fallible(inner) => {
                if *self.ty(inner) != Type::Void {
                    let st = self.llty(t).into_struct_type();
                    let fp = self.b.build_struct_gep(st, p, 0, "").unwrap();
                    self.cyc_visit_part(inner, fp, cb, cx);
                }
            }
            Type::Tuple(ts) => {
                let st = self.llty(t).into_struct_type();
                for (i, ft) in ts.iter().enumerate() {
                    let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                    self.cyc_visit_part(*ft, fp, cb, cx);
                }
            }
            Type::Struct(d) => {
                let st = self.struct_llty(d.0);
                let fts: Vec<TypeId> = self.prog.structs[d.0 as usize].fields.iter().map(|f| f.ty).collect();
                for (i, ft) in fts.iter().enumerate() {
                    if self.cycles.holds_cyclic(*ft) {
                        let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                        self.cyc_visit_part(*ft, fp, cb, cx);
                    }
                }
            }
            Type::Array(e, n) => {
                let at = self.llty(t);
                self.cyc_loop(n, |cg, iv| {
                    let ep = unsafe { cg.b.build_in_bounds_gep(at, p, &[cg.i64_ty().const_zero(), iv], "") }.unwrap();
                    cg.cyc_visit_part(e, ep, cb, cx);
                });
            }
            Type::Enum(d) => {
                self.cyc_enum_variants(d.0, t, p, |cg, ft, fp| {
                    if cg.cycles.holds_cyclic(ft) {
                        cg.cyc_visit_part(ft, fp, cb, cx);
                    }
                });
            }
            _ => {}
        }
    }

    /// A component of an aggregate: leaves inline, aggregates by helper call.
    fn cyc_visit_part(&mut self, t: TypeId, p: PointerValue<'ctx>, cb: PointerValue<'ctx>, cx: PointerValue<'ctx>) {
        if !self.cycles.holds_cyclic(t) {
            return;
        }
        match self.ty(t) {
            Type::Struct(_) | Type::Tuple(_) | Type::Array(..) | Type::Enum(_) | Type::Fallible(_) => {
                let f = self.cyc_visit_helper(t);
                self.b.build_call(f, &[p.into(), cb.into(), cx.into()], "").unwrap();
            }
            _ => self.cyc_visit_slot(t, p, cb, cx),
        }
    }

    /// `for i in 0..n { body(i) }`.
    fn cyc_loop(&mut self, n: u64, mut body: impl FnMut(&mut Self, inkwell::values::IntValue<'ctx>)) {
        let i = self.entry_alloca(self.i64_ty().into(), "i", true);
        let head = self.block("head");
        let bb = self.block("body");
        let done = self.block("done");
        self.b.build_unconditional_branch(head).unwrap();
        self.b.position_at_end(head);
        let iv = self.b.build_load(self.i64_ty(), i, "").unwrap().into_int_value();
        let c = self.b.build_int_compare(IntPredicate::ULT, iv, self.i64_ty().const_int(n, false), "").unwrap();
        self.b.build_conditional_branch(c, bb, done).unwrap();
        self.b.position_at_end(bb);
        body(self, iv);
        let i1 = self.b.build_int_add(iv, self.i64_ty().const_int(1, false), "").unwrap();
        self.b.build_store(i, i1).unwrap();
        self.b.build_unconditional_branch(head).unwrap();
        self.b.position_at_end(done);
    }

    /// Switch on the tag of enum `eid` at `p`; `f(field type, field ptr)`
    /// for every field of the active variant.
    fn cyc_enum_variants(&mut self, eid: u32, _t: TypeId, p: PointerValue<'ctx>, mut f: impl FnMut(&mut Self, TypeId, PointerValue<'ctx>)) {
        if self.prog.enums[eid as usize].is_plain {
            return;
        }
        let et = self.enum_llty(eid);
        let tag = self.b.build_load(self.ctx.i32_type(), p, "tag").unwrap().into_int_value();
        let done = self.block("done");
        let nvar = self.prog.enums[eid as usize].variants.len();
        let mut cases = Vec::new();
        for v in 0..nvar {
            let fts: Vec<TypeId> = self.prog.enums[eid as usize].variants[v].fields.iter().map(|f| f.1).collect();
            if fts.is_empty() {
                continue;
            }
            let bb = self.block("variant");
            cases.push((self.ctx.i32_type().const_int(v as u64, false), bb));
            let cur = self.b.get_insert_block().unwrap();
            self.b.position_at_end(bb);
            let pp = self.b.build_struct_gep(et, p, 1, "").unwrap();
            let vt = self.variant_llty(eid, v as u32);
            for (i, ft) in fts.iter().enumerate() {
                let fp = self.b.build_struct_gep(vt, pp, i as u32, "").unwrap();
                f(self, *ft, fp);
            }
            self.b.build_unconditional_branch(done).unwrap();
            self.b.position_at_end(cur);
        }
        self.b.build_switch(tag, done, &cases).unwrap();
        self.b.position_at_end(done);
    }

    /// `void f(ptr obj)` running `pass` over the payload of a box of `t`.
    fn cyc_pass_helper(&mut self, t: TypeId, pass: Pass) -> FunctionValue<'ctx> {
        let map = if pass == Pass::Fin { &self.cyc.fin } else { &self.cyc.clear };
        if let Some(f) = map.get(&t) {
            return *f;
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let n = self.fresh();
        let name = if pass == Pass::Fin { "cyc.fin" } else { "cyc.clear" };
        let f = self.module.add_function(&format!("{name}.{n}"), fty, Some(Linkage::Internal));
        if pass == Pass::Fin {
            self.cyc.fin.insert(t, f);
        } else {
            self.cyc.clear.insert(t, f);
        }
        let _ = self.with_helper(f, |cg| {
            let obj = f.get_nth_param(0).unwrap().into_pointer_value();
            let p = cg.payload(obj);
            cg.cyc_pass_slot(t, p, pass);
            cg.b.build_return(None).unwrap();
            Ok(())
        });
        f
    }

    /// The destructor-only or release-only walk of slot `p` of type `t`:
    /// aggregates are walked component by component (so nested `Drop`s run
    /// or are skipped), other references are released by `rc_slot`.
    fn cyc_pass_slot(&mut self, t: TypeId, p: PointerValue<'ctx>, pass: Pass) {
        let relevant = |cg: &Self, t: TypeId| if pass == Pass::Fin { cg.has_dtor(t) } else { cg.needs_rc(t) };
        if !relevant(self, t) {
            return;
        }
        match self.ty(t).clone() {
            Type::Struct(d) => {
                let st = self.struct_llty(d.0);
                if pass == Pass::Fin {
                    if let Some(df) = self.drop_fn(d.0) {
                        let f = self.funcs[df.0 as usize];
                        self.b.build_call(f, &[p.into()], "").unwrap();
                    }
                }
                let fts: Vec<TypeId> = self.prog.structs[d.0 as usize].fields.iter().map(|f| f.ty).collect();
                for (i, ft) in fts.iter().enumerate() {
                    if relevant(self, *ft) {
                        let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                        self.cyc_pass_slot(*ft, fp, pass);
                    }
                }
            }
            Type::Tuple(ts) => {
                let st = self.llty(t).into_struct_type();
                for (i, ft) in ts.iter().enumerate() {
                    let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                    self.cyc_pass_slot(*ft, fp, pass);
                }
            }
            Type::Array(e, n) => {
                let at = self.llty(t);
                self.cyc_loop(n, |cg, iv| {
                    let ep = unsafe { cg.b.build_in_bounds_gep(at, p, &[cg.i64_ty().const_zero(), iv], "") }.unwrap();
                    cg.cyc_pass_slot(e, ep, pass);
                });
            }
            Type::Enum(d) => {
                self.cyc_enum_variants(d.0, t, p, |cg, ft, fp| cg.cyc_pass_slot(ft, fp, pass));
            }
            Type::Fallible(inner) => {
                let st = self.llty(t).into_struct_type();
                let (vp, ep) = if *self.ty(inner) == Type::Void {
                    (None, p)
                } else {
                    (Some(self.b.build_struct_gep(st, p, 0, "").unwrap()), self.b.build_struct_gep(st, p, 1, "").unwrap())
                };
                if let Some(vp) = vp {
                    // A failed result holds no value.
                    let e = self.b.build_load(self.ptr_ty(), ep, "err").unwrap().into_pointer_value();
                    let okb = self.block("res.ok");
                    let done = self.block("res.done");
                    let isnull = self.b.build_is_null(e, "").unwrap();
                    self.b.build_conditional_branch(isnull, okb, done).unwrap();
                    self.b.position_at_end(okb);
                    self.cyc_pass_slot(inner, vp, pass);
                    self.b.build_unconditional_branch(done).unwrap();
                    self.b.position_at_end(done);
                }
                if pass == Pass::Clear {
                    self.rc_slot(self.prog.types.error, ep, false);
                }
            }
            _ => {
                if pass == Pass::Clear {
                    self.rc_slot(t, p, false);
                }
            }
        }
    }

    /// The registry entry of the environment of lambda `func` when it
    /// captures a cell of a cyclic type (else `None`: the shared `ti.env`,
    /// unflagged); its key is a per-lambda type info.
    pub(crate) fn cyc_env_entry(&mut self, func: FuncId) -> Option<PointerValue<'ctx>> {
        if !self.cycles.lambdas.contains(&func) {
            return None;
        }
        if let Some((e, _, _)) = self.cyc.envs.get(&func) {
            return Some(e.as_pointer_value());
        }
        let tity = self.type_info_ty();
        let n = self.fresh();
        let g = self.module.add_global(tity, None, &format!("ti.env.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        // The name and drop of the shared environment type info, and the
        // object size.
        let ptr = self.ptr_ty();
        let ncaps = self.prog.funcs[func.0 as usize].captures.len() as u64;
        let shared_g = self.env_ti.expect("env_type_info first");
        let init = shared_g.get_initializer().unwrap().into_struct_value();
        let name = init.get_field_at_index(0).unwrap();
        let drop = init.get_field_at_index(2).unwrap();
        let size = self.i64_ty().const_int(24 + 8 * ncaps, false);
        g.set_initializer(&tity.const_named_struct(&[name, size.into(), drop]));
        let fty = self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false);
        let vf = self.module.add_function(&format!("cyc.env.{n}"), fty, Some(Linkage::Internal));
        let caps: Vec<TypeId> = self.prog.funcs[func.0 as usize].captures.iter().map(|c| c.ty).collect();
        let _ = self.with_helper(vf, |cg| {
            let obj = vf.get_nth_param(0).unwrap().into_pointer_value();
            let cb = vf.get_nth_param(1).unwrap().into_pointer_value();
            let cx = vf.get_nth_param(2).unwrap().into_pointer_value();
            let null = cg.ptr_ty().const_null();
            for (i, ct) in caps.iter().enumerate() {
                if cg.cycles.is_cyclic(*ct) {
                    let cp = cg.byte_gep(obj, 24 + 8 * i as u64);
                    let cell = cg.b.build_load(cg.ptr_ty(), cp, "cell").unwrap().into_pointer_value();
                    cg.cyc_call(cb, cx, cell, null, cell);
                }
            }
            cg.b.build_return(None).unwrap();
            Ok(())
        });
        let e = self.cyc_entry_global("cycles.env");
        self.cyc.envs.insert(func, (e, g, vf));
        Some(e.as_pointer_value())
    }

    /// Initializers of the registry entries (see [`CycState::entries`]).
    pub(crate) fn finish_cycles(&mut self) -> super::R<()> {
        let ptr = self.ptr_ty();
        let ent = self.cyc_entry_ty();
        let null = ptr.const_null();
        let zero = self.i64_ty().const_zero();
        let mut entries: Vec<(TypeId, GlobalValue<'ctx>)> = self.cyc.entries.iter().map(|(t, g)| (*t, *g)).collect();
        entries.sort_by_key(|(t, _)| *t);
        for (t, g) in entries {
            let ti = self.type_info(t);
            // Visitor of the payload, and the two halves of the drop when
            // the type contains destructors.
            let fty = self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false);
            let n = self.fresh();
            let vf = self.module.add_function(&format!("cyc.obj.{n}"), fty, Some(Linkage::Internal));
            let _ = self.with_helper(vf, |cg| {
                let obj = vf.get_nth_param(0).unwrap().into_pointer_value();
                let cb = vf.get_nth_param(1).unwrap().into_pointer_value();
                let cx = vf.get_nth_param(2).unwrap().into_pointer_value();
                let p = cg.payload(obj);
                cg.cyc_visit_part(t, p, cb, cx);
                cg.b.build_return(None).unwrap();
                Ok(())
            });
            let (fin, clear) = if self.has_dtor(t) {
                let fi = self.cyc_pass_helper(t, Pass::Fin).as_global_value().as_pointer_value();
                let cl = self.cyc_pass_helper(t, Pass::Clear).as_global_value().as_pointer_value();
                (fi, cl)
            } else {
                (null, null)
            };
            // The type id of a `*T` stored in an interface names a T object.
            let alias = self.prog.types.lookup(&Type::Pointer(t)).and_then(|pt| self.type_infos.get(&pt).copied()).unwrap_or(null);
            g.set_initializer(&ent.const_named_struct(&[ti.into(), vf.as_global_value().as_pointer_value().into(), fin.into(), clear.into(), alias.into(), zero.into()]));
        }
        let mut envs: Vec<(FuncId, GlobalValue<'ctx>, GlobalValue<'ctx>, FunctionValue<'ctx>)> = self.cyc.envs.iter().map(|(f, (e, g, v))| (*f, *e, *g, *v)).collect();
        envs.sort_by_key(|(f, ..)| *f);
        for (func, e, g, vf) in envs {
            // A closure's function pointer is its type id.
            let fp = self.closure_entry(func)?.as_global_value().as_pointer_value();
            e.set_initializer(&ent.const_named_struct(&[g.as_pointer_value().into(), vf.as_global_value().as_pointer_value().into(), null.into(), null.into(), fp.into(), zero.into()]));
        }
        Ok(())
    }

    /// Records cleanup entry `h` (a retained borrowed value of type `t`) as
    /// a pin when `t` is a `*T` of a cyclic type: reads the collection
    /// epoch now, for [`Self::release_pin`].
    pub(crate) fn mark_pin(&mut self, h: super::cleanup::Hold, t: TypeId) {
        let Some(i) = h else { return };
        let Type::Pointer(x) = self.ty(t).clone() else { return };
        if !self.cycles.is_cyclic(x) {
            return;
        }
        let g = match self.module.get_global("zell_cycles_epoch") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(self.i64_ty(), None, "zell_cycles_epoch");
                g.set_linkage(Linkage::External);
                g
            }
        };
        let ld = self.b.build_load(self.i64_ty(), g.as_pointer_value(), "epoch").unwrap();
        let ins = ld.as_instruction_value().unwrap();
        ins.set_alignment(8).unwrap();
        ins.set_atomic_ordering(inkwell::AtomicOrdering::Monotonic).unwrap();
        if let Some(f) = self.f.as_mut() {
            f.pins.insert(i, ld.into_int_value());
        }
    }

    /// The release paired with a pin: `zell_ptr_release_noroot(p, epoch)`
    /// decrements without recording a possible root unless a collection
    /// ran in between (ABI.md 3.8).
    pub(crate) fn release_pin(&mut self, x: inkwell::values::BasicValueEnum<'ctx>, epoch: inkwell::values::IntValue<'ctx>) {
        self.call_rt("zell_ptr_release_noroot", &[x.into(), epoch.into()]);
    }
}
