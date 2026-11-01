//! Reflection tables and the `reflect` intrinsics (DESIGN.md
//! "Рефлексия", runtime/ABI.md §17).
//!
//! When the program uses `reflect` ([`hir::Program::reflect`]), every
//! run-time type id is a full table: [`CodeGen::type_info`] returns the
//! payload address of an immortal object `{i64 -1, ptr null, TypeInfo}`
//! whose first three words are the runtime's `zell_type_info` header
//! (`name`, `size`, `drop`), so boxes, vtables (`vtable[0]`), `any` values,
//! type switches and errors keep working unchanged, and a `*TypeInfo` read
//! from them is an ordinary (immortal) Zell pointer. Tables are emitted
//! lazily, only for types that some code or table refers to; methods are
//! listed only for the [`hir::Reflect::full`] types.
//!
//! Every metadata object (`FieldInfo`, `MethodInfo`, `VariantInfo`,
//! attribute values) is such an immortal object; slices of them have a
//! NULL backing (no reference counting). An attribute list is a `[]any`
//! whose elements hold a `*A` to the attribute's constant value.

use super::{CodeGen, Val, R};
use crate::hir::{self, Attr, AttrKind, ConstValue, FuncId, FuncKind, Intrinsic};
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::types::{BasicType, BasicTypeEnum, StructType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, PointerValue};
use inkwell::IntPredicate;
use std::collections::HashMap;

/// Short type names (`User`, not `app.User`) for `TypeInfo.Name`.
struct Short<'p>(&'p hir::Program);

impl hir::dump::DeclNames for Short<'_> {
    fn struct_name(&self, id: u32) -> String {
        self.0.structs[id as usize].name.clone()
    }
    fn interface_name(&self, id: u32) -> String {
        self.0.interfaces[id as usize].name.clone()
    }
    fn enum_name(&self, id: u32) -> String {
        self.0.enums[id as usize].name.clone()
    }
}

/// Field values of a constant struct, by name.
type Fields<'ctx> = Vec<(&'static str, BasicValueEnum<'ctx>)>;

/// An enum variant for its table: (name, fields, attributes).
type VariantDesc = (String, Vec<(String, TypeId)>, Vec<Attr>);

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    fn refl(&self) -> &'a hir::Reflect {
        self.prog.reflect.as_ref().expect("codegen: reflection is off")
    }

    /// An immortal heap-object-shaped constant `{-1, null, payload}`;
    /// returns the payload address.
    fn static_object(&mut self, payload: BasicValueEnum<'ctx>, name: &str) -> PointerValue<'ctx> {
        let st = self.ctx.struct_type(&[self.i64_ty().into(), self.ptr_ty().into(), payload.get_type()], false);
        let n = self.fresh();
        let g = self.module.add_global(st, None, &format!("{name}.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_alignment(8);
        g.set_initializer(&st.const_named_struct(&[self.i64_ty().const_int(u64::MAX, true).into(), self.ptr_ty().const_null().into(), payload]));
        self.object_payload(g.as_pointer_value(), st)
    }

    fn object_payload(&self, obj: PointerValue<'ctx>, st: StructType<'ctx>) -> PointerValue<'ctx> {
        let i32t = self.ctx.i32_type();
        unsafe { obj.const_in_bounds_gep(st, &[i32t.const_zero(), i32t.const_int(2, false)]) }
    }

    /// A constant slice `{data, n, n, null}` of `elems` (NULL backing).
    fn const_slice(&mut self, elem: BasicTypeEnum<'ctx>, elems: Vec<BasicValueEnum<'ctx>>) -> BasicValueEnum<'ctx> {
        if elems.is_empty() {
            return self.slice_ty().const_zero().into();
        }
        let n = elems.len() as u64;
        let at = elem.array_type(n as u32);
        let arr = match elem {
            BasicTypeEnum::PointerType(p) => p.const_array(&elems.iter().map(|e| e.into_pointer_value()).collect::<Vec<_>>()),
            BasicTypeEnum::StructType(s) => s.const_array(&elems.iter().map(|e| e.into_struct_value()).collect::<Vec<_>>()),
            BasicTypeEnum::IntType(i) => i.const_array(&elems.iter().map(|e| e.into_int_value()).collect::<Vec<_>>()),
            BasicTypeEnum::FloatType(f) => f.const_array(&elems.iter().map(|e| e.into_float_value()).collect::<Vec<_>>()),
            _ => at.const_zero(),
        };
        let k = self.fresh();
        let g = self.module.add_global(at, None, &format!("rslice.{k}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_alignment(8);
        g.set_initializer(&arr);
        let len = self.i64_ty().const_int(n, false);
        self.slice_ty().const_named_struct(&[g.as_pointer_value().into(), len.into(), len.into(), self.ptr_ty().const_null().into()]).into()
    }

    /// A constant of struct `sid` with the named fields set (others zero).
    fn const_struct(&mut self, sid: u32, vals: &[(&str, BasicValueEnum<'ctx>)]) -> BasicValueEnum<'ctx> {
        let st = self.struct_llty(sid);
        let fields: Vec<(String, TypeId)> = self.prog.structs[sid as usize].fields.iter().map(|f| (f.name.clone(), f.ty)).collect();
        let mut out = Vec::new();
        for (name, ty) in fields {
            match vals.iter().find(|(n, _)| *n == name) {
                Some((_, v)) => out.push(*v),
                None => out.push(self.llty(ty).const_zero()),
            }
        }
        st.const_named_struct(&out).into()
    }

    /// A compile-time constant of type `ty` (attribute arguments).
    fn const_value(&mut self, ty: TypeId, v: &ConstValue) -> BasicValueEnum<'ctx> {
        match (v, self.ty(ty).clone()) {
            (ConstValue::Int(i), Type::Float32) => self.ctx.f32_type().const_float(*i as f64).into(),
            (ConstValue::Int(i), Type::Float64) => self.ctx.f64_type().const_float(*i as f64).into(),
            (ConstValue::Int(i), _) => self.llty(ty).into_int_type().const_int(*i as u64, false).into(),
            (ConstValue::Float(f), Type::Float32) => self.ctx.f32_type().const_float(*f).into(),
            (ConstValue::Float(f), _) => self.ctx.f64_type().const_float(*f).into(),
            (ConstValue::Bool(b), _) => self.ctx.bool_type().const_int(*b as u64, false).into(),
            (ConstValue::Char(c), _) => self.ctx.i32_type().const_int(*c as u64, false).into(),
            (ConstValue::Str(s), _) => self.str_lit(s).into(),
            (ConstValue::Enum(e, v), _) if self.prog.enums[*e as usize].is_plain => self.ctx.i32_type().const_int(*v as u64, false).into(),
            // A nested attribute struct: its field values in order.
            (ConstValue::List(vs), Type::Struct(d)) => {
                let fields: Vec<TypeId> = self.prog.structs[d.0 as usize].fields.iter().map(|f| f.ty).collect();
                let vals: Vec<BasicValueEnum> = fields.iter().zip(vs).map(|(t, v)| self.const_value(*t, v)).collect();
                let st = self.struct_llty(d.0);
                if vals.len() == fields.len() {
                    st.const_named_struct(&vals).into()
                } else {
                    st.const_zero().into()
                }
            }
            (ConstValue::List(vs), Type::Slice(el)) => {
                let et = self.llty(el);
                let elems = vs.iter().map(|x| self.const_value(el, x)).collect();
                self.const_slice(et, elems)
            }
            _ => self.llty(ty).const_zero(),
        }
    }

    /// `*T` of `t`, if sema interned it ([`hir::Reflect::pointers`]).
    fn pointer_type_of(&self, t: TypeId) -> Option<TypeId> {
        self.refl().pointers.iter().find(|(x, _)| *x == t).map(|(_, p)| *p)
    }

    /// The immortal constant value object of attribute `a` (a struct
    /// `structs[sid]` with the supplied arguments and the field defaults
    /// filled in); returns its payload, the `*A`. Also the value of
    /// [`ExprKind::AttrConst`].
    pub(crate) fn attr_object(&mut self, a: &Attr, sid: u32) -> PointerValue<'ctx> {
        let fields: Vec<(String, TypeId, Option<ConstValue>)> = self.prog.structs[sid as usize].fields.iter().map(|f| (f.name.clone(), f.ty, f.default.clone())).collect();
        let st = self.struct_llty(sid);
        let mut vals = Vec::new();
        for (name, ty, default) in fields {
            let v = a.args.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone()).or(default);
            vals.push(match v {
                Some(v) => self.const_value(ty, &v),
                None => self.llty(ty).const_zero(),
            });
        }
        let value = st.const_named_struct(&vals);
        self.static_object(value.into(), "rattr")
    }

    /// The `[]any` of attribute values of a declaration.
    fn attr_list(&mut self, attrs: &[Attr]) -> BasicValueEnum<'ctx> {
        let mut elems = Vec::new();
        for a in attrs {
            let sid = match &a.kind {
                AttrKind::User(s) => *s,
                AttrKind::Builtin(n) => match self.refl().attr_structs.iter().find(|(x, _)| x == n) {
                    Some((_, s)) => *s,
                    None => continue,
                },
            };
            let obj = self.attr_object(a, sid);
            let Some(pt) = self.refl().pointers.iter().find(|(t, _)| matches!(self.ty(*t), Type::Struct(d) if d.0 == sid)).map(|(_, p)| *p) else { continue };
            let box_ = self.static_object(obj.into(), "rattrbox");
            let ti = self.type_info(pt);
            elems.push(self.pair_ty().const_named_struct(&[box_.into(), ti.into()]).into());
        }
        let pt = self.pair_ty().into();
        self.const_slice(pt, elems)
    }

    /// `reflect.Kind` tag of `t`.
    fn kind_tag(&self, t: TypeId) -> u64 {
        let name = hir::reflect_kind_name(&self.prog.types, t);
        self.prog.enums[self.refl().kind as usize].variants.iter().position(|v| v.name == name).unwrap_or(0) as u64
    }

    /// `FieldInfo` objects for fields `(name, type, offset, index,
    /// embedded, exported, attrs, spelling)`.
    #[allow(clippy::type_complexity)]
    fn field_infos(&mut self, fields: Vec<(String, TypeId, u64, bool, bool, Vec<Attr>, Option<String>)>) -> BasicValueEnum<'ctx> {
        let fi = self.refl().field_info;
        let mut elems = Vec::new();
        for (i, (name, ty, off, embedded, exported, attrs, spelling)) in fields.into_iter().enumerate() {
            let tp = match spelling {
                Some(sp) => self.alias_type_info(ty, &sp),
                None => self.type_info(ty),
            };
            let nm = self.str_lit(&name);
            let attrs = self.attr_list(&attrs);
            let i64t = self.i64_ty();
            let b = self.ctx.bool_type();
            let v = self.const_struct(
                fi,
                &[
                    ("Name", nm.into()),
                    ("Type", tp.into()),
                    ("Offset", i64t.const_int(off, false).into()),
                    ("Index", i64t.const_int(i as u64, false).into()),
                    ("Embedded", b.const_int(embedded as u64, false).into()),
                    ("Exported", b.const_int(exported as u64, false).into()),
                    ("attrs", attrs),
                ],
            );
            elems.push(self.static_object(v, "rfield").into());
        }
        let pt = self.ptr_ty().into();
        self.const_slice(pt, elems)
    }

    /// The table of `t` (called by `type_info` when reflection is on).
    pub(crate) fn reflect_type_info(&mut self, t: TypeId) -> PointerValue<'ctx> {
        let r = self.refl();
        let pst = self.struct_llty(r.type_info);
        let obj_ty = self.ctx.struct_type(&[self.i64_ty().into(), self.ptr_ty().into(), pst.into()], false);
        let n = self.fresh();
        let g = self.module.add_global(obj_ty, None, &format!("ti.{n}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        g.set_alignment(8);
        let p = self.object_payload(g.as_pointer_value(), obj_ty);
        self.type_infos.insert(t, p);
        let drop = self.box_drop_fn(t);
        let fields = self.type_info_fields(t, drop, None);
        let v = self.const_struct(r.type_info, &fields);
        g.set_initializer(&obj_ty.const_named_struct(&[self.i64_ty().const_int(u64::MAX, true).into(), self.ptr_ty().const_null().into(), v]));
        p
    }

    /// A second table for a builtin type spelled differently (`int64`):
    /// same contents, its own name, `canon` pointing at the real one.
    fn alias_type_info(&mut self, t: TypeId, spelling: &str) -> PointerValue<'ctx> {
        let key = (t, spelling.to_string());
        if let Some(p) = self.reflect_aliases.get(&key) {
            return *p;
        }
        let canon = self.type_info(t);
        let drop = self.box_drop_fn(t);
        let mut fields = self.type_info_fields(t, drop, Some(spelling));
        fields.push(("canon", canon.into()));
        let sid = self.refl().type_info;
        let v = self.const_struct(sid, &fields);
        let p = self.static_object(v, "ti.alias");
        self.reflect_aliases.insert(key, p);
        p
    }

    /// `drop` of a box holding a `t` (NULL if nothing to release).
    fn box_drop_fn(&mut self, t: TypeId) -> PointerValue<'ctx> {
        if !self.needs_rc(t) {
            return self.ptr_ty().const_null();
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let n = self.fresh();
        let f = self.module.add_function(&format!("drop.{n}"), fty, Some(Linkage::Internal));
        let _ = self.with_helper(f, |cg| {
            let obj = f.get_nth_param(0).unwrap().into_pointer_value();
            let p = cg.payload(obj);
            cg.rc_slot(t, p, false);
            cg.b.build_return(None).unwrap();
            Ok(())
        });
        f.as_global_value().as_pointer_value()
    }

    /// Field values of the `TypeInfo` of `t`.
    fn type_info_fields(&mut self, t: TypeId, drop: PointerValue<'ctx>, spelling: Option<&str>) -> Fields<'ctx> {
        let i64t = self.i64_ty();
        let full = self.refl().full.binary_search(&t).is_ok();
        let short = spelling.map(|s| s.to_string()).unwrap_or_else(|| hir::dump::type_name(&self.prog.types, t, &Short(self.prog)));
        let long = spelling.map(|s| s.to_string()).unwrap_or_else(|| self.type_name(t));
        let tname = self.type_name(t);
        let rt_name = self.cstr(&tname);
        let size = self.size_of(t);
        let align = self.llty_opt(t).map(|x| self.td.get_abi_alignment(&x)).unwrap_or(1) as u64;
        let name_v = self.str_lit(&short);
        let full_v = self.str_lit(&long);
        let kind = self.ctx.i32_type().const_int(self.kind_tag(t), false);
        let mut out: Fields<'ctx> = vec![
            ("rtName", rt_name.const_to_int(i64t).into()),
            ("rtSize", i64t.const_int(16 + size, false).into()),
            ("rtDrop", drop.const_to_int(i64t).into()),
            ("Kind", kind.into()),
            ("Name", name_v.into()),
            ("FullName", full_v.into()),
            ("Size", i64t.const_int(size, false).into()),
            ("Align", i64t.const_int(align, false).into()),
        ];
        let ns = match self.ty(t) {
            Type::Struct(d) => Some(self.prog.structs[d.0 as usize].namespace),
            Type::Enum(d) => Some(self.prog.enums[d.0 as usize].namespace),
            Type::Interface(d) => Some(self.prog.interfaces[d.0 as usize].namespace),
            _ => None,
        };
        if let Some(path) = ns.and_then(|n| self.prog.namespaces.get(n.0 as usize)).map(|n| n.path.clone()) {
            let pv = self.str_lit(&path);
            out.push(("Package", pv.into()));
        }
        match self.ty(t).clone() {
            Type::Struct(d) => {
                let sid = d.0;
                let st = self.struct_llty(sid);
                let prog = self.prog;
                let s = &prog.structs[sid as usize];
                let spellings = &self.refl().field_spellings;
                let fs: Vec<_> = s
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let off = self.td.offset_of_element(&st, i as u32).unwrap_or(0);
                        let sp = spellings.iter().find(|(x, y, _)| *x == sid && *y == i as u32).map(|(_, _, n)| n.clone());
                        (f.name.clone(), f.ty, off, f.embedded, f.exported, f.attrs.clone(), sp)
                    })
                    .collect();
                let attrs = s.attrs.clone();
                let fields = self.field_infos(fs);
                out.push(("Fields", fields));
                if full {
                    let methods = self.method_infos(sid);
                    out.push(("Methods", methods));
                }
                let a = self.attr_list(&attrs);
                out.push(("attrs", a));
            }
            Type::Enum(d) => {
                let eid = d.0;
                let prog = self.prog;
                let e = &prog.enums[eid as usize];
                let attrs = e.attrs.clone();
                let variants: Vec<VariantDesc> = e.variants.iter().map(|v| (v.name.clone(), v.fields.clone(), v.attrs.clone())).collect();
                let plain = e.is_plain;
                let payload_off = if plain {
                    0
                } else {
                    let et = self.enum_llty(eid);
                    self.td.offset_of_element(&et, 1).unwrap_or(8)
                };
                let vi = self.refl().variant_info;
                let mut elems = Vec::new();
                for (k, (name, fields, vattrs)) in variants.into_iter().enumerate() {
                    let vt = self.variant_llty(eid, k as u32);
                    let fs: Vec<_> = fields
                        .into_iter()
                        .enumerate()
                        .map(|(i, (n, ty))| (n.clone(), ty, payload_off + self.td.offset_of_element(&vt, i as u32).unwrap_or(0), false, n.chars().next().is_some_and(|c| c.is_uppercase()), Vec::new(), None))
                        .collect();
                    let fields = self.field_infos(fs);
                    let nm = self.str_lit(&name);
                    let at = self.attr_list(&vattrs);
                    let v = self.const_struct(vi, &[("Name", nm.into()), ("Index", i64t.const_int(k as u64, false).into()), ("Fields", fields), ("attrs", at)]);
                    elems.push(self.static_object(v, "rvariant").into());
                }
                let pt = self.ptr_ty().into();
                let vs = self.const_slice(pt, elems);
                out.push(("Variants", vs));
                let a = self.attr_list(&attrs);
                out.push(("attrs", a));
            }
            Type::Interface(d) => {
                let attrs = self.prog.interfaces[d.0 as usize].attrs.clone();
                let a = self.attr_list(&attrs);
                out.push(("attrs", a));
            }
            Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Fallible(x) | Type::Task(x) => {
                let e = self.type_info(x);
                out.push(("Elem", e.into()));
            }
            Type::Array(x, len) => {
                let e = self.type_info(x);
                out.push(("Elem", e.into()));
                out.push(("Len", i64t.const_int(len, false).into()));
            }
            Type::Map(k, v) => {
                let kp = self.type_info(k);
                let vp = self.type_info(v);
                out.push(("Key", kp.into()));
                out.push(("Elem", vp.into()));
            }
            Type::Tuple(ts) => {
                // The elements as fields `_0`, `_1`, ... (like DWARF).
                let st = self.llty(t).into_struct_type();
                let fs: Vec<_> = ts.iter().enumerate().map(|(i, et)| (format!("_{i}"), *et, self.td.offset_of_element(&st, i as u32).unwrap_or(0), false, false, Vec::new(), None)).collect();
                let fields = self.field_infos(fs);
                out.push(("Fields", fields));
            }
            _ => {}
        }
        if full && spelling.is_none() {
            if let Some(pt) = self.pointer_type_of(t) {
                let p = self.type_info(pt);
                out.push(("ptrTo", p.into()));
            }
        }
        out
    }

    /// `MethodInfo` objects of struct `sid` (non-async methods).
    fn method_infos(&mut self, sid: u32) -> BasicValueEnum<'ctx> {
        let mi = self.refl().method_info;
        let methods = self.prog.structs[sid as usize].methods.clone();
        let mut elems = Vec::new();
        for (name, fid) in methods {
            let Some(&(_, fty)) = self.refl().method_types.iter().find(|(f, _)| *f == fid) else { continue };
            let f = &self.prog.funcs[fid.0 as usize];
            let is_static = matches!(f.kind, FuncKind::Method { is_static: true, .. });
            let attrs = f.attrs.clone();
            let skip = if is_static { 0 } else { 1 };
            let params: Vec<(String, TypeId)> = f.params.iter().skip(skip).map(|l| (f.locals[l.0 as usize].name.clone(), f.locals[l.0 as usize].ty)).collect();
            let pattrs = f.param_attrs.clone();
            let params = self.param_infos(params, pattrs);
            let tp = self.type_info(fty);
            let invoke = match self.invoke_thunk(fid) {
                Ok(f) => f.as_global_value().as_pointer_value(),
                Err(_) => self.ptr_ty().const_null(),
            };
            let fnval = match self.method_value_thunk(fid, fty) {
                Ok(f) => f.as_global_value().as_pointer_value(),
                Err(_) => self.ptr_ty().const_null(),
            };
            let i64t = self.i64_ty();
            let nm = self.str_lit(&name);
            let at = self.attr_list(&attrs);
            let v = self.const_struct(
                mi,
                &[
                    ("Name", nm.into()),
                    ("Type", tp.into()),
                    ("Static", self.ctx.bool_type().const_int(is_static as u64, false).into()),
                    ("invoke", invoke.const_to_int(i64t).into()),
                    ("fnval", fnval.const_to_int(i64t).into()),
                    ("attrs", at),
                    ("Params", params),
                ],
            );
            elems.push(self.static_object(v, "rmethod").into());
        }
        let pt = self.ptr_ty().into();
        self.const_slice(pt, elems)
    }

    /// `ParamInfo` objects of a method's parameters (receiver excluded).
    fn param_infos(&mut self, params: Vec<(String, TypeId)>, attrs: Vec<Vec<Attr>>) -> BasicValueEnum<'ctx> {
        let pi = self.refl().param_info;
        let mut elems = Vec::new();
        for (i, (name, ty)) in params.into_iter().enumerate() {
            let nm = self.str_lit(&name);
            let tp = self.type_info(ty);
            let at = self.attr_list(attrs.get(i).map(|v| v.as_slice()).unwrap_or(&[]));
            let i64t = self.i64_ty();
            let v = self.const_struct(pi, &[("Name", nm.into()), ("Type", tp.into()), ("Index", i64t.const_int(i as u64, false).into()), ("attrs", at)]);
            elems.push(self.static_object(v, "rparam").into());
        }
        let pt = self.ptr_ty().into();
        self.const_slice(pt, elems)
    }

    /// Closure-convention thunk `fn(env, args...)` of method `fid` (the
    /// value of `MethodInfo.Call<F>()`).
    fn method_value_thunk(&mut self, fid: FuncId, fty: TypeId) -> R<FunctionValue<'ctx>> {
        if let Some(t) = self.thunks.get(&fid) {
            return Ok(*t);
        }
        let lty = self.closure_fn_llty(fty);
        let sym = format!("{}$thunk", self.prog.funcs[fid.0 as usize].symbol);
        let thunk = self.module.add_function(&sym, lty, Some(Linkage::Internal));
        self.thunks.insert(fid, thunk);
        let target = self.funcs[fid.0 as usize];
        self.with_helper(thunk, |cg| {
            let args: Vec<BasicMetadataValueEnum> = thunk.get_params().iter().skip(1).map(|p| (*p).into()).collect();
            let r = cg.b.build_call(target, &args, "").unwrap().try_as_basic_value().basic();
            match r {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })?;
        Ok(thunk)
    }

    /// LLVM type of `any!`.
    fn any_result_ty(&self) -> StructType<'ctx> {
        self.ctx.struct_type(&[self.pair_ty().into(), self.ptr_ty().into()], false)
    }

    /// Packed thunk `any! (any recv, []any args)` of method `fid`: checks
    /// the receiver and argument types, unboxes, calls, boxes the result.
    fn invoke_thunk(&mut self, fid: FuncId) -> R<FunctionValue<'ctx>> {
        let res_ty = self.any_result_ty();
        let fty = res_ty.fn_type(&[self.pair_ty().into(), self.slice_ty().into()], false);
        let f: &'a hir::Function = &self.prog.funcs[fid.0 as usize];
        let thunk = self.module.add_function(&format!("{}$invoke", f.symbol), fty, Some(Linkage::Internal));
        let target = self.funcs[fid.0 as usize];
        let is_static = matches!(f.kind, FuncKind::Method { is_static: true, .. });
        let ptys: Vec<TypeId> = f.params.iter().map(|l| f.locals[l.0 as usize].ty).collect();
        let (recv_ty, arg_tys) = if is_static { (None, ptys.clone()) } else { (Some(ptys[0]), ptys[1..].to_vec()) };
        let owner = match f.kind {
            FuncKind::Method { struct_id, .. } => self.prog.structs[struct_id as usize].name.clone(),
            _ => String::new(),
        };
        let mname = format!("{owner}.{}", f.name);
        let ret = f.ret;
        self.with_helper(thunk, |cg| {
            let recv = thunk.get_nth_param(0).unwrap().into_struct_value();
            let args = thunk.get_nth_param(1).unwrap().into_struct_value();
            let out = cg.entry_alloca(res_ty.into(), "res", true);
            let done = cg.block("done");
            let fail = |cg: &mut Self, msg: String| {
                let s = cg.str_lit(&msg);
                let e = cg.call_rt_v("zell_error_new", &[s.into()]);
                let v = cg.b.build_insert_value(res_ty.const_zero(), e, 1, "").unwrap().into_struct_value();
                cg.b.build_store(out, v).unwrap();
                cg.b.build_unconditional_branch(done).unwrap();
            };
            let mut vals: Vec<BasicMetadataValueEnum> = Vec::new();
            if let Some(rt) = recv_ty {
                // *S (data holds the pointer) or S (data is the value).
                let Type::Pointer(st) = *cg.ty(rt) else { return super::unsupported("method receiver") };
                let data = cg.b.build_extract_value(recv, 0, "").unwrap().into_pointer_value();
                let tid = cg.b.build_extract_value(recv, 1, "").unwrap().into_pointer_value();
                let tp = cg.type_info(rt);
                let ts = cg.type_info(st);
                let slot = cg.entry_alloca(cg.ptr_ty().into(), "this", true);
                let is_p = cg.block("recv.ptr");
                let not_p = cg.block("recv.notptr");
                let is_v = cg.block("recv.val");
                let bad = cg.block("recv.bad");
                let ok = cg.block("recv.ok");
                let c = cg.ptr_eq(tid, tp);
                cg.b.build_conditional_branch(c, is_p, not_p).unwrap();
                cg.b.position_at_end(is_p);
                let pv = cg.b.build_load(cg.ptr_ty(), data, "").unwrap();
                cg.b.build_store(slot, pv).unwrap();
                cg.b.build_unconditional_branch(ok).unwrap();
                cg.b.position_at_end(not_p);
                let c2 = cg.ptr_eq(tid, ts);
                cg.b.build_conditional_branch(c2, is_v, bad).unwrap();
                cg.b.position_at_end(is_v);
                cg.b.build_store(slot, data).unwrap();
                cg.b.build_unconditional_branch(ok).unwrap();
                cg.b.position_at_end(bad);
                let sn = hir::dump::type_name(&cg.prog.types, st, &Short(cg.prog));
                fail(cg, format!("reflect: {mname}: the receiver must be *{sn} or {sn}"));
                cg.b.position_at_end(ok);
                let this = cg.b.build_load(cg.ptr_ty(), slot, "this").unwrap().into_pointer_value();
                let isnull = cg.b.build_is_null(this, "").unwrap();
                let nil = cg.block("recv.nil");
                let go = cg.block("recv.go");
                cg.b.build_conditional_branch(isnull, nil, go).unwrap();
                cg.b.position_at_end(nil);
                fail(cg, format!("reflect: {mname}: nil receiver"));
                cg.b.position_at_end(go);
                vals.push(this.into());
            }
            let n = arg_tys.len() as u64;
            let len = cg.b.build_extract_value(args, 1, "").unwrap().into_int_value();
            let lok = cg.b.build_int_compare(IntPredicate::EQ, len, cg.i64_ty().const_int(n, false), "").unwrap();
            let lgood = cg.block("args.n");
            let lbad = cg.block("args.badn");
            cg.b.build_conditional_branch(lok, lgood, lbad).unwrap();
            cg.b.position_at_end(lbad);
            fail(cg, format!("reflect: {mname} takes {n} argument(s)"));
            cg.b.position_at_end(lgood);
            let data = cg.b.build_extract_value(args, 0, "").unwrap().into_pointer_value();
            for (i, pt) in arg_tys.iter().enumerate() {
                let ep = cg.byte_gep(data, 16 * i as u64);
                let pair = cg.b.build_load(cg.pair_ty(), ep, "").unwrap().into_struct_value();
                if matches!(cg.ty(*pt), Type::Any) {
                    vals.push(pair.into());
                    continue;
                }
                let tid = cg.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value();
                let want = cg.type_info(*pt);
                let c = cg.ptr_eq(tid, want);
                let good = cg.block("arg.ok");
                let badb = cg.block("arg.bad");
                cg.b.build_conditional_branch(c, good, badb).unwrap();
                cg.b.position_at_end(badb);
                let tn = hir::dump::type_name(&cg.prog.types, *pt, &Short(cg.prog));
                fail(cg, format!("reflect: {mname}: argument {} must be {tn}", i + 1));
                cg.b.position_at_end(good);
                let d = cg.b.build_extract_value(pair, 0, "").unwrap().into_pointer_value();
                let lt = cg.llty(*pt);
                let v = if cg.any_inline(*pt) { cg.any_word_value(*pt, d) } else { cg.b.build_load(lt, d, "").unwrap() };
                let k = i + usize::from(recv_ty.is_some());
                if cg.facts.moved(fid, k) {
                    cg.retain_val(*pt, v);
                }
                vals.push(v.into());
            }
            let r = cg.b.build_call(target, &vals, "").unwrap().try_as_basic_value().basic();
            // Box the result.
            let (val_ty, err) = match (cg.ty(ret).clone(), r) {
                (Type::Fallible(inner), Some(rv)) if matches!(cg.ty(inner), Type::Void) => (None, Some(rv.into_pointer_value())),
                (Type::Fallible(inner), Some(rv)) => {
                    let sv = rv.into_struct_value();
                    let v = cg.b.build_extract_value(sv, 0, "").unwrap();
                    let e = cg.b.build_extract_value(sv, 1, "").unwrap().into_pointer_value();
                    (Some((inner, v)), Some(e))
                }
                (_, Some(rv)) => (Some((ret, rv)), None),
                (_, None) => (None, None),
            };
            let mut res = res_ty.const_zero();
            if let Some(e) = err {
                let fe = cg.block("call.err");
                let fo = cg.block("call.ok");
                let isnull = cg.b.build_is_null(e, "").unwrap();
                cg.b.build_conditional_branch(isnull, fo, fe).unwrap();
                cg.b.position_at_end(fe);
                let v = cg.b.build_insert_value(res_ty.const_zero(), e, 1, "").unwrap().into_struct_value();
                cg.b.build_store(out, v).unwrap();
                cg.b.build_unconditional_branch(done).unwrap();
                cg.b.position_at_end(fo);
            }
            if let Some((vt, v)) = val_ty {
                let pair = cg.make_any(vt, v);
                res = cg.b.build_insert_value(res, pair, 0, "").unwrap().into_struct_value();
            }
            cg.b.build_store(out, res).unwrap();
            cg.b.build_unconditional_branch(done).unwrap();
            cg.b.position_at_end(done);
            let v = cg.b.build_load(res_ty, out, "").unwrap();
            cg.b.build_return(Some(&v)).unwrap();
            Ok(())
        })?;
        Ok(thunk)
    }

    /// Emits the body of intrinsic `fid` (see [`hir::Intrinsic`]).
    pub(crate) fn emit_intrinsic(&mut self, fid: FuncId, k: Intrinsic) -> R<()> {
        let fv = self.funcs[fid.0 as usize];
        let moved = self.facts.moved(fid, 1);
        self.with_helper(fv, |cg| {
            let p = |i: u32| fv.get_nth_param(i).unwrap();
            let ptr = cg.ptr_ty();
            let ret: Option<BasicValueEnum<'ctx>> = match k {
                Intrinsic::TypeOf(t) => Some(cg.type_info(t).into()),
                Intrinsic::AnyType => Some(cg.b.build_extract_value(p(0).into_struct_value(), 1, "").unwrap()),
                Intrinsic::AnyBox => {
                    // Returned +1: the same value retained, or a new box.
                    let x = p(0).into_struct_value();
                    let data = cg.b.build_extract_value(x, 0, "").unwrap().into_pointer_value();
                    let ti = cg.b.build_extract_value(x, 1, "").unwrap().into_pointer_value();
                    let any_t = cg.prog.types.any;
                    let has = cg.block("has");
                    let same = cg.block("same");
                    let isnull = cg.b.build_is_null(ti, "").unwrap();
                    cg.b.build_conditional_branch(isnull, same, has).unwrap();
                    cg.b.position_at_end(has);
                    let f = cg.any_inline_fn();
                    let inl = cg.b.build_call(f, &[ti.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_int_value();
                    let boxit = cg.block("box");
                    cg.b.build_conditional_branch(inl, boxit, same).unwrap();
                    cg.b.position_at_end(boxit);
                    // A heap copy of the word, typed as an internal holder
                    // (not a scalar type, so the `any` owns and releases it).
                    let holder = cg.any_box_type_info();
                    let obj = cg.call_rt_v("zell_object_new", &[holder.into(), cg.i64_ty().const_int(24, false).into()]).into_pointer_value();
                    let pl = cg.payload(obj);
                    let w = cg.b.build_ptr_to_int(data, cg.i64_ty(), "").unwrap();
                    cg.b.build_store(pl, w).unwrap();
                    let bx = cg.make_pair(pl, holder);
                    cg.b.build_return(Some(&bx)).unwrap();
                    cg.b.position_at_end(same);
                    cg.retain_val(any_t, x.into());
                    Some(x.into())
                }
                Intrinsic::AnyAddr => {
                    let d = cg.b.build_extract_value(p(0).into_struct_value(), 0, "").unwrap().into_pointer_value();
                    Some(cg.b.build_ptr_to_int(d, cg.i64_ty(), "").unwrap().into())
                }
                Intrinsic::IfaceType => {
                    let vt = cg.b.build_extract_value(p(0).into_struct_value(), 1, "").unwrap().into_pointer_value();
                    let has = cg.block("has");
                    let none = cg.block("none");
                    let isnull = cg.b.build_is_null(vt, "").unwrap();
                    cg.b.build_conditional_branch(isnull, none, has).unwrap();
                    cg.b.position_at_end(none);
                    cg.b.build_return(Some(&ptr.const_null())).unwrap();
                    cg.b.position_at_end(has);
                    Some(cg.b.build_load(ptr, vt, "").unwrap())
                }
                Intrinsic::ErrType => {
                    let e = p(0).into_pointer_value();
                    let has = cg.block("has");
                    let none = cg.block("none");
                    let isnull = cg.b.build_is_null(e, "").unwrap();
                    cg.b.build_conditional_branch(isnull, none, has).unwrap();
                    cg.b.position_at_end(none);
                    cg.b.build_return(Some(&ptr.const_null())).unwrap();
                    cg.b.position_at_end(has);
                    let up = cg.byte_gep(e, 32);
                    let ut = cg.b.build_load(ptr, up, "").unwrap().into_pointer_value();
                    let plain = cg.block("plain");
                    let typed = cg.block("typed");
                    let un = cg.b.build_is_null(ut, "").unwrap();
                    cg.b.build_conditional_branch(un, plain, typed).unwrap();
                    cg.b.position_at_end(typed);
                    cg.b.build_return(Some(&ut)).unwrap();
                    cg.b.position_at_end(plain);
                    let et = cg.prog.types.error;
                    Some(cg.type_info(et).into())
                }
                Intrinsic::TypesWith(a) => {
                    let ts = cg.refl().types_with.iter().find(|(x, _)| *x == a).map(|(_, ts)| ts.clone()).unwrap_or_default();
                    // A fresh heap slice per call: the caller may sort or
                    // overwrite it, so it must not alias a constant table.
                    // The tables are immortal, so storing them needs no retain.
                    if ts.is_empty() {
                        Some(cg.slice_ty().const_zero().into())
                    } else {
                        let elems: Vec<BasicValueEnum<'ctx>> = ts.into_iter().map(|t| cg.type_info(t).into()).collect();
                        let out = cg.entry_alloca(cg.slice_ty().into(), "tw", false);
                        let n = cg.i64_ty().const_int(elems.len() as u64, false);
                        let size = cg.i64_ty().const_int(8, false);
                        let rt = cg.prog.funcs[fid.0 as usize].ret;
                        let et = cg.elem_of(rt).unwrap();
                        let ops = cg.value_ops(et);
                        cg.call_rt("zell_slice_new_ops", &[out.into(), size.into(), n.into(), n.into(), ops.into()]);
                        let data = cg.b.build_load(ptr, out, "").unwrap().into_pointer_value();
                        for (i, e) in elems.into_iter().enumerate() {
                            let at = unsafe { cg.b.build_in_bounds_gep(ptr, data, &[cg.i64_ty().const_int(i as u64, false)], "") }.unwrap();
                            cg.b.build_store(at, e).unwrap();
                        }
                        Some(cg.b.build_load(cg.slice_ty(), out, "").unwrap())
                    }
                }
                Intrinsic::LoadAddr => {
                    let a = cg.b.build_int_to_ptr(p(0).into_int_value(), ptr, "").unwrap();
                    Some(cg.b.build_load(cg.i64_ty(), a, "").unwrap())
                }
                Intrinsic::Load(t) => {
                    let a = cg.b.build_int_to_ptr(p(0).into_int_value(), ptr, "").unwrap();
                    let lt = cg.llty(t);
                    let v = cg.b.build_load(lt, a, "").unwrap();
                    cg.retain_val(t, v);
                    Some(v)
                }
                Intrinsic::Store(t) => {
                    let a = cg.b.build_int_to_ptr(p(0).into_int_value(), ptr, "").unwrap();
                    let v = p(1);
                    cg.store_assign(a, t, if moved { Val::owned(v) } else { Val::borrowed(v) });
                    None
                }
                Intrinsic::StoreTag(_) => {
                    let a = p(0).into_pointer_value();
                    let tag = cg.b.build_int_truncate(p(1).into_int_value(), cg.ctx.i32_type(), "").unwrap();
                    cg.b.build_store(a, tag).unwrap();
                    None
                }
                Intrinsic::New => {
                    let t = p(0).into_pointer_value();
                    let sp = cg.byte_gep(t, 8);
                    let size = cg.b.build_load(cg.i64_ty(), sp, "").unwrap();
                    let off = cg.type_info_field_offset("ptrTo");
                    let pp = cg.byte_gep(t, off);
                    let ptr_to = cg.b.build_load(ptr, pp, "").unwrap().into_pointer_value();
                    // A dynamic type: the runtime flags it if it is registered cyclic.
                    let alloc = if cg.cycles.any() { "zell_cycles_new_dyn" } else { "zell_object_new" };
                    let obj = cg.call_rt_v(alloc, &[t.into(), size.into()]).into_pointer_value();
                    let pl = cg.payload(obj);
                    let bx = cg.call_rt_v("zell_object_new", &[ptr_to.into(), cg.i64_ty().const_int(24, false).into()]).into_pointer_value();
                    let bp = cg.payload(bx);
                    cg.b.build_store(bp, pl).unwrap();
                    Some(cg.make_pair(bp, ptr_to))
                }
                Intrinsic::Invoke => {
                    let fnp = cg.b.build_int_to_ptr(p(0).into_int_value(), ptr, "").unwrap();
                    let res_ty = cg.any_result_ty();
                    let fty = res_ty.fn_type(&[cg.pair_ty().into(), cg.slice_ty().into()], false);
                    cg.nil_check(fnp);
                    Some(cg.b.build_indirect_call(fty, fnp, &[p(1).into(), p(2).into()], "").unwrap().try_as_basic_value().basic().unwrap())
                }
                Intrinsic::AsFunc(_) => {
                    let fnp = cg.b.build_int_to_ptr(p(0).into_int_value(), ptr, "").unwrap();
                    Some(cg.make_pair(fnp, ptr.const_null()))
                }
            };
            match ret {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })
    }

    /// Type info of the boxes `anyBox` makes for inline scalars: the `any`
    /// it returns only keeps the copy alive (`reflect.Value.owner`).
    fn any_box_type_info(&mut self) -> PointerValue<'ctx> {
        if let Some(g) = self.module.get_global("any.box.ti") {
            return g.as_pointer_value();
        }
        let tity = self.type_info_ty();
        let name = self.cstr("any box");
        let g = self.module.add_global(tity, None, "any.box.ti");
        g.set_linkage(inkwell::module::Linkage::Internal);
        g.set_constant(true);
        g.set_initializer(&tity.const_named_struct(&[name.into(), self.i64_ty().const_int(24, false).into(), self.ptr_ty().const_null().into()]));
        g.as_pointer_value()
    }

    /// Byte offset of field `name` in the `TypeInfo` payload.
    fn type_info_field_offset(&mut self, name: &str) -> u64 {
        let sid = self.refl().type_info;
        let st = self.struct_llty(sid);
        let idx = self.prog.structs[sid as usize].fields.iter().position(|f| f.name == name).unwrap_or(0);
        self.td.offset_of_element(&st, idx as u32).unwrap_or(0)
    }

    /// Intrinsic functions by id.
    pub(crate) fn intrinsics(&self) -> HashMap<FuncId, Intrinsic> {
        self.prog.reflect.as_ref().map(|r| r.intrinsics.iter().copied().collect()).unwrap_or_default()
    }
}
