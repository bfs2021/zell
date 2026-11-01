//! `println` lowering and `ToString` (the `println` formatting of a value).

use super::{unsupported, CodeGen, Val, R};
use crate::hir::{Expr, PrintItem, PrintOp};
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::values::{BasicValueEnum, FunctionValue};
use inkwell::IntPredicate;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    pub(crate) fn print(&mut self, items: &[PrintItem]) -> R<()> {
        // Every argument is evaluated before anything is written (Go's
        // fmt.Println; spec decision 61). Values that a later argument could
        // invalidate are owned while held.
        let d = self.depth();
        let mut vals = Vec::with_capacity(items.len());
        for (k, it) in items.iter().enumerate() {
            match &it.arg {
                Some(a) => {
                    let later = items[k + 1..].iter().any(|x| x.arg.as_ref().is_some_and(|y| self.may_run_code(y)));
                    let (v, _) = self.arg(a, false, false, later)?;
                    vals.push(Some(v));
                }
                None => vals.push(None),
            }
        }
        for (it, v) in items.iter().zip(vals) {
            match it.op {
                PrintOp::Space => {
                    self.call_rt("zell_print_space", &[]);
                }
                PrintOp::Newline => {
                    self.call_rt("zell_print_newline", &[]);
                }
                op => {
                    let Some(v) = v else { return unsupported("print item without argument") };
                    let x = v.get();
                    match op {
                        PrintOp::Int | PrintOp::Uint => {
                            let xi = x.into_int_value();
                            let w = if xi.get_type().get_bit_width() == 64 {
                                xi
                            } else {
                                self.b.build_int_cast_sign_flag(xi, self.i64_ty(), op == PrintOp::Int, "").unwrap()
                            };
                            let f = if op == PrintOp::Int { "zell_print_int" } else { "zell_print_uint" };
                            self.call_rt(f, &[w.into()]);
                        }
                        PrintOp::Float => {
                            let d = self.float_for_print(x);
                            self.call_rt("zell_print_float", &[d.into()]);
                        }
                        PrintOp::Bool => {
                            self.call_rt("zell_print_bool", &[x.into()]);
                        }
                        PrintOp::Char => {
                            self.call_rt("zell_print_rune", &[x.into()]);
                        }
                        _ => {
                            self.call_rt("zell_print_str", &[x.into()]);
                        }
                    }
                }
            }
        }
        self.release_temps_from(d);
        Ok(())
    }

    /// A float operand as the `double` to format: a `float32` goes through
    /// `zell_float32_shortest` so it prints with float32 precision.
    fn float_for_print(&mut self, x: BasicValueEnum<'ctx>) -> inkwell::values::FloatValue<'ctx> {
        let f = x.into_float_value();
        if f.get_type() == self.ctx.f32_type() {
            return self.call_rt_v("zell_float32_shortest", &[f.into()]).into_float_value();
        }
        f
    }

    /// `ToString(e)`: a string as `println` would print `e`.
    pub(crate) fn to_string(&mut self, e: &Expr) -> R<Val<'ctx>> {
        if *self.ty(e.ty) == Type::String {
            return self.expr(e);
        }
        let v = self.expr(e)?;
        let s = self.stringify(e.ty, v.get())?;
        self.drop_val(e.ty, v);
        Ok(s)
    }

    /// Formats value `x` of type `t`; owned unless it is a literal.
    pub(crate) fn stringify(&mut self, t: TypeId, x: BasicValueEnum<'ctx>) -> R<Val<'ctx>> {
        // A `String() string` method wins (Go's fmt.Stringer).
        if let Some((fid, path)) = self.stringer(t) {
            let fv = self.funcs[fid.0 as usize];
            if matches!(self.ty(t), Type::Enum(_)) {
                return Ok(Val::owned(self.call_fn(fv, &[x.into()]).try_as_basic_value().basic().unwrap()));
            }
            let p = self.spill(x);
            return Ok(Val::owned(self.call_stringer(t, p, fv, &path)));
        }
        if let Type::Pointer(inner) = *self.ty(t) {
            if self.stringer(inner).is_some() {
                let f = self.tostr_fn(t)?;
                return Ok(Val::owned(self.call_fn(f, &[x.into()]).try_as_basic_value().basic().unwrap()));
            }
        }
        Ok(match self.ty(t).clone() {
            Type::String => {
                self.retain_val(t, x);
                Val::owned(x)
            }
            Type::Int(k) => {
                let xi = x.into_int_value();
                let w = if k.bits() == 64 { xi } else { self.b.build_int_cast_sign_flag(xi, self.i64_ty(), k.signed(), "").unwrap() };
                Val::owned(self.call_rt_v(if k.signed() { "zell_string_from_int" } else { "zell_string_from_uint" }, &[w.into()]))
            }
            Type::Float32 | Type::Float64 => {
                let d = self.float_for_print(x);
                Val::owned(self.call_rt_v("zell_string_from_float", &[d.into()]))
            }
            Type::Bool => Val::owned(self.call_rt_v("zell_string_from_bool", &[x.into()])),
            Type::Char => Val::owned(self.call_rt_v("zell_string_from_rune", &[x.into()])),
            Type::Error => {
                let f = self.tostr_fn(t)?;
                Val::owned(self.call_fn(f, &[x.into()]).try_as_basic_value().basic().unwrap())
            }
            Type::Enum(d) if self.prog.enums[d.0 as usize].is_plain => {
                let names = self.enum_name_table(d.0);
                let n = self.prog.enums[d.0 as usize].variants.len() as u32;
                let at = self.ptr_ty().array_type(n);
                let idx = self.b.build_int_z_extend(x.into_int_value(), self.i64_ty(), "").unwrap();
                let p = unsafe { self.b.build_in_bounds_gep(at, names, &[self.i64_ty().const_zero(), idx], "") }.unwrap();
                Val::borrowed(self.b.build_load(self.ptr_ty(), p, "").unwrap())
            }
            Type::Slice(_) | Type::Array(..) | Type::Tuple(_) => {
                let f = self.tostr_fn(t)?;
                let p = self.spill(x);
                Val::owned(self.call_fn(f, &[p.into()]).try_as_basic_value().basic().unwrap())
            }
            Type::Pointer(_) | Type::Map(..) | Type::Func(_) | Type::Task(_) | Type::Nil => {
                // Only nil-ness is printable in v0.1.
                let isnil = self.nil_test(t, x);
                let a = self.str_lit("nil");
                let b = self.str_lit(match self.ty(t) {
                    Type::Map(..) => "map[...]",
                    Type::Func(_) => "<func>",
                    Type::Task(_) => "<task>",
                    _ => "<pointer>",
                });
                Val::borrowed(self.b.build_select(isnil, a, b, "").unwrap())
            }
            Type::Any => {
                // Dynamic: dispatch on the type info (see `finish_any_tostr`).
                let f = self.any_tostr_fn();
                let pair = x.into_struct_value();
                let data = self.b.build_extract_value(pair, 0, "").unwrap();
                let ti = self.b.build_extract_value(pair, 1, "").unwrap();
                Val::owned(self.call_fn(f, &[data.into(), ti.into()]).try_as_basic_value().basic().unwrap())
            }
            Type::Interface(_) => {
                let isnil = self.nil_test(t, x);
                let a = self.str_lit("nil");
                let b = self.str_lit("<interface>");
                Val::borrowed(self.b.build_select(isnil, a, b, "").unwrap())
            }
            _ => return unsupported(format!("printing a value of type {}", self.type_name(t))),
        })
    }

    /// `string any.tostr(ptr data, ptr typeinfo)`: formats the value held by
    /// an `any`. Declared on first use; its body is emitted at the end of
    /// the module by [`finish_any_tostr`](Self::finish_any_tostr), when every
    /// type stored in an `any` (every type info) is known.
    fn any_tostr_fn(&mut self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("any.tostr") {
            return f;
        }
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into(), self.ptr_ty().into()], false);
        self.module.add_function("any.tostr", fty, Some(Linkage::Internal))
    }

    /// Emits the body of `any.tostr`: compares the type info against each
    /// boxed type of the program; `nil` for a nil `any`, `<T>` for types
    /// `println` cannot print.
    pub(crate) fn finish_any_tostr(&mut self) -> R<()> {
        let Some(f) = self.module.get_function("any.tostr") else { return Ok(()) };
        let mut types: Vec<(TypeId, inkwell::values::PointerValue<'ctx>)> = self.type_infos.iter().map(|(t, g)| (*t, *g)).collect();
        types.sort_by_key(|(t, _)| *t);
        self.with_helper(f, |cg| {
            let data = f.get_nth_param(0).unwrap().into_pointer_value();
            let ti = f.get_nth_param(1).unwrap().into_pointer_value();
            let nil = cg.block("nil");
            let start = cg.block("test");
            let isnil = cg.b.build_is_null(ti, "").unwrap();
            cg.b.build_conditional_branch(isnil, nil, start).unwrap();
            cg.b.position_at_end(nil);
            let s = cg.str_lit("nil");
            cg.b.build_return(Some(&s)).unwrap();
            cg.b.position_at_end(start);
            for (t, g) in types {
                let yes = cg.block("is");
                let next = cg.block("next");
                let eq = cg.ptr_eq(ti, g);
                cg.b.build_conditional_branch(eq, yes, next).unwrap();
                cg.b.position_at_end(yes);
                let printable = matches!(cg.ty(t), Type::Int(_) | Type::Float32 | Type::Float64 | Type::Bool | Type::Char | Type::String | Type::Error | Type::Slice(_) | Type::Array(..) | Type::Tuple(_))
                    || matches!(cg.ty(t), Type::Enum(d) if cg.prog.enums[d.0 as usize].is_plain)
                    || cg.stringer(t).is_some()
                    || matches!(cg.ty(t), Type::Pointer(p) if cg.stringer(*p).is_some());
                let r = if printable {
                    let lt = cg.llty(t);
                    let v = if cg.any_inline(t) { cg.any_word_value(t, data) } else { cg.b.build_load(lt, data, "").unwrap() };
                    match cg.stringify(t, v) {
                        // Borrowed results are immortal literals (enum names).
                        Ok(s) => s.get(),
                        Err(_) => cg.str_lit(&format!("<{}>", cg.type_name(t))).into(),
                    }
                } else {
                    cg.str_lit(&format!("<{}>", cg.type_name(t))).into()
                };
                cg.b.build_return(Some(&r)).unwrap();
                cg.b.position_at_end(next);
            }
            let s = cg.str_lit("<any>");
            cg.b.build_return(Some(&s)).unwrap();
            Ok(())
        })
    }

    /// Global `[N x ptr]` of variant-name string literals.
    fn enum_name_table(&mut self, eid: u32) -> inkwell::values::PointerValue<'ctx> {
        if let Some(g) = self.enum_names.get(&eid) {
            return g.as_pointer_value();
        }
        let names: Vec<String> = self.prog.enums[eid as usize].variants.iter().map(|v| v.name.clone()).collect();
        let vals: Vec<_> = names.iter().map(|n| self.str_lit(n)).collect();
        let at = self.ptr_ty().array_type(vals.len() as u32);
        let k = self.fresh();
        let g = self.module.add_global(at, None, &format!("enum.names.{k}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_initializer(&self.ptr_ty().const_array(&vals));
        self.enum_names.insert(eid, g);
        g.as_pointer_value()
    }

    /// Generated formatter: errors take the value, slices/arrays a pointer.
    fn tostr_fn(&mut self, t: TypeId) -> R<FunctionValue<'ctx>> {
        if let Some(f) = self.tostr_fns.get(&t) {
            return Ok(*f);
        }
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into()], false);
        let k = self.fresh();
        let f = self.module.add_function(&format!("tostr.{k}"), fty, Some(Linkage::Internal));
        self.tostr_fns.insert(t, f);
        let ty = self.ty(t).clone();
        self.with_helper(f, |cg| {
            let arg = f.get_nth_param(0).unwrap().into_pointer_value();
            if ty == Type::Error {
                let nil = cg.block("nil");
                let some = cg.block("some");
                let isn = cg.b.build_is_null(arg, "").unwrap();
                cg.b.build_conditional_branch(isn, nil, some).unwrap();
                cg.b.position_at_end(nil);
                let s = cg.str_lit("nil");
                cg.b.build_return(Some(&s)).unwrap();
                cg.b.position_at_end(some);
                let m = cg.call_rt_v("zell_error_message", &[arg.into()]);
                cg.b.build_return(Some(&m)).unwrap();
                return Ok(());
            }
            if let Type::Pointer(inner) = ty {
                // `*S` whose `S` has `String()`: nil, else the method's result.
                let nil = cg.block("nil");
                let some = cg.block("some");
                let isn = cg.b.build_is_null(arg, "").unwrap();
                cg.b.build_conditional_branch(isn, nil, some).unwrap();
                cg.b.position_at_end(nil);
                let s = cg.str_lit("nil");
                cg.b.build_return(Some(&s)).unwrap();
                cg.b.position_at_end(some);
                let (fid, path) = cg.stringer(inner).unwrap();
                let fv = cg.funcs[fid.0 as usize];
                let r = cg.call_stringer(inner, arg, fv, &path);
                cg.b.build_return(Some(&r)).unwrap();
                return Ok(());
            }
            if let Type::Tuple(elems) = &ty {
                // `(a b c)`: elements separated by one space.
                let st = cg.llty(t).into_struct_type();
                let acc = cg.entry_alloca(cg.ptr_ty().into(), "acc", false);
                let open = cg.str_lit("(");
                cg.b.build_store(acc, open).unwrap();
                for (i, et) in elems.iter().enumerate() {
                    if i > 0 {
                        let space = cg.str_lit(" ");
                        cg.append_str(acc, space.into(), false);
                    }
                    let ep = cg.b.build_struct_gep(st, arg, i as u32, "").unwrap();
                    let elt = cg.llty(*et);
                    let ev = cg.b.build_load(elt, ep, "").unwrap();
                    let s = cg.stringify(*et, ev)?;
                    cg.append_str(acc, s.get(), s.owned);
                }
                let close = cg.str_lit(")");
                cg.append_str(acc, close.into(), false);
                let r = cg.b.build_load(cg.ptr_ty(), acc, "").unwrap();
                cg.b.build_return(Some(&r)).unwrap();
                return Ok(());
            }
            let et = cg.elem_of(t).unwrap();
            let elt = cg.llty(et);
            let (data, len) = match ty {
                Type::Array(_, n) => (arg, cg.i64_ty().const_int(n, false)),
                _ => {
                    let st = cg.slice_ty();
                    let v = cg.b.build_load(st, arg, "").unwrap().into_struct_value();
                    (
                        cg.b.build_extract_value(v, 0, "").unwrap().into_pointer_value(),
                        cg.b.build_extract_value(v, 1, "").unwrap().into_int_value(),
                    )
                }
            };
            let acc = cg.entry_alloca(cg.ptr_ty().into(), "acc", false);
            let open = cg.str_lit("[");
            cg.b.build_store(acc, open).unwrap();
            let i = cg.entry_alloca(cg.i64_ty().into(), "i", true);
            let head = cg.block("head");
            let body = cg.block("body");
            let done = cg.block("done");
            cg.b.build_unconditional_branch(head).unwrap();
            cg.b.position_at_end(head);
            let iv = cg.b.build_load(cg.i64_ty(), i, "").unwrap().into_int_value();
            let c = cg.b.build_int_compare(IntPredicate::ULT, iv, len, "").unwrap();
            cg.b.build_conditional_branch(c, body, done).unwrap();
            cg.b.position_at_end(body);
            let sep = cg.block("sep");
            let elem = cg.block("elem");
            let first = cg.b.build_int_compare(IntPredicate::EQ, iv, cg.i64_ty().const_zero(), "").unwrap();
            cg.b.build_conditional_branch(first, elem, sep).unwrap();
            cg.b.position_at_end(sep);
            let space = cg.str_lit(" ");
            cg.append_str(acc, space.into(), false);
            cg.b.build_unconditional_branch(elem).unwrap();
            cg.b.position_at_end(elem);
            let ep = unsafe { cg.b.build_in_bounds_gep(elt, data, &[iv], "") }.unwrap();
            let ev = cg.b.build_load(elt, ep, "").unwrap();
            let s = cg.stringify(et, ev)?;
            cg.append_str(acc, s.get(), s.owned);
            let i1 = cg.b.build_int_add(iv, cg.i64_ty().const_int(1, false), "").unwrap();
            cg.b.build_store(i, i1).unwrap();
            cg.b.build_unconditional_branch(head).unwrap();
            cg.b.position_at_end(done);
            let close = cg.str_lit("]");
            cg.append_str(acc, close.into(), false);
            let r = cg.b.build_load(cg.ptr_ty(), acc, "").unwrap();
            cg.b.build_return(Some(&r)).unwrap();
            Ok(())
        })?;
        Ok(f)
    }

    /// The `String() string` method of struct or enum type `t` (own, or
    /// promoted through embedded fields: the shortest path, as in sema's
    /// `find_method`), mirroring `Sema::has_stringer`.
    pub(crate) fn stringer(&self, t: TypeId) -> Option<(crate::hir::FuncId, Vec<u32>)> {
        let ok = |cg: &Self, f: crate::hir::FuncId| {
            let func = &cg.prog.funcs[f.0 as usize];
            !func.is_async && func.params.len() == 1 && *cg.ty(func.ret) == Type::String
        };
        match self.ty(t) {
            Type::Enum(d) => {
                let (_, f) = self.prog.enums[d.0 as usize].methods.iter().find(|(n, _)| n == "String")?;
                ok(self, *f).then(|| (*f, Vec::new()))
            }
            Type::Struct(d) => self.struct_stringer(d.0, 0).filter(|(f, _)| ok(self, *f)),
            _ => None,
        }
    }

    fn struct_stringer(&self, sid: u32, depth: usize) -> Option<(crate::hir::FuncId, Vec<u32>)> {
        let st = &self.prog.structs[sid as usize];
        if let Some((_, f)) = st.methods.iter().find(|(n, _)| n == "String") {
            if !matches!(self.prog.funcs[f.0 as usize].kind, crate::hir::FuncKind::Method { is_static: true, .. }) {
                return Some((*f, Vec::new()));
            }
        }
        if depth > 8 {
            return None;
        }
        let mut best: Option<(crate::hir::FuncId, Vec<u32>)> = None;
        for (i, f) in st.fields.iter().enumerate().filter(|(_, f)| f.embedded) {
            if let Type::Struct(d) = self.ty(f.ty) {
                if let Some((m, mut p)) = self.struct_stringer(d.0, depth + 1) {
                    p.insert(0, i as u32);
                    if best.as_ref().is_none_or(|b| p.len() < b.1.len()) {
                        best = Some((m, p));
                    }
                }
            }
        }
        best
    }

    /// Calls `String()` on the struct of type `t` at `p`, through the
    /// embedded-field `path`; the result is owned.
    fn call_stringer(&mut self, t: TypeId, p: inkwell::values::PointerValue<'ctx>, f: FunctionValue<'ctx>, path: &[u32]) -> BasicValueEnum<'ctx> {
        let (mut p, mut t) = (p, t);
        for &idx in path {
            let Type::Struct(d) = *self.ty(t) else { break };
            let fty = self.prog.structs[d.0 as usize].fields[idx as usize].ty;
            let st = self.llty(t).into_struct_type();
            let fp = self.b.build_struct_gep(st, p, idx, "").unwrap();
            p = fp;
            t = fty;
        }
        self.call_fn(f, &[p.into()]).try_as_basic_value().basic().unwrap()
    }

    /// `*acc = *acc + s` (the old accumulator is released).
    fn append_str(&mut self, acc: inkwell::values::PointerValue<'ctx>, s: BasicValueEnum<'ctx>, owned: bool) {
        let old = self.b.build_load(self.ptr_ty(), acc, "").unwrap();
        let n = self.call_rt_v("zell_string_concat", &[old.into(), s.into()]);
        self.call_rt("zell_release", &[old.into()]);
        if owned {
            self.call_rt("zell_release", &[s.into()]);
        }
        self.b.build_store(acc, n).unwrap();
    }
}
