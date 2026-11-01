//! Interfaces, `any`, typed errors, type switches and `errors.As`.

use super::{unsupported, CodeGen, Val, R};
use crate::hir::{Expr, FuncId};
use crate::types::{Type, TypeId};
use inkwell::module::Linkage;
use inkwell::types::{BasicMetadataTypeEnum, BasicType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::IntPredicate;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Finds method `name` of struct `sid`, possibly promoted through
    /// embedded fields: (function, path of embedded field indices).
    pub(crate) fn resolve_method(&self, sid: u32, name: &str) -> Option<(FuncId, Vec<u32>)> {
        let s = &self.prog.structs[sid as usize];
        if let Some((_, f)) = s.methods.iter().find(|(n, _)| n == name) {
            return Some((*f, Vec::new()));
        }
        for (i, fd) in s.fields.iter().enumerate() {
            if !fd.embedded {
                continue;
            }
            if let Type::Struct(d) = self.ty(fd.ty) {
                if let Some((f, mut path)) = self.resolve_method(d.0, name) {
                    path.insert(0, i as u32);
                    return Some((f, path));
                }
            }
        }
        None
    }

    /// A function `(ptr this, args...)` calling method `name` of `sid`
    /// (with receiver adjustment for promoted methods).
    fn method_entry(&mut self, sid: u32, name: &str) -> R<FunctionValue<'ctx>> {
        let Some((fid, path)) = self.resolve_method(sid, name) else {
            return unsupported(format!("method {name} not found on {}", self.prog.structs[sid as usize].full_name));
        };
        let target = self.funcs[fid.0 as usize];
        if path.is_empty() {
            return Ok(target);
        }
        let key = (sid, name.to_string());
        if let Some(f) = self.method_thunks.get(&key) {
            return Ok(*f);
        }
        let fty = target.get_type();
        let n = self.fresh();
        let thunk = self.module.add_function(&format!("method.thunk.{n}"), fty, Some(Linkage::Internal));
        self.method_thunks.insert(key, thunk);
        self.with_helper(thunk, |cg| {
            let mut this = thunk.get_nth_param(0).unwrap().into_pointer_value();
            let mut cur = sid;
            for idx in &path {
                let st = cg.struct_llty(cur);
                this = cg.b.build_struct_gep(st, this, *idx, "").unwrap();
                let ft = cg.prog.structs[cur as usize].fields[*idx as usize].ty;
                cur = match cg.ty(ft) {
                    Type::Struct(d) => d.0,
                    _ => cur,
                };
            }
            let mut args: Vec<BasicMetadataValueEnum> = vec![this.into()];
            args.extend(thunk.get_params().iter().skip(1).map(|p| BasicMetadataValueEnum::from(*p)));
            let r = cg.b.build_call(target, &args, "").unwrap().try_as_basic_value().basic();
            match r {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })?;
        Ok(thunk)
    }

    /// Vtable of dynamic type `dyn_ty` (`S` or `*S`) for interface `iface`:
    /// `{ typeinfo, boxed, fn... }` with one slot per interface method. A
    /// default method the struct does not declare gets a thunk calling the
    /// default with the interface value rebuilt from the data pointer.
    fn vtable(&mut self, dyn_ty: TypeId, sid: u32, iface: TypeId) -> R<PointerValue<'ctx>> {
        if let Some(g) = self.vtables.get(&(dyn_ty, iface)) {
            return Ok(g.as_pointer_value());
        }
        let Type::Interface(d) = self.ty(iface) else { return unsupported("vtable for a non-interface") };
        let methods = self.prog.interfaces[d.0 as usize].methods.clone();
        let boxed = !matches!(self.ty(dyn_ty), Type::Pointer(_));
        let tid = self.type_info(dyn_ty);
        let mut tys: Vec<inkwell::types::BasicTypeEnum> = vec![self.ptr_ty().into(), self.i64_ty().into()];
        tys.extend(methods.iter().map(|_| inkwell::types::BasicTypeEnum::from(self.ptr_ty())));
        let st = self.ctx.struct_type(&tys, false);
        let k = self.fresh();
        // Registered before its slots are built: a default method's thunk
        // may need this very vtable.
        let g = self.module.add_global(st, None, &format!("vt.{k}"));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        self.vtables.insert((dyn_ty, iface), g);
        let mut vals: Vec<BasicValueEnum> = vec![tid.into(), self.i64_ty().const_int(boxed as u64, false).into()];
        let is_enum = matches!(self.ty(dyn_ty), Type::Enum(_));
        for m in &methods {
            let own = if is_enum { self.enum_method(sid, &m.name).is_some() } else { self.resolve_method(sid, &m.name).is_some() };
            let fid = if is_enum { self.enum_method(sid, &m.name) } else { self.resolve_method(sid, &m.name).map(|r| r.0) };
            let f = match (own, m.default) {
                (false, Some(def)) => self.default_thunk(dyn_ty, sid, def)?,
                _ if fid.is_some_and(|f| self.prog.funcs[f.0 as usize].is_async) => self.async_entry(sid, &m.name, is_enum)?,
                _ if is_enum => self.enum_entry(sid, &m.name)?,
                _ => self.method_entry(sid, &m.name)?,
            };
            vals.push(f.as_global_value().as_pointer_value().into());
        }
        g.set_initializer(&st.const_named_struct(&vals));
        Ok(g.as_pointer_value())
    }

    /// A method result `ret` (`is_async`: of an `async` method) implements
    /// an interface method returning `want`: equal, or `want` is `Task<ret>`.
    fn ret_satisfies(&self, is_async: bool, ret: TypeId, want: TypeId) -> bool {
        if is_async {
            return matches!(self.ty(want), Type::Task(inner) if *inner == ret);
        }
        ret == want
    }

    /// The vtable slot `(ptr data, args...) -> Task<R>` of `async` method
    /// `name` of struct (or enum) `sid`, for an interface method returning
    /// `Task<R>`: it calls the method's ramp with a local result slot (no
    /// awaiter) and returns its task, or a completed task holding the
    /// result when the call never suspended (ABI.md 10.3, the same as a
    /// kept `Task<T>` of a direct call).
    fn async_entry(&mut self, sid: u32, name: &str, is_enum: bool) -> R<FunctionValue<'ctx>> {
        let key = (sid, name.to_string(), is_enum);
        if let Some(f) = self.async_thunks.get(&key) {
            return Ok(*f);
        }
        let fid = if is_enum { self.enum_method(sid, name) } else { self.resolve_method(sid, name).map(|r| r.0) };
        let Some(fid) = fid else { return unsupported(format!("async method {name} not found")) };
        // The ramp `(this, params..., ptr res)`: for a promoted struct method
        // the receiver-adjusting thunk of the same shape.
        let target = if is_enum { self.funcs[fid.0 as usize] } else { self.method_entry(sid, name)? };
        let f = &self.prog.funcs[fid.0 as usize];
        let ret = f.ret;
        let ety = f.locals[f.params[0].0 as usize].ty;
        let mut ps: Vec<BasicMetadataTypeEnum> = vec![self.ptr_ty().into()];
        for p in f.params.iter().skip(1) {
            let t = f.locals[p.0 as usize].ty;
            ps.push(self.llty(t).into());
        }
        let fty = self.ptr_ty().fn_type(&ps, false);
        let n = self.fresh();
        let thunk = self.module.add_function(&format!("async.thunk.{n}"), fty, Some(Linkage::Internal));
        self.async_thunks.insert(key, thunk);
        self.with_helper(thunk, |cg| {
            let data = thunk.get_nth_param(0).unwrap().into_pointer_value();
            let recv: BasicMetadataValueEnum = if is_enum {
                let lt = cg.llty(ety);
                cg.b.build_load(lt, data, "").unwrap().into()
            } else {
                data.into()
            };
            let mut args: Vec<BasicMetadataValueEnum> = vec![recv];
            args.extend(thunk.get_params().iter().skip(1).map(|p| BasicMetadataValueEnum::from(*p)));
            let t = cg.call_async_thunk(target, ret, args);
            cg.b.build_return(Some(&t)).unwrap();
            Ok(())
        })?;
        Ok(thunk)
    }

    /// Method `name` of enum `eid`.
    fn enum_method(&self, eid: u32, name: &str) -> Option<FuncId> {
        self.prog.enums[eid as usize].methods.iter().find(|(n, _)| n == name).map(|(_, f)| *f)
    }

    /// A vtable slot `(ptr data, args...)` calling method `name` of enum
    /// `eid` with the enum value stored at `data`.
    fn enum_entry(&mut self, eid: u32, name: &str) -> R<FunctionValue<'ctx>> {
        let key = (eid, name.to_string());
        if let Some(f) = self.enum_thunks.get(&key) {
            return Ok(*f);
        }
        let Some(fid) = self.enum_method(eid, name) else {
            return unsupported(format!("method {name} not found on {}", self.prog.enums[eid as usize].full_name));
        };
        let target = self.funcs[fid.0 as usize];
        let f = &self.prog.funcs[fid.0 as usize];
        let ety = f.locals[f.params[0].0 as usize].ty;
        let mut ps: Vec<BasicMetadataTypeEnum> = vec![self.ptr_ty().into()];
        for p in f.params.iter().skip(1) {
            let t = f.locals[p.0 as usize].ty;
            ps.push(self.llty(t).into());
        }
        let fty = match self.llty_opt(f.ret) {
            Some(t) => t.fn_type(&ps, false),
            None => self.ctx.void_type().fn_type(&ps, false),
        };
        let n = self.fresh();
        let thunk = self.module.add_function(&format!("enum.thunk.{n}"), fty, Some(Linkage::Internal));
        self.enum_thunks.insert(key, thunk);
        self.with_helper(thunk, |cg| {
            let data = thunk.get_nth_param(0).unwrap().into_pointer_value();
            let lt = cg.llty(ety);
            let v = cg.b.build_load(lt, data, "").unwrap();
            let mut args: Vec<BasicMetadataValueEnum> = vec![v.into()];
            args.extend(thunk.get_params().iter().skip(1).map(|p| BasicMetadataValueEnum::from(*p)));
            let r = cg.b.build_call(target, &args, "").unwrap().try_as_basic_value().basic();
            match r {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })?;
        Ok(thunk)
    }

    /// A vtable slot `(ptr data, args...)` for dynamic type `dyn_ty` that
    /// calls interface default method `def` (whose `this` is a value of the
    /// declaring interface) on `{data, vtable of dyn_ty for that interface}`.
    fn default_thunk(&mut self, dyn_ty: TypeId, sid: u32, def: FuncId) -> R<FunctionValue<'ctx>> {
        let key = (dyn_ty, def);
        if let Some(f) = self.default_thunks.get(&key) {
            return Ok(*f);
        }
        let df = &self.prog.funcs[def.0 as usize];
        let owner = df.locals[df.params[0].0 as usize].ty;
        let target = self.funcs[def.0 as usize];
        let mut ps: Vec<BasicMetadataTypeEnum> = vec![self.ptr_ty().into()];
        for p in df.params.iter().skip(1) {
            let t = df.locals[p.0 as usize].ty;
            ps.push(self.llty(t).into());
        }
        let fty = match self.llty_opt(df.ret) {
            Some(t) => t.fn_type(&ps, false),
            None => self.ctx.void_type().fn_type(&ps, false),
        };
        let n = self.fresh();
        let thunk = self.module.add_function(&format!("default.thunk.{n}"), fty, Some(Linkage::Internal));
        self.default_thunks.insert(key, thunk);
        let vt = self.vtable(dyn_ty, sid, owner)?;
        self.with_helper(thunk, |cg| {
            let data = thunk.get_nth_param(0).unwrap().into_pointer_value();
            let this = cg.make_pair(data, vt);
            let mut args: Vec<BasicMetadataValueEnum> = vec![this.into()];
            args.extend(thunk.get_params().iter().skip(1).map(|p| BasicMetadataValueEnum::from(*p)));
            let r = cg.b.build_call(target, &args, "").unwrap().try_as_basic_value().basic();
            match r {
                Some(v) => cg.b.build_return(Some(&v)).unwrap(),
                None => cg.b.build_return(None).unwrap(),
            };
            Ok(())
        })?;
        Ok(thunk)
    }

    /// The vtable for interface `to` of the dynamic type behind vtable `vt`
    /// (of another interface), or NULL if `vt` is NULL or the dynamic type
    /// lacks a method of `to`.
    pub(crate) fn convert_vtable(&mut self, vt: PointerValue<'ctx>, to: TypeId) -> PointerValue<'ctx> {
        let isnull = self.b.build_is_null(vt, "").unwrap();
        let has = self.block("conv.has");
        let done = self.block("conv.done");
        let from_bb = self.b.get_insert_block().unwrap();
        self.b.build_conditional_branch(isnull, done, has).unwrap();
        self.b.position_at_end(has);
        let tid = self.b.build_load(self.ptr_ty(), vt, "tid").unwrap().into_pointer_value();
        let f = self.iface_conv_fn(to);
        let nvt = self.b.build_call(f, &[tid.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
        let has_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        let phi = self.b.build_phi(self.ptr_ty(), "vt").unwrap();
        phi.add_incoming(&[(&self.ptr_ty().const_null(), from_bb), (&nvt, has_end)]);
        phi.as_basic_value().into_pointer_value()
    }

    /// `ptr iface.conv.N(ptr typeid)`: vtable of interface `to` for a
    /// dynamic type id, NULL if the type does not implement it. Declared
    /// here; the body is emitted at the end of the module by
    /// [`finish_iface_convs`](Self::finish_iface_convs), when every type
    /// stored in an interface is known.
    pub(crate) fn iface_conv_fn(&mut self, to: TypeId) -> FunctionValue<'ctx> {
        if let Some((_, f)) = self.iface_convs.iter().find(|(t, _)| *t == to) {
            return *f;
        }
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into()], false);
        let k = self.fresh();
        let f = self.module.add_function(&format!("iface.conv.{k}"), fty, Some(Linkage::Internal));
        self.iface_convs.push((to, f));
        f
    }

    /// True if struct `sid` has every method of interface `iface` with the
    /// same signature (default methods may be missing); a sealed interface
    /// also requires the struct to be declared in its namespace.
    fn enum_implements(&self, eid: u32, iface: TypeId) -> bool {
        let Type::Interface(d) = self.ty(iface) else { return false };
        let it = &self.prog.interfaces[d.0 as usize];
        let en = &self.prog.enums[eid as usize];
        if it.sealed && (it.namespace != en.namespace || crate::sema::stdlib::is_std_namespace(&en.full_name) != crate::sema::stdlib::is_std_namespace(&it.full_name)) {
            return false;
        }
        it.methods.iter().all(|m| match self.enum_method(eid, &m.name) {
            Some(fid) => {
                let f = &self.prog.funcs[fid.0 as usize];
                let ps: Vec<TypeId> = f.params.iter().skip(1).map(|l| f.locals[l.0 as usize].ty).collect();
                ps == m.params && self.ret_satisfies(f.is_async, f.ret, m.ret)
            }
            None => m.default.is_some(),
        })
    }

    fn struct_implements(&self, sid: u32, iface: TypeId) -> bool {
        let Type::Interface(d) = self.ty(iface) else { return false };
        let it = &self.prog.interfaces[d.0 as usize];
        let st = &self.prog.structs[sid as usize];
        if it.sealed && (it.namespace != st.namespace || crate::sema::stdlib::is_std_namespace(&st.full_name) != crate::sema::stdlib::is_std_namespace(&it.full_name)) {
            return false;
        }
        it.methods.iter().all(|m| match self.resolve_method(sid, &m.name) {
            Some((fid, _)) => {
                let f = &self.prog.funcs[fid.0 as usize];
                let ps: Vec<TypeId> = f.params.iter().skip(1).map(|l| f.locals[l.0 as usize].ty).collect();
                ps == m.params && self.ret_satisfies(f.is_async, f.ret, m.ret)
            }
            None => m.default.is_some(),
        })
    }

    /// `ptr iface.anybox(ptr typeid, ptr p)`: a new `any` box (+1, owning
    /// `p`) for the `*S` with type id `typeid` stored in an interface.
    /// Declared here; the body is emitted by
    /// [`finish_iface_convs`](Self::finish_iface_convs).
    fn iface_anybox_fn(&mut self) -> FunctionValue<'ctx> {
        if let Some(f) = self.iface_anybox {
            return f;
        }
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into(), self.ptr_ty().into()], false);
        let f = self.module.add_function("iface.anybox", fty, Some(Linkage::Internal));
        self.iface_anybox = Some(f);
        f
    }

    /// Emits the bodies of the `iface.conv.*` functions: a comparison
    /// against the type id of every type stored in some interface.
    pub(crate) fn finish_iface_convs(&mut self) -> R<()> {
        let mut dyn_tys: Vec<TypeId> = self.vtables.keys().map(|(t, _)| *t).collect();
        dyn_tys.extend(self.any_dyn_tys.iter().copied());
        dyn_tys.sort();
        dyn_tys.dedup();
        let mut i = 0;
        while i < self.iface_convs.len() {
            let (to, f) = self.iface_convs[i];
            i += 1;
            let mut cases = Vec::new();
            for &t in &dyn_tys {
                let (sid, ok) = match self.ty(t) {
                    Type::Struct(d) => (d.0, self.struct_implements(d.0, to)),
                    Type::Enum(d) => (d.0, self.enum_implements(d.0, to)),
                    Type::Pointer(p) => match self.ty(*p) {
                        Type::Struct(d) => (d.0, self.struct_implements(d.0, to)),
                        _ => continue,
                    },
                    _ => continue,
                };
                if ok {
                    let tid = self.type_info(t);
                    let vt = self.vtable(t, sid, to)?;
                    cases.push((tid, vt));
                }
            }
            self.with_helper(f, |cg| {
                let tid = f.get_nth_param(0).unwrap().into_pointer_value();
                for (t, vt) in cases {
                    let yes = cg.block("is");
                    let next = cg.block("next");
                    let eq = cg.ptr_eq(tid, t);
                    cg.b.build_conditional_branch(eq, yes, next).unwrap();
                    cg.b.position_at_end(yes);
                    cg.b.build_return(Some(&vt)).unwrap();
                    cg.b.position_at_end(next);
                }
                cg.b.build_return(Some(&cg.ptr_ty().const_null())).unwrap();
                Ok(())
            })?;
        }
        if let Some(f) = self.iface_anybox {
            let mut ptrs: Vec<TypeId> = self.vtables.keys().map(|(t, _)| *t).filter(|t| matches!(self.ty(*t), Type::Pointer(_))).collect();
            ptrs.sort();
            ptrs.dedup();
            self.with_helper(f, |cg| {
                let tid = f.get_nth_param(0).unwrap().into_pointer_value();
                let p = f.get_nth_param(1).unwrap();
                for t in ptrs {
                    let yes = cg.block("is");
                    let next = cg.block("next");
                    let ti = cg.type_info(t);
                    let eq = cg.ptr_eq(tid, ti);
                    cg.b.build_conditional_branch(eq, yes, next).unwrap();
                    cg.b.position_at_end(yes);
                    let obj = cg.new_box(t, p);
                    cg.b.build_return(Some(&obj)).unwrap();
                    cg.b.position_at_end(next);
                }
                cg.b.build_unreachable().unwrap();
                Ok(())
            })?;
        }
        Ok(())
    }

    /// `zell_message_fn` thunk calling `S.Message()` on a box payload.
    fn message_thunk(&mut self, sid: u32) -> R<FunctionValue<'ctx>> {
        if let Some(f) = self.msg_thunks.get(&sid) {
            return Ok(*f);
        }
        let m = self.method_entry(sid, "Message")?;
        let fty = self.ptr_ty().fn_type(&[self.ptr_ty().into()], false);
        let k = self.fresh();
        let f = self.module.add_function(&format!("msg.thunk.{k}"), fty, Some(Linkage::Internal));
        self.msg_thunks.insert(sid, f);
        self.with_helper(f, |cg| {
            let obj = f.get_nth_param(0).unwrap().into_pointer_value();
            let p = cg.payload(obj);
            let r = cg.b.build_call(m, &[p.into()], "").unwrap().try_as_basic_value().basic().unwrap();
            cg.b.build_return(Some(&r)).unwrap();
            Ok(())
        })?;
        Ok(f)
    }

    /// Whether struct `sid` has an instance method `Unwrap() error`.
    fn has_unwrap(&self, sid: u32) -> bool {
        let Some((fid, _)) = self.resolve_method(sid, "Unwrap") else { return false };
        let f = &self.prog.funcs[fid.0 as usize];
        f.params.len() == 1 && !f.is_async && matches!(self.ty(f.ret), Type::Error)
    }

    /// Boxes struct value `v` (owned) as a typed error (+1).
    fn box_error(&mut self, st: TypeId, sid: u32, v: BasicValueEnum<'ctx>) -> R<BasicValueEnum<'ctx>> {
        let thunk = self.message_thunk(sid)?;
        // A candidate dynamic type for `case SomeInterface x:` on errors.
        if !self.any_dyn_tys.contains(&st) {
            self.any_dyn_tys.push(st);
        }
        let obj = self.new_box(st, v);
        let ti = self.type_info(st);
        let e = self.call_rt_v("zell_error_new_typed", &[ti.into(), obj.into(), thunk.as_global_value().as_pointer_value().into()]);
        if self.has_unwrap(sid) {
            // DESIGN.md "Ошибки": a user `Unwrap() error` links the boxed
            // error to its cause for errors.Is/As/Unwrap (ABI.md 7: the
            // `inner` of a typed error; its Message() is the whole text).
            let m = self.method_entry(sid, "Unwrap")?;
            let p = self.payload(obj);
            let cause = self.call_fn(m, &[p.into()]).try_as_basic_value().basic().unwrap();
            let slot = self.byte_gep(e.into_pointer_value(), 24);
            self.b.build_store(slot, cause).unwrap();
        }
        self.call_rt("zell_release", &[obj.into()]);
        Ok(e)
    }

    /// `ToInterface`: box `value` into interface/error/any type `to`.
    pub(crate) fn to_interface(&mut self, value: &Expr, to: TypeId) -> R<Val<'ctx>> {
        let from = value.ty;
        match (self.ty(to).clone(), self.ty(from).clone()) {
            (Type::Error, Type::Error) => self.expr(value),
            (Type::Error, Type::Struct(d)) => {
                let v = self.expr_owned(value)?;
                let e = self.box_error(from, d.0, v)?;
                Ok(Val::owned(self.trace_created(e, value.span.start)))
            }
            (Type::Error, Type::Pointer(inner)) => {
                let Type::Struct(d) = self.ty(inner).clone() else { return unsupported("non-struct pointer as error") };
                let pv = self.expr(value)?;
                let p = pv.get().into_pointer_value();
                self.nil_check(p);
                let lt = self.llty(inner);
                let sv = self.b.build_load(lt, p, "").unwrap();
                self.retain_val(inner, sv);
                let e = self.box_error(inner, d.0, sv)?;
                let e = self.trace_created(e, value.span.start);
                self.drop_val(from, pv);
                Ok(Val::owned(e))
            }
            (Type::Interface(_), Type::Struct(d)) => {
                let v = self.expr_owned(value)?;
                let vt = self.vtable(from, d.0, to)?;
                let obj = self.new_box(from, v);
                let data = self.payload(obj);
                Ok(Val::owned(self.make_pair(data, vt)))
            }
            (Type::Interface(_), Type::Enum(d)) => {
                let v = self.expr_owned(value)?;
                let vt = self.vtable(from, d.0, to)?;
                let obj = self.new_box(from, v);
                let data = self.payload(obj);
                Ok(Val::owned(self.make_pair(data, vt)))
            }
            (Type::Interface(_), Type::Pointer(inner)) => {
                let Type::Struct(d) = self.ty(inner).clone() else { return unsupported("non-struct pointer in an interface") };
                let pv = self.expr(value)?;
                let p = pv.get().into_pointer_value();
                let vt = self.vtable(from, d.0, to)?;
                // The interface value owns the pointer like a box.
                Ok(Val { v: Some(self.make_pair(p, vt)), owned: pv.owned })
            }
            (Type::Interface(_), Type::Interface(_)) if from == to => self.expr(value),
            (Type::Interface(_), Type::Interface(_)) => {
                // Same data, the target's vtable for the dynamic type.
                let pv = self.expr(value)?;
                let pair = pv.get().into_struct_value();
                let data = self.b.build_extract_value(pair, 0, "").unwrap().into_pointer_value();
                let vt = self.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value();
                let nvt = self.convert_vtable(vt, to);
                Ok(Val { v: Some(self.make_pair(data, nvt)), owned: pv.owned })
            }
            (Type::Any, Type::Any) => self.expr(value),
            (Type::Any, Type::Interface(_)) => {
                // The `any` keeps the dynamic type (Go semantics): the type
                // id from the vtable and the same value box; a stored `*S`
                // (vtable `boxed` = 0) is boxed, as `any` boxes pointers.
                let v = self.expr_owned(value)?.into_struct_value();
                let data = self.b.build_extract_value(v, 0, "").unwrap().into_pointer_value();
                let vt = self.b.build_extract_value(v, 1, "").unwrap().into_pointer_value();
                let entry = self.b.get_insert_block().unwrap();
                let has = self.block("toany.has");
                let ptrb = self.block("toany.ptr");
                let done = self.block("toany.done");
                let isnull = self.b.build_is_null(vt, "").unwrap();
                self.b.build_conditional_branch(isnull, done, has).unwrap();
                self.b.position_at_end(has);
                let tid = self.b.build_load(self.ptr_ty(), vt, "tid").unwrap().into_pointer_value();
                let bp = self.byte_gep(vt, 8);
                let boxed = self.b.build_load(self.i64_ty(), bp, "").unwrap().into_int_value();
                let is_ptr = self.b.build_int_compare(IntPredicate::EQ, boxed, self.i64_ty().const_zero(), "").unwrap();
                self.b.build_conditional_branch(is_ptr, ptrb, done).unwrap();
                self.b.position_at_end(ptrb);
                let f = self.iface_anybox_fn();
                let obj = self.b.build_call(f, &[tid.into(), data.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
                let pd = self.payload(obj);
                let ptr_end = self.b.get_insert_block().unwrap();
                self.b.build_unconditional_branch(done).unwrap();
                self.b.position_at_end(done);
                let null = self.ptr_ty().const_null();
                let dphi = self.b.build_phi(self.ptr_ty(), "anyd").unwrap();
                dphi.add_incoming(&[(&null, entry), (&data, has), (&pd, ptr_end)]);
                let tphi = self.b.build_phi(self.ptr_ty(), "anyt").unwrap();
                tphi.add_incoming(&[(&null, entry), (&tid, has), (&tid, ptr_end)]);
                let d = dphi.as_basic_value().into_pointer_value();
                let t = tphi.as_basic_value().into_pointer_value();
                Ok(Val::owned(self.make_pair(d, t)))
            }
            (Type::Any, _) => {
                if !self.any_dyn_tys.contains(&from) {
                    self.any_dyn_tys.push(from);
                }
                let v = self.expr_owned(value)?;
                Ok(Val::owned(self.make_any(from, v)))
            }
            _ => unsupported(format!("conversion of {} to {}", self.type_name(from), self.type_name(to))),
        }
    }

    /// True for the types an `any` holds inline, in its data word instead of
    /// a heap box (runtime/ABI.md 3.7): bool, the integers, floats, char.
    pub(crate) fn any_inline(&self, t: TypeId) -> bool {
        matches!(self.ty(t), Type::Bool | Type::Int(_) | Type::Float32 | Type::Float64 | Type::Char)
    }

    /// An `any` holding `v` (owned) of type `t`: `{payload of a box, type}`,
    /// or `{value bits, type}` for an inline scalar.
    pub(crate) fn make_any(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        let ti = self.type_info(t);
        if self.any_inline(t) {
            if !self.any_inline_tys.contains(&t) {
                self.any_inline_tys.push(t);
            }
            let i64t = self.i64_ty();
            let w = match v {
                BasicValueEnum::IntValue(i) if i.get_type().get_bit_width() < 64 => self.b.build_int_z_extend(i, i64t, "").unwrap(),
                BasicValueEnum::IntValue(i) => i,
                BasicValueEnum::FloatValue(f) if f.get_type() == self.ctx.f32_type() => {
                    let b = self.b.build_bit_cast(f, self.ctx.i32_type(), "").unwrap().into_int_value();
                    self.b.build_int_z_extend(b, i64t, "").unwrap()
                }
                BasicValueEnum::FloatValue(f) => self.b.build_bit_cast(f, i64t, "").unwrap().into_int_value(),
                _ => unreachable!("inline any of a non-scalar"),
            };
            let data = self.b.build_int_to_ptr(w, self.ptr_ty(), "anyw").unwrap();
            return self.make_pair(data, ti);
        }
        let obj = self.new_box(t, v);
        let data = self.payload(obj);
        self.make_pair(data, ti)
    }

    /// The value of inline scalar type `t` held in an `any` data word.
    pub(crate) fn any_word_value(&mut self, t: TypeId, data: PointerValue<'ctx>) -> BasicValueEnum<'ctx> {
        let w = self.b.build_ptr_to_int(data, self.i64_ty(), "").unwrap();
        match self.llty(t) {
            inkwell::types::BasicTypeEnum::IntType(it) if it.get_bit_width() < 64 => self.b.build_int_truncate(w, it, "").unwrap().into(),
            inkwell::types::BasicTypeEnum::IntType(_) => w.into(),
            inkwell::types::BasicTypeEnum::FloatType(ft) if ft == self.ctx.f32_type() => {
                let b = self.b.build_int_truncate(w, self.ctx.i32_type(), "").unwrap();
                self.b.build_bit_cast(b, ft, "").unwrap()
            }
            inkwell::types::BasicTypeEnum::FloatType(ft) => self.b.build_bit_cast(w, ft, "").unwrap(),
            _ => unreachable!("inline any of a non-scalar"),
        }
    }

    /// `i1 any.inline(ptr typeinfo)`: whether an `any` of that dynamic type
    /// holds its value inline; the body is emitted last
    /// ([`finish_any_inline`](Self::finish_any_inline)).
    pub(crate) fn any_inline_fn(&mut self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("any.inline") {
            return f;
        }
        let fty = self.ctx.bool_type().fn_type(&[self.ptr_ty().into()], false);
        self.module.add_function("any.inline", fty, Some(Linkage::Internal))
    }

    /// Emits `any.inline`: compares against the type info of every scalar
    /// type the module stores inline in an `any`.
    pub(crate) fn finish_any_inline(&mut self) -> R<()> {
        let Some(f) = self.module.get_function("any.inline") else { return Ok(()) };
        let tys = self.any_inline_tys.clone();
        let tis: Vec<PointerValue<'ctx>> = tys.iter().map(|t| self.type_info(*t)).collect();
        self.with_helper(f, |cg| {
            let ti = f.get_nth_param(0).unwrap().into_pointer_value();
            let mut r = cg.ctx.bool_type().const_zero();
            for g in tis {
                let eq = cg.ptr_eq(ti, g);
                r = cg.b.build_or(r, eq, "").unwrap();
            }
            cg.b.build_return(Some(&r)).unwrap();
            Ok(())
        })
    }

    /// Dynamic call of interface method `method` on `recv`.
    pub(crate) fn iface_call(&mut self, ty: TypeId, recv: &Expr, method: u32, args: &[Expr]) -> R<Val<'ctx>> {
        if *self.ty(recv.ty) == Type::Error {
            let rv = self.expr(recv)?;
            let r = self.call_rt_v("zell_error_message", &[rv.get().into()]);
            self.drop_val(recv.ty, rv);
            return Ok(Val::owned(r));
        }
        let dep = self.depth();
        let (rv, _) = self.arg(recv, false, true, false)?;
        let Type::Interface(d) = self.ty(recv.ty).clone() else { return unsupported("method call on this type") };
        let sig = self.prog.interfaces[d.0 as usize].methods[method as usize].clone();
        let pair = rv.get().into_struct_value();
        let data = self.b.build_extract_value(pair, 0, "").unwrap();
        let known = match &recv.kind {
            crate::hir::ExprKind::Local(x) => self.fs().known_dyn.iter().rev().find(|(l, _)| l == x).map(|(_, t)| *t),
            _ => None,
        };
        let vt = match known {
            // Inside `case T x:` of a switch on this local: T's vtable.
            Some(dt) => {
                let sid = match self.ty(dt).clone() {
                    Type::Pointer(p) => match self.ty(p) {
                        Type::Struct(s) => s.0,
                        _ => return unsupported("devirtualised call on a non-struct pointer"),
                    },
                    Type::Struct(s) | Type::Enum(s) => s.0,
                    _ => return unsupported("devirtualised call on this type"),
                };
                self.vtable(dt, sid, recv.ty)?
            }
            None => self.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value(),
        };
        self.nil_check(vt);
        let fp = self.byte_gep(vt, 16 + 8 * method as u64);
        let fnp = self.b.build_load(self.ptr_ty(), fp, "").unwrap().into_pointer_value();
        let mut ps: Vec<BasicMetadataTypeEnum> = vec![self.ptr_ty().into()];
        for p in &sig.params {
            ps.push(self.llty(*p).into());
        }
        let fty = match self.llty_opt(sig.ret) {
            Some(t) => t.fn_type(&ps, false),
            None => self.ctx.void_type().fn_type(&ps, false),
        };
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
        let mut ma: Vec<BasicMetadataValueEnum> = vec![data.into()];
        ma.extend(vals.iter().map(|v| BasicMetadataValueEnum::from(v.get())));
        // Moved arguments belong to the callee from here on, also if it panics.
        for h in moved_holds {
            self.unhold(h);
        }
        let r = self.call_site(fty, inkwell::values::AsValueRef::as_value_ref(&fnp), &ma, true).try_as_basic_value().basic();
        self.release_temps_from(dep);
        let _ = ty;
        Ok(Val { v: r, owned: true })
    }

    /// For a type-switch subject `subj` (interface or error value) and
    /// pattern type `pat`: (matches, pointer to the stored `pat` value).
    pub(crate) fn type_test(&mut self, subj_ty: TypeId, subj: BasicValueEnum<'ctx>, pat: TypeId) -> R<(IntValue<'ctx>, PointerValue<'ctx>)> {
        let res = self.entry_alloca(self.ctx.bool_type().into(), "tm", false);
        self.b.build_store(res, self.ctx.bool_type().const_zero()).unwrap();
        let datap = self.entry_alloca(self.ptr_ty().into(), "td", false);
        self.b.build_store(datap, self.ptr_ty().const_null()).unwrap();
        let has = self.block("tt.has");
        let done = self.block("tt.done");
        match self.ty(subj_ty) {
            Type::Error if matches!(self.ty(pat), Type::Interface(_)) => {
                // `case ExitCoder c:` on an error: like on `any`, the
                // vtable is looked up by the payload's dynamic type (the
                // user type at +32; NULL for message-only errors). Every
                // struct boxed into an error is a candidate
                // (`box_error` records it); the payload is a value box,
                // so `data` is its payload address.
                let e = subj.into_pointer_value();
                let nn = self.b.build_is_not_null(e, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let utp = self.byte_gep(e, 32);
                let ut = self.b.build_load(self.ptr_ty(), utp, "").unwrap().into_pointer_value();
                let typed = self.block("tt.typed");
                let utn = self.b.build_is_not_null(ut, "").unwrap();
                self.b.build_conditional_branch(utn, typed, done).unwrap();
                self.b.position_at_end(typed);
                let f = self.iface_conv_fn(pat);
                let nvt = self.b.build_call(f, &[ut.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
                let ok = self.b.build_is_not_null(nvt, "").unwrap();
                self.b.build_store(res, ok).unwrap();
                let pp = self.byte_gep(e, 40);
                let payload = self.b.build_load(self.ptr_ty(), pp, "").unwrap().into_pointer_value();
                let d = self.payload(payload);
                let pt = self.llty(pat);
                let slot = self.entry_alloca(pt, "tiface", false);
                let np = self.make_pair(d, nvt);
                self.b.build_store(slot, np).unwrap();
                self.b.build_store(datap, slot).unwrap();
            }
            Type::Error => {
                let ti = self.type_info(pat);
                let e = subj.into_pointer_value();
                let nn = self.b.build_is_not_null(e, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let utp = self.byte_gep(e, 32);
                let ut = self.b.build_load(self.ptr_ty(), utp, "").unwrap().into_pointer_value();
                let eq = self.ptr_eq(ut, ti);
                self.b.build_store(res, eq).unwrap();
                let pp = self.byte_gep(e, 40);
                let payload = self.b.build_load(self.ptr_ty(), pp, "").unwrap().into_pointer_value();
                let d = self.payload(payload);
                if matches!(self.ty(pat), Type::Pointer(_)) {
                    return unsupported("pointer type patterns on errors");
                }
                self.b.build_store(datap, d).unwrap();
            }
            Type::Interface(_) if matches!(self.ty(pat), Type::Interface(_)) => {
                // `case Mover m:` on an interface: the dynamic type must
                // implement `Mover`; the binding gets its vtable.
                let pair = subj.into_struct_value();
                let data = self.b.build_extract_value(pair, 0, "").unwrap().into_pointer_value();
                let vt = self.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value();
                let nn = self.b.build_is_not_null(vt, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let nvt = self.convert_vtable(vt, pat);
                let ok = self.b.build_is_not_null(nvt, "").unwrap();
                self.b.build_store(res, ok).unwrap();
                let pt = self.llty(pat);
                let slot = self.entry_alloca(pt, "tiface", false);
                let np = self.make_pair(data, nvt);
                self.b.build_store(slot, np).unwrap();
                self.b.build_store(datap, slot).unwrap();
            }
            Type::Any if matches!(self.ty(pat), Type::Interface(_)) => {
                // `case Named n:` on `any`: look the vtable up by the
                // dynamic type id. An `any` boxes a stored `*S`, while an
                // interface stores the pointer itself (vtable `boxed` = 0).
                let pair = subj.into_struct_value();
                let data = self.b.build_extract_value(pair, 0, "").unwrap().into_pointer_value();
                let tid = self.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value();
                let nn = self.b.build_is_not_null(tid, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let f = self.iface_conv_fn(pat);
                let nvt = self.b.build_call(f, &[tid.into()], "").unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
                let ok = self.b.build_is_not_null(nvt, "").unwrap();
                self.b.build_store(res, ok).unwrap();
                let found = self.block("tt.iface");
                self.b.build_conditional_branch(ok, found, done).unwrap();
                self.b.position_at_end(found);
                let bp = self.byte_gep(nvt, 8);
                let boxed = self.b.build_load(self.i64_ty(), bp, "").unwrap().into_int_value();
                let is_ptr = self.b.build_int_compare(IntPredicate::EQ, boxed, self.i64_ty().const_zero(), "").unwrap();
                let inner = self.b.build_load(self.ptr_ty(), data, "").unwrap().into_pointer_value();
                let d = self.b.build_select(is_ptr, inner, data, "").unwrap().into_pointer_value();
                let pt = self.llty(pat);
                let slot = self.entry_alloca(pt, "tiface", false);
                let np = self.make_pair(d, nvt);
                self.b.build_store(slot, np).unwrap();
                self.b.build_store(datap, slot).unwrap();
            }
            Type::Interface(_) | Type::Any => {
                let pair = subj.into_struct_value();
                let data = self.b.build_extract_value(pair, 0, "").unwrap().into_pointer_value();
                let vt = self.b.build_extract_value(pair, 1, "").unwrap().into_pointer_value();
                let nn = self.b.build_is_not_null(vt, "").unwrap();
                self.b.build_conditional_branch(nn, has, done).unwrap();
                self.b.position_at_end(has);
                let ti = self.type_info(pat);
                let tid = if matches!(self.ty(subj_ty), Type::Any) {
                    vt
                } else {
                    self.b.build_load(self.ptr_ty(), vt, "").unwrap().into_pointer_value()
                };
                let eq = self.ptr_eq(tid, ti);
                self.b.build_store(res, eq).unwrap();
                if matches!(self.ty(pat), Type::Pointer(_)) && !matches!(self.ty(subj_ty), Type::Any) {
                    // An interface stores a `*S` itself (an `any` boxes it):
                    // the binding is the data pointer, so store its address.
                    let slot = self.entry_alloca(self.ptr_ty().into(), "tp", false);
                    self.b.build_store(slot, data).unwrap();
                    self.b.build_store(datap, slot).unwrap();
                } else if matches!(self.ty(subj_ty), Type::Any) && self.any_inline(pat) {
                    // An inline scalar: the data word is the value.
                    let slot = self.entry_alloca(self.i64_ty().into(), "tw", false);
                    let w = self.b.build_ptr_to_int(data, self.i64_ty(), "").unwrap();
                    self.b.build_store(slot, w).unwrap();
                    self.b.build_store(datap, slot).unwrap();
                } else {
                    self.b.build_store(datap, data).unwrap();
                }
            }
            _ => return unsupported("type switch on this type"),
        }
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        let m = self.b.build_load(self.ctx.bool_type(), res, "").unwrap().into_int_value();
        let d = self.b.build_load(self.ptr_ty(), datap, "").unwrap().into_pointer_value();
        Ok((m, d))
    }

    pub(crate) fn ptr_eq(&mut self, a: PointerValue<'ctx>, b: PointerValue<'ctx>) -> IntValue<'ctx> {
        let ai = self.b.build_ptr_to_int(a, self.i64_ty(), "").unwrap();
        let bi = self.b.build_ptr_to_int(b, self.i64_ty(), "").unwrap();
        self.b.build_int_compare(IntPredicate::EQ, ai, bi, "").unwrap()
    }

    /// `errors.As(err, target)`.
    pub(crate) fn errors_as(&mut self, err: &Expr, target: &Expr) -> R<Val<'ctx>> {
        let Type::Pointer(st) = self.ty(target.ty).clone() else { return unsupported("errors.As target") };
        let ev = self.expr(err)?;
        let h = self.hold(err.ty, ev);
        let tp = self.expr(target)?.get().into_pointer_value();
        self.unhold(h);
        let ti = self.type_info(st);
        let payload = self.call_rt_v("zell_error_as", &[ev.get().into(), ti.into()]).into_pointer_value();
        let found = self.b.build_is_not_null(payload, "").unwrap();
        let yes = self.block("as.yes");
        let done = self.block("as.done");
        self.b.build_conditional_branch(found, yes, done).unwrap();
        self.b.position_at_end(yes);
        self.nil_check(tp);
        let src = self.payload(payload);
        let lt = self.llty(st);
        let v = self.b.build_load(lt, src, "").unwrap();
        self.store_assign(tp, st, Val::borrowed(v));
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        self.drop_val(err.ty, ev);
        Ok(Val::borrowed(found.into()))
    }
}
