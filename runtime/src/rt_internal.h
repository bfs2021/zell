/* Internal helpers shared between runtime source files. Not part of the ABI. */
#ifndef ZELL_RT_INTERNAL_H
#define ZELL_RT_INTERNAL_H

#include "zell_rt.h"
#include "platform/platform.h"

#include <setjmp.h>

#define ZELL_HIDDEN __attribute__((visibility("hidden")))

ZELL_HIDDEN ZELL_NORETURN void zell__panicf(const char *fmt, ...)
    __attribute__((format(printf, 1, 2)));

/* Allocate a string object with room for len bytes (+NUL); data is zeroed. */
ZELL_HIDDEN zell_string *zell__string_alloc(int64_t len);

/* Format a double the way Zell prints it (shortest round-trip). buf >= 32. */
ZELL_HIDDEN int zell__format_float(char *buf, size_t cap, double v);

/* Encode a rune as UTF-8 into buf (>= 4 bytes); returns number of bytes. */
ZELL_HIDDEN int zell__utf8_encode(char *buf, int32_t r);

/* Scheduler bootstrap (task.c), used by main.c. zell__process_init does
 * the process-wide setup only; the workers start with the first task. */
ZELL_HIDDEN void zell__process_init(void);
ZELL_HIDDEN void zell__sched_start(void);
ZELL_HIDDEN void zell__sched_stop(void);
/* Stops the I/O poller thread (task_io.c); pending waits are dropped. */
ZELL_HIDDEN void zell__io_stop(void);
/* Shutdown hooks, set by the subsystems when they start (so a program that
 * never does I/O does not link their stop code): zell__io_stop (task_io.c)
 * and zell__net_stop (std_net.c). Defined in task.c. */
ZELL_HIDDEN extern void (*zell__io_stop_fn)(void);
ZELL_HIDDEN extern void (*zell__net_stop_fn)(void);
/* Network polling from the scheduler's workers (ABI.md 10.1, 10.9), set
 * by task_io.c when its pollers are shared (zell__plat_poller_shared) and
 * NULL otherwise. Called by task.c on worker threads only. */
typedef struct zell__netpoll {
    /* Out of local work: a poll with timeout 0 of the shards that have
     * readiness, plus due deadlines. The completed waits post their
     * continuations to this worker. Returns whether it completed any. */
    bool (*poll)(void);
    /* q_mu held, by the last worker to go idle: takes the right to block
     * in the hub poller; false when somebody already blocks there. */
    bool (*claim)(void);
    /* Blocks in the hub (claimed) until readiness, the next deadline of a
     * wait or kick(); collects the completions and drops the claim. */
    void (*block)(void);
    /* Completes what block() collected (the worker is no longer idle). */
    void (*finish)(void);
    /* Makes a concurrent or the next block() return (q_mu held is fine). */
    void (*kick)(void);
    /* A worker left the idle state: wakes the I/O watchdog thread if it
     * sleeps without a timeout. */
    void (*awake)(void);
} zell__netpoll;
ZELL_HIDDEN extern const zell__netpoll *zell__netpoll_hooks;  /* atomic */
/* Workers neither parked nor blocked in zell_task_await (task.c): the
 * ones that will poll the network when they run out of work. */
ZELL_HIDDEN int64_t zell__sched_awake(void);
/* Wakes one parked worker unless one is already polling (task.c); false
 * when no worker is parked on q_cv. */
ZELL_HIDDEN bool zell__sched_poke(void);
/* diag (tests): whether a worker is blocked in the hub (task.c); -1 when
 * the I/O pollers are not shared, else the waits the I/O watchdog thread
 * completed so far (task_io.c). */
ZELL_HIDDEN bool zell__sched_hub_parked(void);
ZELL_HIDDEN int64_t zell__io_netpoll_watchdog(void);
/* Debug agent hooks (debug.c), set by zell_dbg_boot. */
ZELL_HIDDEN extern void (*zell__dbg_panic_fn)(const char *msg, int64_t len);
ZELL_HIDDEN extern void (*zell__dbg_error_fn)(const zell_error *err);
/* Runs fn(w) when the current task is cancelled (task.c, ABI.md 10.11). */
ZELL_HIDDEN void zell__cancel_hook(void (*fn)(zell_task *), zell_task *w);
/* The same with a word of data passed to fn (task_io.c: the fd of the wait). */
ZELL_HIDDEN void zell__cancel_hook_data(void (*fn)(zell_task *, uintptr_t), zell_task *w, uintptr_t data);
/* A cancel hook node (task.c, ABI.md 10.11). zell__cancel_hook* allocate
 * one; a channel wait embeds one in its task slot (task_sync.c) and links
 * it with zell__hook_link, so a wait allocates nothing. A linked node holds
 * a reference to w (for an embedded node: to the task it lives in), so it
 * stays valid until the owner prunes it (w done, on a later link), the
 * owner is dropped, or zell_task_cancel takes it; completing w never
 * touches the owner. The list is guarded by the owner's lock. */
typedef struct zell__hook {
    union {
        void (*fn)(zell_task *w);
        void (*fn_data)(zell_task *w, uintptr_t data);  /* when has_data */
    };
    zell_task *w;              /* owned while linked */
    struct zell__hook *next;
    uintptr_t data;
    int64_t seq;               /* the owner's link number (zell_coro_enter marks) */
    int32_t embedded;          /* lives inside w: released, never freed */
    int32_t has_data;
} zell__hook;
/* Links the embedded hook h: fn(w) runs when `owner` is cancelled (at once,
 * on this thread, if it already is). Retains w; owner NULL links nothing.
 * h->w must already be w: the wait is queued (and its waker reads h->w
 * without the owner's lock) before it is linked, so it is never written. */
ZELL_HIDDEN void zell__hook_link(zell__hook *h, zell_task *owner, void (*fn)(zell_task *), zell_task *w);
/* Sets fn to run when t itself is the target of zell_task_cancel (task.Any
 * cancelling a pending Recv). For tasks with no parent (waits); set before
 * t is published. */
ZELL_HIDDEN void zell__task_set_cancel_fn(zell_task *t, void (*fn)(zell_task *));
/* Task internals for task_sync.c (channels, mutexes; ABI.md 10.6, 10.7):
 * a plain task whose slot has slot_bytes (>= result_size, so a pending
 * wait keeps its queue links there) with ops applied at slot + ops_off;
 * the slot itself; the task's short spin lock; completion of a task that
 * nobody else has seen, whose result is already in the slot. */
ZELL_HIDDEN zell_task *zell__task_new_slot(int64_t result_size, const zell_value_ops *ops, int64_t ops_off, int64_t slot_bytes);
ZELL_HIDDEN void *zell__task_slot(zell_task *t);
ZELL_HIDDEN int64_t zell__task_result_size(const zell_task *t);
ZELL_HIDDEN void zell__task_lock(zell_task *t);
ZELL_HIDDEN void zell__task_unlock(zell_task *t);
ZELL_HIDDEN void zell__task_mark_done(zell_task *t);
/* The mutex's scheduler hooks (task_sync.c, ABI.md 10.6): posts fn(arg)
 * with no context and no current task, to this worker's run-next slot
 * (`next`) or its ring tail (the global queue off a worker); and whether a
 * contended Lock may spin before parking (a worker, more than one worker,
 * no more threads than cores, nothing else queued on this worker). */
ZELL_HIDDEN void zell__sched_post_plain(zell_resume_fn fn, void *arg, bool next);
ZELL_HIDDEN bool zell__sched_can_spin(void);
/* Stops the resolver pool (std_net.c); called by zell__io_stop. Queued
 * lookups are dropped without completing, like pending I/O waits. */
ZELL_HIDDEN void zell__net_stop(void);
/* task_io.c introspection (tests, diag): the shard owning fd (-1 if
 * none) and the number of poller shards running (0 before the first wait). */
ZELL_HIDDEN int64_t zell__io_shard_of(int64_t fd);
ZELL_HIDDEN int64_t zell__io_shards(void);

/* Arguments registry (main.c). */
ZELL_HIDDEN void zell__io_flush_all(void);

/* Object lifecycle helpers (mem.c, weak.c). */
ZELL_HIDDEN void zell__object_immortalize(zell_object *o);
/* Usable bytes of an object's allocation (header included), and resizing of
 * a uniquely referenced object (it may move; new bytes are not zeroed). */
ZELL_HIDDEN int64_t zell__object_capacity(zell_object *o);
ZELL_HIDDEN zell_object *zell__object_resize(zell_object *o, int64_t size);
/* Like zell_object_new, but the payload (after the header) is NOT zeroed. */
ZELL_HIDDEN zell_object *zell__object_new_uninit(const zell_type_info *type, int64_t size);
/* The object heap (heap.c; the allocation entry points are in heap.h).
 * collect: take back this thread's remotely freed pages; with `force`,
 * also retire its empty pages and give every free page and cached mapping
 * back to the OS (runtime.FreeOSMemory). stats: bytes in use (owned pages,
 * medium and large objects), retained (free but still backed by memory),
 * released (free pages returned to the OS whose address range is kept),
 * returned (cumulative bytes given back to the OS). */
#define ZELL__HP_CLASSES 36
#define ZELL__HP_SMALL_MAX 16384
ZELL_HIDDEN void zell__heap_collect(int force);
/* The memory limit in bytes (0: none) and where it came from (heap.c). */
ZELL_HIDDEN int64_t zell__heap_mem_limit(const char **src);
/* Biased reference counting (rc.c, ABI.md 3.1). thread_enable gives the
 * calling thread an owner slot (workers, the main thread); retain/release
 * of biased and of unowned words while threaded (mem.c dispatches); count:
 * the count a word stands for (negative words unchanged); unique: the
 * caller holds the only reference (in-place updates); try_retain_biased:
 * retain unless the count is zero (weak.c); poll merges the calling
 * thread's queue (pending: it is not empty); park/unpark bracket a
 * blocking wait of a slot thread, during which others may merge its queue
 * (park(1) merges first, so it must not be called with runtime locks held
 * that a destructor could need; park(0) only gives the slot lock away);
 * drain_all merges every queue
 * (shutdown, before the leak report). */
ZELL_HIDDEN void zell__rc_thread_enable(void);
/* Marks the calling thread as one that may own objects (it gets a slot at
 * its first biasing): the main thread (zell_runtime_init) and workers. */
ZELL_HIDDEN void zell__rc_thread_eligible(void);
ZELL_HIDDEN void zell__rc_retain_biased(zell_object *o, int64_t w);
ZELL_HIDDEN void zell__rc_release_biased(zell_object *o, int64_t w);
ZELL_HIDDEN void zell__rc_retain_unowned(zell_object *o, int64_t w);
ZELL_HIDDEN void zell__rc_release_unowned(zell_object *o, int64_t w);
ZELL_HIDDEN int64_t zell__rc_count(int64_t w);
ZELL_HIDDEN int zell__rc_unique(const zell_object *o);
ZELL_HIDDEN int zell__rc_try_retain_biased(zell_object *o);
ZELL_HIDDEN void zell__rc_poll(void);
ZELL_HIDDEN int zell__rc_pending(void);
ZELL_HIDDEN void zell__rc_park(int merge);
ZELL_HIDDEN void zell__rc_unpark(void);
ZELL_HIDDEN void zell__rc_drain_all(void);
/* zell__rc_drain_all once a thread took an owner slot (rc.c), else NULL. */
ZELL_HIDDEN extern void (*zell__rc_drain_fn)(void);
/* Whether zell__heap_collect(0) has work on this thread (idle workers). */
ZELL_HIDDEN int zell__heap_needs_collect(void);
ZELL_HIDDEN void zell__heap_stats(int64_t *in_use, int64_t *retained, int64_t *released, int64_t *returned);
/* Allocate a string object of len bytes; only len and the NUL are set. */
ZELL_HIDDEN zell_string *zell__string_alloc_uninit(int64_t len);
/* The byte written at data[len] of a new owned string: '\0', or a non-NUL
 * byte in the ZELL_STRESS_NONUL test build, which makes every string look
 * like a view to find code that wrongly relies on the terminator. */
#ifdef ZELL_STRESS_NONUL
#define ZELL__STR_END '~'
#else
#define ZELL__STR_END '\0'
#endif
/* Views (string.c, ABI.md 4.1). */
typedef struct zell__sview { zell_string s; zell_object *owner; } zell__sview;
typedef struct zell__vblock {
    zell_object hdr;
    zell_object *owner;       /* bytes owner (+1), NULL for static bytes */
    int64_t n, cap;            /* views made / room */
    zell_string v[];
} zell__vblock;
/* s[lo:hi] (bounds already checked): a standalone view, or a copy when the
 * piece is short or s cannot be viewed. */
ZELL_HIDDEN zell_string *zell__string_sub(const zell_string *s, int64_t lo, int64_t hi);
/* A block for up to cap views of parent's bytes (+1 to the caller), or
 * NULL when parent cannot be viewed; then _add copies. _add returns a +1
 * string of p[0..n) (NULL for n == 0; p must lie in parent's bytes); _done
 * drops the builder's reference. A block must not be shared before _done. */
ZELL_HIDDEN zell__vblock *zell__vblock_new(const zell_string *parent, int64_t cap);
ZELL_HIDDEN zell_string *zell__vblock_add(zell__vblock *b, const char *p, int64_t n);
ZELL_HIDDEN void zell__vblock_done(zell__vblock *b);
/* First occurrence of needle[0..m) in hay[0..n), or NULL (m > 0). */
ZELL_HIDDEN const char *zell__memmem(const char *hay, int64_t n, const char *needle, int64_t m);
ZELL_HIDDEN void zell__weak_clear(zell_object *o);
/* The weak table lock, and clearing an object's box with it held (cycles.c). */
ZELL_HIDDEN void zell__weak_lock(void);
ZELL_HIDDEN void zell__weak_unlock(void);
ZELL_HIDDEN void zell__weak_clear_locked(zell_object *o);
/* Cycle collector (cycles.c, ABI.md 3.8). retain/release of an object with
 * ZELL_RC_CYCLIC go here (mem.c); _dirty notes a count change of a MARKED
 * object made by an atomic increment elsewhere (mem.c, weak.c); _flag sets
 * ZELL_RC_CYCLIC on a fresh object unless the collector is off (slice.c
 * backings, map.c maps); _at_exit runs before the leak report: a final
 * collection in auto mode (returns 0), else a dry run counting the leaked
 * objects the collector could free (-1 when it is off). */
/* These are weak references outside cycles.c: a program that never
 * allocates an object of a cyclic type does not link the collector (only
 * flagged objects reach _retain/_release/_dirty, and a flagged object
 * exists only once cycles.c is linked; the others are tested for NULL). */
#define ZELL_CYC_WEAK __attribute__((weak))
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_retain(zell_object *o);
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_release(zell_object *o);
/* A release that never records a possible root (zell_ptr_release_noroot). */
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_release_noroot(zell_object *o);
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_dirty(zell_object *o);
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_flag(zell_object *o);
/* Bytes to allocate for an object of `size` that zell__cycles_flag will
 * flag: room for the collector's hidden word after it (ABI.md 3.8). */
ZELL_HIDDEN ZELL_CYC_WEAK int64_t zell__cycles_extra(int64_t size);
ZELL_HIDDEN ZELL_CYC_WEAK int64_t zell__cycles_at_exit(void);
/* A scheduler worker is about to park (task.c): it publishes its buffered
 * possible roots, which would otherwise wait for its next record. */
ZELL_HIDDEN ZELL_CYC_WEAK void zell__cycles_park(void);
/* Runtime containers (slice.c, map.c): their type infos and visitors. */
ZELL_HIDDEN extern const zell_type_info zell__backing_type;
ZELL_HIDDEN void zell__backing_visit(void *obj, zell_cycle_cb cb, void *ctx);
ZELL_HIDDEN extern const zell_type_info zell__map_type;
ZELL_HIDDEN void zell__map_visit(void *obj, zell_cycle_cb cb, void *ctx);
/* mem.c, for cycles.c: free an object's memory (stats: one live object
 * less), and move one object out of / back into the live count (an object
 * whose Drop ran but whose memory is kept for a while). */
ZELL_HIDDEN void zell__object_free(zell_object *o);
/* ---- bounded release depth (mem.c; ABI.md 3.2 "deferred drop") ----
 *
 * A dead object's drop releases its children, and a child whose count
 * reaches zero is finalized in turn: without a bound, releasing a 2M-node
 * linked list recurses release -> drop -> release ... 2M frames deep and
 * overflows the stack. Every finalizer (the plain release in mem.c, the
 * biased/unowned one in rc.c, the collector's in cycles.c) brackets its
 * `drop` with zell__dead_enter/zell__dead_leave:
 *
 *     if (!zell__dead_enter()) { zell__dead_push(fin, o, w); return; }
 *     ... drop, free ...
 *     zell__dead_leave();
 *
 * Fewer than ZELL_DEAD_DEPTH finalizations nested on this thread run on
 * the stack as before (the shallow common case costs two thread-local
 * loads). The finalizer that would nest deeper is queued instead:
 * `fin(o, w)` is pushed onto a thread-local worklist, which the outermost
 * finalization (depth 1) drains after its own drop. The frame at the
 * maximum depth reverses the entries its children queued, so the stack
 * pops them first-pushed first and the order of Drop() calls is exactly
 * the recursive one: a parent before its children, siblings and their
 * subtrees in field order. */
#define ZELL_DEAD_DEPTH 64
#define ZELL_DEAD_PENDING ((int64_t)1 << 32)   /* the worklist is not empty */
typedef void (*zell__dead_fn)(zell_object *o, int64_t w);
/* The nesting depth (low 32 bits) | ZELL_DEAD_PENDING: one word, so the
 * hot path touches a single thread-local. */
ZELL_HIDDEN extern _Thread_local int64_t zell__dead_state;
/* Queues `fin(o, w)` (mem.c); sets ZELL_DEAD_PENDING. */
ZELL_HIDDEN void zell__dead_push(zell__dead_fn fin, zell_object *o, int64_t w);
/* The last two levels: enters depth ZELL_DEAD_DEPTH remembering the
 * worklist size (the mark of the entries its children will queue), or
 * returns 0 at the bound. */
ZELL_HIDDEN int zell__dead_enter_slow(void);
/* The worklist is not empty as a frame at `depth` leaves: reverses the
 * entries its children queued (depth ZELL_DEAD_DEPTH) or drains the
 * worklist (depth 1). */
ZELL_HIDDEN void zell__dead_settle(int64_t depth);

static inline __attribute__((always_inline)) int zell__dead_enter(void) {
    int64_t s = zell__dead_state;
    if (__builtin_expect((int32_t)s >= ZELL_DEAD_DEPTH - 1, 0)) return zell__dead_enter_slow();
    zell__dead_state = s + 1;
    return 1;
}

static inline __attribute__((always_inline)) void zell__dead_leave(void) {
    int64_t s = zell__dead_state;
    if (__builtin_expect(s & ZELL_DEAD_PENDING, 0)) zell__dead_settle((int32_t)s);
    zell__dead_state = s - 1;
}
/* Per-type live counters for diag (diag_heap.c): on with
 * ZELL_DIAG_HEAP=1; mem.c counts every allocation (+1) and free (-1). */
/* Weak: diag_heap.c is linked only by programs that use diag's heap
 * report, so mem.c tests the flag's address first (ZELL_DIAG_HEAP_ON). */
ZELL_HIDDEN ZELL_CYC_WEAK extern uint8_t zell__diag_heap_on;
ZELL_HIDDEN ZELL_CYC_WEAK void zell__diag_heap_count(const zell_type_info *type, int64_t d);
#define ZELL_DIAG_HEAP_ON() __builtin_expect(&zell__diag_heap_on != NULL && zell__diag_heap_on, 0)
ZELL_HIDDEN void zell__object_uncount(void);
ZELL_HIDDEN void zell__object_recount(void);
ZELL_HIDDEN void zell__leak_report(void);
/* Leak check mode 2: calls cb on every live object (mem.c), and names the
 * pointer fields that close reference cycles among them (leak.c). */
ZELL_HIDDEN void zell__live_each(void (*cb)(zell_object *obj, void *arg), void *arg);
ZELL_HIDDEN void zell__leak_cycles(void);
/* diag (mem.c): objects allocated so far, and a walk over the live
 * objects (type name, allocation size) while leak tracking with type names
 * is on (`--leak-check` mode 2; false otherwise). `cb` must not allocate
 * Zell objects. */
ZELL_HIDDEN int64_t zell__total_objects(void);
ZELL_HIDDEN bool zell__heap_each(void (*cb)(const char *type, int64_t bytes, void *arg), void *arg);
/* diag (task.c): live task objects and jobs waiting in the run queue. */
ZELL_HIDDEN int64_t zell__live_tasks(void);
ZELL_HIDDEN int64_t zell__sched_queue_len(void);
ZELL_HIDDEN int64_t zell__sched_threads_peak(void);   /* diag: most worker threads alive at once since the previous call */
ZELL_HIDDEN int64_t zell__sched_max_threads(void);    /* core workers + the spare limit (ABI.md 10.5) */
ZELL_HIDDEN int64_t zell__await_spurious_wakeups(void);  /* diag: zell_task_await wake-ups that found the task pending */
/* diag (diag.c): releases the process-wide slots before the leak report. */
ZELL_HIDDEN void zell__diag_shutdown(void);
/* Set once the diag slot map was set (so programs without it do not link
 * the release path through zell__diag_shutdown). */
ZELL_HIDDEN extern void (*zell__diag_shutdown_fn)(void);

/* Panic capture scopes (std_testing.c): while a thread's scope is set, a
 * panic on that thread saves its message (malloc'd) and longjmps to `jb`
 * instead of ending the process (panic.c). */
typedef struct zell__panic_capture {
    jmp_buf jb;
    char *msg;
    int64_t len;
} zell__panic_capture;
ZELL_HIDDEN extern _Thread_local zell__panic_capture *zell__panic_capture_tls;

/* Task panics (ABI.md 8.1): every scheduler job runs under an unwind
 * point (task.c run_job). A panic in a job with a current task destroys
 * the coroutine frames on the thread's frame chain (zell_coro_chain, set
 * up by compiled coroutines), which run their pending defers and complete
 * their tasks as panicked, then jumps back to the job's start. */
typedef struct zell__job_unwind {
    zell__plat_jmpbuf jb;              /* zell__plat_setjmp buffer */
    void *chain0;                      /* zell_coro_chain when the job started */
    int resumed;                       /* the job resumes coroutine frames: a panic is delivered to a task or a linked awaiter */
    struct zell__job_unwind *prev;
} zell__job_unwind;
ZELL_HIDDEN extern _Thread_local zell__job_unwind *zell__job_unwind_tls;
/* Whether the calling thread runs code of a task (task.c t_cur != NULL). */
ZELL_HIDDEN int zell__task_running(void);
/* Destroys the frames on the chain down to `stop` (task.c). */
ZELL_HIDDEN void zell__coro_unwind(void *stop);
/* The panic being unwound on this thread (borrowed strings), counted as
 * delivered to a task (panic.c); NULL message when none. */
ZELL_HIDDEN void zell__panic_current(zell_string **msg, zell_string **trace);
/* zell__panic_traced that consumes msg and trace (+1 each, released
 * before the panic goes on: nothing else would). */
ZELL_HIDDEN ZELL_NORETURN void zell__panic_traced_owned(const char *prefix, zell_string *msg, zell_string *trace);
/* Re-panics with `prefix` + msg and the given trace text (task.c, the
 * awaiter of a panicked Task<T>). quiet: no debugger stop (the original
 * site already stopped). */
ZELL_HIDDEN ZELL_NORETURN void zell__panic_traced(const char *prefix, const zell_string *msg, const zell_string *trace, int quiet);
/* Prints a panic nobody observes (a fire-and-forget task's): the usual
 * panic text, and the process goes on. */
ZELL_HIDDEN void zell__panic_print(const zell_string *msg, const zell_string *trace);
/* The `error` an awaiter of a panicked Task<T!> receives (task.Panic,
 * wrapping task.ErrPanic), +1. */
ZELL_HIDDEN zell_error *zell__panic_error(zell_string *msg, zell_string *trace);

/* Debug agent (debug.c, ABI.md 22): the hook flag, the panic and error
 * hooks, and task.c's view of the per-task debug records. */
ZELL_HIDDEN extern uint8_t zell_dbg_on;
ZELL_HIDDEN void zell__dbg_panic(const char *msg, int64_t len);
ZELL_HIDDEN void zell__dbg_error(const zell_error *err);
ZELL_HIDDEN int zell__dbg_error_wanted(void);
ZELL_HIDDEN void zell__dbg_task_drop(int32_t id);
ZELL_HIDDEN zell_task *zell__task_cur(void);
ZELL_HIDDEN int32_t zell__task_dbg_id(const zell_task *t);
ZELL_HIDDEN void zell__task_set_dbg_id(zell_task *t, int32_t id);
ZELL_HIDDEN zell_task *zell__task_parent(zell_task *t);
ZELL_HIDDEN zell_task *zell__task_awaiter(zell_task *t);

#define ZELL_ROUND8(n) (((n) + 7) & ~(int64_t)7)

#endif
