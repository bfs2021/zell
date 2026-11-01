//! The per-thread coroutine frame cache inlined into generated code
//! (ABI.md 10.3 "Frame cache"). After optimisation (coroutines split, frame
//! sizes constant) and [`super::coro_frame_fixup`], every call of
//! `zell_coro_frame_alloc_uninit`, `zell_coro_frame_alloc` and
//! `zell_coro_frame_free_sized` goes through an always-inline wrapper that
//! pops or pushes the thread's LIFO list of the frame's size class and calls
//! the runtime only when the list is empty, full or not enabled (`lim == 0`).
//! The size class and block size fold to constants once inlined.

use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::builder::Builder;
use inkwell::context::ContextRef;
use inkwell::intrinsics::Intrinsic;
use inkwell::llvm_sys::core as llc;
use inkwell::llvm_sys::prelude::LLVMValueRef;
use inkwell::module::{Linkage, Module};
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::TargetMachine;
use inkwell::values::{AsValueRef, FunctionValue, GlobalValue, IntValue, PointerValue};
use inkwell::IntPredicate;

/// `zell_frame_cache` layout: `void *head[36]; uint32_t n[36]; int64_t
/// bytes; uint32_t lim` (runtime/src/mem.c asserts the same offsets).
const N_OFF: u64 = 288;
const BYTES_OFF: u64 = 432;
const LIM_OFF: u64 = 440;
const CACHE_SIZE: u32 = 448;
/// Largest small size class; larger frames always go to the runtime.
const SMALL_MAX: u64 = 16384;
/// Per-class and per-thread byte caps of the cache (mem.c FC_MAX_*).
const MAX_BYTES: u64 = 256 << 10;
const MAX_TOTAL: u64 = 256 << 10;

/// Routes the frame calls of `module` through the inlined cache and inlines
/// them. A module without coroutine frames is left alone.
pub fn inline_frame_cache(module: &Module, machine: &TargetMachine) -> Result<(), String> {
    let get = |n: &str| module.get_function(n).filter(|f| !calls_of(*f).is_empty());
    let (uninit, zeroed, free) = (get("zell_coro_frame_alloc_uninit"), get("zell_coro_frame_alloc"), get("zell_coro_frame_free_sized"));
    if uninit.is_none() && zeroed.is_none() && free.is_none() {
        return Ok(());
    }
    // The calls to route, taken before the wrappers add their own slow-path
    // calls of the same functions.
    let sites: Vec<(FunctionValue, Vec<LLVMValueRef>)> = [uninit, zeroed, free].into_iter().flatten().map(|f| (f, calls_of(f))).collect();
    let ctx = module.get_context();
    let cache = cache_global(module, &ctx);
    let b = ctx.create_builder();
    for (f, calls) in sites {
        let w = if Some(f) == free {
            free_wrapper(module, &ctx, &b, cache, f)
        } else {
            alloc_wrapper(module, &ctx, &b, cache, f, Some(f) == zeroed)
        };
        redirect(&calls, w);
    }
    module
        .run_passes("always-inline,function(instcombine<no-verify-fixpoint>,simplifycfg)", machine, PassBuilderOptions::create())
        .map_err(|e| format!("internal compiler error: frame cache inlining failed: {}", e.to_string()))
}

fn cache_global<'ctx>(module: &Module<'ctx>, ctx: &ContextRef<'ctx>) -> GlobalValue<'ctx> {
    module.get_global("zell_frame_cache").unwrap_or_else(|| {
        let g = module.add_global(ctx.i8_type().array_type(CACHE_SIZE), None, "zell_frame_cache");
        g.set_thread_local(true);
        g.set_linkage(Linkage::External);
        g.set_alignment(8);
        g
    })
}

/// The direct calls of `f`.
fn calls_of(f: FunctionValue) -> Vec<LLVMValueRef> {
    let mut calls = Vec::new();
    unsafe {
        let mut u = llc::LLVMGetFirstUse(f.as_value_ref());
        while !u.is_null() {
            let c = llc::LLVMGetUser(u);
            if !llc::LLVMIsACallInst(c).is_null() && llc::LLVMGetCalledValue(c) == f.as_value_ref() {
                calls.push(c);
            }
            u = llc::LLVMGetNextUse(u);
        }
    }
    calls
}

/// Makes the calls `calls` call `to`.
fn redirect(calls: &[LLVMValueRef], to: FunctionValue) {
    for &c in calls {
        unsafe {
            let n = llc::LLVMGetNumOperands(c);
            llc::LLVMSetOperand(c, (n - 1) as u32, to.as_value_ref());
        }
    }
}

fn internal_inline<'ctx>(module: &Module<'ctx>, ctx: &ContextRef<'ctx>, name: &str, ty: inkwell::types::FunctionType<'ctx>) -> FunctionValue<'ctx> {
    let f = module.add_function(name, ty, Some(Linkage::Internal));
    f.add_attribute(AttributeLoc::Function, ctx.create_enum_attribute(Attribute::get_named_enum_kind_id("alwaysinline"), 0));
    f
}

fn byte_ptr<'ctx>(ctx: &ContextRef<'ctx>, b: &Builder<'ctx>, base: PointerValue<'ctx>, off: IntValue<'ctx>) -> PointerValue<'ctx> {
    unsafe { b.build_gep(ctx.i8_type(), base, &[off], "") }.unwrap()
}

/// The size class of `size` (1 ..= SMALL_MAX) and its block size, as in
/// `zell__hp_cls` / `zell__hp_sizes`: 16-byte steps to 128, then four
/// classes per power of two.
fn class_of<'ctx>(module: &Module<'ctx>, ctx: &ContextRef<'ctx>, b: &Builder<'ctx>, size: IntValue<'ctx>) -> (IntValue<'ctx>, IntValue<'ctx>) {
    let i64t = ctx.i64_type();
    let k = |v: u64| i64t.const_int(v, false);
    let s = b.build_int_sub(size, k(1), "").unwrap();
    let small = b.build_int_compare(IntPredicate::ULT, s, k(128), "").unwrap();
    let c_small = b.build_right_shift(s, k(4), false, "").unwrap();
    let ctlz = Intrinsic::find("llvm.ctlz").unwrap().get_declaration(module, &[i64t.into()]).unwrap();
    let lz = b.build_call(ctlz, &[s.into(), ctx.bool_type().const_zero().into()], "").unwrap().try_as_basic_value().basic().unwrap().into_int_value();
    let hb = b.build_int_sub(k(63), lz, "").unwrap();
    let base = b.build_left_shift(k(1), hb, "").unwrap();
    let sh = b.build_int_sub(hb, k(2), "").unwrap();
    let q = b.build_right_shift(b.build_int_sub(s, base, "").unwrap(), sh, false, "").unwrap();
    let c_big = b.build_int_add(b.build_int_add(k(8), b.build_int_mul(b.build_int_sub(hb, k(7), "").unwrap(), k(4), "").unwrap(), "").unwrap(), q, "").unwrap();
    let c = b.build_select(small, c_small, c_big, "cls").unwrap().into_int_value();
    let sz_small = b.build_int_mul(b.build_int_add(c_small, k(1), "").unwrap(), k(16), "").unwrap();
    let sz_big = b.build_int_add(base, b.build_left_shift(b.build_int_add(q, k(1), "").unwrap(), sh, "").unwrap(), "").unwrap();
    let bsz = b.build_select(small, sz_small, sz_big, "bsz").unwrap().into_int_value();
    (c, bsz)
}

/// `ptr wrapper(i64 size)`: pop a cached block of the size's class, else
/// call `slow` (the runtime allocator the call used). `zero`: the block is
/// cleared like `zell_coro_frame_alloc` does.
fn alloc_wrapper<'ctx>(module: &Module<'ctx>, ctx: &ContextRef<'ctx>, b: &Builder<'ctx>, cache: GlobalValue<'ctx>, slow: FunctionValue<'ctx>, zero: bool) -> FunctionValue<'ctx> {
    let (i64t, i32t, ptr) = (ctx.i64_type(), ctx.i32_type(), ctx.ptr_type(inkwell::AddressSpace::default()));
    let f = internal_inline(module, ctx, if zero { "zell.frame.alloc.zeroed" } else { "zell.frame.alloc" }, ptr.fn_type(&[i64t.into()], false));
    let size = f.get_nth_param(0).unwrap().into_int_value();
    let (entry, cls, pop, call) = (ctx.append_basic_block(f, ""), ctx.append_basic_block(f, ""), ctx.append_basic_block(f, ""), ctx.append_basic_block(f, ""));
    b.position_at_end(entry);
    let big = b.build_int_compare(IntPredicate::UGT, size, i64t.const_int(SMALL_MAX, false), "").unwrap();
    b.build_conditional_branch(big, call, cls).unwrap();
    b.position_at_end(cls);
    let (c, bsz) = class_of(module, ctx, b, size);
    let cp = cache.as_pointer_value();
    let hp = byte_ptr(ctx, b, cp, b.build_int_mul(c, i64t.const_int(8, false), "").unwrap());
    let p = b.build_load(ptr, hp, "").unwrap().into_pointer_value();
    let empty = b.build_is_null(p, "").unwrap();
    b.build_conditional_branch(empty, call, pop).unwrap();
    b.position_at_end(pop);
    let next = b.build_load(ptr, p, "").unwrap();
    b.build_store(hp, next).unwrap();
    let np = byte_ptr(ctx, b, cp, b.build_int_add(i64t.const_int(N_OFF, false), b.build_int_mul(c, i64t.const_int(4, false), "").unwrap(), "").unwrap());
    let n = b.build_load(i32t, np, "").unwrap().into_int_value();
    b.build_store(np, b.build_int_sub(n, i32t.const_int(1, false), "").unwrap()).unwrap();
    let bp = byte_ptr(ctx, b, cp, i64t.const_int(BYTES_OFF, false));
    let bytes = b.build_load(i64t, bp, "").unwrap().into_int_value();
    b.build_store(bp, b.build_int_sub(bytes, bsz, "").unwrap()).unwrap();
    if zero {
        b.build_memset(p, 8, ctx.i8_type().const_zero(), size).unwrap();
    }
    b.build_return(Some(&p)).unwrap();
    b.position_at_end(call);
    let r = b.build_call(slow, &[size.into()], "").unwrap().try_as_basic_value().basic().unwrap();
    b.build_return(Some(&r)).unwrap();
    f
}

/// `void wrapper(ptr p, i64 size)`: push `p` onto its class's list while the
/// caps allow, else call `zell_coro_frame_free_sized`.
fn free_wrapper<'ctx>(module: &Module<'ctx>, ctx: &ContextRef<'ctx>, b: &Builder<'ctx>, cache: GlobalValue<'ctx>, slow: FunctionValue<'ctx>) -> FunctionValue<'ctx> {
    let (i64t, i32t, ptr) = (ctx.i64_type(), ctx.i32_type(), ctx.ptr_type(inkwell::AddressSpace::default()));
    let f = internal_inline(module, ctx, "zell.frame.free", ctx.void_type().fn_type(&[ptr.into(), i64t.into()], false));
    let p = f.get_nth_param(0).unwrap().into_pointer_value();
    let size = f.get_nth_param(1).unwrap().into_int_value();
    let bb = |n: &str| ctx.append_basic_block(f, n);
    let (entry, small, cls, push, call, done) = (bb(""), bb(""), bb(""), bb(""), bb(""), bb(""));
    b.position_at_end(entry);
    b.build_conditional_branch(b.build_is_null(p, "").unwrap(), done, small).unwrap();
    b.position_at_end(small);
    let big = b.build_int_compare(IntPredicate::UGT, size, i64t.const_int(SMALL_MAX, false), "").unwrap();
    b.build_conditional_branch(big, call, cls).unwrap();
    b.position_at_end(cls);
    let (c, bsz) = class_of(module, ctx, b, size);
    let cp = cache.as_pointer_value();
    let np = byte_ptr(ctx, b, cp, b.build_int_add(i64t.const_int(N_OFF, false), b.build_int_mul(c, i64t.const_int(4, false), "").unwrap(), "").unwrap());
    let n = b.build_load(i32t, np, "").unwrap().into_int_value();
    let lim = b.build_load(i32t, byte_ptr(ctx, b, cp, i64t.const_int(LIM_OFF, false)), "").unwrap().into_int_value();
    let bp = byte_ptr(ctx, b, cp, i64t.const_int(BYTES_OFF, false));
    let bytes = b.build_load(i64t, bp, "").unwrap().into_int_value();
    let n1 = b.build_int_add(n, i32t.const_int(1, false), "").unwrap();
    let ok_n = b.build_int_compare(IntPredicate::ULT, n, lim, "").unwrap();
    let cls_bytes = b.build_int_mul(b.build_int_z_extend(n1, i64t, "").unwrap(), bsz, "").unwrap();
    let ok_c = b.build_int_compare(IntPredicate::ULE, cls_bytes, i64t.const_int(MAX_BYTES, false), "").unwrap();
    let total = b.build_int_add(bytes, bsz, "").unwrap();
    let ok_t = b.build_int_compare(IntPredicate::SLE, total, i64t.const_int(MAX_TOTAL, false), "").unwrap();
    let ok = b.build_and(b.build_and(ok_n, ok_c, "").unwrap(), ok_t, "").unwrap();
    b.build_conditional_branch(ok, push, call).unwrap();
    b.position_at_end(push);
    let hp = byte_ptr(ctx, b, cp, b.build_int_mul(c, i64t.const_int(8, false), "").unwrap());
    let head = b.build_load(ptr, hp, "").unwrap();
    b.build_store(p, head).unwrap();
    b.build_store(hp, p).unwrap();
    b.build_store(np, n1).unwrap();
    b.build_store(bp, total).unwrap();
    b.build_unconditional_branch(done).unwrap();
    b.position_at_end(call);
    b.build_call(slow, &[p.into(), size.into()], "").unwrap();
    b.build_unconditional_branch(done).unwrap();
    b.position_at_end(done);
    b.build_return(None).unwrap();
    f
}
