; Reference lowering of Zell async/await onto LLVM switched-resume coroutines
; and the Zell runtime (ABI.md 10.3). Built and run by `make test` (needs clang).
;
; Source being modelled:
;
;   async func work(x int) int {
;       await task.Delay(5)          // suspends: the task is made here
;       return x * 2
;   }
;
;   async func outer(x int) int {
;       var a = await work(x)        // awaiting another async func
;       return a + 1
;   }
;
;   async func leaf(x int) int {     // no suspend point: no frame, no task
;       return x + 1
;   }
;
;   async func sum(x int) int {
;       var a = await leaf(x)        // completes synchronously: fast path
;       var b = await work(x)        // suspends
;       return a + b
;   }
;
;   func main() {                    // non-async main: blocking await
;       var t1 = outer(10)
;       var t2 = outer(20)
;       println(await t1)            // 21
;       println(await t2)            // 41
;       println(await sum(1))        // 4
;       println(await leaf(2))       // 3
;   }
;
; and, as the entry of an `async func main` (ABI.md 11), two ramps run by
; zell_task_run_main on a worker: one that suspends (sum(5) = 16, awaited
; with zell_task_await_main) and one that does not (leaf(7) = 8, read
; from res.value).
;
;
; `work` is written the way the compiler emits it (zell_coro_enter /
; zell_coro_leave / zell_task_is_done inline on zell_coro_tls and the task
; layout of ABI.md 10.2, a frame that is not zeroed); the other functions
; use the equivalent runtime calls.
;
; Protocol: `void @f(params, ptr %res)` with %res -> { ptr task; T value }.
; The ramp stores the coroutine's Task<T> (+1) into res.task when the body
; suspends for the first time, or NULL plus the result (+1) into res.value
; when the body returns without ever suspending. The task itself is made by
; zell_task_lazy at the first suspension, not when the call starts. (A
; compiled caller first stores the awaiting frame's handle, or NULL, into
; res.task, which the callee reads at its first suspension: the functions
; below are only called with NULL, so they never read it; the frame-linked
; protocol for a non-NULL handle is modelled at the end of the file.)

declare ptr  @zell_task_lazy(ptr, i64, ptr, ptr, i64)
declare ptr  @zell_coro_enter(ptr)
declare ptr  @zell_task_completed(i64, ptr, ptr)
declare void @zell_coro_leave(ptr)
declare ptr  @zell_task_get_ctx()
declare void @zell_task_complete(ptr, ptr)
declare zeroext i1 @zell_task_is_done(ptr)
declare zeroext i1 @zell_task_add_continuation(ptr, ptr, ptr)
declare ptr  @zell_task_result(ptr)
declare ptr  @zell_task_await(ptr)
declare void @zell_task_run_main(ptr, ptr)
declare ptr  @zell_task_await_main(ptr)
declare ptr  @zell_task_delay(i64)
declare void @zell_coro_resume(ptr)
declare ptr  @zell_coro_frame_alloc(i64)
declare ptr  @zell_coro_frame_alloc_uninit(i64)
@zell_coro_tls = external thread_local global { ptr, ptr, i64, i32, i32 }   ; ctx, cur, orphan_seq, orphan_on
declare void @zell_coro_frame_free(ptr)
declare void @zell_retain(ptr)
declare void @zell_release(ptr)
declare void @zell_println_int(i64)
declare void @zell_runtime_init(i32, ptr)
declare void @zell_runtime_shutdown()

declare token @llvm.coro.id(i32, ptr, ptr, ptr)
declare i1    @llvm.coro.alloc(token)
declare i64   @llvm.coro.size.i64()
declare ptr   @llvm.coro.begin(token, ptr)
declare token @llvm.coro.save(ptr)
declare i8    @llvm.coro.suspend(token, i1)
declare ptr   @llvm.coro.free(token, ptr)
declare void  @llvm.coro.end(ptr, i1, token)

%res.i64 = type { ptr, i64 }     ; { Task<int> task; int value }

; ---------------------------------------------------------------- work(x)
define void @work(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8      ; the coroutine's task: NULL until its first suspension
  %ctx.slot = alloca ptr, align 8       ; the caller's context, +1
  %mark.slot = alloca i64, align 8     ; zell_coro_enter's hook mark
  ; 1. frame on the zell heap, unless LLVM proves the coroutine never suspends.
  ;    Not zeroed: every slot the destroy path reads is stored below first
  ;    (the compiler keeps the zeroing zell_coro_frame_alloc only where the
  ;    destroy function dispatches on LLVM's suspend index, ABI.md 10.3)
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc_uninit(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  ; zell_coro_enter(%mark.slot) inline, as the compiler emits it: the hook
  ; mark of the current task (or the orphan number, recording on), and the
  ; current context, +1
  %cur = load ptr, ptr getelementptr inbounds (i8, ptr @zell_coro_tls, i64 8)
  %has.cur = icmp ne ptr %cur, null
  br i1 %has.cur, label %enter.task, label %enter.orphan

enter.task:
  %seq.p = getelementptr inbounds i8, ptr %cur, i64 144
  %seq = load atomic i64, ptr %seq.p monotonic, align 8
  store i64 %seq, ptr %mark.slot
  br label %enter.ctx

enter.orphan:
  store i32 1, ptr getelementptr inbounds (i8, ptr @zell_coro_tls, i64 24)
  %oseq = load i64, ptr getelementptr inbounds (i8, ptr @zell_coro_tls, i64 16)
  store i64 %oseq, ptr %mark.slot
  br label %enter.ctx

enter.ctx:
  %ctx0 = load ptr, ptr @zell_coro_tls
  call void @zell_retain(ptr %ctx0)
  store ptr %ctx0, ptr %ctx.slot
  ; 2. body: `await task.Delay(5)` (result type void)
  %d = call ptr @zell_task_delay(i64 5)
  %d.st.p = getelementptr inbounds i8, ptr %d, i64 24   ; zell_task_is_done inline: state == DONE
  %d.st = load atomic i32, ptr %d.st.p acquire, align 4
  %d.done = icmp eq i32 %d.st, 1
  br i1 %d.done, label %d.ready, label %d.try

d.try:
  ; the coroutine's own task (made now if this is its first suspension)
  %d.cur = load ptr, ptr %task.slot
  %d.ctx = load ptr, ptr %ctx.slot
  %d.mark = load i64, ptr %mark.slot
  %d.own = call ptr @zell_task_lazy(ptr %d.cur, i64 8, ptr null, ptr %d.ctx, i64 %d.mark)
  store ptr %d.own, ptr %task.slot
  %d.first = icmp eq ptr %d.cur, null
  br i1 %d.first, label %d.publish, label %d.save

d.publish:                                  ; first suspension: hand the task to the caller
  %d.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %d.own, ptr %d.rt
  br label %d.save

d.save:
  %d.sv  = call token @llvm.coro.save(ptr %hdl)
  %d.reg = call i1 @zell_task_add_continuation(ptr %d, ptr @zell_coro_resume, ptr %hdl)
  br i1 %d.reg, label %d.suspend, label %d.ready     ; false: completed meanwhile

d.suspend:
  %d.s = call i8 @llvm.coro.suspend(token %d.sv, i1 false)
  switch i8 %d.s, label %done [i8 0, label %d.ready
                               i8 1, label %cleanup]

d.ready:
  call void @zell_release(ptr %d)
  ; 3. `return x * 2`
  %r = mul i64 %x, 2
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:                                   ; never suspended: result and NULL task to the caller
  %ctx1 = load ptr, ptr %ctx.slot
  %ctx.old = load ptr, ptr @zell_coro_tls      ; zell_coro_leave(%ctx1) inline
  store ptr %ctx1, ptr @zell_coro_tls
  call void @zell_release(ptr %ctx.old)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:                                   ; complete the task, drop the frame's reference
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:                                    ; no final suspend: the frame frees itself
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- outer(x)
define void @outer(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %mark.slot = alloca i64, align 8
  %a.res = alloca %res.i64, align 8       ; result slot of the awaited call
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_coro_enter(ptr %mark.slot)   ; +1, and the hook mark for zell_task_lazy
  store ptr %ctx0, ptr %ctx.slot
  ; `var a = await work(x)`: a NULL task means work completed synchronously
  call void @work(i64 %x, ptr %a.res)
  %w.tp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 0
  %w = load ptr, ptr %w.tp
  %w.sync = icmp eq ptr %w, null
  br i1 %w.sync, label %w.fast, label %w.check

w.fast:
  %w.vp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 1
  %a.f = load i64, ptr %w.vp
  br label %w.join

w.check:
  %w.done = call i1 @zell_task_is_done(ptr %w)
  br i1 %w.done, label %w.ready, label %w.try

w.try:
  %w.cur = load ptr, ptr %task.slot
  %w.ctx = load ptr, ptr %ctx.slot
  %w.mark = load i64, ptr %mark.slot
  %w.own = call ptr @zell_task_lazy(ptr %w.cur, i64 8, ptr null, ptr %w.ctx, i64 %w.mark)
  store ptr %w.own, ptr %task.slot
  %w.first = icmp eq ptr %w.cur, null
  br i1 %w.first, label %w.publish, label %w.save

w.publish:
  %w.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %w.own, ptr %w.rt
  br label %w.save

w.save:
  %w.sv  = call token @llvm.coro.save(ptr %hdl)
  %w.reg = call i1 @zell_task_add_continuation(ptr %w, ptr @zell_coro_resume, ptr %hdl)
  br i1 %w.reg, label %w.suspend, label %w.ready

w.suspend:
  %w.s = call i8 @llvm.coro.suspend(token %w.sv, i1 false)
  switch i8 %w.s, label %done [i8 0, label %w.ready
                               i8 1, label %cleanup]

w.ready:
  %w.slot = call ptr @zell_task_result(ptr %w)   ; borrowed slot
  %a.s = load i64, ptr %w.slot                    ; (retain here if T is refcounted)
  call void @zell_release(ptr %w)
  br label %w.join

w.join:
  %a = phi i64 [ %a.f, %w.fast ], [ %a.s, %w.ready ]
  %r = add i64 %a, 1
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- leaf(x)
; No suspend point: coro.alloc folds to false and coro.free to null, so the
; frame is never allocated and the function is a plain call.
define void @leaf(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  %r = add i64 %x, 1
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- sum(x)
define void @sum(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %mark.slot = alloca i64, align 8
  %a.res = alloca %res.i64, align 8
  %b.res = alloca %res.i64, align 8
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_coro_enter(ptr %mark.slot)
  store ptr %ctx0, ptr %ctx.slot
  ; `var a = await leaf(x)`: always the fast path
  call void @leaf(i64 %x, ptr %a.res)
  %l.tp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 0
  %l = load ptr, ptr %l.tp
  %l.sync = icmp eq ptr %l, null
  br i1 %l.sync, label %l.fast, label %l.check

l.fast:
  %l.vp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 1
  %a.f = load i64, ptr %l.vp
  br label %l.join

l.check:
  %l.done = call i1 @zell_task_is_done(ptr %l)
  br i1 %l.done, label %l.ready, label %l.try

l.try:
  %l.cur = load ptr, ptr %task.slot
  %l.ctx = load ptr, ptr %ctx.slot
  %l.mark = load i64, ptr %mark.slot
  %l.own = call ptr @zell_task_lazy(ptr %l.cur, i64 8, ptr null, ptr %l.ctx, i64 %l.mark)
  store ptr %l.own, ptr %task.slot
  %l.first = icmp eq ptr %l.cur, null
  br i1 %l.first, label %l.publish, label %l.save

l.publish:
  %l.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %l.own, ptr %l.rt
  br label %l.save

l.save:
  %l.sv  = call token @llvm.coro.save(ptr %hdl)
  %l.reg = call i1 @zell_task_add_continuation(ptr %l, ptr @zell_coro_resume, ptr %hdl)
  br i1 %l.reg, label %l.suspend, label %l.ready

l.suspend:
  %l.s = call i8 @llvm.coro.suspend(token %l.sv, i1 false)
  switch i8 %l.s, label %done [i8 0, label %l.ready
                               i8 1, label %cleanup]

l.ready:
  %l.slot = call ptr @zell_task_result(ptr %l)
  %a.s = load i64, ptr %l.slot
  call void @zell_release(ptr %l)
  br label %l.join

l.join:
  %a = phi i64 [ %a.f, %l.fast ], [ %a.s, %l.ready ]
  ; `var b = await work(x)`: suspends
  call void @work(i64 %x, ptr %b.res)
  %w.tp = getelementptr inbounds %res.i64, ptr %b.res, i64 0, i32 0
  %w = load ptr, ptr %w.tp
  %w.sync = icmp eq ptr %w, null
  br i1 %w.sync, label %w.fast, label %w.check

w.fast:
  %w.vp = getelementptr inbounds %res.i64, ptr %b.res, i64 0, i32 1
  %b.f = load i64, ptr %w.vp
  br label %w.join

w.check:
  %w.done = call i1 @zell_task_is_done(ptr %w)
  br i1 %w.done, label %w.ready, label %w.try

w.try:
  %w.cur = load ptr, ptr %task.slot
  %w.ctx = load ptr, ptr %ctx.slot
  %w.mark = load i64, ptr %mark.slot
  %w.own = call ptr @zell_task_lazy(ptr %w.cur, i64 8, ptr null, ptr %w.ctx, i64 %w.mark)
  store ptr %w.own, ptr %task.slot
  %w.first = icmp eq ptr %w.cur, null
  br i1 %w.first, label %w.publish, label %w.save

w.publish:
  %w.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %w.own, ptr %w.rt
  br label %w.save

w.save:
  %w.sv  = call token @llvm.coro.save(ptr %hdl)
  %w.reg = call i1 @zell_task_add_continuation(ptr %w, ptr @zell_coro_resume, ptr %hdl)
  br i1 %w.reg, label %w.suspend, label %w.ready

w.suspend:
  %w.s = call i8 @llvm.coro.suspend(token %w.sv, i1 false)
  switch i8 %w.s, label %done [i8 0, label %w.ready
                               i8 1, label %cleanup]

w.ready:
  %w.slot = call ptr @zell_task_result(ptr %w)
  %b.s = load i64, ptr %w.slot
  call void @zell_release(ptr %w)
  br label %w.join

w.join:
  %b = phi i64 [ %b.f, %w.fast ], [ %b.s, %w.ready ]
  %r = add i64 %a, %b
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- main
define i32 @main(i32 %argc, ptr %argv) {
entry:
  %r1 = alloca %res.i64, align 8
  %r2 = alloca %res.i64, align 8
  %r3 = alloca %res.i64, align 8
  %r4 = alloca %res.i64, align 8
  call void @zell_runtime_init(i32 %argc, ptr %argv)
  ; `var t1 = outer(10)`: a kept task is materialised when the call completed synchronously
  call void @outer(i64 10, ptr %r1)
  %t1.tp = getelementptr inbounds %res.i64, ptr %r1, i64 0, i32 0
  %t1.r = load ptr, ptr %t1.tp
  %t1.n = icmp eq ptr %t1.r, null
  br i1 %t1.n, label %t1.mat, label %t1.ok

t1.mat:
  %t1.vp = getelementptr inbounds %res.i64, ptr %r1, i64 0, i32 1
  %t1.c = call ptr @zell_task_completed(i64 8, ptr null, ptr %t1.vp)
  br label %t1.ok

t1.ok:
  %t1 = phi ptr [ %t1.r, %entry ], [ %t1.c, %t1.mat ]
  call void @outer(i64 20, ptr %r2)
  %t2.tp = getelementptr inbounds %res.i64, ptr %r2, i64 0, i32 0
  %t2.r = load ptr, ptr %t2.tp
  %t2.n = icmp eq ptr %t2.r, null
  br i1 %t2.n, label %t2.mat, label %t2.ok

t2.mat:
  %t2.vp = getelementptr inbounds %res.i64, ptr %r2, i64 0, i32 1
  %t2.c = call ptr @zell_task_completed(i64 8, ptr null, ptr %t2.vp)
  br label %t2.ok

t2.ok:
  %t2 = phi ptr [ %t2.r, %t1.ok ], [ %t2.c, %t2.mat ]
  %p1 = call ptr @zell_task_await(ptr %t1)      ; blocks the main thread
  %v1 = load i64, ptr %p1
  call void @zell_println_int(i64 %v1)
  %p2 = call ptr @zell_task_await(ptr %t2)
  %v2 = load i64, ptr %p2
  call void @zell_println_int(i64 %v2)
  call void @zell_release(ptr %t1)
  call void @zell_release(ptr %t2)
  ; `println(await sum(1))` and `println(await leaf(2))` in non-async code
  call void @sum(i64 1, ptr %r3)
  %s.tp = getelementptr inbounds %res.i64, ptr %r3, i64 0, i32 0
  %s.t = load ptr, ptr %s.tp
  %s.n = icmp eq ptr %s.t, null
  br i1 %s.n, label %s.fast, label %s.slow

s.fast:
  %s.vp = getelementptr inbounds %res.i64, ptr %r3, i64 0, i32 1
  %s.f = load i64, ptr %s.vp
  br label %s.join

s.slow:
  %s.p = call ptr @zell_task_await(ptr %s.t)
  %s.s = load i64, ptr %s.p
  call void @zell_release(ptr %s.t)
  br label %s.join

s.join:
  %s.v = phi i64 [ %s.f, %s.fast ], [ %s.s, %s.slow ]
  call void @zell_println_int(i64 %s.v)
  call void @leaf(i64 2, ptr %r4)
  %l.tp = getelementptr inbounds %res.i64, ptr %r4, i64 0, i32 0
  %l.t = load ptr, ptr %l.tp
  %l.n = icmp eq ptr %l.t, null
  br i1 %l.n, label %l.fast, label %l.slow

l.fast:
  %l.vp = getelementptr inbounds %res.i64, ptr %r4, i64 0, i32 1
  %l.f = load i64, ptr %l.vp
  br label %l.join

l.slow:
  %l.p = call ptr @zell_task_await(ptr %l.t)
  %l.s = load i64, ptr %l.p
  call void @zell_release(ptr %l.t)
  br label %l.join

l.join:
  %l.v = phi i64 [ %l.f, %l.fast ], [ %l.s, %l.slow ]
  call void @zell_println_int(i64 %l.v)
  %m1 = call i64 @async_main(ptr @main_ramp_sum)
  call void @zell_println_int(i64 %m1)
  %m2 = call i64 @async_main(ptr @main_ramp_leaf)
  call void @zell_println_int(i64 %m2)
  %m3 = call i64 @await_lroot()
  call void @zell_println_int(i64 %m3)
  call void @zell_runtime_shutdown()
  ret i32 0
}

; The ramps of two modelled `async func main`s: `void(ptr %res)`.
define void @main_ramp_sum(ptr %res) {
  call void @sum(i64 5, ptr %res)
  ret void
}

define void @main_ramp_leaf(ptr %res) {
  call void @leaf(i64 7, ptr %res)
  ret void
}

; What the compiler emits in C `main` for an `async func main` (ABI.md 11).
define i64 @async_main(ptr %ramp) {
entry:
  %res = alloca %res.i64, align 8
  call void @zell_task_run_main(ptr %ramp, ptr %res)  ; ramp runs on a worker
  %tp = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  %t = load ptr, ptr %tp
  %n = icmp eq ptr %t, null
  br i1 %n, label %fast, label %slow

fast:
  %vp = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %f = load i64, ptr %vp
  br label %join

slow:
  %p = call ptr @zell_task_await_main(ptr %t)
  %s = load i64, ptr %p
  call void @zell_release(ptr %t)
  br label %join

join:
  %v = phi i64 [ %f, %fast ], [ %s, %slow ]
  ret i64 %v
}

; ======================================================================
; Frame-linked awaits (ABI.md 10.3 "Frame-linked await"). Source modelled:
;
;   async func lwork(x int) int {     // always awaited directly: linked
;       if x > 0 {
;           await task.Delay(5)       // suspends: no task, linked to the awaiter
;       }
;       return x * 3
;   }
;
;   async func lboom() int! {         // awaited directly: linked
;       await task.Delay(1)
;       panic("boom")                 // unwound: the panic goes to the awaiter
;   }
;
;   async func lroot(x int) int {     // kept as a Task by main: makes its task
;       var a = await lwork(x)         // linked, suspends
;       var b = await lwork(0)         // linked, completes synchronously
;       var c = try await lboom() catch { 100 }   // linked, panics
;       return a + b + c
;   }
;
;   println(await lroot(7))           // in non-async main: 121
;
; The slot is the same { ptr word; T value }: the caller stores its frame
; handle into `word`; the callee leaves NULL (returned synchronously) or 1
; (LINKED: suspended without a task), and once it ended 1 (the value is in
; the slot) or its task.Panic error. The frame's task slot holds the
; awaiter + 1 once linked. A linked frame defers its registration
; (zell_coro_defer); the frame that suspends last on the stack publishes
; it (zell_coro_register), so the chain links with plain stores. lwork and
; lboom handle only the linked case (their callers always pass an
; awaiter; a NULL word is lowered as in work()); lroot is only kept as a
; task (it makes a task at its first suspension).

%res.lk.e = type { ptr, { i64, ptr } }   ; int!: { value, error }

@zell_coro_next = external thread_local global ptr
@zell_coro_chain = external thread_local global ptr
@boom.msg = private constant [5 x i8] c"boom\00"

declare void @zell_coro_defer(ptr, ptr, ptr)
declare void @zell_coro_register(i1 zeroext)
declare void @zell_coro_unwound(ptr, ptr)
declare void @zell_coro_unwound_linked(ptr, ptr, ptr)
declare void @zell_coro_link_panic(ptr, ptr, i64, i64)
declare void @zell_panic(ptr)
declare void @llvm.trap()

; ---------------------------------------------------------------- lwork(x)
define void @lwork(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ctx.slot = alloca ptr, align 8
  %task.slot = alloca ptr, align 8      ; NULL, or the awaiter + 1 once linked
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  %wait = icmp sgt i64 %x, 0
  br i1 %wait, label %d.wait, label %body

d.wait:
  %d = call ptr @zell_task_delay(i64 5)
  %d.done = call i1 @zell_task_is_done(ptr %d)
  br i1 %d.done, label %d.ready, label %d.try

d.try:                                      ; first suspension: link, make no task
  %aw = load ptr, ptr %res                  ; the awaiter's handle (res.word)
  %aw.tag = getelementptr i8, ptr %aw, i64 1
  store ptr %aw.tag, ptr %task.slot
  store i64 1, ptr %res                     ; LINKED
  %ctxr = load ptr, ptr %ctx.slot
  call void @zell_retain(ptr %ctxr)         ; made current once published
  %d.sv  = call token @llvm.coro.save(ptr %hdl)
  ; deferred: lroot publishes it; ctxr (the caller's context) is current again
  call void @zell_coro_defer(ptr %d, ptr %hdl, ptr %ctxr)
  %d.s = call i8 @llvm.coro.suspend(token %d.sv, i1 false)
  switch i8 %d.s, label %done [i8 0, label %d.ready
                               i8 1, label %cleanup]  ; never destroyed: no panic here

d.ready:
  call void @zell_release(ptr %d)
  br label %body

body:
  %r = mul i64 %x, 3
  %ts = load ptr, ptr %task.slot
  %unlinked = icmp eq ptr %ts, null
  br i1 %unlinked, label %fin.sync, label %fin.link

fin.sync:                                   ; never suspended: NULL word, the value
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  store ptr null, ptr %res
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  store i64 %r, ptr %s.rv
  br label %cleanup

fin.link:                                   ; the result moved, hand the awaiter over
  %lk = getelementptr i8, ptr %ts, i64 -1
  %l.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  store i64 %r, ptr %l.rv                   ; the word stays LINKED
  %ctx2 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx2)     ; the awaiter's context
  store ptr %lk, ptr @zell_coro_next        ; run_job resumes it after this frame is gone
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- lboom()
define void @lboom(ptr %res) presplitcoroutine {
entry:
  %ctx.slot = alloca ptr, align 8
  %task.slot = alloca ptr, align 8
  %node = alloca { ptr, ptr }, align 8      ; this frame on zell_coro_chain (8.1)
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  %n.hdl = getelementptr inbounds { ptr, ptr }, ptr %node, i64 0, i32 1
  store ptr %hdl, ptr %n.hdl
  %chain0 = load ptr, ptr @zell_coro_chain
  store ptr %chain0, ptr %node
  store ptr %node, ptr @zell_coro_chain     ; push
  %d = call ptr @zell_task_delay(i64 1)
  %d.done = call i1 @zell_task_is_done(ptr %d)
  br i1 %d.done, label %d.ready, label %d.try

d.try:
  %aw = load ptr, ptr %res
  %aw.tag = getelementptr i8, ptr %aw, i64 1
  store ptr %aw.tag, ptr %task.slot
  store i64 1, ptr %res
  %ctxr = load ptr, ptr %ctx.slot
  call void @zell_retain(ptr %ctxr)
  %prev.n = load ptr, ptr %node
  store ptr %prev.n, ptr @zell_coro_chain   ; pop before the frame may resume elsewhere
  %d.sv  = call token @llvm.coro.save(ptr %hdl)
  call void @zell_coro_defer(ptr %d, ptr %hdl, ptr %ctxr)
  %d.s = call i8 @llvm.coro.suspend(token %d.sv, i1 false)
  switch i8 %d.s, label %done [i8 0, label %d.repush
                               i8 1, label %unwind]

d.repush:
  %chain1 = load ptr, ptr @zell_coro_chain
  store ptr %chain1, ptr %node
  store ptr %node, ptr @zell_coro_chain
  br label %d.ready

d.ready:
  call void @zell_release(ptr %d)
  call void @zell_panic(ptr @boom.msg)      ; the panic destroys this frame: %unwind
  unreachable

unwind:                                     ; the destroy edge (8.1)
  %u.ctx = load ptr, ptr %ctx.slot
  %u.ts = load ptr, ptr %task.slot
  %u.linked = icmp ne ptr %u.ts, null       ; this frame never makes a task: non-NULL = linked
  br i1 %u.linked, label %unwind.link, label %unwind.sync

unwind.link:                                ; the task.Panic error into the word, ctx0 current
  %u.lk = getelementptr i8, ptr %u.ts, i64 -1
  call void @zell_coro_unwound_linked(ptr %res, ptr %u.lk, ptr %u.ctx)
  br label %cleanup

unwind.sync:                                ; never suspended: part of the caller
  call void @zell_coro_unwound(ptr null, ptr %u.ctx)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- lroot(x)
define void @lroot(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %mark.slot = alloca i64, align 8
  %ra = alloca %res.i64, align 8
  %rb = alloca %res.i64, align 8
  %rc = alloca %res.lk.e, align 8
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_coro_enter(ptr %mark.slot)
  store ptr %ctx0, ptr %ctx.slot
  ; `var a = await lwork(x)`: the frame handle as the slot's word
  store ptr %hdl, ptr %ra
  call void @lwork(i64 %x, ptr %ra)
  %a.st = load i64, ptr %ra
  %a.linked = icmp eq i64 %a.st, 1
  br i1 %a.linked, label %a.try, label %a.ready      ; NULL: synchronous

a.try:                                      ; this frame suspends last: it publishes the chain
  %a.cur = load ptr, ptr %task.slot
  %a.first = icmp eq ptr %a.cur, null
  br i1 %a.first, label %a.mk, label %a.save

a.mk:                                       ; its first suspension: its task, made current
  %a.ctx = load ptr, ptr %ctx.slot
  %a.mark = load i64, ptr %mark.slot
  %a.own = call ptr @zell_task_lazy(ptr null, i64 8, ptr null, ptr %a.ctx, i64 %a.mark)
  store ptr %a.own, ptr %task.slot
  store ptr %a.own, ptr %res                ; the caller gets the task (its word was NULL)
  br label %a.save

a.save:
  %a.sv = call token @llvm.coro.save(ptr %hdl)
  ; the deferred registration, under this frame's task (which stops being
  ; current); no frame access from here to the suspend
  call void @zell_coro_register(i1 zeroext true)
  %a.s = call i8 @llvm.coro.suspend(token %a.sv, i1 false)
  switch i8 %a.s, label %done [i8 0, label %a.ready
                               i8 1, label %cleanup]

a.ready:
  %a.vp = getelementptr inbounds %res.i64, ptr %ra, i64 0, i32 1
  %a = load i64, ptr %a.vp                  ; moved
  ; `var b = await lwork(0)`: the same protocol; lwork(0) never suspends,
  ; so only a NULL word is possible here (a compiler emits the full protocol)
  store ptr %hdl, ptr %rb
  call void @lwork(i64 0, ptr %rb)
  %b.st = load i64, ptr %rb
  %b.sync = icmp eq i64 %b.st, 0
  br i1 %b.sync, label %b.ready, label %b.bad

b.bad:
  call void @llvm.trap()
  unreachable

b.ready:
  %b.vp = getelementptr inbounds %res.i64, ptr %rb, i64 0, i32 1
  %b = load i64, ptr %b.vp
  ; `var c = try await lboom() catch { 100 }`
  store ptr %hdl, ptr %rc
  call void @lboom(ptr %rc)
  %c.st = load i64, ptr %rc
  %c.linked = icmp eq i64 %c.st, 1
  br i1 %c.linked, label %c.try, label %c.ready

c.try:                                      ; this frame suspends last: it publishes the chain
  %c.cur = load ptr, ptr %task.slot
  %c.first = icmp eq ptr %c.cur, null
  br i1 %c.first, label %c.mk, label %c.save

c.mk:                                       ; its first suspension: its task, made current
  %c.ctx = load ptr, ptr %ctx.slot
  %c.mark = load i64, ptr %mark.slot
  %c.own = call ptr @zell_task_lazy(ptr null, i64 8, ptr null, ptr %c.ctx, i64 %c.mark)
  store ptr %c.own, ptr %task.slot
  store ptr %c.own, ptr %res                ; the caller gets the task (its word was NULL)
  br label %c.save

c.save:
  %c.sv = call token @llvm.coro.save(ptr %hdl)
  ; the deferred registration, under this frame's task (which stops being
  ; current); no frame access from here to the suspend
  call void @zell_coro_register(i1 zeroext true)
  %c.s = call i8 @llvm.coro.suspend(token %c.sv, i1 false)
  switch i8 %c.s, label %done [i8 0, label %c.ready
                               i8 1, label %cleanup]

c.ready:
  %c.w = load i64, ptr %rc
  %c.panicked = icmp ugt i64 %c.w, 1        ; not LINKED (nor NULL): the task.Panic error
  br i1 %c.panicked, label %c.panic, label %c.value

c.panic:                                    ; int!: { 0, the error (moved) } into the value
  %c.e = inttoptr i64 %c.w to ptr
  %c.vp0 = getelementptr inbounds %res.lk.e, ptr %rc, i64 0, i32 1
  call void @zell_coro_link_panic(ptr %c.e, ptr %c.vp0, i64 16, i64 8)
  br label %c.value

c.value:
  %c.vp = getelementptr inbounds %res.lk.e, ptr %rc, i64 0, i32 1
  %c.v = load { i64, ptr }, ptr %c.vp       ; moved
  %c.err = extractvalue { i64, ptr } %c.v, 1
  %c.val = extractvalue { i64, ptr } %c.v, 0
  %c.failed = icmp ne ptr %c.err, null
  call void @zell_release(ptr %c.err)       ; the catch drops the error (NULL ok)
  %c = select i1 %c.failed, i64 100, i64 %c.val
  ; `return a + b + c`
  %ab = add i64 %a, %b
  %r = add i64 %ab, %c
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  store ptr null, ptr %res
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  store i64 %r, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  call void @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; `println(await lroot(7))` from non-async code: a NULL word (no awaiter).
define i64 @await_lroot() {
entry:
  %r = alloca %res.i64, align 8
  store ptr null, ptr %r
  call void @lroot(i64 7, ptr %r)
  %t = load ptr, ptr %r
  %n = icmp eq ptr %t, null
  br i1 %n, label %fast, label %slow

fast:
  %vp = getelementptr inbounds %res.i64, ptr %r, i64 0, i32 1
  %f = load i64, ptr %vp
  br label %join

slow:
  %p = call ptr @zell_task_await(ptr %t)
  %s = load i64, ptr %p
  call void @zell_release(ptr %t)
  br label %join

join:
  %v = phi i64 [ %f, %fast ], [ %s, %slow ]
  ret i64 %v
}
