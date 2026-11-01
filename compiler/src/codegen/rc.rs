//! Reference counting: which types are managed, retain/release of values
//! and slots, per-type helpers, `zell_value_ops`, type infos and boxes.

use super::{CodeGen, Val};
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::types::StructType;
use inkwell::values::{BasicValue, BasicValueEnum, FunctionValue, PointerValue};
use inkwell::IntPredicate;

/// Biased refcount words (runtime/ABI.md 3.1, `zell_rt.h`): the owner tag
/// is `(word >> ZELL_RC_OWNER_SHIFT) & ZELL_RC_OWNER_MASK`.
const ZELL_RC_OWNER_SHIFT: u64 = 47;
const ZELL_RC_OWNER_MASK: u64 = 0x16FFF;

/// How a value of some type holds references.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RcKind {
    /// Plain data.
    None,
    /// One heap-object pointer (string, error, map, task, weak box).
    Ref,
    /// A `*T`: pointer to an object payload (header at -16), or NULL.
    Ptr,
    /// A slice value: the backing (field 3) is the reference.
    Slice,
    /// A closure: the env (field 1) is the reference.
    Func,
    /// An interface value; the box is referenced when `vtable.boxed`.
    Iface,
    /// An `any` value; always boxed.
    Any,
    /// An aggregate containing references (struct, tuple, array, `T!`,
    /// data enum).
    Agg,
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    pub(crate) fn rc_kind(&self, t: TypeId) -> RcKind {
        if let Some(k) = self.rc_cache.borrow().get(&t) {
            return *k;
        }
        let k = self.rc_kind_uncached(t);
        self.rc_cache.borrow_mut().insert(t, k);
        k
    }

    fn rc_kind_uncached(&self, t: TypeId) -> RcKind {
        match self.ty(t) {
            Type::String | Type::Error | Type::Map(..) | Type::Task(_) | Type::Weak(_) => RcKind::Ref,
            Type::Pointer(_) => RcKind::Ptr,
            Type::Slice(_) => RcKind::Slice,
            Type::Func(_) => RcKind::Func,
            Type::Interface(_) => RcKind::Iface,
            Type::Any => RcKind::Any,
            Type::Fallible(inner) => {
                if *self.ty(*inner) == Type::Void {
                    RcKind::Ref
                } else {
                    RcKind::Agg
                }
            }
            Type::Array(e, n) => {
                if *n > 0 && self.needs_rc(*e) {
                    RcKind::Agg
                } else {
                    RcKind::None
                }
            }
            Type::Tuple(ts) => {
                if ts.iter().any(|t| self.needs_rc(*t)) {
                    RcKind::Agg
                } else {
                    RcKind::None
                }
            }
            Type::Struct(d) => {
                if self.drop_fn(d.0).is_some() || self.prog.structs[d.0 as usize].fields.iter().any(|f| self.needs_rc(f.ty)) {
                    RcKind::Agg
                } else {
                    RcKind::None
                }
            }
            Type::Enum(d) => {
                let e = &self.prog.enums[d.0 as usize];
                if !e.is_plain && e.variants.iter().any(|v| v.fields.iter().any(|f| self.needs_rc(f.1))) {
                    RcKind::Agg
                } else {
                    RcKind::None
                }
            }
            _ => RcKind::None,
        }
    }

    pub(crate) fn needs_rc(&self, t: TypeId) -> bool {
        self.rc_kind(t) != RcKind::None
    }

    /// The `Drop` method of struct `sid`, if any.
    pub(crate) fn drop_fn(&self, sid: u32) -> Option<crate::hir::FuncId> {
        crate::hir::ownership::drop_method(self.prog, sid)
    }

    /// True if values of `t` contain a struct with a destructor.
    pub(crate) fn has_dtor(&self, t: TypeId) -> bool {
        crate::hir::ownership::has_dtor(self.prog, t)
    }

    /// Makes `v` owned: retains it if borrowed.
    pub(crate) fn to_owned(&mut self, t: TypeId, v: Val<'ctx>) -> BasicValueEnum<'ctx> {
        let x = v.get();
        if !v.owned {
            self.retain_val(t, x);
        }
        x
    }

    /// Releases `v` if it is an owned temporary.
    pub(crate) fn drop_val(&mut self, t: TypeId, v: Val<'ctx>) {
        if v.owned {
            if let Some(x) = v.v {
                self.release_val(t, x);
            }
        }
    }

    /// `loc = v` for an initialised location: retain new, store, release old.
    pub(crate) fn store_assign(&mut self, p: PointerValue<'ctx>, t: TypeId, v: Val<'ctx>) {
        let x = self.to_owned(t, v);
        if self.needs_rc(t) {
            let old = self.b.build_load(x.get_type(), p, "old").unwrap();
            self.b.build_store(p, x).unwrap();
            self.release_val(t, old);
        } else {
            self.b.build_store(p, x).unwrap();
        }
    }

    pub(crate) fn retain_val(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) {
        self.rc_val(t, v, true)
    }

    pub(crate) fn release_val(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) {
        self.rc_val(t, v, false)
    }

    fn rc_name(retain: bool) -> &'static str {
        if retain {
            "zell_retain"
        } else {
            "zell_release"
        }
    }

    fn ptr_rc_name(retain: bool) -> &'static str {
        if retain {
            "zell_ptr_retain"
        } else {
            "zell_ptr_release"
        }
    }

    /// The inline fast path of `zell_retain` / `zell_release` (and the
    /// `zell_ptr_*` variants on a payload pointer): an internal
    /// `alwaysinline` function that `call_rt` uses instead of the runtime
    /// symbol when `Options::inline_rc` (runtime/ABI.md 3.2).
    ///
    /// While `zell_threaded == 0`: retain with count >= 0 stores `+1`,
    /// release with 2 <= count < the lowest flag bit (`ZELL_RC_ZOMBIE`)
    /// stores `-1`; NULL does nothing;
    /// everything else (other threads exist, immortal/arena/view counts,
    /// the last reference, a weak box) calls the runtime function.
    /// Otherwise, if the word is a biased count owned by this thread
    /// (`(word >> 47) & ZELL_RC_OWNER_MASK == zell_rc_owner`, a
    /// thread-local), retain with B != 0xFFFF and release with B >= 2 store
    /// the owner's half-word B +- 1 (16 bits at the object's address).
    pub(crate) fn rc_inline_fn(&mut self, name: &str) -> Option<FunctionValue<'ctx>> {
        let (retain, ptr) = match name {
            "zell_retain" => (true, false),
            "zell_release" => (false, false),
            "zell_ptr_retain" => (true, true),
            "zell_ptr_release" => (false, true),
            _ => return None,
        };
        if !self.opts.inline_rc {
            return None;
        }
        let fname = format!("zell.rc.{name}");
        if let Some(f) = self.module.get_function(&fname) {
            return Some(f);
        }
        let i64t = self.i64_ty();
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let f = self.module.add_function(&fname, fty, Some(Linkage::Internal));
        f.add_attribute(inkwell::attributes::AttributeLoc::Function, self.attr("alwaysinline"));
        f.add_attribute(inkwell::attributes::AttributeLoc::Function, self.attr("nounwind"));
        let slow_name = if retain { "zell_retain" } else { "zell_release" };
        let slow = self.rt(slow_name);
        let mt_g = match self.module.get_global("zell_threaded") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(self.ctx.i8_type(), None, "zell_threaded");
                g.set_linkage(Linkage::External);
                g
            }
        };
        let b = self.ctx.create_builder();
        let entry = self.ctx.append_basic_block(f, "entry");
        let chk = self.ctx.append_basic_block(f, "chk");
        let plain = self.ctx.append_basic_block(f, "plain");
        let chk_own = self.ctx.append_basic_block(f, "chk.own");
        let plain16 = self.ctx.append_basic_block(f, "plain.own");
        let call_slow = self.ctx.append_basic_block(f, "slow");
        let done = self.ctx.append_basic_block(f, "done");
        b.position_at_end(entry);
        let p = f.get_nth_param(0).unwrap().into_pointer_value();
        let isnull = b.build_is_null(p, "").unwrap();
        b.build_conditional_branch(isnull, done, chk).unwrap();
        b.position_at_end(chk);
        let obj = if ptr {
            unsafe { b.build_gep(self.ctx.i8_type(), p, &[i64t.const_int((-16i64) as u64, true)], "obj") }.unwrap()
        } else {
            p
        };
        let ld = b.build_load(i64t, obj, "rc").unwrap();
        let ldi = ld.as_instruction_value().unwrap();
        ldi.set_alignment(8).unwrap();
        ldi.set_atomic_ordering(inkwell::AtomicOrdering::Monotonic).unwrap();
        let rc = ld.into_int_value();
        // Fast: one thread and retain: rc >= 0; release: 2 <= rc < 1 << 57.
        let count_ok = if retain {
            b.build_int_compare(IntPredicate::SGE, rc, i64t.const_zero(), "").unwrap()
        } else {
            let m2 = b.build_int_sub(rc, i64t.const_int(2, false), "").unwrap();
            // Below the lowest flag bit (ZELL_RC_ZOMBIE, bit 57): objects with
            // a weak box or a cycle-collector flag take the runtime path.
            b.build_int_compare(IntPredicate::ULT, m2, i64t.const_int((1u64 << 57) - 2, false), "").unwrap()
        };
        let mt = b.build_load(self.ctx.i8_type(), mt_g.as_pointer_value(), "mt").unwrap().into_int_value();
        let mtz = b.build_int_compare(IntPredicate::EQ, mt, self.ctx.i8_type().const_zero(), "").unwrap();
        let fast = b.build_and(count_ok, mtz, "").unwrap();
        b.build_conditional_branch(fast, plain, chk_own).unwrap();
        // Biased count owned by this thread (ABI.md 3.1): the owner's half-word
        // B (bits 0..15) changes with a plain 16-bit store; retain while
        // B != 0xFFFF, release while B >= 2.
        b.position_at_end(chk_own);
        let owner_g = match self.module.get_global("zell_rc_owner") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(i64t, None, "zell_rc_owner");
                g.set_linkage(Linkage::External);
                g.set_thread_local(true);
                g
            }
        };
        let tag = b.build_load(i64t, owner_g.as_pointer_value(), "owner").unwrap().into_int_value();
        let hi = b.build_right_shift(rc, i64t.const_int(ZELL_RC_OWNER_SHIFT, false), false, "").unwrap();
        let hi = b.build_and(hi, i64t.const_int(ZELL_RC_OWNER_MASK, false), "").unwrap();
        let own = b.build_int_compare(IntPredicate::EQ, hi, tag, "").unwrap();
        let i16t = self.ctx.i16_type();
        let bias = b.build_int_truncate(rc, i16t, "b").unwrap();
        let b_ok = if retain {
            b.build_int_compare(IntPredicate::NE, bias, i16t.const_all_ones(), "").unwrap()
        } else {
            b.build_int_compare(IntPredicate::UGE, bias, i16t.const_int(2, false), "").unwrap()
        };
        let fast16 = b.build_and(own, b_ok, "").unwrap();
        b.build_conditional_branch(fast16, plain16, call_slow).unwrap();
        b.position_at_end(plain16);
        let one16 = i16t.const_int(1, false);
        let nb = if retain { b.build_int_add(bias, one16, "").unwrap() } else { b.build_int_sub(bias, one16, "").unwrap() };
        let st16 = b.build_store(obj, nb).unwrap();
        st16.set_alignment(8).unwrap();
        // A release store for a release (like rc.c `store_b`): a thread that
        // later drops the last reference (acquire CAS on the whole word) must
        // see every write this thread made before; free on x86 (a plain mov).
        st16.set_atomic_ordering(if retain { inkwell::AtomicOrdering::Monotonic } else { inkwell::AtomicOrdering::Release }).unwrap();
        b.build_unconditional_branch(done).unwrap();
        b.position_at_end(plain);
        let one = i64t.const_int(1, false);
        let nv = if retain { b.build_int_add(rc, one, "").unwrap() } else { b.build_int_sub(rc, one, "").unwrap() };
        let st = b.build_store(obj, nv).unwrap();
        st.set_alignment(8).unwrap();
        st.set_atomic_ordering(inkwell::AtomicOrdering::Monotonic).unwrap();
        b.build_unconditional_branch(done).unwrap();
        b.position_at_end(call_slow);
        b.build_call(slow, &[obj.into()], "").unwrap();
        b.build_unconditional_branch(done).unwrap();
        b.position_at_end(done);
        b.build_return(None).unwrap();
        Some(f)
    }

    fn rc_val(&mut self, t: TypeId, v: BasicValueEnum<'ctx>, retain: bool) {
        match self.rc_kind(t) {
            RcKind::None => {}
            // A constant pointer is NULL or an immortal global: nothing to do.
            RcKind::Ref | RcKind::Ptr if v.is_pointer_value() && v.into_pointer_value().is_const() => {}
            RcKind::Ref => {
                self.call_rt(Self::rc_name(retain), &[v.into()]);
            }
            RcKind::Ptr => {
                self.call_rt(Self::ptr_rc_name(retain), &[v.into()]);
            }
            RcKind::Slice => {
                let b = self.b.build_extract_value(v.into_struct_value(), 3, "backing").unwrap();
                self.call_rt(Self::rc_name(retain), &[b.into()]);
            }
            RcKind::Func => {
                let e = self.b.build_extract_value(v.into_struct_value(), 1, "env").unwrap();
                self.call_rt(Self::rc_name(retain), &[e.into()]);
            }
            RcKind::Iface | RcKind::Any | RcKind::Agg => {
                let p = self.spill(v);
                let (r, rl) = self.rc_helpers(t);
                let f = if retain { r } else { rl };
                self.b.build_call(f, &[p.into()], "").unwrap();
            }
        }
    }

    /// Retains or releases the references held in slot `p` of type `t`.
    pub(crate) fn rc_slot(&mut self, t: TypeId, p: PointerValue<'ctx>, retain: bool) {
        match self.rc_kind(t) {
            RcKind::None => {}
            RcKind::Ref => {
                let v = self.b.build_load(self.ptr_ty(), p, "").unwrap();
                self.call_rt(Self::rc_name(retain), &[v.into()]);
            }
            RcKind::Ptr => {
                let v = self.b.build_load(self.ptr_ty(), p, "").unwrap();
                self.call_rt(Self::ptr_rc_name(retain), &[v.into()]);
            }
            RcKind::Slice => {
                let bp = self.byte_gep(p, 24);
                let v = self.b.build_load(self.ptr_ty(), bp, "").unwrap();
                self.call_rt(Self::rc_name(retain), &[v.into()]);
            }
            RcKind::Func => {
                let bp = self.byte_gep(p, 8);
                let v = self.b.build_load(self.ptr_ty(), bp, "").unwrap();
                self.call_rt(Self::rc_name(retain), &[v.into()]);
            }
            _ => {
                let (r, rl) = self.rc_helpers(t);
                let f = if retain { r } else { rl };
                self.b.build_call(f, &[p.into()], "").unwrap();
            }
        }
    }

    /// `void retain(ptr slot)` / `void release(ptr slot)` for type `t`.
    pub(crate) fn rc_helpers(&mut self, t: TypeId) -> (FunctionValue<'ctx>, FunctionValue<'ctx>) {
        if let Some(h) = self.rc_helpers.get(&t) {
            return *h;
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let n = self.fresh();
        let r = self.module.add_function(&format!("rc.retain.{n}"), fty, Some(Linkage::Internal));
        let rl = self.module.add_function(&format!("rc.release.{n}"), fty, Some(Linkage::Internal));
        self.rc_helpers.insert(t, (r, rl));
        for (f, retain) in [(r, true), (rl, false)] {
            let _ = self.with_helper(f, |cg| {
                let p = f.get_nth_param(0).unwrap().into_pointer_value();
                cg.rc_slot_body(t, p, retain);
                cg.b.build_return(None).unwrap();
                Ok(())
            });
        }
        (r, rl)
    }

    /// Body of a slot helper: walks the components of `t`.
    fn rc_slot_body(&mut self, t: TypeId, p: PointerValue<'ctx>, retain: bool) {
        match self.ty(t).clone() {
            Type::Interface(_) | Type::Any => {
                // A non-nil interface value always owns its data: a box
                // payload (struct values, `any`) or a `*S` payload; an
                // `any` holding a scalar inline owns nothing.
                let data = self.b.build_load(self.ptr_ty(), p, "data").unwrap().into_pointer_value();
                let vp = self.byte_gep(p, 8);
                let vt = self.b.build_load(self.ptr_ty(), vp, "vt").unwrap().into_pointer_value();
                let has = self.block("has");
                let done = self.block("done");
                let nn = self.b.build_is_not_null(vt, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                if matches!(self.ty(t), Type::Any) {
                    let f = self.any_inline_fn();
                    let inl = self.b.build_call(f, &[vt.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_int_value();
                    let boxed = self.block("boxed");
                    self.b.build_conditional_branch(inl, done, boxed).unwrap();
                    self.b.position_at_end(boxed);
                }
                self.call_rt(Self::ptr_rc_name(retain), &[data.into()]);
                self.b.build_unconditional_branch(done).unwrap();
                self.b.position_at_end(done);
            }
            Type::Fallible(inner) => {
                let st = self.llty(t).into_struct_type();
                if *self.ty(inner) != Type::Void {
                    let fp = self.b.build_struct_gep(st, p, 0, "").unwrap();
                    let ep = self.b.build_struct_gep(st, p, 1, "").unwrap();
                    if !retain && self.has_dtor(inner) {
                        // A failed result holds no value: do not run the
                        // destructor on the zero placeholder.
                        let e = self.b.build_load(self.ptr_ty(), ep, "err").unwrap().into_pointer_value();
                        let okb = self.block("res.ok");
                        let done = self.block("res.done");
                        let isnull = self.b.build_is_null(e, "").unwrap();
                        self.b.build_conditional_branch(isnull, okb, done).unwrap();
                        self.b.position_at_end(okb);
                        self.rc_slot(inner, fp, retain);
                        self.b.build_unconditional_branch(done).unwrap();
                        self.b.position_at_end(done);
                    } else {
                        self.rc_slot(inner, fp, retain);
                    }
                    self.rc_slot(self.prog.types.error, ep, retain);
                } else {
                    self.rc_slot(self.prog.types.error, p, retain);
                }
            }
            Type::Tuple(ts) => {
                let st = self.llty(t).into_struct_type();
                for (i, ft) in ts.iter().enumerate() {
                    let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                    self.rc_slot(*ft, fp, retain);
                }
            }
            Type::Struct(d) => {
                let st = self.struct_llty(d.0);
                if !retain {
                    // The destructor runs first, while the fields are intact.
                    if let Some(df) = self.drop_fn(d.0) {
                        let f = self.funcs[df.0 as usize];
                        self.b.build_call(f, &[p.into()], "").unwrap();
                    }
                }
                let fts: Vec<TypeId> = self.prog.structs[d.0 as usize].fields.iter().map(|f| f.ty).collect();
                for (i, ft) in fts.iter().enumerate() {
                    if self.needs_rc(*ft) {
                        let fp = self.b.build_struct_gep(st, p, i as u32, "").unwrap();
                        self.rc_slot(*ft, fp, retain);
                    }
                }
            }
            Type::Array(e, n) => {
                let at = self.llty(t);
                let i = self.entry_alloca(self.i64_ty().into(), "i", true);
                let head = self.block("head");
                let body = self.block("body");
                let done = self.block("done");
                self.b.build_unconditional_branch(head).unwrap();
                self.b.position_at_end(head);
                let iv = self.b.build_load(self.i64_ty(), i, "").unwrap().into_int_value();
                let c = self.b.build_int_compare(IntPredicate::ULT, iv, self.i64_ty().const_int(n, false), "").unwrap();
                self.b.build_conditional_branch(c, body, done).unwrap();
                self.b.position_at_end(body);
                let ep = unsafe { self.b.build_in_bounds_gep(at, p, &[self.i64_ty().const_zero(), iv], "") }.unwrap();
                self.rc_slot(e, ep, retain);
                let i1 = self.b.build_int_add(iv, self.i64_ty().const_int(1, false), "").unwrap();
                self.b.build_store(i, i1).unwrap();
                self.b.build_unconditional_branch(head).unwrap();
                self.b.position_at_end(done);
            }
            Type::Enum(d) => {
                let et = self.enum_llty(d.0);
                let tag = self.b.build_load(self.ctx.i32_type(), p, "tag").unwrap().into_int_value();
                let done = self.block("done");
                let nvar = self.prog.enums[d.0 as usize].variants.len();
                let mut cases = Vec::new();
                for v in 0..nvar {
                    let fts: Vec<TypeId> = self.prog.enums[d.0 as usize].variants[v].fields.iter().map(|f| f.1).collect();
                    if !fts.iter().any(|f| self.needs_rc(*f)) {
                        continue;
                    }
                    let bb = self.block("variant");
                    cases.push((self.ctx.i32_type().const_int(v as u64, false), bb));
                    let cur = self.b.get_insert_block().unwrap();
                    self.b.position_at_end(bb);
                    let pp = self.b.build_struct_gep(et, p, 1, "").unwrap();
                    let vt = self.variant_llty(d.0, v as u32);
                    for (i, ft) in fts.iter().enumerate() {
                        let fp = self.b.build_struct_gep(vt, pp, i as u32, "").unwrap();
                        self.rc_slot(*ft, fp, retain);
                    }
                    self.b.build_unconditional_branch(done).unwrap();
                    self.b.position_at_end(cur);
                }
                self.b.build_switch(tag, done, &cases).unwrap();
                self.b.position_at_end(done);
            }
            _ => self.rc_slot(t, p, retain),
        }
    }

    /// `zell_value_ops*` for elements of type `t` (NULL for plain data).
    pub(crate) fn value_ops(&mut self, t: TypeId) -> PointerValue<'ctx> {
        // Element types holding references to cyclic objects get their own
        // ops with a visitor (`{retain, release, visit}`, ABI.md 3.3/3.8).
        let cyclic = self.cycles.holds_cyclic(t);
        match self.rc_kind(t) {
            _ if cyclic => {}
            RcKind::None => return self.ptr_ty().const_null(),
            RcKind::Ref => return self.rt_global("zell_ops_ref"),
            RcKind::Ptr => return self.rt_global("zell_ops_ptr"),
            RcKind::Slice => return self.rt_global("zell_ops_slice"),
            _ => {}
        }
        if let Some(p) = self.value_ops.get(&t) {
            return *p;
        }
        let (r, rl) = self.rc_helpers(t);
        let visit = self.ops_visit(t);
        let st = self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.ptr_ty().into()], false);
        let n = self.fresh();
        let g = self.module.add_global(st, None, &format!("ops.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_initializer(&st.const_named_struct(&[r.as_global_value().as_pointer_value().into(), rl.as_global_value().as_pointer_value().into(), visit.into()]));
        self.value_ops.insert(t, g.as_pointer_value());
        g.as_pointer_value()
    }

    /// `%zell_type_info = { ptr name, i64 size, ptr drop }`.
    pub(crate) fn type_info_ty(&self) -> StructType<'ctx> {
        self.ctx.struct_type(&[self.ptr_ty().into(), self.i64_ty().into(), self.ptr_ty().into()], false)
    }

    /// The type info of a box (heap object) holding a `t`; also the run-time
    /// type id of `t`.
    pub(crate) fn type_info(&mut self, t: TypeId) -> PointerValue<'ctx> {
        if let Some(p) = self.type_infos.get(&t) {
            return *p;
        }
        if self.prog.reflect.is_some() {
            return self.reflect_type_info(t);
        }
        let tity = self.type_info_ty();
        let n = self.fresh();
        let g = self.module.add_global(tity, None, &format!("ti.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        self.type_infos.insert(t, g.as_pointer_value());
        let drop = if self.needs_rc(t) {
            let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
            let f = self.module.add_function(&format!("drop.{n}"), fty, Some(Linkage::Internal));
            let _ = self.with_helper(f, |cg| {
                let obj = f.get_nth_param(0).unwrap().into_pointer_value();
                let p = cg.payload(obj);
                cg.rc_slot(t, p, false);
                cg.b.build_return(None).unwrap();
                Ok(())
            });
            f.as_global_value().as_pointer_value()
        } else {
            self.ptr_ty().const_null()
        };
        let name = self.type_name(t);
        let np = self.cstr(&name);
        let size = 16 + self.size_of(t);
        g.set_initializer(&tity.const_named_struct(&[np.into(), self.i64_ty().const_int(size, false).into(), drop.into()]));
        g.as_pointer_value()
    }

    /// Body of `leak.fields` (see [`CodeGen::leak_fields_fn`]): one
    /// `zell_leak_fields(ti, {offset, "Type.Field"}..., {-1, NULL})` per
    /// struct type info with `*T` fields (runtime/ABI.md 3.5).
    pub(crate) fn finish_leak_fields(&mut self) -> super::R<()> {
        let Some(f) = self.leak_fields_fn else { return Ok(()) };
        let mut tis: Vec<(TypeId, PointerValue<'ctx>)> = self.type_infos.iter().map(|(t, p)| (*t, *p)).collect();
        tis.sort_by_key(|(t, _)| *t);
        let ent = self.ctx.struct_type(&[self.i64_ty().into(), self.ptr_ty().into()], false);
        let mut regs = Vec::new();
        for (t, ti) in tis {
            let Type::Struct(d) = self.ty(t).clone() else { continue };
            let st = self.struct_llty(d.0);
            let sname = self.prog.structs[d.0 as usize].name.clone();
            let fields = self.prog.structs[d.0 as usize].fields.clone();
            let mut ents = Vec::new();
            for (i, fd) in fields.iter().enumerate() {
                if !matches!(self.ty(fd.ty), Type::Pointer(_)) {
                    continue;
                }
                let off = self.td.offset_of_element(&st, i as u32).unwrap_or(0);
                let name = self.cstr(&format!("{sname}.{}", fd.name));
                ents.push(ent.const_named_struct(&[self.i64_ty().const_int(off, false).into(), name.into()]));
            }
            if ents.is_empty() {
                continue;
            }
            ents.push(ent.const_named_struct(&[self.i64_ty().const_all_ones().into(), self.ptr_ty().const_null().into()]));
            let arr = ent.const_array(&ents);
            let n = self.fresh();
            let g = self.module.add_global(arr.get_type(), None, &format!("leak.fields.{n}"));
            g.set_linkage(Linkage::Internal);
            g.set_constant(true);
            g.set_initializer(&arr);
            let acyclic = crate::sema::cycles::has_attr(&self.prog.structs[d.0 as usize].attrs, "acyclic").then(|| self.cstr(&sname));
            regs.push((ti, g.as_pointer_value(), acyclic));
        }
        self.with_helper(f, |cg| {
            for (ti, g, acyclic) in regs {
                cg.call_rt("zell_leak_fields", &[ti.into(), g.into()]);
                // `@acyclic` promises are checked against the leaked cycles.
                if let Some(name) = acyclic {
                    cg.call_rt("zell_leak_acyclic", &[ti.into(), name.into()]);
                }
            }
            cg.b.build_return(None).unwrap();
            Ok(())
        })
    }

    /// Allocates a heap object holding `v` (owned, moved in); returns the
    /// object pointer (+1).
    pub(crate) fn new_box(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) -> PointerValue<'ctx> {
        let ti = self.type_info(t);
        let size = self.i64_ty().const_int(16 + self.size_of(t), false);
        let obj = self.cyc_alloc(t, ti, size);
        let p = self.payload(obj);
        self.b.build_store(p, v).unwrap();
        obj
    }

    /// A private NUL-terminated C string constant.
    pub(crate) fn cstr(&mut self, s: &str) -> PointerValue<'ctx> {
        if let Some(p) = self.cstrings.get(s) {
            return *p;
        }
        let bytes = self.ctx.const_string(s.as_bytes(), true);
        let n = self.fresh();
        let g = self.module.add_global(bytes.get_type(), None, &format!(".cstr.{n}"));
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_unnamed_addr(true);
        g.set_initializer(&bytes);
        self.cstrings.insert(s.to_string(), g.as_pointer_value());
        g.as_pointer_value()
    }

    /// An immortal string literal (ABI.md §4.1); borrowed:
    /// `{ i64 -1, ptr @zell_type_string, i64 len, ptr <bytes>, [len+1 x i8] }`
    /// whose data pointer (+24) addresses its own bytes (+32).
    pub(crate) fn str_lit(&mut self, s: &str) -> PointerValue<'ctx> {
        if let Some(p) = self.strings.get(s) {
            return *p;
        }
        let bytes = self.ctx.const_string(s.as_bytes(), true);
        let st = self.ctx.struct_type(&[self.i64_ty().into(), self.ptr_ty().into(), self.i64_ty().into(), self.ptr_ty().into(), bytes.get_type().into()], false);
        let tis = self.rt_global("zell_type_string");
        let n = self.fresh();
        let g = self.module.add_global(st, None, &format!(".str.{n}"));
        let i32t = self.ctx.i32_type();
        // SAFETY: a constant in-bounds GEP to field 4 of the global itself.
        let data = unsafe { g.as_pointer_value().const_in_bounds_gep(st, &[i32t.const_zero(), i32t.const_int(4, false)]) };
        let init = st.const_named_struct(&[
            self.i64_ty().const_int(u64::MAX, true).into(),
            tis.into(),
            self.i64_ty().const_int(s.len() as u64, false).into(),
            data.into(),
            bytes.into(),
        ]);
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_unnamed_addr(true);
        g.set_alignment(8);
        g.set_initializer(&init);
        self.strings.insert(s.to_string(), g.as_pointer_value());
        g.as_pointer_value()
    }

    /// Zero value of `t`.
    pub(crate) fn zero(&mut self, t: TypeId) -> BasicValueEnum<'ctx> {
        self.llty(t).const_zero()
    }
}
