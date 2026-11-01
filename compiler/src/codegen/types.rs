//! Mapping of semantic types to LLVM types, plus layout queries.
//!
//! See the module documentation of [`super`] for the representation rules.

use super::{CodeGen, R};
use crate::types::{Type, TypeId};
use inkwell::types::{BasicType, BasicTypeEnum, FunctionType, StructType};
use inkwell::AddressSpace;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// The semantic type behind `id`.
    pub(crate) fn ty(&self, id: TypeId) -> &'a Type {
        self.prog.types.get(id)
    }

    pub(crate) fn ptr_ty(&self) -> inkwell::types::PointerType<'ctx> {
        self.ctx.ptr_type(AddressSpace::default())
    }

    pub(crate) fn i64_ty(&self) -> inkwell::types::IntType<'ctx> {
        self.ctx.i64_type()
    }

    /// `{ ptr, i64, i64, ptr }`.
    pub(crate) fn slice_ty(&self) -> StructType<'ctx> {
        self.slice_struct
    }

    /// Two-word value: closures `{fn, env}`, interfaces `{data, vtable}`,
    /// `any` `{data, typeinfo}`.
    pub(crate) fn pair_ty(&self) -> StructType<'ctx> {
        self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into()], false)
    }

    /// LLVM type of a value of type `id`; `None` for `void`.
    pub(crate) fn llty_opt(&mut self, id: TypeId) -> Option<BasicTypeEnum<'ctx>> {
        let ptr: BasicTypeEnum<'ctx> = self.ptr_ty().into();
        Some(match self.ty(id) {
            Type::Void => return None,
            Type::Bool => self.ctx.bool_type().into(),
            Type::Int(k) => self.ctx.custom_width_int_type(std::num::NonZeroU32::new(k.bits()).unwrap()).expect("valid int width").into(),
            Type::Float32 => self.ctx.f32_type().into(),
            Type::Float64 => self.ctx.f64_type().into(),
            Type::Char => self.ctx.i32_type().into(),
            Type::String | Type::Error | Type::Nil | Type::Pointer(_) | Type::Weak(_) | Type::Map(..) | Type::Task(_) => ptr,
            Type::Any | Type::Interface(_) | Type::Func(_) => self.pair_ty().into(),
            Type::Slice(_) => self.slice_ty().into(),
            Type::Array(t, n) => {
                let et = self.llty(*t);
                et.array_type(*n as u32).into()
            }
            Type::Tuple(ts) => {
                let fs: Vec<BasicTypeEnum> = ts.iter().map(|t| self.llty(*t)).collect();
                self.ctx.struct_type(&fs, false).into()
            }
            Type::Fallible(t) => match self.llty_opt(*t) {
                None => ptr,
                Some(inner) => self.ctx.struct_type(&[inner, ptr], false).into(),
            },
            Type::Struct(d) => self.struct_llty(d.0).into(),
            Type::Enum(d) => {
                if self.prog.enums[d.0 as usize].is_plain {
                    self.ctx.i32_type().into()
                } else {
                    self.enum_llty(d.0).into()
                }
            }
            // Never present after sema; map to a harmless type.
            Type::Infer(_) | Type::Param(_) | Type::Invalid => self.ctx.i8_type().into(),
        })
    }

    /// LLVM type of a non-void value (void becomes `i8`, only used for
    /// placeholder slots).
    pub(crate) fn llty(&mut self, id: TypeId) -> BasicTypeEnum<'ctx> {
        self.llty_opt(id).unwrap_or_else(|| self.ctx.i8_type().into())
    }

    /// Named LLVM struct for a Zell struct (fields in HIR order).
    pub(crate) fn struct_llty(&mut self, sid: u32) -> StructType<'ctx> {
        if let Some(t) = self.struct_tys.get(&sid) {
            return *t;
        }
        let s = &self.prog.structs[sid as usize];
        let st = self.ctx.opaque_struct_type(&s.full_name);
        self.struct_tys.insert(sid, st);
        let fields: Vec<BasicTypeEnum> = s.fields.iter().map(|f| f.ty).collect::<Vec<_>>().into_iter().map(|t| self.llty(t)).collect();
        st.set_body(&fields, false);
        st
    }

    /// `{ i32 tag, [W x i64] payload }` for a data enum.
    pub(crate) fn enum_llty(&mut self, eid: u32) -> StructType<'ctx> {
        if let Some(t) = self.enum_tys.get(&eid) {
            return *t;
        }
        let e = &self.prog.enums[eid as usize];
        let st = self.ctx.opaque_struct_type(&e.full_name);
        self.enum_tys.insert(eid, st);
        let mut max = 0u64;
        for v in 0..e.variants.len() {
            let vt = self.variant_llty(eid, v as u32);
            max = max.max(self.td.get_abi_size(&vt));
        }
        let words = max.div_ceil(8) as u32;
        st.set_body(&[self.ctx.i32_type().into(), self.i64_ty().array_type(words).into()], false);
        st
    }

    /// Payload layout of one enum variant: an anonymous struct of its fields.
    pub(crate) fn variant_llty(&mut self, eid: u32, v: u32) -> StructType<'ctx> {
        let tys: Vec<TypeId> = self.prog.enums[eid as usize].variants[v as usize].fields.iter().map(|f| f.1).collect();
        let fs: Vec<BasicTypeEnum> = tys.into_iter().map(|t| self.llty(t)).collect();
        self.ctx.struct_type(&fs, false)
    }

    /// `sizeof(T)` in bytes (0 for void).
    pub(crate) fn size_of(&mut self, id: TypeId) -> u64 {
        match self.llty_opt(id) {
            None => 0,
            Some(t) => self.td.get_abi_size(&t),
        }
    }

    /// LLVM function type of a direct call to HIR function `f`.
    pub(crate) fn fn_llty(&mut self, f: &crate::hir::Function) -> R<FunctionType<'ctx>> {
        let mut params: Vec<inkwell::types::BasicMetadataTypeEnum> = Vec::new();
        if matches!(f.kind, crate::hir::FuncKind::Lambda { .. }) {
            params.push(self.ptr_ty().into());
        }
        for p in &f.params {
            let t = f.locals[p.0 as usize].ty;
            params.push(self.llty(t).into());
        }
        // An async function returns through its trailing result slot
        // (`CodeGen::coro_res_ty`, ABI.md §10.3).
        let ret = if f.is_async {
            params.push(self.ptr_ty().into());
            None
        } else {
            self.llty_opt(f.ret)
        };
        Ok(match ret {
            None => self.ctx.void_type().fn_type(&params, false),
            Some(r) => r.fn_type(&params, false),
        })
    }

    /// LLVM function type used to call a closure of Zell type `func(..)`:
    /// the environment pointer comes first.
    pub(crate) fn closure_fn_llty(&mut self, fty: TypeId) -> FunctionType<'ctx> {
        let Type::Func(sig) = self.ty(fty) else {
            return self.ctx.void_type().fn_type(&[], false);
        };
        let sig = sig.clone();
        let mut params: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![self.ptr_ty().into()];
        for p in &sig.params {
            params.push(self.llty(*p).into());
        }
        match self.llty_opt(sig.ret) {
            None => self.ctx.void_type().fn_type(&params, false),
            Some(r) => r.fn_type(&params, false),
        }
    }

    pub(crate) fn is_signed(&self, id: TypeId) -> bool {
        matches!(self.ty(id), Type::Int(k) if k.signed())
    }

    /// Element type of a slice or array.
    pub(crate) fn elem_of(&self, id: TypeId) -> Option<TypeId> {
        match self.ty(id) {
            Type::Slice(t) | Type::Array(t, _) => Some(*t),
            _ => None,
        }
    }

    /// Readable name of a type (diagnostics, type-info names).
    pub(crate) fn type_name(&self, id: TypeId) -> String {
        self.prog.type_name(id)
    }
}
