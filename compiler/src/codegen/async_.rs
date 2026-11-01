//! `async` functions as LLVM switched-resume coroutines (ABI.md §10.3,
//! modelled on `runtime/tests/coro_lowering.ll`).
//!
//! An `async func f(params) T` is `void @f(params, ptr %res)` with
//! `%res -> { ptr task; T value }` ([`CodeGen::coro_res_ty`]; the caller
//! first stores NULL or its own frame handle into `task`). Ramp layout:
//! `entry` (allocas) -> `init` (`coro.id`, `coro.alloc`) -> `coro.alloc`
//! (frame from `zell_coro_frame_alloc`, skipped when LLVM proves the
//! coroutine never suspends) -> `coro.begin` (`coro.begin`, the caller's
//! context and the cancel-hook mark saved by `zell_coro_enter`, then
//! zeroing of locals) -> body.
//! The coroutine makes **no task until its first real suspension**: the
//! body runs as part of the caller's current task, and an `await t` that
//! has to suspend first calls `zell_task_lazy` (which makes the task,
//! attaches it under the current task and makes it current), stores it
//! into `res.task` and registers `zell_coro_resume(hdl)` as a continuation
//! of `t`. `return` goes through the common exit block (defers, release of
//! locals) and then [`CodeGen::coro_return`]: with no task the result is
//! moved into `res.value` under a NULL `res.task` after `zell_coro_leave`
//! restored the caller's context (the synchronous fast path: no task, and
//! no frame unless the body has a suspend point); with a task it is
//! completed as before. Both branch to `cleanup` (frees the frame if one
//! was allocated; there is no final suspend) and `done` (`coro.end`, `ret
//! void`). Callers that await a direct call read `res.value` when
//! `res.task` is NULL ([`CodeGen::await_expr`]); callers that keep the
//! `Task<T>` materialise a completed one ([`CodeGen::coro_res_task`]).
//! Outside a coroutine (C `main`), `await` blocks with `zell_task_await`.
//!
//! **Frame-linked await** (ABI.md 10.3): `await g()` inside a coroutine
//! passes the frame handle in `res.task`, and `g` makes no task: its
//! first suspension takes the link step of [`CodeGen::coro_suspend_begin`]
//! (`coro.task` = awaiter + 1, `res.task = LINKED`) and defers its
//! registration (`zell_coro_defer`); each awaiter above that links as well
//! just suspends, and the last frame of the synchronous chain to suspend
//! publishes the registration (`zell_coro_register`,
//! [`CodeGen::await_linked`]). `g`'s return moves the result into
//! `res.value` and hands the awaiter to `zell_coro_next` (resumed by the
//! job loop); its destroy path hands a panic over as a `task.Panic` error
//! in `res.task`.

use super::{unsupported, CodeGen, Coro, Val, R};
use crate::hir::{self, Callee, Expr, ExprKind, FuncId};
use crate::types::{Type, TypeId};
use inkwell::llvm_sys::core as llc;
use inkwell::llvm_sys::prelude::{LLVMTypeRef, LLVMValueRef};
use inkwell::types::{AsTypeRef, StructType};
use inkwell::values::{AsValueRef, BasicValueEnum, IntValue, PointerValue};
use inkwell::IntPredicate;

/// States of a result slot's `state` word (ABI.md 10.3, `ZELL_LINK_*`).
const LINK_LINKED: u64 = 1;
/// Byte offset of a task's cancel-hook link number (ABI.md 10.2 "Task layout").
const TASK_HOOK_SEQ_OFF: u64 = 144;
/// Byte offsets of a task's state word (int32), panic message (NULL unless
/// it panicked) and result slot, and the state of a done task (ABI.md 10.2
/// "Task layout").
const TASK_STATE_OFF: u64 = 24;
const TASK_PANIC_MSG_OFF: u64 = 200;
const TASK_SLOT_OFF: u64 = 224;
const TASK_DONE: u64 = 1;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Calls intrinsic `name` (non-overloaded) with raw arguments.
    fn intrinsic(&mut self, name: &str, args: &mut [LLVMValueRef]) -> LLVMValueRef {
        unsafe {
            // `llvm.coro.size.i64` is overloaded on its result type.
            let (base, mut tys): (&str, Vec<LLVMTypeRef>) = match name.strip_suffix(".i64") {
                Some(b) => (b, vec![self.i64_ty().as_type_ref()]),
                None => (name, vec![]),
            };
            let id = llc::LLVMLookupIntrinsicID(base.as_ptr() as *const _, base.len());
            assert!(id != 0, "codegen: unknown intrinsic {name}");
            let f = llc::LLVMGetIntrinsicDeclaration(self.module.as_mut_ptr(), id, tys.as_mut_ptr(), tys.len());
            let fty = llc::LLVMGlobalGetValueType(f);
            llc::LLVMBuildCall2(self.b.as_mut_ptr(), fty, f, args.as_mut_ptr(), args.len() as u32, c"".as_ptr())
        }
    }

    fn token_none(&self) -> LLVMValueRef {
        unsafe {
            let t: LLVMTypeRef = llc::LLVMTokenTypeInContext(self.ctx.raw());
            llc::LLVMConstNull(t)
        }
    }

    /// The result slot an async call writes: `{ ptr task; T value }` (no
    /// `value` for `Task<void>`). The caller stores the awaiting frame's
    /// handle into `task` (a direct `await` in a coroutine: frame-linked,
    /// ABI.md 10.3) or NULL (the caller keeps the `Task<T>` or blocks).
    /// With NULL the callee leaves its `Task<T>` (+1) there once it
    /// suspended, or NULL with the result (+1) in `value` when it
    /// completed synchronously. Linked, `task` becomes the hand-off state:
    /// NULL (returned synchronously) or `LINK_LINKED` after the call, and
    /// once the callee ended `LINK_LINKED` (returned, the result moved
    /// into `value`) or the `task.Panic` error (+1) of its panic.
    pub(crate) fn coro_res_ty(&mut self, ret: TypeId) -> StructType<'ctx> {
        let mut fs: Vec<inkwell::types::BasicTypeEnum<'ctx>> = vec![self.ptr_ty().into()];
        if let Some(t) = self.llty_opt(ret) {
            fs.push(t);
        }
        self.ctx.struct_type(&fs, false)
    }

    /// Stores the caller's part of a result slot before the call: the
    /// awaiting frame's handle, or NULL.
    pub(crate) fn coro_res_init(&mut self, ret: TypeId, res: PointerValue<'ctx>, awaiter: Option<PointerValue<'ctx>>) {
        let wp = self.coro_res_field(ret, res, 0);
        self.b.build_store(wp, awaiter.unwrap_or(self.ptr_ty().const_null())).unwrap();
    }

    /// The thread-local `zell_coro_next` (ABI.md 10.3): the awaiter a
    /// linked callee hands over when it ends, resumed by the job loop.
    fn coro_next_global(&mut self) -> PointerValue<'ctx> {
        let g = match self.module.get_global("zell_coro_next") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(self.ptr_ty(), None, "zell_coro_next");
                g.set_thread_local(true);
                g.set_linkage(inkwell::module::Linkage::External);
                g
            }
        };
        g.as_pointer_value()
    }

    /// The thread-local `zell_coro_tls` (ABI.md 10.3): `{ ctx, cur,
    /// orphan_seq, orphan_on }`, read and written by the inlined
    /// `zell_coro_enter` / `zell_coro_leave`.
    fn coro_tls(&mut self, field: u32) -> PointerValue<'ctx> {
        let g = match self.module.get_global("zell_coro_tls") {
            Some(g) => g,
            None => {
                let i32t = self.ctx.i32_type();
                let st = self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.i64_ty().into(), i32t.into(), i32t.into()], false);
                let g = self.module.add_global(st, None, "zell_coro_tls");
                g.set_thread_local(true);
                g.set_linkage(inkwell::module::Linkage::External);
                g
            }
        };
        let off = [0u64, 8, 16, 24][field as usize];
        self.byte_gep(g.as_pointer_value(), off)
    }

    /// `zell_coro_enter(mark)` inline (ABI.md 10.3): the current task's
    /// cancel-hook link number into `mark` (with no task: the thread's
    /// orphan number, and orphan recording on), and the current context,
    /// retained (+1).
    fn coro_enter_inline(&mut self, mark: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let i64t = self.i64_ty();
        let cur = self.coro_tls(1);
        let cur = self.b.build_load(self.ptr_ty(), cur, "cur").unwrap().into_pointer_value();
        let has = self.b.build_is_not_null(cur, "").unwrap();
        let bt = self.block("enter.task");
        let bo = self.block("enter.orphan");
        let join = self.block("enter.ctx");
        self.b.build_conditional_branch(has, bt, bo).unwrap();
        self.b.position_at_end(bt);
        let hp = self.byte_gep(cur, TASK_HOOK_SEQ_OFF);
        let m1 = self.atomic_load(i64t.into(), hp, 8, inkwell::AtomicOrdering::Monotonic, "hook.seq");
        self.b.build_store(mark, m1).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(bo);
        let on = self.coro_tls(3);
        self.b.build_store(on, self.ctx.i32_type().const_int(1, false)).unwrap();
        let sp = self.coro_tls(2);
        let m2 = self.b.build_load(i64t, sp, "orphan.seq").unwrap();
        self.b.build_store(mark, m2).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let cp = self.coro_tls(0);
        let ctx = self.b.build_load(self.ptr_ty(), cp, "ctx").unwrap().into_pointer_value();
        self.call_rt("zell_retain", &[ctx.into()]);
        ctx
    }

    /// An atomic load of `ty` from `p` (aligned to `align` bytes).
    fn atomic_load(&mut self, ty: inkwell::types::BasicTypeEnum<'ctx>, p: PointerValue<'ctx>, align: u32, ord: inkwell::AtomicOrdering, name: &str) -> BasicValueEnum<'ctx> {
        use inkwell::values::BasicValue;
        let v = self.b.build_load(ty, p, name).unwrap();
        let i = v.as_instruction_value().unwrap();
        i.set_alignment(align).unwrap();
        i.set_atomic_ordering(ord).unwrap();
        v
    }

    /// `zell_coro_leave(ctx0)` inline (ABI.md 10.3): `ctx0` (+1, consumed)
    /// is the current context again, the one current until now released.
    pub(crate) fn coro_leave_inline(&mut self, ctx0: PointerValue<'ctx>) {
        let cp = self.coro_tls(0);
        let old = self.b.build_load(self.ptr_ty(), cp, "ctx.old").unwrap();
        self.b.build_store(cp, ctx0).unwrap();
        self.call_rt("zell_release", &[old.into()]);
    }

    /// Splits a `coro.task` slot value: the frame's own task (NULL when it
    /// has none) and its awaiter when the frame is linked (the slot then
    /// holds the awaiter's handle + 1; NULL otherwise).
    fn coro_own_link(&mut self, ts: PointerValue<'ctx>) -> (PointerValue<'ctx>, PointerValue<'ctx>) {
        let i64t = self.i64_ty();
        let null = self.ptr_ty().const_null();
        let bits = self.b.build_ptr_to_int(ts, i64t, "").unwrap();
        let tag = self.b.build_and(bits, i64t.const_int(1, false), "").unwrap();
        let linked = self.b.build_int_compare(IntPredicate::NE, tag, i64t.const_zero(), "linked").unwrap();
        let own = self.b.build_select(linked, null, ts, "own").unwrap().into_pointer_value();
        let aw = unsafe { self.b.build_gep(self.ctx.i8_type(), ts, &[i64t.const_all_ones()], "") }.unwrap();
        let lk = self.b.build_select(linked, aw, null, "lk").unwrap().into_pointer_value();
        (own, lk)
    }

    /// The slot word at `p` as an integer. Plain: the callee wrote it on
    /// this thread (synchronously, or before the cascade that resumed us).
    fn load_word(&mut self, p: PointerValue<'ctx>) -> IntValue<'ctx> {
        self.b.build_load(self.i64_ty(), p, "res.word").unwrap().into_int_value()
    }

    /// Address of `res.task` (`i == 0`) or `res.value` (`i == 1`).
    fn coro_res_field(&mut self, ret: TypeId, res: PointerValue<'ctx>, i: u32) -> PointerValue<'ctx> {
        let st = self.coro_res_ty(ret);
        self.b.build_struct_gep(st, res, i, "").unwrap()
    }

    /// Emits the ramp prologue of async function `f` into the init block.
    pub(crate) fn coro_prologue(&mut self, f: &hir::Function) -> R<()> {
        let body = self.b.get_insert_block().unwrap();
        let init = self.fs().init;
        let func = self.fs().func;
        self.b.position_at_end(init);
        let ret_size = self.i64_ty().const_int(self.size_of(f.ret), false);
        let ret_ops = self.value_ops(f.ret);
        let null = self.ptr_ty().const_null();
        let zero = self.ctx.i32_type().const_zero().as_value_ref();
        let id = self.intrinsic("llvm.coro.id", &mut [zero, null.as_value_ref(), null.as_value_ref(), null.as_value_ref()]);
        // The frame is allocated only when LLVM cannot prove that the
        // coroutine never suspends (`coro.alloc` folds to false then).
        let need = self.intrinsic("llvm.coro.alloc", &mut [id]);
        let alloc = self.block("coro.alloc");
        let begin = self.block("coro.begin");
        self.b.build_conditional_branch(unsafe { IntValue::new(need) }, alloc, begin).unwrap();
        self.b.position_at_end(alloc);
        let sz = self.intrinsic("llvm.coro.size.i64", &mut []);
        let mem = self.call_rt_v("zell_coro_frame_alloc_uninit", &[unsafe { IntValue::new(sz) }.into()]).into_pointer_value();
        self.b.build_unconditional_branch(begin).unwrap();
        self.b.position_at_end(begin);
        // Zeroing stores emitted later by `entry_alloca` follow this code.
        self.fs().init_b.position_at_end(begin);
        let memphi = self.b.build_phi(self.ptr_ty(), "coro.mem").unwrap();
        memphi.add_incoming(&[(&null, init), (&mem, alloc)]);
        let hdl = self.intrinsic("llvm.coro.begin", &mut [id, memphi.as_basic_value().as_value_ref()]);
        let hdl = unsafe { PointerValue::new(hdl) };
        // The coroutine's task (NULL until its first suspension) and the
        // caller's context (+1), put back when the body returns without
        // having suspended.
        let task_slot = self.entry_alloca(self.ptr_ty().into(), "coro.task", false);
        self.b.build_store(task_slot, null).unwrap();
        let ctx_slot = self.entry_alloca(self.ptr_ty().into(), "coro.ctx", false);
        let mark_slot = self.entry_alloca(self.i64_ty().into(), "coro.mark", false);
        let ctx0 = self.coro_enter_inline(mark_slot);
        self.b.build_store(ctx_slot, ctx0).unwrap();
        // The inlined enter ends `begin`: the zeroing stores go after it.
        let after = self.b.get_insert_block().unwrap();
        self.fs().init_b.position_at_end(after);
        let res = func.get_last_param().unwrap().into_pointer_value();
        let begin_end = self.b.get_insert_block().unwrap();
        let cleanup = self.block("coro.cleanup");
        let free = self.block("coro.free");
        let done = self.block("coro.done");
        self.b.position_at_end(cleanup);
        let fr = self.intrinsic("llvm.coro.free", &mut [id, hdl.as_value_ref()]);
        let frv = unsafe { PointerValue::new(fr) };
        let has = self.b.build_is_not_null(frv, "").unwrap();
        self.b.build_conditional_branch(has, free, done).unwrap();
        self.b.position_at_end(free);
        // The frame's size (a constant after CoroSplit) gives its size
        // class without a heap lookup.
        let fsz = self.intrinsic("llvm.coro.size.i64", &mut []);
        self.call_rt("zell_coro_frame_free_sized", &[frv.into(), unsafe { IntValue::new(fsz) }.into()]);
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        let fls = self.ctx.bool_type().const_zero().as_value_ref();
        let none = self.token_none();
        self.intrinsic("llvm.coro.end", &mut [hdl.as_value_ref(), fls, none]);
        self.b.build_return(None).unwrap();
        let unwind = self.block("coro.unwind");
        let awaiting = self.entry_alloca(self.ptr_ty().into(), "coro.awaiting", true);
        // The frame chain (ABI.md 8.1): the node records the frame handle,
        // or NULL when no frame was allocated (no destroy function to call).
        let chain = {
            let g = match self.module.get_global("zell_coro_chain") {
                Some(g) => g,
                None => {
                    let g = self.module.add_global(self.ptr_ty(), None, "zell_coro_chain");
                    g.set_thread_local(true);
                    g.set_linkage(inkwell::module::Linkage::External);
                    g
                }
            };
            let node_ty = self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into()], false);
            let node = self.entry_alloca(node_ty.into(), "coro.node", false);
            self.b.position_at_end(begin_end);
            let has = self.b.build_is_not_null(memphi.as_basic_value().into_pointer_value(), "").unwrap();
            let h = self.b.build_select(has, hdl, null, "").unwrap();
            let hp = self.b.build_struct_gep(node_ty, node, 1, "").unwrap();
            self.b.build_store(hp, h).unwrap();
            Some((node, g.as_pointer_value()))
        };
        self.fs_mut().coro = Some(Coro { hdl, ret: f.ret, ret_size, ret_ops, task_slot, ctx_slot, mark_slot, res, cleanup, done, unwind, chain, awaiting });
        self.b.position_at_end(begin_end);
        self.coro_chain_push();
        self.b.position_at_end(body);
        Ok(())
    }

    /// The prologue of a never-suspending async function lowered as a
    /// plain function (ABI.md 10.3): no frame, no task, no frame chain;
    /// only the caller's context is saved (+1) to be current again at the
    /// exit, so whatever the body set stays with it, as `zell_coro_leave`
    /// does for a coroutine that never suspended. Without `save_ctx` (a
    /// context-neutral body, [`crate::hir::suspend::PlainAsync`]) nothing
    /// is saved.
    pub(crate) fn plain_async_prologue(&mut self, save_ctx: bool) {
        let body = self.b.get_insert_block().unwrap();
        let ctx0 = if save_ctx {
            let init = self.fs().init;
            self.b.position_at_end(init);
            let cp = self.coro_tls(0);
            let ctx0 = self.b.build_load(self.ptr_ty(), cp, "ctx0").unwrap().into_pointer_value();
            self.call_rt("zell_retain", &[ctx0.into()]);
            let after = self.b.get_insert_block().unwrap();
            self.fs().init_b.position_at_end(after);
            Some(ctx0)
        } else {
            None
        };
        let res = self.fs().func.get_last_param().unwrap().into_pointer_value();
        self.fs_mut().plain = Some((ctx0, res));
        self.b.position_at_end(body);
    }

    /// The exit of a plain-lowered async function: the caller's context
    /// again, then a NULL task and the result (moved) into `res`.
    pub(crate) fn plain_async_return(&mut self) {
        let (ctx0, res) = self.fs().plain.unwrap();
        let ret = self.fs().hir.unwrap().ret;
        if let Some(c) = ctx0 {
            self.coro_leave_inline(c);
        }
        let tp = self.coro_res_field(ret, res, 0);
        self.b.build_store(tp, self.ptr_ty().const_null()).unwrap();
        if let Some((slot, t)) = self.fs().ret_slot {
            let lt = self.llty(t);
            let v = self.b.build_load(lt, slot, "").unwrap();
            let vp = self.coro_res_field(ret, res, 1);
            self.b.build_store(vp, v).unwrap();
        }
        self.b.build_return(None).unwrap();
    }

    /// Leaves the coroutine with the result slot: completes the task if the
    /// body suspended, else hands the result to the caller through `res`.
    pub(crate) fn coro_return(&mut self) -> R<()> {
        let co = self.fs().coro.unwrap();
        self.coro_chain_pop();
        let (slot, rt) = match self.fs().ret_slot {
            Some((p, t)) => (p, Some(t)),
            None => (self.ptr_ty().const_null(), None),
        };
        let own = self.b.build_load(self.ptr_ty(), co.task_slot, "coro.own").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(own, "").unwrap();
        let fin_sync = self.block("coro.fin.sync");
        let fin_task = self.block("coro.fin.task");
        let fin_chk = self.block("coro.fin.chk");
        let fin_link = self.block("coro.fin.link");
        self.b.build_conditional_branch(sync, fin_sync, fin_chk).unwrap();
        self.b.position_at_end(fin_chk);
        let (_, lk) = self.coro_own_link(own);
        let linked = self.b.build_is_not_null(lk, "").unwrap();
        self.b.build_conditional_branch(linked, fin_link, fin_task).unwrap();
        // Suspended linked to the awaiter (no task): the result moved into
        // its slot, the awaiter's context again, then DONE; an awaiter that
        // suspended meanwhile is handed to the job loop (zell_coro_next).
        self.b.position_at_end(fin_link);
        if let Some(t) = rt {
            self.settle_fallible_slot(t, slot);
            let lt = self.llty(t);
            let v = self.b.build_load(lt, slot, "").unwrap();
            let vp = self.coro_res_field(co.ret, co.res, 1);
            self.b.build_store(vp, v).unwrap();
        }
        let ctx0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        self.coro_leave_inline(ctx0.into_pointer_value());
        let next = self.coro_next_global();
        self.b.build_store(next, lk).unwrap();
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        // Never suspended: the caller's context again, the result (moved,
        // no retain) and a NULL task into the caller's slot.
        self.b.position_at_end(fin_sync);
        let ctx0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        self.coro_leave_inline(ctx0.into_pointer_value());
        let tp = self.coro_res_field(co.ret, co.res, 0);
        self.b.build_store(tp, self.ptr_ty().const_null()).unwrap();
        if let Some(t) = rt {
            let lt = self.llty(t);
            let v = self.b.build_load(lt, slot, "").unwrap();
            let vp = self.coro_res_field(co.ret, co.res, 1);
            self.b.build_store(vp, v).unwrap();
        }
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        // Suspended at least once: publish the result through the task
        // (which retains it) and drop the frame's reference.
        self.b.position_at_end(fin_task);
        if let Some(t) = rt {
            self.settle_fallible_slot(t, slot);
        }
        self.call_rt("zell_task_complete", &[own.into(), slot.into()]);
        if let Some(t) = rt {
            if self.needs_rc(t) {
                let lt = self.llty(t);
                let v = self.b.build_load(lt, slot, "").unwrap();
                self.release_val(t, v);
            }
        }
        self.call_rt("zell_release", &[own.into()]);
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        Ok(())
    }

    /// Waits for task `t`; returns its (borrowed) result slot.
    fn await_slot(&mut self, t: PointerValue<'ctx>, err_off: Option<u64>) -> PointerValue<'ctx> {
        let Some(co) = self.fs().coro else {
            return match err_off {
                Some(off) => {
                    let o = self.i64_ty().const_int(off, false);
                    self.call_rt_v("zell_task_await_err", &[t.into(), o.into()]).into_pointer_value()
                }
                None => self.call_rt_v("zell_task_await", &[t.into()]).into_pointer_value(),
            };
        };
        let ready = self.block("await.ready");
        let try_ = self.block("await.try");
        let done = self.task_done_inline(t);
        self.b.build_conditional_branch(done, ready, try_).unwrap();
        self.b.position_at_end(try_);
        let (own, kind, ctxr) = self.coro_suspend_begin();
        self.dbga_suspend();
        // Off the thread's frame chain before the frame may resume elsewhere.
        self.coro_chain_pop();
        let repush = self.block("await.repush");
        let sv = self.intrinsic("llvm.coro.save", &mut [co.hdl.as_value_ref()]);
        let resume = self.rt("zell_coro_resume").as_global_value().as_pointer_value();
        let susp = self.block("await.suspend");
        // Linked just now: the registration waits for the frame that
        // suspends last on this stack (zell_coro_register), so nothing can
        // resume this frame before its awaiters have suspended too.
        let i8t = self.ctx.i8_type();
        let now = self.b.build_int_compare(IntPredicate::EQ, kind, i8t.const_int(1, false), "").unwrap();
        let d_b = self.block("await.defer");
        let r_b = self.block("await.register");
        self.b.build_conditional_branch(now, d_b, r_b).unwrap();
        self.b.position_at_end(d_b);
        self.call_rt("zell_coro_defer", &[t.into(), co.hdl.into(), ctxr.into()]);
        self.b.build_unconditional_branch(susp).unwrap();
        // A frame with a task suspends it (the parent is current again); a
        // linked frame registers under the current task, which is not its own.
        self.b.position_at_end(r_b);
        let has_own = self.b.build_is_not_null(own, "").unwrap();
        let r_own = self.block("await.reg.own");
        let r_link = self.block("await.reg.link");
        let r_join = self.block("await.reg");
        self.b.build_conditional_branch(has_own, r_own, r_link).unwrap();
        self.b.position_at_end(r_own);
        let a = self.call_rt_v("zell_task_add_continuation", &[t.into(), resume.into(), co.hdl.into()]).into_int_value();
        self.b.build_unconditional_branch(r_join).unwrap();
        self.b.position_at_end(r_link);
        let b = self.call_rt_v("zell_task_add_continuation_linked", &[t.into(), resume.into(), co.hdl.into()]).into_int_value();
        self.b.build_unconditional_branch(r_join).unwrap();
        self.b.position_at_end(r_join);
        let reg = self.b.build_phi(self.ctx.bool_type(), "reg").unwrap();
        reg.add_incoming(&[(&a, r_own), (&b, r_link)]);
        self.b.build_conditional_branch(reg.as_basic_value().into_int_value(), susp, repush).unwrap();
        self.b.position_at_end(susp);
        let fls = self.ctx.bool_type().const_zero().as_value_ref();
        let s = self.intrinsic("llvm.coro.suspend", &mut [sv, fls]);
        let sv = unsafe { IntValue::new(s) };
        self.b.build_switch(sv, co.done, &[(i8t.const_zero(), repush), (i8t.const_int(1, false), co.unwind)]).unwrap();
        self.b.position_at_end(repush);
        self.coro_chain_push();
        self.b.build_unconditional_branch(ready).unwrap();
        self.b.position_at_end(ready);
        self.dbga_resume();
        self.task_result_inline(t, err_off)
    }

    /// The first-suspension step of an `await` that has to suspend (ABI.md
    /// 10.3): a frame whose caller put its handle into `res.task` links to
    /// it (`coro.task` = handle + 1, `res.task = LINKED`, no task; the
    /// caller's context, retained, is to be made current once the
    /// suspension is published), any other frame makes its task
    /// (`zell_task_lazy`, published in `res.task`). Returns the frame's
    /// task (NULL when linked), the step taken now (0: not the first
    /// suspension, 1: linked, 2: task made) and the retained context of
    /// step 1 (NULL otherwise).
    fn coro_suspend_begin(&mut self) -> (PointerValue<'ctx>, IntValue<'ctx>, PointerValue<'ctx>) {
        let co = self.fs().coro.unwrap();
        let null = self.ptr_ty().const_null();
        let i8t = self.ctx.i8_type();
        let ts = self.b.build_load(self.ptr_ty(), co.task_slot, "").unwrap().into_pointer_value();
        let first = self.b.build_is_null(ts, "first").unwrap();
        let (own0, _) = self.coro_own_link(ts);
        let from = self.b.get_insert_block().unwrap();
        let fb = self.block("susp.first");
        let fl = self.block("susp.link");
        let fo = self.block("susp.task");
        let join = self.block("susp.join");
        self.b.build_conditional_branch(first, fb, join).unwrap();
        self.b.position_at_end(fb);
        let wp = self.coro_res_field(co.ret, co.res, 0);
        let aw = self.b.build_load(self.ptr_ty(), wp, "awaiter").unwrap().into_pointer_value();
        let has = self.b.build_is_not_null(aw, "").unwrap();
        self.b.build_conditional_branch(has, fl, fo).unwrap();
        self.b.position_at_end(fl);
        let tagged = unsafe { self.b.build_gep(i8t, aw, &[self.i64_ty().const_int(1, false)], "") }.unwrap();
        self.b.build_store(co.task_slot, tagged).unwrap();
        let linked = self.b.build_int_to_ptr(self.i64_ty().const_int(LINK_LINKED, false), self.ptr_ty(), "").unwrap();
        self.b.build_store(wp, linked).unwrap();
        let c0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap().into_pointer_value();
        self.call_rt("zell_retain", &[c0.into()]);
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(fo);
        let c1 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        let mark = self.b.build_load(self.i64_ty(), co.mark_slot, "mark").unwrap();
        let own = self.call_rt_v("zell_task_lazy", &[null.into(), co.ret_size.into(), co.ret_ops.into(), c1.into(), mark.into()]).into_pointer_value();
        self.b.build_store(co.task_slot, own).unwrap();
        self.b.build_store(wp, own).unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let o = self.b.build_phi(self.ptr_ty(), "own").unwrap();
        o.add_incoming(&[(&own0, from), (&null, fl), (&own, fo)]);
        let k = self.b.build_phi(i8t, "step").unwrap();
        k.add_incoming(&[(&i8t.const_zero(), from), (&i8t.const_int(1, false), fl), (&i8t.const_int(2, false), fo)]);
        let c = self.b.build_phi(self.ptr_ty(), "ctxr").unwrap();
        c.add_incoming(&[(&null, from), (&c0, fl), (&null, fo)]);
        (o.as_basic_value().into_pointer_value(), k.as_basic_value().into_int_value(), c.as_basic_value().into_pointer_value())
    }

    /// The `T!` error word's byte offset in a task result of type `t`
    /// (None for other types): a panicked `Task<T!>` yields its error there.
    pub(crate) fn fallible_err_off(&mut self, t: TypeId) -> Option<u64> {
        let Type::Fallible(inner) = self.ty(t).clone() else { return None };
        if *self.ty(inner) == Type::Void {
            return Some(0);
        }
        let st = self.llty(t).into_struct_type();
        self.td.offset_of_element(&st, 1)
    }

    /// `zell_task_is_done(t)` inline (ABI.md 10.2 "Task layout"): an
    /// acquire load of the state word; a nil `t` goes to the runtime call,
    /// which panics.
    fn task_done_inline(&mut self, t: PointerValue<'ctx>) -> IntValue<'ctx> {
        let nil = self.block("await.nil");
        let chk = self.block("await.chk");
        let nn = self.b.build_is_not_null(t, "").unwrap();
        self.b.build_conditional_branch(nn, chk, nil).unwrap();
        self.b.position_at_end(nil);
        self.call_rt("zell_task_is_done", &[t.into()]);
        self.b.build_unreachable().unwrap();
        self.b.position_at_end(chk);
        let sp = self.byte_gep(t, TASK_STATE_OFF);
        let st = self.atomic_load(self.ctx.i32_type().into(), sp, 4, inkwell::AtomicOrdering::Acquire, "task.state").into_int_value();
        self.b.build_int_compare(IntPredicate::EQ, st, self.ctx.i32_type().const_int(TASK_DONE, false), "done").unwrap()
    }

    /// `Mutex.Lock` / `Unlock` (`zell_task_mutex_lock` / `_unlock` on
    /// `&m.state`, ABI.md 10.6) with the uncontended case inline: Lock is
    /// a CAS of the state word 0 -> 1 (acquire) that yields the immortal
    /// `zell_task_done_task`, Unlock a CAS 1 -> 0 (release: locked, no
    /// waiters, no woken waiter, not starving, no guard). A failed CAS (or
    /// a nil pointer, which the runtime reports) takes the runtime call.
    pub(crate) fn mutex_intrinsic(&mut self, x: crate::hir::ExternId, args: &[Expr]) -> R<Option<Val<'ctx>>> {
        let lock = match self.prog.externs[x.0 as usize].symbol.as_str() {
            "zell_task_mutex_lock" => true,
            "zell_task_mutex_unlock" => false,
            _ => return Ok(None),
        };
        if args.len() != 1 {
            return Ok(None);
        }
        let f = self.extern_decl(x);
        let d = self.depth();
        let v = self.operand(&args[0], false)?;
        self.hold(args[0].ty, v);
        let m = v.get().into_pointer_value();
        let i64t = self.i64_ty();
        let cas = self.block("mu.cas");
        let fast = self.block("mu.fast");
        let slow = self.block("mu.slow");
        let join = self.block("mu.done");
        let nn = self.b.build_is_not_null(m, "").unwrap();
        self.b.build_conditional_branch(nn, cas, slow).unwrap();
        self.b.position_at_end(cas);
        let (from, to, ord) = if lock { (0, 1, inkwell::AtomicOrdering::Acquire) } else { (1, 0, inkwell::AtomicOrdering::Release) };
        let r = self.b.build_cmpxchg(m, i64t.const_int(from, false), i64t.const_int(to, false), ord, inkwell::AtomicOrdering::Monotonic).unwrap();
        let ok = self.b.build_extract_value(r, 1, "mu.ok").unwrap().into_int_value();
        self.b.build_conditional_branch(ok, fast, slow).unwrap();
        self.b.position_at_end(fast);
        let done = if lock { Some(self.task_done_global()) } else { None };
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        let rv = self.call_c(f, &[m.into()]);
        let slow_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        self.release_temps_from(d);
        Ok(Some(match (done, rv) {
            (Some(a), Some(b)) => {
                let phi = self.b.build_phi(self.ptr_ty(), "mu.task").unwrap();
                phi.add_incoming(&[(&a, fast), (&b, slow_end)]);
                Val::owned(phi.as_basic_value())
            }
            _ => Val::void(),
        }))
    }

    /// The immortal done `Task<void>` (`zell_task_done_task`, ABI.md 10.2).
    fn task_done_global(&mut self) -> PointerValue<'ctx> {
        let g = match self.module.get_global("zell_task_done_task") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(self.ctx.i8_type(), None, "zell_task_done_task");
                g.set_linkage(inkwell::module::Linkage::External);
                g
            }
        };
        g.as_pointer_value()
    }

    /// The result slot of done task `t` ([`Self::task_result_rt`]) with the
    /// common case inline (ABI.md 10.2 "Task layout"): a done task that did
    /// not panic has its result at `t + TASK_SLOT_OFF`, which is what
    /// `zell_task_result` returns for it (borrowed). Anything else (a
    /// panicked task, `task.Yield`'s sentinel) takes the runtime call.
    fn task_result_inline(&mut self, t: PointerValue<'ctx>, err_off: Option<u64>) -> PointerValue<'ctx> {
        let done = self.task_done_inline(t);
        let pm = self.block("result.chk");
        let fast = self.block("result.fast");
        let slow = self.block("result.slow");
        let join = self.block("result");
        self.b.build_conditional_branch(done, pm, slow).unwrap();
        self.b.position_at_end(pm);
        let mp = self.byte_gep(t, TASK_PANIC_MSG_OFF);
        let msg = self.b.build_load(self.ptr_ty(), mp, "panic.msg").unwrap().into_pointer_value();
        let ok = self.b.build_is_null(msg, "").unwrap();
        self.b.build_conditional_branch(ok, fast, slow).unwrap();
        self.b.position_at_end(fast);
        let p1 = self.byte_gep(t, TASK_SLOT_OFF);
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        let p2 = self.task_result_rt(t, err_off);
        let slow_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let phi = self.b.build_phi(self.ptr_ty(), "result.slot").unwrap();
        phi.add_incoming(&[(&p1, fast), (&p2, slow_end)]);
        phi.as_basic_value().into_pointer_value()
    }

    /// The result slot of done task `t` (`zell_task_result`, or the `_err`
    /// form for a `Task<T!>`).
    fn task_result_rt(&mut self, t: PointerValue<'ctx>, err_off: Option<u64>) -> PointerValue<'ctx> {
        match err_off {
            Some(off) => {
                let o = self.i64_ty().const_int(off, false);
                self.call_rt_v("zell_task_result_err", &[t.into(), o.into()]).into_pointer_value()
            }
            None => self.call_rt_v("zell_task_result", &[t.into()]).into_pointer_value(),
        }
    }

    /// Makes the defer flags, the defer array and every local slot escape
    /// (an empty inline asm taking each address, at the function's entry),
    /// so no store to them is promoted to a register or dropped as dead:
    /// the unwind path then reads their current values from the frame.
    pub(crate) fn coro_escape_state(&mut self, open: &[(PointerValue<'ctx>, *const hir::Expr)], full: bool) {
        let co = self.fs().coro.unwrap();
        // The task slot too: a frame destroyed before its first suspension
        // must read the NULL stored at entry, not a spill only a suspend
        // point writes.
        let mut ptrs: Vec<PointerValue<'ctx>> = vec![co.awaiting, co.task_slot];
        if full {
            ptrs.extend(open.iter().map(|(a, _)| *a));
            if let Some((arr, n)) = self.fs().defers {
                ptrs.push(arr);
                ptrs.push(n);
            }
            for s in self.fs().locals.iter().flatten() {
                ptrs.push(s.ptr);
            }
            for f in self.fs().flags.iter().flatten() {
                ptrs.push(*f);
            }
            for t in &self.fs().temp_slots {
                ptrs.push(t.ptr);
                ptrs.extend(t.flag);
            }
            ptrs.extend(self.fs().scoped.iter().map(|(p, _)| *p));
            ptrs.extend(self.interior_slot_ptrs());
        }
        let fty = self.ctx.void_type().fn_type(&[self.ptr_ty().into()], false);
        let asm = self.ctx.create_inline_asm(fty, String::new(), "r,~{memory}".to_string(), true, false, None, false);
        let init_b = &self.fs().init_b;
        for p in ptrs {
            init_b.build_indirect_call(fty, asm, &[p.into()], "").unwrap();
        }
    }

    /// Records (or clears) the task an `await` owns while it waits.
    fn set_awaiting(&mut self, t: Option<PointerValue<'ctx>>) {
        let Some(co) = self.fs().coro else { return };
        let v = t.unwrap_or(self.ptr_ty().const_null());
        self.b.build_store(co.awaiting, v).unwrap();
    }

    /// Pushes this frame's node on the thread's coroutine chain.
    fn coro_chain_push(&mut self) {
        let Some((node, chain)) = self.fs().coro.and_then(|c| c.chain) else { return };
        let prev = self.b.build_load(self.ptr_ty(), chain, "chain").unwrap();
        self.b.build_store(node, prev).unwrap();
        self.b.build_store(chain, node).unwrap();
    }

    /// Pops this frame's node (the innermost one) off the chain.
    fn coro_chain_pop(&mut self) {
        let Some((node, chain)) = self.fs().coro.and_then(|c| c.chain) else { return };
        let prev = self.b.build_load(self.ptr_ty(), node, "chain.prev").unwrap();
        self.b.build_store(chain, prev).unwrap();
    }

    /// The unwind path (a suspend point's destroy edge, taken when a panic
    /// destroys the frame): the pending defers, then the task completes as
    /// panicked (`zell_coro_unwound`); live locals are not released.
    pub(crate) fn coro_unwind_exit(&mut self) {
        let co = self.fs().coro.unwrap();
        let aw = self.b.build_load(self.ptr_ty(), co.awaiting, "").unwrap();
        self.call_rt("zell_release", &[aw.into()]);
        let ts = self.b.build_load(self.ptr_ty(), co.task_slot, "coro.own").unwrap().into_pointer_value();
        let (own, lk) = self.coro_own_link(ts);
        let ctx0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        let linked = self.b.build_is_not_null(lk, "").unwrap();
        let ul = self.block("coro.unwind.link");
        let uo = self.block("coro.unwind.task");
        self.b.build_conditional_branch(linked, ul, uo).unwrap();
        // Linked: the panic goes to the awaiter through its slot.
        self.b.position_at_end(ul);
        let wp = self.coro_res_field(co.ret, co.res, 0);
        self.call_rt("zell_coro_unwound_linked", &[wp.into(), lk.into(), ctx0.into()]);
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        self.b.position_at_end(uo);
        self.call_rt("zell_coro_unwound", &[own.into(), ctx0.into()]);
        self.b.build_unconditional_branch(co.cleanup).unwrap();
    }

    /// Loads an owned copy of a `T` result from `slot`.
    fn load_result(&mut self, t: TypeId, slot: PointerValue<'ctx>) -> Option<BasicValueEnum<'ctx>> {
        let lt = self.llty_opt(t)?;
        let v = self.b.build_load(lt, slot, "").unwrap();
        self.retain_val(t, v);
        Some(v)
    }

    /// The `Task<T>` (+1) of a direct async call that wrote `res`: the task
    /// it stored, or a completed one holding the moved result.
    pub(crate) fn coro_res_task(&mut self, ret: TypeId, res: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let tp = self.coro_res_field(ret, res, 0);
        let t = self.b.build_load(self.ptr_ty(), tp, "res.task").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(t, "").unwrap();
        let from = self.b.get_insert_block().unwrap();
        let mat = self.block("res.materialize");
        let join = self.block("res.join");
        self.b.build_conditional_branch(sync, mat, join).unwrap();
        self.b.position_at_end(mat);
        let size = self.i64_ty().const_int(self.size_of(ret), false);
        let ops = self.value_ops(ret);
        let vp: PointerValue<'ctx> = if self.llty_opt(ret).is_some() { self.coro_res_field(ret, res, 1) } else { self.ptr_ty().const_null() };
        let c = self.call_rt_v("zell_task_completed", &[size.into(), ops.into(), vp.into()]).into_pointer_value();
        let mat_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let phi = self.b.build_phi(self.ptr_ty(), "task").unwrap();
        phi.add_incoming(&[(&t, from), (&c, mat_end)]);
        phi.as_basic_value().into_pointer_value()
    }

    /// `await f(args)` of an async function: the result straight from the
    /// call's slot when `f` completed synchronously; outside a coroutine
    /// the usual (blocking) await on the task it returned; inside one the
    /// frame-linked protocol of ABI.md 10.3 ([`Self::await_linked`]).
    fn await_direct_call(&mut self, ty: TypeId, fid: FuncId, args: &[Expr]) -> R<Val<'ctx>> {
        if self.is_plain_async(fid.0 as usize) {
            // Never suspends (ABI.md 10.3): `res.task` is NULL, the result
            // is in `res.value` (+1, moved).
            let res = self.call_async_direct(fid, args, None)?;
            return Ok(match self.llty_opt(ty) {
                Some(lt) => {
                    let vp = self.coro_res_field(ty, res, 1);
                    Val::owned(self.b.build_load(lt, vp, "").unwrap())
                }
                None => Val::void(),
            });
        }
        if let Some(co) = self.fs().coro {
            let res = self.call_async_direct(fid, args, Some(co.hdl))?;
            return Ok(self.await_linked(ty, res));
        }
        let res = self.call_async_direct(fid, args, None)?;
        let tp = self.coro_res_field(ty, res, 0);
        let t = self.b.build_load(self.ptr_ty(), tp, "res.task").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(t, "").unwrap();
        let fast = self.block("await.fast");
        let slow = self.block("await.slow");
        let join = self.block("await.join");
        self.b.build_conditional_branch(sync, fast, slow).unwrap();
        self.b.position_at_end(fast);
        let fv = match self.llty_opt(ty) {
            Some(lt) => {
                let vp = self.coro_res_field(ty, res, 1);
                Some(self.b.build_load(lt, vp, "").unwrap())
            }
            None => None,
        };
        let fast_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        let eo = self.fallible_err_off(ty);
        self.set_awaiting(Some(t));
        let slot = self.await_slot(t, eo);
        let sv = self.load_result(ty, slot);
        self.call_rt("zell_release", &[t.into()]);
        self.set_awaiting(None);
        let slow_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        Ok(match (fv, sv) {
            (Some(a), Some(b)) => {
                let phi = self.b.build_phi(a.get_type(), "await.val").unwrap();
                phi.add_incoming(&[(&a, fast_end), (&b, slow_end)]);
                Val::owned(phi.as_basic_value())
            }
            _ => Val::void(),
        })
    }

    /// The awaiter's side of a linked call that wrote `res` (ABI.md 10.3):
    /// a NULL word is a synchronous return; LINKED means the callee
    /// suspended without a task, so this frame suspends too, after its own
    /// first-suspension step: linked as well (the registration the leaf
    /// deferred stays pending for the frame below) or, as the last frame
    /// of the synchronous chain, publishing it (`zell_coro_register`).
    /// Resumed by the callee's end (`zell_coro_next`): the moved result,
    /// or the panic it left in the word (`zell_coro_link_panic`).
    fn await_linked(&mut self, ty: TypeId, res: PointerValue<'ctx>) -> Val<'ctx> {
        let co = self.fs().coro.unwrap();
        let i64t = self.i64_ty();
        let i8t = self.ctx.i8_type();
        let null = self.ptr_ty().const_null();
        let wp = self.coro_res_field(ty, res, 0);
        let st = self.load_word(wp);
        let fast = self.block("await.fast");
        let slow = self.block("await.linked");
        let join = self.block("await.join");
        let sync = self.b.build_int_compare(IntPredicate::EQ, st, i64t.const_zero(), "").unwrap();
        self.b.build_conditional_branch(sync, fast, slow).unwrap();
        self.b.position_at_end(fast);
        let lt = self.llty_opt(ty);
        let fv = lt.map(|lt| {
            let vp = self.coro_res_field(ty, res, 1);
            self.b.build_load(lt, vp, "").unwrap()
        });
        let fast_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        let (own, kind, ctxr) = self.coro_suspend_begin();
        // The debug agent sees a task made just now as current (it may take
        // the record a step was observed under, docs/DEBUGGER.md).
        self.dbga_suspend();
        self.coro_chain_pop();
        let sv = self.intrinsic("llvm.coro.save", &mut [co.hdl.as_value_ref()]);
        let now = self.b.build_int_compare(IntPredicate::EQ, kind, i8t.const_int(1, false), "").unwrap();
        let lb = self.block("link.leave");
        let rb = self.block("link.register");
        let susp = self.block("link.suspend");
        let repush = self.block("link.repush");
        self.b.build_conditional_branch(now, lb, rb).unwrap();
        self.b.position_at_end(lb);
        self.coro_leave_inline(ctxr);
        self.b.build_unconditional_branch(susp).unwrap();
        // Publishes the chain: no frame access from here to the suspend.
        self.b.position_at_end(rb);
        let has_own = self.b.build_is_not_null(own, "").unwrap();
        self.call_rt("zell_coro_register", &[has_own.into()]);
        self.b.build_unconditional_branch(susp).unwrap();
        self.b.position_at_end(susp);
        let fls = self.ctx.bool_type().const_zero().as_value_ref();
        let s = self.intrinsic("llvm.coro.suspend", &mut [sv, fls]);
        let sv = unsafe { IntValue::new(s) };
        self.b.build_switch(sv, co.done, &[(i8t.const_zero(), repush), (i8t.const_int(1, false), co.unwind)]).unwrap();
        self.b.position_at_end(repush);
        self.coro_chain_push();
        self.dbga_resume();
        let w = self.load_word(wp);
        let panicked = self.b.build_int_compare(IntPredicate::NE, w, i64t.const_int(LINK_LINKED, false), "").unwrap();
        let pan = self.block("link.panicked");
        let rd = self.block("link.value");
        self.b.build_conditional_branch(panicked, pan, rd).unwrap();
        // The callee panicked: the word holds its task.Panic error.
        self.b.position_at_end(pan);
        let e = self.b.build_int_to_ptr(w, self.ptr_ty(), "panic.err").unwrap();
        let vp = if lt.is_some() { self.coro_res_field(ty, res, 1) } else { null };
        let size = self.i64_ty().const_int(self.size_of(ty), false);
        let eo = match self.fallible_err_off(ty) {
            Some(o) => self.i64_ty().const_int(o, false),
            None => self.i64_ty().const_all_ones(),
        };
        self.call_rt("zell_coro_link_panic", &[e.into(), vp.into(), size.into(), eo.into()]);
        self.b.build_unconditional_branch(rd).unwrap();
        self.b.position_at_end(rd);
        let sv = lt.map(|lt| {
            let vp = self.coro_res_field(ty, res, 1);
            self.b.build_load(lt, vp, "").unwrap()
        });
        let slow_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        match (fv, sv) {
            (Some(a), Some(b)) => {
                let phi = self.b.build_phi(a.get_type(), "await.val").unwrap();
                phi.add_incoming(&[(&a, fast_end), (&b, slow_end)]);
                Val::owned(phi.as_basic_value())
            }
            _ => Val::void(),
        }
    }

    /// `await t` with result type `ty`.
    pub(crate) fn await_expr(&mut self, ty: TypeId, t: &Expr) -> R<Val<'ctx>> {
        if let ExprKind::TaskAll(tasks) = &t.kind {
            return self.await_all(ty, tasks);
        }
        if let ExprKind::Builtin { op: crate::hir::Builtin::TaskAny, args } = &t.kind {
            return self.await_any(ty, args);
        }
        if let ExprKind::Call { callee: Callee::Func(fid), args } = &t.kind {
            if self.prog.funcs[fid.0 as usize].is_async {
                let mut v = self.await_direct_call(ty, *fid, args)?;
                v.v = v.v.map(|r| self.trace_std_call(*fid, ty, r, t.span.start));
                return Ok(v);
            }
        }
        let tv = self.expr(t)?;
        let tp = tv.get().into_pointer_value();
        let eo = self.fallible_err_off(ty);
        if tv.owned {
            self.set_awaiting(Some(tp));
        }
        let slot = self.await_slot(tp, eo);
        let r = self.load_result(ty, slot);
        self.drop_val(t.ty, tv);
        if tv.owned {
            self.set_awaiting(None);
        }
        Ok(match r {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// `await task.Any(t1, ..., tn)`: `zell_task_any` yields the index of
    /// the first task to complete; its result is copied out (retained).
    fn await_any(&mut self, ty: TypeId, tasks: &[Expr]) -> R<Val<'ctx>> {
        if tasks.len() == 1 && matches!(self.ty(tasks[0].ty), Type::Slice(_)) {
            return self.await_any_slice(ty, &tasks[0]);
        }
        let n = tasks.len() as u32;
        let arr_ty = self.ptr_ty().array_type(n);
        let arr = self.entry_alloca(arr_ty.into(), "any.tasks", false);
        let mut tvs = Vec::new();
        let d = self.depth();
        for (i, t) in tasks.iter().enumerate() {
            let v = self.expr(t)?;
            self.hold(t.ty, v);
            let p = unsafe { self.b.build_in_bounds_gep(arr_ty, arr, &[self.i64_ty().const_zero(), self.i64_ty().const_int(i as u64, false)], "") }.unwrap();
            self.b.build_store(p, v.get()).unwrap();
            tvs.push(v);
        }
        self.forget_from(d);
        let any = self.call_rt_v("zell_task_any", &[arr.into(), self.i64_ty().const_int(n as u64, false).into()]).into_pointer_value();
        let slot = self.await_slot(any, None);
        let idx = self.b.build_load(self.i64_ty(), slot, "any.idx").unwrap().into_int_value();
        self.call_rt("zell_release", &[any.into()]);
        let p = unsafe { self.b.build_in_bounds_gep(arr_ty, arr, &[self.i64_ty().const_zero(), idx], "") }.unwrap();
        let winner = self.b.build_load(self.ptr_ty(), p, "").unwrap().into_pointer_value();
        let eo = self.fallible_err_off(ty);
        let rslot = self.task_result_rt(winner, eo);
        let r = self.load_result(ty, rslot);
        for (t, tv) in tasks.iter().zip(tvs) {
            self.drop_val(t.ty, tv);
        }
        Ok(match r {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// `await task.Any(ts)` over a slice `[]Task<T>`: `zell_task_any` on
    /// the slice's elements (it panics for an empty slice).
    fn await_any_slice(&mut self, ty: TypeId, ts: &Expr) -> R<Val<'ctx>> {
        let d = self.depth();
        let sv = self.expr(ts)?;
        self.hold(ts.ty, sv);
        self.forget_from(d);
        let s = sv.get().into_struct_value();
        let data = self.b.build_extract_value(s, 0, "any.data").unwrap().into_pointer_value();
        let len = self.b.build_extract_value(s, 1, "any.len").unwrap();
        let any = self.call_rt_v("zell_task_any", &[data.into(), len.into()]).into_pointer_value();
        let slot = self.await_slot(any, None);
        let idx = self.b.build_load(self.i64_ty(), slot, "any.idx").unwrap().into_int_value();
        self.call_rt("zell_release", &[any.into()]);
        let p = unsafe { self.b.build_in_bounds_gep(self.ptr_ty(), data, &[idx], "") }.unwrap();
        let winner = self.b.build_load(self.ptr_ty(), p, "").unwrap().into_pointer_value();
        let eo = self.fallible_err_off(ty);
        let rslot = self.task_result_rt(winner, eo);
        let r = self.load_result(ty, rslot);
        self.drop_val(ts.ty, sv);
        Ok(match r {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// `await task.All(t1, ..., tn)`: awaits each task in order and builds
    /// the tuple (or `(..)!`, failing with the first error in order).
    fn await_all(&mut self, ty: TypeId, tasks: &[Expr]) -> R<Val<'ctx>> {
        let mut tvs = Vec::new();
        let d = self.depth();
        for t in tasks {
            let v = self.expr(t)?;
            self.hold(t.ty, v);
            tvs.push(v);
        }
        self.forget_from(d);
        // The first task to fail cancels the pending ones; `winner` is its index.
        let n = tasks.len() as u32;
        let arr_ty = self.ptr_ty().array_type(n);
        let arr = self.entry_alloca(arr_ty.into(), "all.tasks", false);
        let fl_ty = self.ctx.i8_type().array_type(n);
        let fl = self.entry_alloca(fl_ty.into(), "all.fallible", false);
        for (i, (t, tv)) in tasks.iter().zip(&tvs).enumerate() {
            let Type::Task(rt) = self.ty(t.ty).clone() else { return unsupported("task.All over a non-task") };
            let z = self.i64_ty().const_zero();
            let ix = self.i64_ty().const_int(i as u64, false);
            let p = unsafe { self.b.build_in_bounds_gep(arr_ty, arr, &[z, ix], "") }.unwrap();
            self.b.build_store(p, tv.get()).unwrap();
            let q = unsafe { self.b.build_in_bounds_gep(fl_ty, fl, &[z, ix], "") }.unwrap();
            let f = self.fallible_err_off(rt).is_some() as u64;
            self.b.build_store(q, self.ctx.i8_type().const_int(f, false)).unwrap();
        }
        let allt = self.call_rt_v("zell_task_all_cancel", &[arr.into(), self.i64_ty().const_int(n as u64, false).into(), fl.into()]).into_pointer_value();
        let wslot = self.await_slot(allt, None);
        let winner = self.b.build_load(self.i64_ty(), wslot, "all.winner").unwrap().into_int_value();
        self.call_rt("zell_release", &[allt.into()]);
        let mut results = Vec::new();
        let mut wins = Vec::new();
        for (i, (t, tv)) in tasks.iter().zip(&tvs).enumerate() {
            let Type::Task(rt) = self.ty(t.ty).clone() else { return unsupported("task.All over a non-task") };
            wins.push(self.b.build_int_compare(IntPredicate::EQ, winner, self.i64_ty().const_int(i as u64, false), "").unwrap());
            let eo = self.fallible_err_off(rt);
            let slot = self.await_slot(tv.get().into_pointer_value(), eo);
            results.push((rt, self.load_result(rt, slot)));
        }
        for (t, tv) in tasks.iter().zip(tvs) {
            self.drop_val(t.ty, tv);
        }
        let (tuple_ty, fallible) = match self.ty(ty) {
            Type::Fallible(inner) => (*inner, true),
            _ => (ty, false),
        };
        let st = self.llty(tuple_ty).into_struct_type();
        let mut agg: BasicValueEnum = st.const_zero().into();
        let mut err = self.ptr_ty().const_null();
        let mut idx = 0u32;
        let mut errs = Vec::new();
        for (k, (rt, v)) in results.into_iter().enumerate() {
            let Some(v) = v else { continue };
            let (val, e) = match self.ty(rt) {
                Type::Fallible(inner) if fallible => {
                    if *self.ty(*inner) == Type::Void {
                        (None, Some(v.into_pointer_value()))
                    } else {
                        let s = v.into_struct_value();
                        (Some(self.b.build_extract_value(s, 0, "").unwrap()), Some(self.b.build_extract_value(s, 1, "").unwrap().into_pointer_value()))
                    }
                }
                _ => (Some(v), None),
            };
            if let Some(e) = e {
                errs.push((k, e));
            }
            if let Some(val) = val {
                if idx < st.count_fields() {
                    agg = self.b.build_insert_value(agg.into_struct_value(), val, idx, "").unwrap().into_struct_value().into();
                }
                idx += 1;
            }
        }
        if !fallible {
            return Ok(Val::owned(agg));
        }
        // The error of the first task to complete with one wins (the
        // runtime's `winner`); a task's error is released unless chosen.
        for (k, e) in errs {
            let win = wins[k];
            err = self.b.build_select(win, e, err, "").unwrap().into_pointer_value();
            let drop = self.b.build_select(win, self.ptr_ty().const_null(), e, "").unwrap();
            self.call_rt("zell_release", &[drop.into()]);
        }
        // On failure drop the partial values and return {zero, err}.
        let failed = self.b.build_is_not_null(err, "").unwrap();
        let rel = self.block("all.fail");
        let join = self.block("all.join");
        self.b.build_conditional_branch(failed, rel, join).unwrap();
        self.b.position_at_end(rel);
        self.release_val(tuple_ty, agg);
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let z = st.const_zero();
        let tuple = self.b.build_select(failed, BasicValueEnum::from(z), agg, "").unwrap();
        let ft = self.llty(ty).into_struct_type();
        let s = self.b.build_insert_value(ft.get_undef(), tuple, 0, "").unwrap();
        let s = self.b.build_insert_value(s, err, 1, "").unwrap();
        let _ = IntPredicate::EQ;
        Ok(Val::owned(s.into_struct_value().into()))
    }

    /// Attaches the pending trace of the error in the `T!` result `slot`
    /// (type `t`) to the error itself (`trace_caught` with `keep`): the
    /// result is published to a task and may be awaited on another thread,
    /// where the thread-local record is out of reach. No-op for other types.
    fn settle_fallible_slot(&mut self, t: TypeId, slot: PointerValue<'ctx>) {
        if self.opts.no_error_trace {
            return;
        }
        let Type::Fallible(inner) = self.ty(t).clone() else { return };
        if *self.ty(inner) == Type::Void {
            let e = self.b.build_load(self.ptr_ty(), slot, "err").unwrap();
            let e = self.trace_caught(e, None, true);
            self.b.build_store(slot, e).unwrap();
        } else {
            let st = self.llty(t).into_struct_type();
            let v = self.b.build_load(st, slot, "res").unwrap().into_struct_value();
            let e = self.b.build_extract_value(v, 1, "err").unwrap();
            let e = self.trace_caught(e, None, true);
            let v = self.b.build_insert_value(v, e, 1, "").unwrap();
            self.b.build_store(slot, v).unwrap();
        }
    }
}

/// Name of an LLVM value (empty when it has none).
fn value_name(v: LLVMValueRef) -> String {
    let mut len = 0usize;
    let p = unsafe { llc::LLVMGetValueName2(v, &mut len) };
    if p.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p as *const u8, len) }).into_owned()
}

/// The users of `v`.
fn users(v: LLVMValueRef) -> Vec<LLVMValueRef> {
    let mut out = Vec::new();
    unsafe {
        let mut u = llc::LLVMGetFirstUse(v);
        while !u.is_null() {
            out.push(llc::LLVMGetUser(u));
            u = llc::LLVMGetNextUse(u);
        }
    }
    out
}

/// The destroy function a ramp stores into the frame `mem` (the second
/// header word, `store ptr @f.destroy, gep(mem, 0, 1)`), if it can be
/// found among the uses of the allocation.
fn frame_destroy_fn(mem: LLVMValueRef) -> Option<LLVMValueRef> {
    let mut ptrs = vec![mem];
    ptrs.extend(users(mem).into_iter().filter(|u| unsafe { !llc::LLVMIsAGetElementPtrInst(*u).is_null() }));
    for p in ptrs {
        for u in users(p) {
            unsafe {
                if llc::LLVMIsAStoreInst(u).is_null() || llc::LLVMGetOperand(u, 1) != p {
                    continue;
                }
                let v = llc::LLVMGetOperand(u, 0);
                if !llc::LLVMIsAFunction(v).is_null() && value_name(v).ends_with(".destroy") {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Whether destroy function `f` reads the suspend index LLVM keeps in the
/// frame (`%index = load iN, ptr %index.addr`, CoroSplit's names): it then
/// dispatches on a field that only a suspension writes, so a frame destroyed
/// before its first suspension (a panic in the synchronous part) must come
/// zeroed (index 0) for that dispatch to be defined.
fn destroy_reads_index(f: LLVMValueRef) -> bool {
    unsafe {
        let mut bb = llc::LLVMGetFirstBasicBlock(f);
        while !bb.is_null() {
            let mut i = llc::LLVMGetFirstInstruction(bb);
            while !i.is_null() {
                if !llc::LLVMIsALoadInst(i).is_null() && (value_name(i).starts_with("index") || value_name(llc::LLVMGetOperand(i, 0)).starts_with("index.addr")) {
                    return true;
                }
                i = llc::LLVMGetNextInstruction(i);
            }
            bb = llc::LLVMGetNextBasicBlock(bb);
        }
    }
    false
}

/// After optimisation (coroutines split): every frame comes from
/// `zell_coro_frame_alloc_uninit` (ABI.md 10.3: the lowering stores every
/// slot the destroy path reads before it can run), except where the
/// destroy function dispatches on LLVM's suspend index (unoptimised code,
/// or suspend points whose destroy edges reload different spills) or the
/// destroy function cannot be identified: those calls switch to the
/// zeroing `zell_coro_frame_alloc`.
pub fn coro_frame_fixup(module: &inkwell::module::Module) {
    let Some(raw) = module.get_function("zell_coro_frame_alloc_uninit") else { return };
    let calls: Vec<LLVMValueRef> = users(raw.as_value_ref()).into_iter().filter(|u| unsafe { !llc::LLVMIsACallInst(*u).is_null() && llc::LLVMGetCalledValue(*u) == raw.as_value_ref() }).collect();
    let mut zeroed: Option<LLVMValueRef> = None;
    for call in calls {
        if frame_destroy_fn(call).is_some_and(|d| !destroy_reads_index(d)) {
            continue;
        }
        let z = *zeroed.get_or_insert_with(|| match module.get_function("zell_coro_frame_alloc") {
            Some(f) => f.as_value_ref(),
            None => unsafe { llc::LLVMAddFunction(module.as_mut_ptr(), c"zell_coro_frame_alloc".as_ptr(), llc::LLVMGlobalGetValueType(raw.as_value_ref())) },
        });
        unsafe {
            let n = llc::LLVMGetNumOperands(call);
            llc::LLVMSetOperand(call, (n - 1) as u32, z);
        }
    }
}
