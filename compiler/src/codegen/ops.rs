//! Operators, conversions and built-in operations.

use super::{unsupported, CodeGen, Val, R};
use crate::hir::{BinOp, Builtin, ConvKind, Expr, UnOp};
use crate::types::{Type, TypeId};
use inkwell::values::{BasicValueEnum, IntValue};
use inkwell::{FloatPredicate, IntPredicate};

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    pub(crate) fn unary(&mut self, op: UnOp, x: &Expr) -> R<Val<'ctx>> {
        let v = self.expr(x)?.get();
        Ok(Val::borrowed(match op {
            UnOp::Not => self.b.build_not(v.into_int_value(), "").unwrap().into(),
            UnOp::Neg => {
                if v.is_float_value() {
                    self.b.build_float_neg(v.into_float_value(), "").unwrap().into()
                } else {
                    self.b.build_int_neg(v.into_int_value(), "").unwrap().into()
                }
            }
        }))
    }

    pub(crate) fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> R<Val<'ctx>> {
        match op {
            BinOp::And | BinOp::Or => return self.short_circuit(op, lhs, rhs),
            _ => {}
        }
        let d = self.depth();
        let later = self.may_run_code(rhs);
        let l = self.operand(lhs, later)?;
        self.hold(lhs.ty, l);
        let r = self.expr(rhs)?;
        self.hold(rhs.ty, r);
        let v = self.binop_vals(op, lhs.ty, l.get(), r.get(), rhs.ty)?;
        self.release_temps_from(d);
        Ok(v)
    }

    /// Binary operation on evaluated operands (also used by `op=`).
    pub(crate) fn binop_vals(&mut self, op: BinOp, ty: TypeId, l: BasicValueEnum<'ctx>, r: BasicValueEnum<'ctx>, rty: TypeId) -> R<Val<'ctx>> {
        let b = &self.b;
        let bool_ = |v: IntValue<'ctx>| Val::borrowed(v.into());
        match op {
            BinOp::StrConcat => return Ok(Val::owned(self.call_rt_v("zell_string_concat", &[l.into(), r.into()]))),
            BinOp::StrEq | BinOp::StrNe => {
                let eq = self.call_rt_v("zell_string_eq", &[l.into(), r.into()]).into_int_value();
                return Ok(bool_(if op == BinOp::StrEq { eq } else { self.b.build_not(eq, "").unwrap() }));
            }
            BinOp::StrLt | BinOp::StrLe | BinOp::StrGt | BinOp::StrGe => {
                let c = self.call_rt_v("zell_string_compare", &[l.into(), r.into()]).into_int_value();
                let pred = match op {
                    BinOp::StrLt => IntPredicate::SLT,
                    BinOp::StrLe => IntPredicate::SLE,
                    BinOp::StrGt => IntPredicate::SGT,
                    _ => IntPredicate::SGE,
                };
                let z = self.ctx.i32_type().const_zero();
                return Ok(bool_(self.b.build_int_compare(pred, c, z, "").unwrap()));
            }
            _ => {}
        }
        if l.is_float_value() {
            let (x, y) = (l.into_float_value(), r.into_float_value());
            let v: BasicValueEnum = match op {
                BinOp::Add => b.build_float_add(x, y, "").unwrap().into(),
                BinOp::Sub => b.build_float_sub(x, y, "").unwrap().into(),
                BinOp::Mul => b.build_float_mul(x, y, "").unwrap().into(),
                BinOp::Div => b.build_float_div(x, y, "").unwrap().into(),
                BinOp::Rem => b.build_float_rem(x, y, "").unwrap().into(),
                BinOp::Eq => b.build_float_compare(FloatPredicate::OEQ, x, y, "").unwrap().into(),
                BinOp::Ne => b.build_float_compare(FloatPredicate::UNE, x, y, "").unwrap().into(),
                BinOp::Lt => b.build_float_compare(FloatPredicate::OLT, x, y, "").unwrap().into(),
                BinOp::Le => b.build_float_compare(FloatPredicate::OLE, x, y, "").unwrap().into(),
                BinOp::Gt => b.build_float_compare(FloatPredicate::OGT, x, y, "").unwrap().into(),
                BinOp::Ge => b.build_float_compare(FloatPredicate::OGE, x, y, "").unwrap().into(),
                _ => return unsupported(format!("operator {op:?} on floats")),
            };
            return Ok(Val::borrowed(v));
        }
        // Pointers / interfaces / closures compared by address.
        let (x, y) = match (l, r) {
            (BasicValueEnum::IntValue(x), BasicValueEnum::IntValue(y)) => (x, y),
            (BasicValueEnum::PointerValue(x), BasicValueEnum::PointerValue(y)) => {
                (self.b.build_ptr_to_int(x, self.i64_ty(), "").unwrap(), self.b.build_ptr_to_int(y, self.i64_ty(), "").unwrap())
            }
            (BasicValueEnum::StructValue(x), BasicValueEnum::StructValue(y)) if matches!(op, BinOp::Eq | BinOp::Ne) && x.get_type().count_fields() == 2 => {
                let mut acc = self.ctx.bool_type().const_int(1, false);
                for i in 0..2 {
                    let a = self.b.build_extract_value(x, i, "").unwrap();
                    let c = self.b.build_extract_value(y, i, "").unwrap();
                    if !a.is_pointer_value() {
                        return unsupported("comparison of these values");
                    }
                    let ai = self.b.build_ptr_to_int(a.into_pointer_value(), self.i64_ty(), "").unwrap();
                    let ci = self.b.build_ptr_to_int(c.into_pointer_value(), self.i64_ty(), "").unwrap();
                    let e = self.b.build_int_compare(IntPredicate::EQ, ai, ci, "").unwrap();
                    acc = self.b.build_and(acc, e, "").unwrap();
                }
                if op == BinOp::Ne {
                    acc = self.b.build_not(acc, "").unwrap();
                }
                return Ok(bool_(acc));
            }
            _ => return unsupported(format!("operator {op:?} on {}", self.type_name(ty))),
        };
        let signed = self.is_signed(ty);
        let b = &self.b;
        let v: IntValue = match op {
            BinOp::Add => b.build_int_add(x, y, "").unwrap(),
            BinOp::Sub => b.build_int_sub(x, y, "").unwrap(),
            BinOp::Mul => b.build_int_mul(x, y, "").unwrap(),
            BinOp::Div | BinOp::Rem => return Ok(Val::borrowed(self.int_divrem(op, x, y, signed).into())),
            BinOp::BitAnd => b.build_and(x, y, "").unwrap(),
            BinOp::BitOr => b.build_or(x, y, "").unwrap(),
            BinOp::BitXor => b.build_xor(x, y, "").unwrap(),
            BinOp::Shl | BinOp::Shr => return Ok(Val::borrowed(self.shift(op, x, y, signed, self.is_signed(rty)).into())),
            BinOp::Eq => b.build_int_compare(IntPredicate::EQ, x, y, "").unwrap(),
            BinOp::Ne => b.build_int_compare(IntPredicate::NE, x, y, "").unwrap(),
            BinOp::Lt => b.build_int_compare(if signed { IntPredicate::SLT } else { IntPredicate::ULT }, x, y, "").unwrap(),
            BinOp::Le => b.build_int_compare(if signed { IntPredicate::SLE } else { IntPredicate::ULE }, x, y, "").unwrap(),
            BinOp::Gt => b.build_int_compare(if signed { IntPredicate::SGT } else { IntPredicate::UGT }, x, y, "").unwrap(),
            BinOp::Ge => b.build_int_compare(if signed { IntPredicate::SGE } else { IntPredicate::UGE }, x, y, "").unwrap(),
            _ => return unsupported(format!("operator {op:?}")),
        };
        Ok(Val::borrowed(v.into()))
    }

    /// Go integer division: panics on zero; `MIN / -1` wraps.
    fn int_divrem(&mut self, op: BinOp, x: IntValue<'ctx>, y: IntValue<'ctx>, signed: bool) -> IntValue<'ctx> {
        let t = x.get_type();
        let zero = self.b.build_int_compare(IntPredicate::EQ, y, t.const_zero(), "").unwrap();
        let bad = self.block("div0");
        let ok = self.block("divok");
        self.b.build_conditional_branch(zero, bad, ok).unwrap();
        self.b.position_at_end(bad);
        let msg = self.cstr("runtime error: integer divide by zero");
        self.call_rt("zell_panic", &[msg.into()]);
        self.b.build_unreachable().unwrap();
        self.b.position_at_end(ok);
        if !signed {
            return if op == BinOp::Div { self.b.build_int_unsigned_div(x, y, "").unwrap() } else { self.b.build_int_unsigned_rem(x, y, "").unwrap() };
        }
        // Avoid the UB of MIN / -1: divide by 1 instead (MIN / -1 == -MIN == MIN, MIN % -1 == 0).
        let m1 = self.b.build_int_compare(IntPredicate::EQ, y, t.const_all_ones(), "").unwrap();
        let ysafe = self.b.build_select(m1, t.const_int(1, false), y, "").unwrap().into_int_value();
        if op == BinOp::Div {
            let q = self.b.build_int_signed_div(x, ysafe, "").unwrap();
            let neg = self.b.build_int_neg(x, "").unwrap();
            self.b.build_select(m1, neg, q, "").unwrap().into_int_value()
        } else {
            let r = self.b.build_int_signed_rem(x, ysafe, "").unwrap();
            self.b.build_select(m1, t.const_zero(), r, "").unwrap().into_int_value()
        }
    }

    /// Go shifts: counts >= width give 0 (or the sign for `>>` on signed).
    fn shift(&mut self, op: BinOp, x: IntValue<'ctx>, y: IntValue<'ctx>, signed: bool, rsigned: bool) -> IntValue<'ctx> {
        let t = x.get_type();
        let bits = t.get_bit_width();
        let yb = y.get_type().get_bit_width();
        let y64 = if yb < 64 {
            if rsigned {
                self.b.build_int_s_extend(y, self.i64_ty(), "").unwrap()
            } else {
                self.b.build_int_z_extend(y, self.i64_ty(), "").unwrap()
            }
        } else {
            y
        };
        let big = self.b.build_int_compare(IntPredicate::UGE, y64, self.i64_ty().const_int(bits as u64, false), "").unwrap();
        let cnt = if bits < 64 { self.b.build_int_truncate(y64, t, "").unwrap() } else { y64 };
        let safe = self.b.build_select(big, t.const_zero(), cnt, "").unwrap().into_int_value();
        if op == BinOp::Shl {
            let v = self.b.build_left_shift(x, safe, "").unwrap();
            self.b.build_select(big, t.const_zero(), v, "").unwrap().into_int_value()
        } else if signed {
            let maxs = self.b.build_select(big, t.const_int(bits as u64 - 1, false), safe, "").unwrap().into_int_value();
            self.b.build_right_shift(x, maxs, true, "").unwrap()
        } else {
            let v = self.b.build_right_shift(x, safe, false, "").unwrap();
            self.b.build_select(big, t.const_zero(), v, "").unwrap().into_int_value()
        }
    }

    fn short_circuit(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> R<Val<'ctx>> {
        let res = self.entry_alloca(self.ctx.bool_type().into(), "sc", false);
        let l = self.cond_expr(lhs)?;
        self.b.build_store(res, l).unwrap();
        let rhs_bb = self.block("sc.rhs");
        let done = self.block("sc.done");
        if op == BinOp::And {
            self.b.build_conditional_branch(l, rhs_bb, done).unwrap();
        } else {
            self.b.build_conditional_branch(l, done, rhs_bb).unwrap();
        }
        self.b.position_at_end(rhs_bb);
        let r = self.cond_expr(rhs)?;
        self.b.build_store(res, r).unwrap();
        self.br(done);
        self.b.position_at_end(done);
        Ok(Val::borrowed(self.b.build_load(self.ctx.bool_type(), res, "").unwrap()))
    }

    /// True if `v` (of type `t`) is nil.
    pub(crate) fn nil_test(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) -> IntValue<'ctx> {
        let p = match self.ty(t) {
            Type::Slice(_) | Type::Func(_) => self.b.build_extract_value(v.into_struct_value(), 0, "").unwrap().into_pointer_value(),
            Type::Interface(_) | Type::Any => self.b.build_extract_value(v.into_struct_value(), 1, "").unwrap().into_pointer_value(),
            _ => v.into_pointer_value(),
        };
        self.b.build_is_null(p, "").unwrap()
    }

    pub(crate) fn is_nil(&mut self, value: &Expr, negated: bool) -> R<Val<'ctx>> {
        let v = self.expr(value)?;
        let mut r = self.nil_test(value.ty, v.get());
        if negated {
            r = self.b.build_not(r, "").unwrap();
        }
        self.drop_val(value.ty, v);
        Ok(Val::borrowed(r.into()))
    }

    pub(crate) fn convert(&mut self, kind: ConvKind, value: &Expr, to: TypeId) -> R<Val<'ctx>> {
        let v = self.expr(value)?;
        let x = v.get();
        let from = value.ty;
        let tt = self.llty(to);
        let r: BasicValueEnum = match kind {
            ConvKind::IntToInt | ConvKind::IntToChar | ConvKind::CharToInt | ConvKind::EnumToInt => {
                let xi = x.into_int_value();
                let src_signed = match kind {
                    ConvKind::EnumToInt => false,
                    ConvKind::CharToInt => true,
                    _ => self.is_signed(from),
                };
                self.b.build_int_cast_sign_flag(xi, tt.into_int_type(), src_signed, "").unwrap().into()
            }
            ConvKind::IntToFloat => {
                let xi = x.into_int_value();
                if self.is_signed(from) {
                    self.b.build_signed_int_to_float(xi, tt.into_float_type(), "").unwrap().into()
                } else {
                    self.b.build_unsigned_int_to_float(xi, tt.into_float_type(), "").unwrap().into()
                }
            }
            ConvKind::FloatToInt => {
                let xf = x.into_float_value();
                if self.is_signed(to) {
                    self.b.build_float_to_signed_int(xf, tt.into_int_type(), "").unwrap().into()
                } else {
                    self.b.build_float_to_unsigned_int(xf, tt.into_int_type(), "").unwrap().into()
                }
            }
            ConvKind::FloatToFloat => self.b.build_float_cast(x.into_float_value(), tt.into_float_type(), "").unwrap().into(),
            ConvKind::CharToString => return Ok(Val::owned(self.call_rt_v("zell_string_from_rune", &[x.into()]))),
            ConvKind::PtrToWeak | ConvKind::WeakToPtr => {
                let f = if kind == ConvKind::PtrToWeak { "zell_weak_new" } else { "zell_weak_get" };
                let r = self.call_rt_v(f, &[x.into()]);
                self.drop_val(from, v);
                return Ok(Val::owned(r));
            }
        };
        Ok(Val::borrowed(r))
    }

    /// `*sp = append(*sp, x)` for the slice variable at `sp` with element
    /// type `et`; consumes `x` (released if it is an owned temporary).
    ///
    /// The common case `len < cap` is inlined: store the element (retaining
    /// it and releasing the slot's previous value, ABI.md 5.1) and bump
    /// `len`. Only a full slice calls `zell_slice_append`, which grows the
    /// backing (in place when uniquely referenced, ABI.md 5.2).
    pub(crate) fn append_fast(&mut self, sp: inkwell::values::PointerValue<'ctx>, et: TypeId, x: Val<'ctx>) {
        let size = self.size_of(et);
        let slow_call = |cg: &mut Self| {
            let xp = cg.spill(x.get());
            let sz = cg.i64_ty().const_int(size, false);
            let ops = cg.value_ops(et);
            cg.call_rt("zell_slice_append", &[sp.into(), sz.into(), ops.into(), xp.into()]);
        };
        if size == 0 {
            slow_call(self);
            self.drop_val(et, x);
            return;
        }
        let st = self.slice_ty();
        let lenp = self.b.build_struct_gep(st, sp, 1, "len.p").unwrap();
        let capp = self.b.build_struct_gep(st, sp, 2, "cap.p").unwrap();
        let len = self.b.build_load(self.i64_ty(), lenp, "len").unwrap().into_int_value();
        let cap = self.b.build_load(self.i64_ty(), capp, "cap").unwrap().into_int_value();
        let room = self.b.build_int_compare(IntPredicate::ULT, len, cap, "room").unwrap();
        let fast = self.block("append.fast");
        let slow = self.block("append.grow");
        let join = self.block("append.done");
        // Growth is rare: weight the branch so the fast path is laid out inline.
        let br = self.b.build_conditional_branch(room, fast, slow).unwrap();
        let md = self.ctx.metadata_node(&[
            self.ctx.metadata_string("branch_weights").into(),
            self.ctx.i32_type().const_int(2000, false).into(),
            self.ctx.i32_type().const_int(1, false).into(),
        ]);
        let _ = br.set_metadata(md, self.ctx.get_kind_id("prof"));
        self.b.position_at_end(fast);
        let data = self.b.build_load(self.ptr_ty(), sp, "data").unwrap().into_pointer_value();
        let elt = self.llty(et);
        let slot = unsafe { self.b.build_in_bounds_gep(elt, data, &[len], "slot") }.unwrap();
        let v = x.get();
        if self.needs_rc(et) {
            // Retain first: the old slot value may be the same object.
            self.retain_val(et, v);
            let old = self.b.build_load(elt, slot, "old").unwrap();
            self.b.build_store(slot, v).unwrap();
            self.release_val(et, old);
        } else {
            self.b.build_store(slot, v).unwrap();
        }
        let len1 = self.b.build_int_add(len, self.i64_ty().const_int(1, false), "len1").unwrap();
        self.b.build_store(lenp, len1).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        slow_call(self);
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        self.drop_val(et, x);
    }

    /// `math.Sqrt/Abs/Floor/Ceil` as LLVM intrinsics instead of calls into
    /// the runtime (same IEEE results as the C functions they wrap; lets
    /// LLVM emit `sqrtsd`/`roundsd` inline and vectorise). `None` for other
    /// externs.
    pub(crate) fn math_intrinsic(&mut self, x: crate::hir::ExternId, args: &[Expr]) -> R<Option<Val<'ctx>>> {
        let name = match self.prog.externs[x.0 as usize].symbol.as_str() {
            "zell_math_sqrt" => "llvm.sqrt",
            "zell_math_abs" => "llvm.fabs",
            "zell_math_floor" => "llvm.floor",
            "zell_math_ceil" => "llvm.ceil",
            _ => return Ok(None),
        };
        if args.len() != 1 {
            return Ok(None);
        }
        let v = self.expr(&args[0])?.get();
        if !v.is_float_value() {
            return unsupported(format!("{name} on a non-float argument"));
        }
        let f64t = self.ctx.f64_type();
        let Some(f) = inkwell::intrinsics::Intrinsic::find(name).and_then(|i| i.get_declaration(&self.module, &[f64t.into()])) else {
            return unsupported(format!("intrinsic {name}"));
        };
        let r = self.b.build_call(f, &[v.into()], "").unwrap().try_as_basic_value().basic().unwrap();
        Ok(Some(Val::borrowed(r)))
    }

    pub(crate) fn builtin(&mut self, e: &Expr, op: Builtin, args: &[Expr]) -> R<Val<'ctx>> {
        let ty = e.ty;
        match op {
            Builtin::LenString => {
                // Inline `s ? s->len : 0` (ABI.md 4.1: len at +16 of every
                // string, views included); folds for literals.
                let s = self.expr(&args[0])?;
                let p = s.get().into_pointer_value();
                let from = self.b.get_insert_block().unwrap();
                let load = self.block("len.load");
                let done = self.block("len.done");
                let nn = self.b.build_is_not_null(p, "").unwrap();
                self.b.build_conditional_branch(nn, load, done).unwrap();
                self.b.position_at_end(load);
                let lp = self.byte_gep(p, 16);
                let n = self.b.build_load(self.i64_ty(), lp, "len").unwrap();
                self.b.build_unconditional_branch(done).unwrap();
                self.b.position_at_end(done);
                let zero = self.i64_ty().const_zero();
                let phi = self.b.build_phi(self.i64_ty(), "len").unwrap();
                phi.add_incoming(&[(&zero, from), (&n, load)]);
                let r = phi.as_basic_value();
                self.drop_val(args[0].ty, s);
                Ok(Val::borrowed(r))
            }
            Builtin::LenSlice => {
                let s = self.expr(&args[0])?;
                let r = self.b.build_extract_value(s.get().into_struct_value(), 1, "len").unwrap();
                self.drop_val(args[0].ty, s);
                Ok(Val::borrowed(r))
            }
            Builtin::LenArray => {
                let n = match self.ty(args[0].ty) {
                    Type::Array(_, n) => *n,
                    _ => 0,
                };
                if !self.is_place(&args[0]) {
                    self.expr_discard(&args[0])?;
                }
                Ok(Val::borrowed(self.i64_ty().const_int(n, false).into()))
            }
            Builtin::MapLookup => self.map_lookup(ty, &args[0], &args[1]),
            Builtin::LenMap => {
                let m = self.expr(&args[0])?;
                let r = self.call_rt_v("zell_map_len", &[m.get().into()]);
                self.drop_val(args[0].ty, m);
                Ok(Val::borrowed(r))
            }
            Builtin::Append => {
                let et = self.elem_of(ty).unwrap();
                let s = self.expr_owned(&args[0])?;
                let sp = self.spill(s);
                let hs = self.hold_raw(args[0].ty, s);
                // Every value is evaluated before the first one is appended
                // (Go: `append(xs, len(xs), len(xs))`).
                let mut xs = Vec::with_capacity(args.len() - 1);
                for a in &args[1..] {
                    xs.push(self.expr(a)?);
                }
                self.unhold(hs);
                for x in xs {
                    self.append_fast(sp, et, x);
                }
                Ok(Val::owned(self.b.build_load(self.slice_ty(), sp, "").unwrap()))
            }
            Builtin::AppendSlice => {
                let et = self.elem_of(ty).unwrap();
                let s = self.expr_owned(&args[0])?;
                let sp = self.spill(s);
                let hs = self.hold_raw(args[0].ty, s);
                let o = self.expr(&args[1])?;
                self.unhold(hs);
                let op = self.spill(o.get());
                let sz = self.i64_ty().const_int(self.size_of(et), false);
                let ops = self.value_ops(et);
                self.call_rt("zell_slice_append_slice", &[sp.into(), sz.into(), ops.into(), op.into()]);
                self.drop_val(args[1].ty, o);
                Ok(Val::owned(self.b.build_load(self.slice_ty(), sp, "").unwrap()))
            }
            Builtin::Delete => {
                let (kt, _) = self.map_kv(args[0].ty);
                let m = self.expr(&args[0])?;
                let hm = self.hold(args[0].ty, m);
                let k = self.expr(&args[1])?;
                self.unhold(hm);
                match self.key_kind(kt) {
                    super::expr::KeyKind::Str => self.call_rt("zell_map_delete_str", &[m.get().into(), k.get().into()]),
                    super::expr::KeyKind::Int => self.call_rt("zell_map_delete_int", &[m.get().into(), k.get().into()]),
                    super::expr::KeyKind::Generic => {
                        let kp = self.spill(k.get());
                        self.call_rt("zell_map_delete", &[m.get().into(), kp.into()])
                    }
                };
                self.drop_val(args[1].ty, k);
                self.drop_val(args[0].ty, m);
                Ok(Val::void())
            }
            Builtin::Panic => {
                let v = self.expr(&args[0])?;
                let f = if *self.ty(args[0].ty) == Type::String { "zell_panic_str" } else { "zell_panic_error" };
                // Held: a task panic copies the message, then unwinding
                // this frame releases the value (unwind.rs).
                let h = self.hold(args[0].ty, v);
                self.call_noreturn(f, &[v.get().into()]);
                self.unhold(h);
                Ok(Val::void())
            }
            Builtin::ErrorNew => {
                let m = self.expr(&args[0])?;
                let r = self.call_rt_v("zell_error_new", &[m.get().into()]);
                let r = self.trace_created(r, e.span.start);
                self.drop_val(args[0].ty, m);
                Ok(Val::owned(r))
            }
            Builtin::ErrorsAs => self.errors_as(&args[0], &args[1]),
            Builtin::TaskAny => unsupported("task.Any whose result is not awaited directly"),
            Builtin::ArenaNew => {
                let Type::Pointer(t) = self.ty(ty).clone() else { return unsupported("arena New of a non-pointer") };
                let h = self.expr(&args[0])?.get();
                let ti = self.type_info(t);
                let size = self.i64_ty().const_int(16 + self.size_of(t), false);
                let obj = self.call_rt_v("zell_arena_object", &[h.into(), ti.into(), size.into()]).into_pointer_value();
                // Arena objects ignore retain/release: owned or not is the same.
                Ok(Val::owned(self.payload(obj).into()))
            }
            Builtin::ArenaSlice => {
                let et = self.elem_of(ty).unwrap();
                let h = self.expr(&args[0])?.get();
                let n = self.expr(&args[1])?.get();
                let out = self.entry_alloca(self.slice_ty().into(), "aslice", false);
                let size = self.i64_ty().const_int(self.size_of(et), false);
                let ops = self.value_ops(et);
                self.call_rt("zell_arena_slice", &[out.into(), h.into(), size.into(), n.into(), ops.into()]);
                Ok(Val::owned(self.b.build_load(self.slice_ty(), out, "").unwrap()))
            }
            Builtin::SliceMake => {
                let et = self.elem_of(ty).unwrap();
                let n = self.expr(&args[0])?.get();
                let out = self.entry_alloca(self.slice_ty().into(), "mslice", false);
                let size = self.i64_ty().const_int(self.size_of(et), false);
                let ops = self.value_ops(et);
                self.call_rt("zell_slice_new_ops", &[out.into(), size.into(), n.into(), n.into(), ops.into()]);
                Ok(Val::owned(self.b.build_load(self.slice_ty(), out, "").unwrap()))
            }
            Builtin::ChanNew { elem, fallible, tuple } => {
                // The runtime stores elements with T's ops and writes the
                // receive results in the compiled layouts of `T!` and
                // `(T, bool)`: value at 0, status word at its field offset.
                let cap = self.expr(&args[0])?.get();
                let esz = self.i64_ty().const_int(self.size_of(elem), false);
                let eops = self.value_ops(elem);
                let fsz = self.i64_ty().const_int(self.size_of(fallible), false);
                let fops = self.value_ops(fallible);
                let ferr = self.i64_ty().const_int(self.field_offset(fallible, 1), false);
                let tsz = self.i64_ty().const_int(self.size_of(tuple), false);
                let tops = self.value_ops(tuple);
                let tok = self.i64_ty().const_int(self.field_offset(tuple, 1), false);
                let h = self.call_rt_v(
                    "zell_task_chan_new",
                    &[cap.into(), esz.into(), eops.into(), fsz.into(), fops.into(), ferr.into(), tsz.into(), tops.into(), tok.into()],
                );
                Ok(Val::borrowed(h))
            }
            Builtin::ChanSend => {
                let h = self.expr(&args[0])?.get();
                let v = self.expr(&args[1])?;
                let vt = args[1].ty;
                let vty = self.llty(vt);
                let tmp = self.entry_alloca(vty, "send.v", false);
                self.b.build_store(tmp, v.get()).unwrap();
                let t = self.call_rt_v("zell_task_chan_send", &[h.into(), tmp.into()]);
                self.drop_val(vt, v);   /* the runtime retained its own copy */
                Ok(Val::owned(t))
            }
            Builtin::TaskAllCancel => {
                let d = self.depth();
                let sv = self.expr(&args[0])?;
                self.hold(args[0].ty, sv);
                let fl = self.expr(&args[1])?.get().into_int_value();
                self.forget_from(d);
                let s = sv.get().into_struct_value();
                let data = self.b.build_extract_value(s, 0, "all.data").unwrap();
                let len = self.b.build_extract_value(s, 1, "all.len").unwrap();
                let t = self.call_rt_v("zell_task_all_cancel_uniform", &[data.into(), len.into(), fl.into()]);
                self.drop_val(args[0].ty, sv);
                Ok(Val::owned(t))
            }
            Builtin::ChanRecvAny { tuple } => {
                // The runtime writes the index at 0 and the value and the
                // status at their field offsets in the compiled tuple.
                let d = self.depth();
                let sv = self.expr(&args[0])?;
                self.hold(args[0].ty, sv);
                self.forget_from(d);
                let s = sv.get().into_struct_value();
                let data = self.b.build_extract_value(s, 0, "any.data").unwrap();
                let len = self.b.build_extract_value(s, 1, "any.len").unwrap();
                let i64t = self.i64_ty();
                let size = i64t.const_int(self.size_of(tuple), false);
                let voff = i64t.const_int(self.field_offset(tuple, 1), false);
                let ooff = i64t.const_int(self.field_offset(tuple, 2), false);
                let t = self.call_rt_v("zell_task_chan_recv_any", &[data.into(), len.into(), size.into(), voff.into(), ooff.into()]);
                self.drop_val(args[0].ty, sv);
                Ok(Val::owned(t))
            }
            Builtin::ChanRecv | Builtin::ChanNext => {
                let h = self.expr(&args[0])?.get();
                let f = if op == Builtin::ChanRecv { "zell_task_chan_recv" } else { "zell_task_chan_next" };
                Ok(Val::owned(self.call_rt_v(f, &[h.into()])))
            }
        }
    }

    /// Byte offset of field `i` of the aggregate type `t` (0 when `t` is
    /// not an aggregate, e.g. `void!`, which is just the error pointer).
    fn field_offset(&mut self, t: TypeId, i: u32) -> u64 {
        match self.llty(t) {
            inkwell::types::BasicTypeEnum::StructType(st) => self.td.offset_of_element(&st, i).unwrap_or(0),
            _ => 0,
        }
    }
}
