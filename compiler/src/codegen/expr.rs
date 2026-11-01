//! Expressions: places, loads, literals, calls, indexing and maps.

use super::{unsupported, CodeGen, Val, R};
use crate::hir::{Callee, ConstValue, Expr, ExprKind, ExternId, FuncId};
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::types::{BasicMetadataTypeEnum, BasicType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::IntPredicate;

/// How a map's keys are passed to the runtime.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    Str,
    Int,
    Generic,
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Evaluates `e` and returns an owned (+1) value.
    pub(crate) fn expr_owned(&mut self, e: &Expr) -> R<BasicValueEnum<'ctx>> {
        let v = self.expr(e)?;
        if v.v.is_none() {
            return unsupported("void value used");
        }
        Ok(self.to_owned(e.ty, v))
    }

    /// Evaluates `e` for its effects and releases the result.
    pub(crate) fn expr_discard(&mut self, e: &Expr) -> R<()> {
        let v = self.expr(e)?;
        self.drop_val(e.ty, v);
        Ok(())
    }

    /// Whether `e` denotes an addressable location.
    pub(crate) fn is_place(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) | ExprKind::Deref(_) => true,
            ExprKind::Field { base, .. } => self.is_place(base),
            ExprKind::Index { base, .. } => match self.ty(base.ty) {
                Type::Slice(_) => true,
                Type::Array(..) => self.is_place(base),
                _ => false,
            },
            _ => false,
        }
    }

    /// Address of place expression `e`. Owned intermediate values (a
    /// pointer or slice returned by a call, a struct temporary) are held
    /// until the end of the statement.
    pub(crate) fn place(&mut self, e: &Expr) -> R<PointerValue<'ctx>> {
        self.place_ex(e, false)
    }

    /// [`place`](Self::place); with `pin` the owners of the storage (the
    /// pointer of a `Deref`, the slice of an `Index`) are also kept alive
    /// until the end of the statement or call, for `&` of an interior place.
    pub(crate) fn place_ex(&mut self, e: &Expr, pin: bool) -> R<PointerValue<'ctx>> {
        match &e.kind {
            ExprKind::Local(l) => Ok(self.local_addr(*l)),
            ExprKind::Upvalue(i) => Ok(self.upvalue_addr(*i)),
            ExprKind::Global(g) => Ok(self.globals[g.0 as usize].as_pointer_value()),
            ExprKind::Deref(p) => {
                let pv = self.expr(p)?;
                self.keep(p, pv, pin);
                let pv = pv.get().into_pointer_value();
                self.nil_check(pv);
                Ok(pv)
            }
            ExprKind::Field { base, index } => {
                let bp = if self.is_place(base) {
                    self.place_ex(base, pin)?
                } else {
                    let v = self.expr_owned(base)?;
                    self.hold_raw(base.ty, v);
                    self.spill(v)
                };
                let st = self.llty(base.ty).into_struct_type();
                Ok(self.b.build_struct_gep(st, bp, *index, "").unwrap())
            }
            ExprKind::Index { base, index } => match self.ty(base.ty) {
                Type::Slice(et) => {
                    let et = *et;
                    let s = self.expr(base)?;
                    self.keep(base, s, pin);
                    let i = self.expr(index)?.get().into_int_value();
                    Ok(self.slice_elem_ptr(et, s.get(), i, index.ty))
                }
                Type::Array(et, n) => {
                    let (et, n) = (*et, *n);
                    let bp = if self.is_place(base) {
                        self.place_ex(base, pin)?
                    } else {
                        let v = self.expr_owned(base)?;
                        self.hold_raw(base.ty, v);
                        self.spill(v)
                    };
                    let i = self.expr(index)?.get().into_int_value();
                    let i = self.index_i64(i, index.ty);
                    self.bounds_check(i, self.i64_ty().const_int(n, false));
                    let at = self.llty(base.ty);
                    let _ = et;
                    Ok(unsafe { self.b.build_in_bounds_gep(at, bp, &[self.i64_ty().const_zero(), i], "") }.unwrap())
                }
                _ => unsupported("assignment to a string index"),
            },
            _ => {
                // Not a place: materialise a temporary (only read through);
                // a stable borrowed value (a borrow-returning call) needs
                // no count of its own.
                let v = self.expr(e)?;
                let v = if v.owned || !self.needs_rc(e.ty) || self.is_stable(e) { v } else { Val::owned(self.to_owned(e.ty, v)) };
                self.hold(e.ty, v);
                Ok(self.spill(v.get()))
            }
        }
    }

    /// Keeps the owner value `v` of expression `e` alive until the end of
    /// the statement: owned values are held; with `pin`, borrowed values
    /// that could be released meanwhile are retained and held.
    fn keep(&mut self, e: &Expr, v: Val<'ctx>, pin: bool) {
        if v.owned {
            self.hold(e.ty, v);
        } else if pin && self.needs_rc(e.ty) && !self.is_stable(e) {
            self.retain_val(e.ty, v.get());
            let h = self.hold_raw(e.ty, v.get());
            self.mark_pin(h, e.ty);
        }
    }

    /// True if the value of `e` cannot be released while the current
    /// statement runs: constants, non-cell locals (only this function
    /// writes them), the cells of `&x`, and interior pointers (whose owner
    /// is pinned).
    pub(crate) fn is_stable(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Const(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::FuncRef(_) | ExprKind::AttrConst { .. } => true,
            ExprKind::Local(l) => self.fs().locals.get(l.0 as usize).copied().flatten().is_some_and(|s| !s.cell),
            // A field of a struct value held in a stable local: only this
            // function writes the local, and not during the statement.
            ExprKind::Field { base, .. } if matches!(self.ty(base.ty), Type::Struct(_)) && matches!(base.kind, ExprKind::Local(_) | ExprKind::Field { .. }) => self.is_stable(base),
            // A result borrowed from the arguments of a direct call: they are
            // held (or stable) until the end of the statement.
            ExprKind::Call { callee: Callee::Func(fid), args } => self.borrow_call(*fid, args).is_some(),
            ExprKind::AddrOf(x) => match &x.kind {
                ExprKind::Local(_) | ExprKind::Upvalue(_) => true,
                ExprKind::Deref(y) => self.is_stable(y),
                _ => crate::hir::ownership::is_interior_addr(e),
            },
            _ => false,
        }
    }

    /// The borrowed-result body of `fid` (`CodeGen::funcs_borrow`) if a
    /// direct call with `args` may take its result borrowed: for every
    /// parameter the result reads through, the argument is the address of
    /// a place that stays stable for the statement (a non-cell local, an
    /// inline struct field of one, or another such call's temporary).
    pub(crate) fn borrow_call(&self, fid: FuncId, args: &[Expr]) -> Option<FunctionValue<'ctx>> {
        let bf = *self.funcs_borrow.get(&fid.0)?;
        let deref = self.borrow_ret.get(fid.0 as usize)?.as_ref()?;
        for (k, a) in args.iter().enumerate() {
            if !deref.get(k).copied().unwrap_or(true) {
                continue;
            }
            let ExprKind::AddrOf(x) = &a.kind else { return None };
            if !self.is_stable(x) {
                return None;
            }
        }
        Some(bf)
    }

    /// True if evaluating `e` may run user code (a call, `await`, a
    /// `catch` block, a `Message()` through formatting), which could
    /// release a value borrowed from a non-local place.
    pub(crate) fn may_run_code(&self, e: &Expr) -> bool {
        let mut found = false;
        crate::hir::ownership::walk_expr(self.prog, e, crate::hir::ownership::Pos::Read, &mut |x, _| {
            if matches!(
                &x.kind,
                ExprKind::Call { callee: Callee::Func(_) | Callee::Interface { .. } | Callee::Value(_), .. }
                    | ExprKind::Await(_)
                    | ExprKind::TaskAll(_)
                    | ExprKind::TryCatch { .. }
                    | ExprKind::ToString(_)
                    | ExprKind::Print(_)
            ) {
                found = true;
            }
        });
        found
    }

    /// Evaluates `e`; if the value is borrowed from a place that code run
    /// by a later operand could overwrite (`later` is true), it is retained.
    pub(crate) fn operand(&mut self, e: &Expr, later: bool) -> R<Val<'ctx>> {
        let v = self.expr(e)?;
        if later && !v.owned && v.v.is_some() && self.needs_rc(e.ty) && !self.is_stable(e) {
            return Ok(Val::owned(self.to_owned(e.ty, v)));
        }
        Ok(v)
    }

    /// Evaluates a call argument for a parameter of type `t`: `moved`
    /// parameters (destructors) take an owned value; for a `borrowed`
    /// parameter (or an unknown callee) a value that could be released
    /// during the call is retained (pinned); owned values are held.
    pub(crate) fn arg(&mut self, a: &Expr, moved: bool, borrowed: bool, later: bool) -> R<(Val<'ctx>, super::cleanup::Hold)> {
        let v = self.expr(a)?;
        let mut pinned = false;
        let v = if v.v.is_none() {
            v
        } else if moved {
            Val::owned(self.to_owned(a.ty, v))
        } else if (borrowed || later) && !v.owned && self.needs_rc(a.ty) && !self.is_stable(a) {
            pinned = true;
            Val::owned(self.to_owned(a.ty, v))
        } else {
            v
        };
        let h = self.hold(a.ty, v);
        if pinned {
            self.mark_pin(h, a.ty);
        }
        Ok((v, h))
    }

    /// Integer index converted to `i64`.
    pub(crate) fn index_i64(&mut self, i: IntValue<'ctx>, ty: TypeId) -> IntValue<'ctx> {
        let bits = i.get_type().get_bit_width();
        if bits == 64 {
            i
        } else if self.is_signed(ty) {
            self.b.build_int_s_extend(i, self.i64_ty(), "").unwrap()
        } else {
            self.b.build_int_z_extend(i, self.i64_ty(), "").unwrap()
        }
    }

    /// Panics with `index out of range` unless `0 <= i < len`.
    pub(crate) fn bounds_check(&mut self, i: IntValue<'ctx>, len: IntValue<'ctx>) {
        let ok = self.b.build_int_compare(IntPredicate::ULT, i, len, "inb").unwrap();
        let good = self.block("inbounds");
        let bad = self.block("oob");
        self.b.build_conditional_branch(ok, good, bad).unwrap();
        self.b.position_at_end(bad);
        self.call_rt("zell_panic_bounds", &[i.into(), len.into()]);
        self.b.build_unreachable().unwrap();
        self.b.position_at_end(good);
    }

    /// Bounds-checked address of element `i` of slice value `s`.
    pub(crate) fn slice_elem_ptr(&mut self, et: TypeId, s: BasicValueEnum<'ctx>, i: IntValue<'ctx>, ity: TypeId) -> PointerValue<'ctx> {
        let sv = s.into_struct_value();
        let data = self.b.build_extract_value(sv, 0, "data").unwrap().into_pointer_value();
        let len = self.b.build_extract_value(sv, 1, "len").unwrap().into_int_value();
        let i = self.index_i64(i, ity);
        self.bounds_check(i, len);
        let elt = self.llty(et);
        unsafe { self.b.build_in_bounds_gep(elt, data, &[i], "elem") }.unwrap()
    }

    /// Loads the value at place `e` (borrowed).
    fn load_place(&mut self, e: &Expr) -> R<Val<'ctx>> {
        let p = self.place(e)?;
        let t = self.llty(e.ty);
        Ok(Val::borrowed(self.b.build_load(t, p, "").unwrap()))
    }

    /// Emits expression `e`.
    pub(crate) fn expr(&mut self, e: &Expr) -> R<Val<'ctx>> {
        if matches!(e.kind, ExprKind::Call { .. } | ExprKind::Await(_) | ExprKind::Print(_)) {
            self.dbg_span(e.span);
        }
        match &e.kind {
            ExprKind::Const(c) => self.constant(c, e.ty),
            ExprKind::Nil | ExprKind::Zero => {
                if self.llty_opt(e.ty).is_none() {
                    return Ok(Val::void());
                }
                Ok(Val::borrowed(self.zero(e.ty)))
            }
            // An immortal constant object: never retained or released.
            ExprKind::AttrConst { attr, sid } => Ok(Val::borrowed(self.attr_object(attr, *sid).into())),
            ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) => self.load_place(e),
            ExprKind::FuncRef(f) => self.func_ref(*f, e.ty),
            ExprKind::Field { base, index } => {
                if self.is_place(base) {
                    return self.load_place(e);
                }
                let bv = self.expr(base)?;
                let fv = self.b.build_extract_value(bv.get().into_struct_value(), *index, "").unwrap();
                if bv.owned && self.needs_rc(base.ty) {
                    self.retain_val(e.ty, fv);
                    self.release_val(base.ty, bv.get());
                    return Ok(Val::owned(fv));
                }
                Ok(Val::borrowed(fv))
            }
            ExprKind::Deref(_) => self.load_place(e),
            ExprKind::AddrOf(inner) => {
                if let ExprKind::Deref(p) = &inner.kind {
                    // `&*p` is `p` (with its ownership).
                    let pv = self.expr(p)?;
                    self.nil_check(pv.get().into_pointer_value());
                    return Ok(pv);
                }
                if self.is_place(inner) {
                    // `&x` of a variable (a cell) or an interior pointer
                    // (whose owners are pinned): borrowed.
                    let p = self.place_ex(inner, true)?;
                    return Ok(Val::borrowed(p.into()));
                }
                let v = self.expr_owned(inner)?;
                let obj = self.new_box(inner.ty, v);
                Ok(Val::owned(self.payload(obj).into()))
            }
            ExprKind::New(inner) => {
                let v = self.expr_owned(inner)?;
                let obj = self.new_box(inner.ty, v);
                Ok(Val::owned(self.payload(obj).into()))
            }
            ExprKind::Index { base, index } => self.index(e, base, index),
            ExprKind::MapGet { map, key } => self.map_get(e.ty, map, key),
            ExprKind::SubSlice { base, lo, hi } => self.sub_slice(e, base, lo.as_deref(), hi.as_deref()),
            ExprKind::Unary { op, operand } => self.unary(*op, operand),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs),
            ExprKind::IsNil { value, negated } => self.is_nil(value, *negated),
            ExprKind::Convert { kind, value } => self.convert(*kind, value, e.ty),
            ExprKind::Call { callee, args } => self.call(e, callee, args),
            ExprKind::StructLit { struct_id, fields } => {
                let st = self.struct_llty(*struct_id);
                let mut agg: BasicValueEnum = st.const_zero().into();
                // Each value is held while the later fields are evaluated
                // (an early exit releases it), then moved into the
                // aggregate. Only those holds are consumed: temporaries the
                // field expressions created (an owned argument to a call
                // in them) stay until the end of the statement.
                let mut holds = Vec::with_capacity(fields.len());
                for (idx, fe) in fields {
                    let v = self.expr_owned(fe)?;
                    holds.push(self.hold_raw(fe.ty, v));
                    agg = self.b.build_insert_value(agg.into_struct_value(), v, *idx, "").unwrap().into_struct_value().into();
                }
                for h in holds {
                    self.unhold(h);
                }
                Ok(Val::owned(agg))
            }
            ExprKind::SliceLit(elems) => self.slice_lit(e.ty, elems),
            ExprKind::MapLit(pairs) => {
                let m = self.new_map(e.ty)?;
                let (kt, vt) = self.map_kv(e.ty);
                let d = self.depth();
                self.hold_raw(e.ty, m.into());
                for (k, v) in pairs {
                    let d2 = self.depth();
                    let kv = self.expr(k)?;
                    self.hold(k.ty, kv);
                    let vv = self.expr(v)?;
                    self.hold(v.ty, vv);
                    self.map_set(kt, vt, m, kv.get(), vv.get())?;
                    self.release_temps_from(d2);
                }
                self.forget_from(d);
                Ok(Val::owned(m.into()))
            }
            ExprKind::Tuple(es) => {
                let tt = self.llty(e.ty).into_struct_type();
                let mut agg: BasicValueEnum = tt.get_undef().into();
                let mut holds = Vec::with_capacity(es.len());
                for (i, x) in es.iter().enumerate() {
                    let v = self.expr_owned(x)?;
                    holds.push(self.hold_raw(x.ty, v));
                    agg = self.b.build_insert_value(agg.into_struct_value(), v, i as u32, "").unwrap().into_struct_value().into();
                }
                for h in holds {
                    self.unhold(h);
                }
                Ok(Val::owned(agg))
            }
            ExprKind::EnumLit { enum_id, variant, args } => {
                let en = &self.prog.enums[*enum_id as usize];
                if en.is_plain {
                    return Ok(Val::borrowed(self.ctx.i32_type().const_int(*variant as u64, false).into()));
                }
                let et = self.enum_llty(*enum_id);
                let p = self.entry_alloca(et.into(), "enum", false);
                self.b.build_store(p, et.const_zero()).unwrap();
                let tp = self.b.build_struct_gep(et, p, 0, "").unwrap();
                self.b.build_store(tp, self.ctx.i32_type().const_int(*variant as u64, false)).unwrap();
                let pp = self.b.build_struct_gep(et, p, 1, "").unwrap();
                let vt = self.variant_llty(*enum_id, *variant);
                let mut holds = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    let v = self.expr_owned(a)?;
                    holds.push(self.hold_raw(a.ty, v));
                    let fp = self.b.build_struct_gep(vt, pp, i as u32, "").unwrap();
                    self.b.build_store(fp, v).unwrap();
                }
                for h in holds {
                    self.unhold(h);
                }
                Ok(Val::owned(self.b.build_load(et, p, "").unwrap()))
            }
            ExprKind::ToInterface { value } => self.to_interface(value, e.ty),
            ExprKind::Closure { func, captures } => self.emit_closure(*func, captures),
            ExprKind::EnvClosures { funcs, env } => self.emit_env_closures(e.ty, funcs, env.as_deref()),
            ExprKind::Propagate { value, context } => self.propagate(value, context.as_deref(), e.span),
            ExprKind::TryCatch { value, err, handler, result_used } => self.try_catch(e, value, *err, handler, *result_used),
            ExprKind::TryElse { value, fallback } => self.try_else(e.ty, value, fallback),
            ExprKind::Await(t) => self.await_expr(e.ty, t),
            ExprKind::TaskAll(_) => unsupported("task.All whose result is not awaited directly"),
            ExprKind::Builtin { op, args } => self.builtin(e, *op, args),
            ExprKind::ToString(v) => self.to_string(v),
            ExprKind::Print(items) => {
                self.print(items)?;
                Ok(Val::void())
            }
        }
    }

    fn constant(&mut self, c: &ConstValue, ty: TypeId) -> R<Val<'ctx>> {
        let v: BasicValueEnum = match c {
            ConstValue::Int(i) => match self.ty(ty) {
                Type::Float32 => self.ctx.f32_type().const_float(*i as f64).into(),
                Type::Float64 => self.ctx.f64_type().const_float(*i as f64).into(),
                _ => {
                    let t = self.llty(ty).into_int_type();
                    t.const_int(*i as u64, false).into()
                }
            },
            ConstValue::Float(f) => match self.ty(ty) {
                Type::Float32 => self.ctx.f32_type().const_float(*f).into(),
                _ => self.ctx.f64_type().const_float(*f).into(),
            },
            ConstValue::Bool(b) => self.ctx.bool_type().const_int(*b as u64, false).into(),
            ConstValue::Char(ch) => self.ctx.i32_type().const_int(*ch as u64, false).into(),
            ConstValue::Str(s) => self.str_lit(s).into(),
            ConstValue::Enum(eid, v) => {
                if self.prog.enums[*eid as usize].is_plain {
                    self.ctx.i32_type().const_int(*v as u64, false).into()
                } else {
                    let et = self.enum_llty(*eid);
                    let z = et.get_field_type_at_index(1).unwrap().const_zero();
                    et.const_named_struct(&[self.ctx.i32_type().const_int(*v as u64, false).into(), z]).into()
                }
            }
            ConstValue::List(_) | ConstValue::Name(_) => return unsupported("list/name constants outside attributes"),
        };
        Ok(Val::borrowed(v))
    }

    fn index(&mut self, e: &Expr, base: &Expr, index: &Expr) -> R<Val<'ctx>> {
        match self.ty(base.ty) {
            Type::Slice(et) => {
                let et = *et;
                let later = self.may_run_code(index);
                let bv = self.operand(base, later)?;
                let h = self.hold(base.ty, bv);
                let i = self.expr(index)?.get().into_int_value();
                self.unhold(h);
                let p = self.slice_elem_ptr(et, bv.get(), i, index.ty);
                let t = self.llty(et);
                let v = self.b.build_load(t, p, "").unwrap();
                if bv.owned {
                    self.retain_val(et, v);
                    self.release_val(base.ty, bv.get());
                    return Ok(Val::owned(v));
                }
                Ok(Val::borrowed(v))
            }
            Type::Array(..) => {
                if self.is_place(base) {
                    return self.load_place(e);
                }
                let bv = self.expr(base)?;
                let p = self.spill(bv.get());
                let h = self.hold(base.ty, bv);
                let i = self.expr(index)?.get().into_int_value();
                self.unhold(h);
                let i = self.index_i64(i, index.ty);
                let Type::Array(et, n) = self.ty(base.ty).clone() else { unreachable!() };
                self.bounds_check(i, self.i64_ty().const_int(n, false));
                let at = self.llty(base.ty);
                let ep = unsafe { self.b.build_in_bounds_gep(at, p, &[self.i64_ty().const_zero(), i], "") }.unwrap();
                let t = self.llty(et);
                let v = self.b.build_load(t, ep, "").unwrap();
                if bv.owned {
                    self.retain_val(et, v);
                    self.release_val(base.ty, bv.get());
                    return Ok(Val::owned(v));
                }
                Ok(Val::borrowed(v))
            }
            Type::String => {
                let later = self.may_run_code(index);
                let s = self.operand(base, later)?;
                let h = self.hold(base.ty, s);
                let i = self.expr(index)?.get().into_int_value();
                self.unhold(h);
                let i = self.index_i64(i, index.ty);
                let f = self.byte_at_fn();
                let r = self.call_c(f, &[s.get().into(), i.into()]).unwrap();
                self.drop_val(base.ty, s);
                Ok(Val::borrowed(r))
            }
            _ => unsupported(format!("indexing a value of type {}", self.type_name(base.ty))),
        }
    }

    fn byte_at_fn(&mut self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("zell_string_byte_at") {
            return f;
        }
        let fty = self.ctx.i8_type().fn_type(&[self.ptr_ty().into(), self.i64_ty().into()], false);
        self.module.add_function("zell_string_byte_at", fty, Some(Linkage::External))
    }

    fn sub_slice(&mut self, e: &Expr, base: &Expr, lo: Option<&Expr>, hi: Option<&Expr>) -> R<Val<'ctx>> {
        let bound = |cg: &mut Self, x: Option<&Expr>| -> R<Option<IntValue<'ctx>>> {
            match x {
                None => Ok(None),
                Some(x) => {
                    let v = cg.expr(x)?.get().into_int_value();
                    Ok(Some(cg.index_i64(v, x.ty)))
                }
            }
        };
        match self.ty(base.ty).clone() {
            Type::String => {
                let s = self.expr(base)?;
                let h = self.hold(base.ty, s);
                let lo = bound(self, lo)?.unwrap_or(self.i64_ty().const_zero());
                let hi = match bound(self, hi)? {
                    Some(h) => h,
                    None => self.call_rt_v("zell_string_len", &[s.get().into()]).into_int_value(),
                };
                self.unhold(h);
                let r = self.call_rt_v("zell_string_slice", &[s.get().into(), lo.into(), hi.into()]);
                self.drop_val(base.ty, s);
                Ok(Val::owned(r))
            }
            Type::Slice(et) => {
                let s = self.expr(base)?;
                let sp = self.spill(s.get());
                let h = self.hold(base.ty, s);
                let lo = bound(self, lo)?.unwrap_or(self.i64_ty().const_zero());
                let hi = match bound(self, hi)? {
                    Some(h) => h,
                    None => self.b.build_extract_value(s.get().into_struct_value(), 1, "").unwrap().into_int_value(),
                };
                self.unhold(h);
                let out = self.entry_alloca(self.slice_ty().into(), "sub", false);
                let size = self.i64_ty().const_int(self.size_of(et), false);
                self.call_rt("zell_slice_sub", &[out.into(), sp.into(), lo.into(), hi.into(), size.into()]);
                self.drop_val(base.ty, s);
                Ok(Val::owned(self.b.build_load(self.slice_ty(), out, "").unwrap()))
            }
            Type::Array(et, n) => {
                let bp = self.place(base)?;
                let lo = bound(self, lo)?.unwrap_or(self.i64_ty().const_zero());
                let nv = self.i64_ty().const_int(n, false);
                let hi = bound(self, hi)?.unwrap_or(nv);
                let c1 = self.b.build_int_compare(IntPredicate::ULE, lo, hi, "").unwrap();
                let c2 = self.b.build_int_compare(IntPredicate::ULE, hi, nv, "").unwrap();
                let ok = self.b.build_and(c1, c2, "").unwrap();
                let good = self.block("sl.ok");
                let bad = self.block("sl.bad");
                self.b.build_conditional_branch(ok, good, bad).unwrap();
                self.b.position_at_end(bad);
                self.call_rt("zell_panic_bounds", &[hi.into(), nv.into()]);
                self.b.build_unreachable().unwrap();
                self.b.position_at_end(good);
                let elt = self.llty(et);
                let data = unsafe { self.b.build_in_bounds_gep(elt, bp, &[lo], "") }.unwrap();
                let len = self.b.build_int_sub(hi, lo, "").unwrap();
                let cap = self.b.build_int_sub(nv, lo, "").unwrap();
                let st = self.slice_ty();
                let mut v = self.b.build_insert_value(st.const_zero(), data, 0, "").unwrap();
                v = self.b.build_insert_value(v, len, 1, "").unwrap();
                v = self.b.build_insert_value(v, cap, 2, "").unwrap();
                let _ = e;
                // A local array lives in a heap cell (it is address-taken):
                // the slice owns a reference to the cell. Arrays elsewhere
                // (globals: static; others: borrowed-only, see sema) have no
                // backing object.
                // The root variable of `v.f.arr[i..j]` / `arr[i..j]`.
                let mut root: &Expr = base;
                loop {
                    match &root.kind {
                        ExprKind::Field { base: b, .. } => root = b,
                        ExprKind::Index { base: b, .. } if matches!(self.ty(b.ty), Type::Array(..)) => root = b,
                        _ => break,
                    }
                }
                let cell = match &root.kind {
                    ExprKind::Local(l) => {
                        let slot = self.fs().locals[l.0 as usize].unwrap();
                        slot.cell.then(|| self.b.build_load(self.ptr_ty(), slot.ptr, "cell").unwrap().into_pointer_value())
                    }
                    ExprKind::Upvalue(i) => Some(self.upvalue_cell_ptr(*i)),
                    _ => None,
                };
                if let Some(c) = cell {
                    self.call_rt("zell_retain", &[c.into()]);
                    v = self.b.build_insert_value(v, c, 3, "").unwrap();
                    return Ok(Val::owned(v.into_struct_value().into()));
                }
                Ok(Val::borrowed(v.into_struct_value().into()))
            }
            _ => unsupported("sub-slicing this type"),
        }
    }

    fn slice_lit(&mut self, ty: TypeId, elems: &[Expr]) -> R<Val<'ctx>> {
        if elems.is_empty() {
            return Ok(Val::borrowed(self.zero(ty)));
        }
        let et = self.elem_of(ty).unwrap();
        if let Type::Array(..) = self.ty(ty) {
            // A fixed-array literal: each owned element is held until the
            // aggregate owns it (an early exit releases those built so far).
            let mut agg = self.zero(ty).into_array_value();
            let mut holds = Vec::with_capacity(elems.len());
            for (i, x) in elems.iter().enumerate() {
                let v = self.expr_owned(x)?;
                holds.push(self.hold_raw(et, v));
                agg = self.b.build_insert_value(agg, v, i as u32, "").unwrap().into_array_value();
            }
            for h in holds.into_iter().rev() {
                self.unhold(h);
            }
            return Ok(Val::owned(agg.into()));
        }
        let out = self.entry_alloca(self.slice_ty().into(), "lit", false);
        let size = self.i64_ty().const_int(self.size_of(et), false);
        let n = self.i64_ty().const_int(elems.len() as u64, false);
        let ops = self.value_ops(et);
        self.call_rt("zell_slice_new_ops", &[out.into(), size.into(), n.into(), n.into(), ops.into()]);
        let elt = self.llty(et);
        // The slice is held while the elements are evaluated (an early exit
        // releases it with the elements stored so far) and consumed at the
        // end; temporaries the element expressions created (the receiver of
        // `-f()`, an owned argument) stay until the end of the statement.
        let whole = self.b.build_load(self.slice_ty(), out, "").unwrap();
        let h = self.hold_raw(ty, whole);
        for (i, x) in elems.iter().enumerate() {
            let v = self.expr_owned(x)?;
            let data = self.b.build_load(self.ptr_ty(), out, "data").unwrap().into_pointer_value();
            let p = unsafe { self.b.build_in_bounds_gep(elt, data, &[self.i64_ty().const_int(i as u64, false)], "") }.unwrap();
            self.b.build_store(p, v).unwrap();
        }
        self.unhold(h);
        Ok(Val::owned(self.b.build_load(self.slice_ty(), out, "").unwrap()))
    }

    // ------------------------------------------------------------------
    // Calls
    // ------------------------------------------------------------------

    /// Evaluates the arguments of a direct call of `fid` (see [`Self::arg`]);
    /// returns them with the holds of moved arguments and the cleanup depth
    /// before them.
    fn direct_args(&mut self, fid: FuncId, args: &[Expr]) -> R<(Vec<BasicMetadataValueEnum<'ctx>>, Vec<super::cleanup::Hold>, usize)> {
        let d = self.depth();
        let mut vals = Vec::new();
        let mut moved_holds = Vec::new();
        for (k, a) in args.iter().enumerate() {
            let moved = self.facts.moved(fid, k);
            let borrowed = !self.facts.owned(fid, k);
            let later = args[k + 1..].iter().any(|x| self.may_run_code(x));
            let (v, h) = self.arg(a, moved, borrowed, later)?;
            if moved {
                moved_holds.push(h);
            }
            vals.push(v.get().into());
        }
        Ok((vals, moved_holds, d))
    }

    /// A direct call of async function `fid`: returns the result slot it
    /// wrote (`CodeGen::coro_res_ty`), to be read by an `await` or turned
    /// into a `Task<T>` by `coro_res_task`.
    /// `awaiter` is the awaiting frame's handle for a linked call (ABI.md
    /// 10.3), None when the caller keeps the `Task<T>` or blocks.
    pub(crate) fn call_async_direct(&mut self, fid: FuncId, args: &[Expr], awaiter: Option<PointerValue<'ctx>>) -> R<PointerValue<'ctx>> {
        let (mut ma, moved_holds, d) = self.direct_args(fid, args)?;
        let ret = self.prog.funcs[fid.0 as usize].ret;
        let rty = self.coro_res_ty(ret);
        let res = self.entry_alloca(rty.into(), "res", false);
        self.coro_res_init(ret, res, awaiter);
        ma.push(res.into());
        let fv = self.funcs[fid.0 as usize];
        // Moved arguments belong to the callee from here on, also if it panics.
        for h in moved_holds {
            self.unhold(h);
        }
        self.call_fn(fv, &ma);
        self.release_temps_from(d);
        Ok(res)
    }

    fn call(&mut self, e: &Expr, callee: &Callee, args: &[Expr]) -> R<Val<'ctx>> {
        match callee {
            Callee::Func(fid) => {
                let f = &self.prog.funcs[fid.0 as usize];
                if f.is_async {
                    // The call's `Task<T>` is kept (not awaited here).
                    let ret = f.ret;
                    let res = self.call_async_direct(*fid, args, None)?;
                    let t = self.coro_res_task(ret, res);
                    return Ok(Val::owned(t.into()));
                }
                let (ma, moved_holds, d) = self.direct_args(*fid, args)?;
                if let Some(bf) = self.borrow_call(*fid, args) {
                    // Borrow-returning (hir::borrow::borrow_returning): the
                    // result's references belong to the argument values,
                    // which stay held (or are stable) to the end of the
                    // statement; the result is borrowed from them.
                    let r = self.call_fn(bf, &ma).try_as_basic_value().basic();
                    return Ok(Val { v: r, owned: false });
                }
                let fv = self.funcs[fid.0 as usize];
                // Moved arguments belong to the callee from here on, also if it panics.
                for h in moved_holds {
                    self.unhold(h);
                }
                let r = self.call_fn(fv, &ma).try_as_basic_value().basic();
                self.release_temps_from(d);
                let r = r.map(|v| self.trace_std_call(*fid, e.ty, v, e.span.start));
                Ok(Val { v: r, owned: true })
            }
            Callee::Extern(x) => self.extern_call(*x, args, e.span.start),
            Callee::Interface { recv, method } => self.iface_call(e.ty, recv, *method, args),
            Callee::Value(c) => {
                let d = self.depth();
                let (cv, _) = self.arg(c, false, true, false)?;
                let Type::Func(sig) = self.ty(c.ty).clone() else { return unsupported("call of a non-function value") };
                let mut vals = Vec::new();
                let mut moved_holds = Vec::new();
                for (k, a) in args.iter().enumerate() {
                    let moved = sig.params.get(k).is_some_and(|t| self.has_dtor(*t));
                    let (v, h) = self.arg(a, moved, true, false)?;
                    if moved {
                        moved_holds.push(h);
                    }
                    vals.push(v);
                }
                let raw: Vec<BasicValueEnum> = vals.iter().map(|v| v.get()).collect();
                // Moved arguments belong to the callee from here on, also if it panics.
                for h in moved_holds {
                    self.unhold(h);
                }
                let r = self.call_closure(c.ty, cv.get(), &raw)?;
                self.release_temps_from(d);
                Ok(Val { v: r, owned: true })
            }
        }
    }

    /// Declaration of a stdlib extern per `zell_rt.h` §14.
    pub(crate) fn extern_decl(&mut self, x: ExternId) -> FunctionValue<'ctx> {
        let ex = &self.prog.externs[x.0 as usize];
        if let Some(f) = self.module.get_function(&ex.symbol) {
            return f;
        }
        let mut ps: Vec<BasicMetadataTypeEnum> = Vec::new();
        for p in &ex.params {
            let t = match self.ty(*p) {
                Type::Slice(_) => self.ptr_ty().into(),
                _ => self.llty(*p),
            };
            ps.push(t.into());
        }
        let (ret_inner, fallible) = match self.ty(ex.ret) {
            Type::Fallible(t) => (*t, true),
            _ => (ex.ret, false),
        };
        let ret_ll = match self.ty(ret_inner) {
            Type::Slice(_) => {
                ps.push(self.ptr_ty().into());
                None
            }
            _ => self.llty_opt(ret_inner),
        };
        if fallible {
            ps.push(self.ptr_ty().into());
        }
        let fty = match ret_ll {
            Some(t) => t.fn_type(&ps, false),
            None => self.ctx.void_type().fn_type(&ps, false),
        };
        let f = self.module.add_function(&ex.symbol, fty, Some(Linkage::External));
        let zext = self.attr("zeroext");
        for (i, p) in ex.params.iter().enumerate() {
            if *self.ty(*p) == Type::Bool {
                f.add_attribute(inkwell::attributes::AttributeLoc::Param(i as u32), zext);
            }
        }
        if *self.ty(ret_inner) == Type::Bool {
            f.add_attribute(inkwell::attributes::AttributeLoc::Return, zext);
        }
        f
    }

    /// A stdlib extern call; `at` is the call's offset (errors made by the
    /// runtime are "created" at the call, see [`super::trace`]).
    fn extern_call(&mut self, x: ExternId, args: &[Expr], at: u32) -> R<Val<'ctx>> {
        if let Some(v) = self.math_intrinsic(x, args)? {
            return Ok(v);
        }
        if let Some(v) = self.mutex_intrinsic(x, args)? {
            return Ok(v);
        }
        let f = self.extern_decl(x);
        let ex = &self.prog.externs[x.0 as usize];
        let d = self.depth();
        let mut ma: Vec<BasicMetadataValueEnum> = Vec::new();
        for (k, a) in args.iter().enumerate() {
            let later = args[k + 1..].iter().any(|x| self.may_run_code(x));
            let v = self.operand(a, later)?;
            self.hold(a.ty, v);
            let pass: BasicValueEnum = match self.ty(a.ty) {
                Type::Slice(_) => self.spill(v.get()).into(),
                _ => v.get(),
            };
            ma.push(pass.into());
        }
        let (ret_inner, fallible) = match self.ty(ex.ret) {
            Type::Fallible(t) => (*t, true),
            _ => (ex.ret, false),
        };
        let out = if matches!(self.ty(ret_inner), Type::Slice(_)) {
            let o = self.entry_alloca(self.slice_ty().into(), "out", false);
            ma.push(o.into());
            Some(o)
        } else {
            None
        };
        let errp = if fallible {
            let p = self.entry_alloca(self.ptr_ty().into(), "err", false);
            self.b.build_store(p, self.ptr_ty().const_null()).unwrap();
            ma.push(p.into());
            Some(p)
        } else {
            None
        };
        let mut r = self.call_c(f, &ma);
        self.release_temps_from(d);
        if let Some(o) = out {
            r = Some(self.b.build_load(self.slice_ty(), o, "").unwrap());
        }
        if ex.symbol == "zell_errors_wrap" {
            if let Some(w) = r {
                r = Some(self.trace_created(w, at));
            }
        }
        if let Some(ep) = errp {
            let err = self.b.build_load(self.ptr_ty(), ep, "err").unwrap();
            let err = self.trace_created(err, at);
            let v = match r {
                None => err,
                Some(val) => {
                    let ft = self.llty(ex.ret).into_struct_type();
                    let s = self.b.build_insert_value(ft.get_undef(), val, 0, "").unwrap();
                    self.b.build_insert_value(s, err, 1, "").unwrap().into_struct_value().into()
                }
            };
            return Ok(Val::owned(v));
        }
        Ok(Val { v: r, owned: true })
    }

    // ------------------------------------------------------------------
    // Maps
    // ------------------------------------------------------------------

    pub(crate) fn map_kv(&self, mt: TypeId) -> (TypeId, TypeId) {
        match self.ty(mt) {
            Type::Map(k, v) => (*k, *v),
            _ => panic!("codegen: not a map type"),
        }
    }

    pub(crate) fn key_kind(&self, kt: TypeId) -> KeyKind {
        match self.ty(kt) {
            Type::String => KeyKind::Str,
            Type::Int(k) if k.bits() == 64 => KeyKind::Int,
            _ => KeyKind::Generic,
        }
    }

    /// A new empty map of type `mt` (+1).
    pub(crate) fn new_map(&mut self, mt: TypeId) -> R<PointerValue<'ctx>> {
        let (kt, vt) = self.map_kv(mt);
        let vs = self.i64_ty().const_int(self.size_of(vt), false);
        let vops = self.value_ops(vt);
        Ok(match self.key_kind(kt) {
            KeyKind::Str => self.call_rt_v("zell_map_new_str", &[vs.into(), vops.into()]),
            KeyKind::Int => self.call_rt_v("zell_map_new_int", &[vs.into(), vops.into()]),
            KeyKind::Generic => {
                let d = self.map_desc(kt, vt)?;
                self.call_rt_v("zell_map_new", &[d.into(), self.i64_ty().const_zero().into()])
            }
        }
        .into_pointer_value())
    }

    /// Constant `zell_map_desc` for scalar keys hashed bytewise.
    fn map_desc(&mut self, kt: TypeId, vt: TypeId) -> R<PointerValue<'ctx>> {
        let mt = self.prog.types.get(kt).clone();
        if !matches!(mt, Type::Int(_) | Type::Bool | Type::Char | Type::Enum(_) | Type::Pointer(_)) || self.needs_rc(kt) {
            return unsupported(format!("maps with key type {}", self.type_name(kt)));
        }
        let cache_key = self.prog.types.len() as u64 * kt.0 as u64 + vt.0 as u64;
        if let Some(p) = self.map_descs.get(&cache_key) {
            return Ok(*p);
        }
        let ks = self.size_of(kt);
        let n = self.fresh();
        let lk = self.llty(kt);
        // hash(ptr) -> i64
        let hty = self.i64_ty().fn_type(&[self.ptr_ty().into()], false);
        let hash = self.module.add_function(&format!("map.hash.{n}"), hty, Some(Linkage::Internal));
        self.with_helper(hash, |cg| {
            let p = hash.get_nth_param(0).unwrap();
            let h = cg.call_rt_v("zell_hash_bytes", &[p.into(), cg.i64_ty().const_int(ks, false).into()]);
            cg.b.build_return(Some(&h)).unwrap();
            Ok(())
        })?;
        let ety = self.ctx.bool_type().fn_type(&[self.ptr_ty().into(), self.ptr_ty().into()], false);
        let eq = self.module.add_function(&format!("map.eq.{n}"), ety, Some(Linkage::Internal));
        eq.add_attribute(inkwell::attributes::AttributeLoc::Return, self.attr("zeroext"));
        self.with_helper(eq, |cg| {
            let a = cg.b.build_load(lk, eq.get_nth_param(0).unwrap().into_pointer_value(), "").unwrap();
            let b = cg.b.build_load(lk, eq.get_nth_param(1).unwrap().into_pointer_value(), "").unwrap();
            let (a, b) = if a.is_pointer_value() {
                (
                    cg.b.build_ptr_to_int(a.into_pointer_value(), cg.i64_ty(), "").unwrap(),
                    cg.b.build_ptr_to_int(b.into_pointer_value(), cg.i64_ty(), "").unwrap(),
                )
            } else {
                (a.into_int_value(), b.into_int_value())
            };
            let r = cg.b.build_int_compare(IntPredicate::EQ, a, b, "").unwrap();
            cg.b.build_return(Some(&r)).unwrap();
            Ok(())
        })?;
        let p = self.ptr_ty();
        let dt = self.ctx.struct_type(&[self.i64_ty().into(), self.i64_ty().into(), p.into(), p.into(), p.into(), p.into()], false);
        let vops = self.value_ops(vt);
        let vs = self.size_of(vt);
        let g = self.module.add_global(dt, None, &format!("map.desc.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_initializer(&dt.const_named_struct(&[
            self.i64_ty().const_int(ks, false).into(),
            self.i64_ty().const_int(vs, false).into(),
            hash.as_global_value().as_pointer_value().into(),
            eq.as_global_value().as_pointer_value().into(),
            p.const_null().into(),
            vops.into(),
        ]));
        self.map_descs.insert(cache_key, g.as_pointer_value());
        Ok(g.as_pointer_value())
    }

    /// Borrowed value slot for `key`, or NULL.
    pub(crate) fn map_slot(&mut self, kt: TypeId, m: BasicValueEnum<'ctx>, key: BasicValueEnum<'ctx>, insert: bool) -> PointerValue<'ctx> {
        let r = match (self.key_kind(kt), insert) {
            (KeyKind::Str, false) => self.call_rt_v("zell_map_get_str", &[m.into(), key.into()]),
            (KeyKind::Int, false) => self.call_rt_v("zell_map_get_int", &[m.into(), key.into()]),
            (_, false) => {
                let kp = self.spill(key);
                self.call_rt_v("zell_map_get", &[m.into(), kp.into()])
            }
            (KeyKind::Str, true) => {
                let f = self.map_insert_fn("zell_map_get_or_insert_str", self.ptr_ty().into());
                self.call_c(f, &[m.into(), key.into()]).unwrap()
            }
            (KeyKind::Int, true) => {
                let f = self.map_insert_fn("zell_map_get_or_insert_int", self.i64_ty().into());
                self.call_c(f, &[m.into(), key.into()]).unwrap()
            }
            (_, true) => {
                let kp = self.spill(key);
                self.call_rt_v("zell_map_get_or_insert", &[m.into(), kp.into()])
            }
        };
        r.into_pointer_value()
    }

    /// Declaration of `void *name(zell_map *, K key)` (ABI.md 6.2 key-typed
    /// `get_or_insert` fast paths).
    fn map_insert_fn(&mut self, name: &str, key_ty: inkwell::types::BasicMetadataTypeEnum<'ctx>) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into(), key_ty], false);
        self.module.add_function(name, fty, Some(Linkage::External))
    }

    /// `m[k] = v` (the map retains key and value).
    pub(crate) fn map_set(&mut self, kt: TypeId, vt: TypeId, m: PointerValue<'ctx>, key: BasicValueEnum<'ctx>, v: BasicValueEnum<'ctx>) -> R<()> {
        let vp = self.spill(v);
        let _ = vt;
        match self.key_kind(kt) {
            KeyKind::Str => self.call_rt("zell_map_set_str", &[m.into(), key.into(), vp.into()]),
            KeyKind::Int => self.call_rt("zell_map_set_int", &[m.into(), key.into(), vp.into()]),
            KeyKind::Generic => {
                let kp = self.spill(key);
                self.call_rt("zell_map_set", &[m.into(), kp.into(), vp.into()])
            }
        };
        Ok(())
    }

    /// Comma-ok lookup (`hir::Builtin::MapLookup`): the owned tuple
    /// `(value or zero, found)` of type `tt`.
    pub(crate) fn map_lookup(&mut self, tt: TypeId, map: &Expr, key: &Expr) -> R<Val<'ctx>> {
        let (kt, vt) = self.map_kv(map.ty);
        let later = self.may_run_code(key);
        let mv = self.operand(map, later)?;
        let h = self.hold(map.ty, mv);
        let kv = self.expr(key)?;
        self.unhold(h);
        let slot = self.map_slot(kt, mv.get(), kv.get(), false);
        let lt = self.llty(vt);
        let res = self.entry_alloca(lt, "mapv", false);
        self.b.build_store(res, lt.const_zero()).unwrap();
        let found_bb = self.block("map.found");
        let join = self.block("map.join");
        let isnull = self.b.build_is_null(slot, "").unwrap();
        self.b.build_conditional_branch(isnull, join, found_bb).unwrap();
        self.b.position_at_end(found_bb);
        let v = self.b.build_load(lt, slot, "").unwrap();
        self.b.build_store(res, v).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let v = self.b.build_load(lt, res, "").unwrap();
        let found = self.b.build_is_not_null(slot, "found").unwrap();
        // The tuple owns its value (a zero value needs no retain).
        self.retain_val(vt, v);
        self.drop_val(kt, kv);
        self.drop_val(map.ty, mv);
        let st = self.llty(tt).into_struct_type();
        let agg = self.b.build_insert_value(st.get_undef(), v, 0, "").unwrap().into_struct_value();
        let agg = self.b.build_insert_value(agg, found, 1, "").unwrap().into_struct_value();
        Ok(Val::owned(agg.into()))
    }

    fn map_get(&mut self, vt: TypeId, map: &Expr, key: &Expr) -> R<Val<'ctx>> {
        let (kt, _) = self.map_kv(map.ty);
        let later = self.may_run_code(key);
        let mv = self.operand(map, later)?;
        let h = self.hold(map.ty, mv);
        let kv = self.expr(key)?;
        self.unhold(h);
        let slot = self.map_slot(kt, mv.get(), kv.get(), false);
        let lt = self.llty(vt);
        let res = self.entry_alloca(lt, "mapv", false);
        self.b.build_store(res, lt.const_zero()).unwrap();
        let found = self.block("map.found");
        let join = self.block("map.join");
        let isnull = self.b.build_is_null(slot, "").unwrap();
        self.b.build_conditional_branch(isnull, join, found).unwrap();
        self.b.position_at_end(found);
        let v = self.b.build_load(lt, slot, "").unwrap();
        self.b.build_store(res, v).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let v = self.b.build_load(lt, res, "").unwrap();
        self.drop_val(key.ty, kv);
        if mv.owned {
            self.retain_val(vt, v);
            self.drop_val(map.ty, mv);
            return Ok(Val::owned(v));
        }
        Ok(Val::borrowed(v))
    }
}
