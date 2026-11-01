/* task.c - thread-pool scheduler, tasks, timers, the per-task context slot
 * and small primitives for task (signals, spin locks, yield). Threads, the
 * CPU count and the clock come from the platform layer
 * (platform/platform.h); the I/O poller is in task_io.c, channels and
 * mutexes in task_sync.c. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ======================================================================
 * Scheduler: N workers, each with a local run queue (a ring of LOCALQ
 * jobs pushed by its owner, popped by the owner and stolen in halves by
 * idle workers), a "run next" slot for the continuation a completing task
 * posts (it runs next on the same worker, without any queue), and one
 * global FIFO for posts from other threads (I/O, timers, main) and ring
 * overflow. ABI.md 10.1 describes the policy; the rules are Go's.
 *
 * Idle workers (ABI.md 10.1): an out-of-work worker steals from rings
 * (never run-next slots), then, if it gets the single spinning token
 * (q_spinning), polls the queues for SPIN_NS with cpu_relax between polls
 * (sched_yield only with more threads than CPUs), then parks. Idle
 * workers must not disturb the busy one: a hand-off chain runs on one
 * worker through its run-next slot, and every idle thread that yields or
 * reads that worker's rn_state line in a loop slows it down (ping-pong on
 * 16 workers took 520 ms vs 410 on one while every parker lingered on the
 * slot with sched_yield; 420 with the watchers below). So only two
 * threads look at run-next slots: the spinner, once per pass
 * (watch_runnext: a slot whose rn_state, sequence included, stays the
 * same for RUNNEXT_GRACE_NS is taken), and one parked worker that waits
 * with a timeout (q_watcher, WATCH_MIN_NS doubling to WATCH_MAX_NS) and
 * takes the slot it saw if it is unchanged when it wakes. That is the
 * guarantee for a job that never yields after posting to run-next; the
 * other parks have timeouts too (the sentry, worker_main), for posts that
 * woke nobody because a spinner then took other work.
 *
 * Network polling (netpoll, ABI.md 10.9), when task_io.c installs
 * zell__netpoll_hooks (shared pollers: epoll): readiness is picked up by
 * the workers themselves, as in Go's findrunnable, instead of by poller
 * threads that would post through the global queue and wake a worker.
 *   - find_work: out of local and global work, a worker polls the network
 *     with timeout 0 before stealing; the completed waits post their
 *     continuations to its own ring (t_ring_only: FIFO, no run-next), so
 *     the readiness costs no thread switch. The spinning worker polls too
 *     (every 64 rounds of spin_for_work).
 *   - park: the worker that makes the pool idle (q_idle + q_blocked ==
 *     q_alive) blocks in the hub poller instead of on q_cv, with the next
 *     deadline of an I/O wait as timeout ("one M in netpoll"); it is
 *     q_hub_w. It counts in q_idle like a q_cv sleeper, so the store-then-
 *     load protocol of posters and parkers is unchanged: a poster that sees
 *     q_idle > 0 takes q_mu and, with nobody on q_cv to signal, kicks the
 *     hub (zell__plat_poller_wake through the hook) and reserves the
 *     spinning token for q_hub_w (q_hub_kicked), exactly like a q_cv wake.
 *     The claim on the hub is taken under q_mu before q_hub_w is set, and
 *     only the claim holder waits on the hub, so a kick is never consumed
 *     by another thread's poll: no lost wake-up.
 *   - task_io.c keeps one watchdog thread that blocks in the hub only
 *     when no worker is awake to poll (the I/O started while every worker
 *     slept) and otherwise polls on the workers' behalf when they have not
 *     polled for 10 ms (all of them busy), like Go's sysmon.
 * The spin/park protocol itself (spinning token, run-next watching) is
 * the same with and without netpoll.
 * ====================================================================== */

/* One unit of work: fn(arg) run with the task context `ctx` as task `cur`
 * (both owned references that move with the record; ABI.md 10.8, 10.11). */
typedef struct jobrec {
    zell_resume_fn fn;
    void *arg;
    zell_object *ctx;
    zell_task *cur;
} jobrec;

/* A node of the global queue. */
typedef struct job {
    jobrec r;
    struct job *next;
} job;

#define LOCALQ 256                 /* ring slots per worker (a power of two) */
#define MAX_WORKERS 1024           /* core workers + spares, ever */
#define GLOBAL_EVERY 61            /* ticks between forced looks at the global queue */
#define RUNNEXT_STREAK 32          /* run-next hand-offs in a row before the ring gets a turn */
#define STEAL_PASSES 4             /* rounds over the victims' rings */
#define RUNNEXT_GRACE_NS 20000     /* a run-next slot seen unchanged this long is taken by the spinner */

/* rn_state: the low two bits are the state, the rest a sequence number
 * the owner bumps on every fill, so a watcher can tell a slot that stayed
 * full from one that was refilled (RN_SEQ). */
enum { RN_EMPTY = 0, RN_FULL = 1, RN_BUSY = 2, RN_MASK = 3, RN_SEQ = 4 };
#define WATCH_MIN_NS 20000         /* the parked run-next watcher's first timeout */
#define WATCH_MAX_NS 1000000       /* ... doubled while the slots it sees keep moving, up to this */
#define SENTRY_NS 1000000          /* the parked sentry's timeout while some worker runs */
#define SENTRY_MAX_NS 64000000     /* ... doubled up to this while the whole pool is idle */
#define PARK_NS 10000000           /* the other parked workers' timeout while some worker runs */
#define PARK_MAX_NS 1000000000     /* ... doubled up to this while the whole pool is idle */

typedef struct worker {
    /* The ring: the owner stores records at `tail`; the owner and thieves
     * take them at `head` with a compare-and-swap (a record is read before
     * the swap and only used when the swap succeeds; the owner never
     * overwrites a slot before `head` has passed it). Record fields are
     * accessed with relaxed atomics: a thief whose swap fails may have
     * read a slot the owner was refilling. */
    uint32_t head;                 /* atomic */
    uint32_t tail;                 /* atomic, written by the owner */
    jobrec ring[LOCALQ];
    /* The run-next slot: RN_EMPTY, RN_FULL, or RN_BUSY while someone copies
     * it out. Only the owner fills it. */
    uint32_t rn_state;             /* atomic: RN_* state | sequence */
    jobrec runnext;
    /* Owner-only bookkeeping. */
    uint64_t tick;                 /* jobs taken */
    uint64_t rn_streak;            /* consecutive jobs taken from run-next */
    uint64_t rng;                  /* victim order */
    int spinning;                  /* holds the pool's spinning token */
    int64_t spin_until;            /* monotonic ns: when to give the token up and park */
    int64_t cand;                  /* spinner: the peer whose full run-next slot it follows, or -1 */
    uint32_t cand_val;             /* ... that slot's rn_state when first seen */
    int64_t cand_t;                /* ... monotonic ns of the second sighting (0: not yet) */
    int64_t watch_ns;              /* the watcher's current timeout */
    int64_t sentry_ns;             /* the sentry's current timeout */
    int64_t park_ns;               /* a plain park's current timeout */
    int spare;
    int alive;                     /* under q_mu: the slot is in use */
    int64_t live_tasks;            /* tasks made minus tasks dropped on this thread (diag; summed by zell__live_tasks) */
} worker;

static worker *workers[MAX_WORKERS];   /* slots, allocated once and reused by spares */
static int64_t q_nslots;               /* atomic: slots ever allocated */

static zell__plat_mutex q_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond q_cv = ZELL_PLAT_COND_INIT;     /* work available / stop */
static zell__plat_cond idle_cv = ZELL_PLAT_COND_INIT;  /* drained / thread exited */
static job *q_head, *q_tail;  /* the global queue, under q_mu */
/* q_len, q_idle and q_spinning are read on every post and find_work: each on
 * its own cache line, away from q_mu and the fields parkers write. */
static _Alignas(64) int64_t q_len;  /* atomic: its length (read without q_mu) */
static int q_stopping;
static int q_started;
static int64_t q_nworkers;    /* target parallelism N */
static int64_t q_alive;       /* worker threads alive (N core + spares), under q_mu */
static _Alignas(64) int64_t q_idle; /* atomic: workers parked on q_cv */
static int64_t q_blocked;     /* workers blocked inside zell_task_await, under q_mu */
static int64_t q_max_spares;  /* spare workers allowed at once (ABI.md 10.1, 10.5) */
static int64_t q_ncpu;        /* online CPUs (more worker threads than this: the spinner yields) */
static int64_t q_alive_peak;  /* most worker threads ever alive at once, under q_mu (diag) */
static int64_t q_unblocks;    /* under q_mu: workers that left zell_task_await, ever (stall detection) */
#define STALL_NS 10000000     /* a blocked pool with queued work and no progress for this long gets one more thread */
static _Alignas(64) int64_t q_spinning; /* atomic: workers polling for work before parking (the spinning tokens) */
static _Alignas(64) int64_t q_wake_pending; /* under q_mu: wake-ups sent whose spinning token is not yet taken */
static worker *q_hub_w;        /* under q_mu: the idle worker blocked in the network hub (netpoll), if any */
static int q_hub_kicked;       /* under q_mu: q_hub_w was kicked for a post and inherits the spinning token */
static worker *q_watcher;      /* under q_mu: the parked worker that waits with a timeout for a run-next slot it saw full */

/* How long an out-of-work worker polls the queues before it parks on q_cv
 * (and at most one does at a time). A job posted meanwhile is taken
 * without a futex wake-up of a parked thread: hand-offs between tasks
 * (channels, mutexes, awaits) would otherwise pay a sleep/wake per job. */
#define SPIN_NS 50000
#define SPIN_RELAX 4               /* cpu_relax hints between two polls of the spinner */

#ifdef ZELL_SCHED_STATS
static int64_t st_wake, st_spin, st_spin_found, st_steal, st_linger, st_park, st_recheck, st_jobs_rn, st_jobs_ring, st_jobs_glob, st_rn_kick, st_stall, st_watch;
#define STAT(x) __atomic_add_fetch(&st_##x, 1, __ATOMIC_RELAXED)
/* Cycle profile of the resume path (x86 only): PROF_T(name) accumulates
 * the cycles from the matching PROF_START() in a section. */
static int64_t pr_lazy, pr_addcont, pr_complete, pr_drop, pr_findwork, pr_jobtail;
#if defined(__x86_64__)
#define PROF_START() uint64_t pr_t0_ = __builtin_ia32_rdtsc()
#define PROF_T(x) __atomic_add_fetch(&pr_##x, (int64_t)(__builtin_ia32_rdtsc() - pr_t0_), __ATOMIC_RELAXED)
#else
#define PROF_START() ((void)0)
#define PROF_T(x) ((void)0)
#endif
#else
#define STAT(x) ((void)0)
#define PROF_START() ((void)0)
#define PROF_T(x) ((void)0)
#endif

static _Thread_local int t_is_worker;
static _Thread_local int t_ring_only;  /* posts from this worker skip run-next (netpoll completions: FIFO) */
const zell__netpoll *zell__netpoll_hooks;
static inline const zell__netpoll *netpoll(void) { return __atomic_load_n(&zell__netpoll_hooks, __ATOMIC_ACQUIRE); }
/* Posts from a non-worker thread that batches them (timer_main): the jobs
 * are collected here and pushed to the global queue in one lock round
 * trip by post_batch_flush. NULL on every other thread. */
typedef struct post_batch {
    job *head, *tail;
    int64_t n;
} post_batch;
static _Thread_local post_batch *t_post_batch;
static void post_batch_flush(post_batch *b);
static _Thread_local worker *t_w;   /* this thread's worker, NULL on other threads */

/* This thread's task state (exported: compiled ramps inline
 * zell_coro_enter / zell_coro_leave on it, ABI.md 10.3).
 * t_ctx: the context of the code running on this thread (owned reference,
 * NULL = empty). Jobs carry the context they were posted with; see ABI.md
 * 10.8.
 * t_cur: the task whose code runs on this thread (owned reference, NULL =
 * none): the parent of tasks created now and the target of cancellation
 * hooks. Travels with jobs and continuations like t_ctx; see ABI.md 10.11.
 * t_orphan_on / t_orphan_seq: below (orphan cancel hooks). */
_Thread_local zell_coro_tls_t zell_coro_tls;
_Static_assert(offsetof(zell_coro_tls_t, ctx) == 0 && offsetof(zell_coro_tls_t, cur) == 8 && offsetof(zell_coro_tls_t, orphan_seq) == 16 && offsetof(zell_coro_tls_t, orphan_on) == 24, "ABI.md 10.3: zell_coro_tls layout");
#define t_ctx (zell_coro_tls.ctx)
#define t_cur (zell_coro_tls.cur)
#define t_orphan_seq (zell_coro_tls.orphan_seq)
#define t_orphan_on (zell_coro_tls.orphan_on)
static void orphans_clear(void);   /* cancel hooks with no owner (below) */

/* ---- job records ---- */

static inline void rec_store(jobrec *dst, const jobrec *src) {
    __atomic_store_n(&dst->fn, src->fn, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->arg, src->arg, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->ctx, src->ctx, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->cur, src->cur, __ATOMIC_RELAXED);
}

static inline void rec_load(jobrec *dst, const jobrec *src) {
    dst->fn = __atomic_load_n(&src->fn, __ATOMIC_RELAXED);
    dst->arg = __atomic_load_n(&src->arg, __ATOMIC_RELAXED);
    dst->ctx = __atomic_load_n(&src->ctx, __ATOMIC_RELAXED);
    dst->cur = __atomic_load_n(&src->cur, __ATOMIC_RELAXED);
}

/* ---- the global queue (q_mu held unless said otherwise) ---- */

static void global_push_locked(job *first, job *last, int64_t n) {
    last->next = NULL;
    if (q_tail) q_tail->next = first; else q_head = first;
    q_tail = last;
    __atomic_add_fetch(&q_len, n, __ATOMIC_SEQ_CST);
}

static job *global_pop_locked(void) {
    job *j = q_head;
    if (j) {
        q_head = j->next;
        if (!q_head) q_tail = NULL;
        __atomic_sub_fetch(&q_len, 1, __ATOMIC_SEQ_CST);
    }
    return j;
}

static inline bool global_nonempty(void) { return __atomic_load_n(&q_len, __ATOMIC_SEQ_CST) > 0; }

static job *job_node(const jobrec *r) {
    job *j = malloc(sizeof *j);
    if (!j) zell_panic("out of memory (job)");
    j->r = *r;
    j->next = NULL;
    return j;
}

/* ---- the local ring ---- */

static inline uint32_t ring_len(const worker *w) {
    uint32_t h = __atomic_load_n(&w->head, __ATOMIC_SEQ_CST);
    uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_SEQ_CST);
    return t - h;
}

/* Owner: appends r; false when the ring is full. */
static bool ring_push(worker *w, const jobrec *r) {
    uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
    uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
    if (t - h >= LOCALQ) return false;
    rec_store(&w->ring[t % LOCALQ], r);
    /* Sequentially consistent, like a parking worker's idle count: one of
     * the two sees the other (the wake-up protocol below). */
    __atomic_store_n(&w->tail, t + 1, __ATOMIC_SEQ_CST);
    return true;
}

/* Owner: appends r to a full ring by moving half of the ring and r to the
 * global queue (Go's runqputslow). */
static void ring_push_slow(worker *w, const jobrec *r) {
    for (;;) {
        if (ring_push(w, r)) return;
        uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
        uint32_t n = (t - h) / 2;
        if (n == 0) continue;   /* thieves emptied it meanwhile: the push will succeed */
        jobrec batch[LOCALQ / 2];
        for (uint32_t i = 0; i < n; i++) rec_load(&batch[i], &w->ring[(h + i) % LOCALQ]);
        if (!__atomic_compare_exchange_n(&w->head, &h, h + n, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) continue;
        job *first = job_node(&batch[0]), *last = first;
        for (uint32_t i = 1; i < n; i++) { last->next = job_node(&batch[i]); last = last->next; }
        last->next = job_node(r);
        last = last->next;
        zell__plat_mutex_lock(&q_mu);
        global_push_locked(first, last, (int64_t)n + 1);
        zell__plat_mutex_unlock(&q_mu);
        return;
    }
}

/* Owner or thief: takes the oldest record. */
static bool ring_pop(worker *w, jobrec *out) {
    for (;;) {
        uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_ACQUIRE);
        if (h == t) return false;
        rec_load(out, &w->ring[h % LOCALQ]);
        if (__atomic_compare_exchange_n(&w->head, &h, h + 1, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) return true;
    }
}

/* Thief w (its own ring empty): moves half of v's ring into its ring and
 * returns the last stolen record in *out (Go's runqgrab). */
static bool steal_ring(worker *w, worker *v, jobrec *out) {
    for (;;) {
        uint32_t h = __atomic_load_n(&v->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&v->tail, __ATOMIC_ACQUIRE);
        uint32_t n = t - h;
        n = n - n / 2;
        if (n == 0) return false;
        if (n > LOCALQ / 2) continue;   /* h and t read around a push and pops: inconsistent */
        uint32_t wt = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
        for (uint32_t i = 0; i < n; i++) {
            jobrec r;
            rec_load(&r, &v->ring[(h + i) % LOCALQ]);
            rec_store(&w->ring[(wt + i) % LOCALQ], &r);   /* beyond w's tail: invisible yet */
        }
        if (!__atomic_compare_exchange_n(&v->head, &h, h + n, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) continue;
        rec_load(out, &w->ring[(wt + n - 1) % LOCALQ]);
        if (n > 1) __atomic_store_n(&w->tail, wt + n - 1, __ATOMIC_SEQ_CST);
        return true;
    }
}

/* ---- the run-next slot ---- */

/* Takes w's run-next record if rn_state is still exactly v (full). */
static bool runnext_take_exact(worker *w, uint32_t v, jobrec *out) {
    if ((v & RN_MASK) != RN_FULL) return false;
    if (!__atomic_compare_exchange_n(&w->rn_state, &v, (v & ~(uint32_t)RN_MASK) | RN_BUSY, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) return false;
    rec_load(out, &w->runnext);
    __atomic_store_n(&w->rn_state, (v & ~(uint32_t)RN_MASK) | RN_EMPTY, __ATOMIC_RELEASE);
    return true;
}

static bool runnext_take(worker *w, jobrec *out) {
    return runnext_take_exact(w, __atomic_load_n(&w->rn_state, __ATOMIC_ACQUIRE), out);
}

/* Owner: r runs next; a record already there goes behind the ring. */
static bool runnext_put(worker *w, const jobrec *r) {
    uint32_t s;
    bool kicked = false;
    for (;;) {
        s = __atomic_load_n(&w->rn_state, __ATOMIC_ACQUIRE);
        if ((s & RN_MASK) == RN_EMPTY) break;
        jobrec old;
        if (runnext_take_exact(w, s, &old)) { STAT(rn_kick); ring_push_slow(w, &old); kicked = true; }
        /* RN_BUSY: a thief is copying it out */
    }
    rec_store(&w->runnext, r);
    __atomic_store_n(&w->rn_state, (s & ~(uint32_t)RN_MASK) + RN_SEQ + RN_FULL, __ATOMIC_SEQ_CST);
    return kicked;
}

static inline bool runnext_full(const worker *w) { return (__atomic_load_n(&w->rn_state, __ATOMIC_SEQ_CST) & RN_MASK) == RN_FULL; }

/* ---- wake-ups ---- */

/* Wakes one parked worker if nobody is polling for work. The woken worker
 * inherits the spinning token (q_spinning is raised here, on its behalf),
 * so posts made before it runs do not wake more workers (Go's wakep). */
static void wake_locked(void) {
    if (__atomic_load_n(&q_spinning, __ATOMIC_RELAXED) != 0) return;
    /* a worker on q_cv first: the one in the hub keeps covering the network */
    if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) - (q_hub_w ? 1 : 0) - q_wake_pending > 0) {
        __atomic_add_fetch(&q_spinning, 1, __ATOMIC_SEQ_CST);
        q_wake_pending++;
        STAT(wake);
        zell__plat_cond_signal(&q_cv);
    } else if (q_hub_w && !q_hub_kicked) {
        __atomic_add_fetch(&q_spinning, 1, __ATOMIC_SEQ_CST);
        q_hub_kicked = 1;
        STAT(wake);
        netpoll()->kick();
    }
}

static void wake_one(void) {
    if (__atomic_load_n(&q_idle, __ATOMIC_SEQ_CST) == 0 || __atomic_load_n(&q_spinning, __ATOMIC_SEQ_CST) != 0) return;
    zell__plat_mutex_lock(&q_mu);
    wake_locked();
    zell__plat_mutex_unlock(&q_mu);
}

/* Whether the global queue or any ring holds work (run-next slots are
 * their owners' business: see watch_runnext). */
static bool queued_work(void) {
    if (global_nonempty()) return true;
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++)
        if (ring_len(workers[i]) != 0) return true;
    return false;
}

/* Whether another worker's run-next slot is full (sequentially consistent
 * loads: the parking protocol). */
static bool any_runnext_full(worker *w) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++)
        if (workers[i] != w && runnext_full(workers[i])) return true;
    return false;
}

/* The first other worker whose run-next slot is full (its rn_state in
 * *val), or -1. */
static int64_t first_runnext_full(worker *w, uint32_t *val) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++) {
        if (workers[i] == w) continue;
        uint32_t v = __atomic_load_n(&workers[i]->rn_state, __ATOMIC_SEQ_CST);
        if ((v & RN_MASK) == RN_FULL) { *val = v; return i; }
    }
    return -1;
}

/* The spinner's look at the other workers' run-next slots, once per pass
 * (every read pulls a line the busy owner writes on every hand-off): a
 * slot seen holding the same record (same rn_state, sequence included)
 * for RUNNEXT_GRACE_NS belongs to an owner busy with a long job rather
 * than about to take it, and w takes it (true, record in *out). One
 * candidate slot is followed at a time; the clock is read only while it
 * stays unchanged. */
static bool watch_runnext(worker *w, jobrec *out) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++) {
        worker *v = workers[i];
        if (v == w) continue;
        uint32_t s = __atomic_load_n(&v->rn_state, __ATOMIC_ACQUIRE);
        if ((s & RN_MASK) != RN_FULL) { if (i == w->cand) w->cand = -1; continue; }
        if (i == w->cand && s == w->cand_val) {
            int64_t now = zell__plat_mono_ns();
            if (w->cand_t == 0) w->cand_t = now;
            else if (now - w->cand_t >= RUNNEXT_GRACE_NS && runnext_take_exact(v, s, out)) {
                STAT(linger);
                w->cand = -1;
                return true;
            }
        } else if (w->cand < 0 || i == w->cand) {
            w->cand = i;
            w->cand_val = s;
            w->cand_t = 0;
        }
    }
    return false;
}

/* ---- finding work (the owner of w) ---- */

static inline uint64_t rng_next(worker *w) {
    uint64_t x = w->rng;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    w->rng = x;
    return x;
}

/* Takes a batch from the global queue: the first record in *out, the rest
 * (up to the ring's free space) into w's ring. */
static bool global_get(worker *w, jobrec *out) {
    if (!global_nonempty()) return false;
    zell__plat_mutex_lock(&q_mu);
    job *j = global_pop_locked();
    if (!j) { zell__plat_mutex_unlock(&q_mu); return false; }
    int64_t want = __atomic_load_n(&q_len, __ATOMIC_RELAXED) / (q_nworkers > 0 ? q_nworkers : 1);
    int64_t room = LOCALQ - (int64_t)ring_len(w);
    if (want > room) want = room;
    if (want > LOCALQ / 2) want = LOCALQ / 2;
    job *extra = NULL;
    for (int64_t i = 0; i < want; i++) {
        job *e = global_pop_locked();
        if (!e) break;
        e->next = extra;
        extra = e;
    }
    zell__plat_mutex_unlock(&q_mu);
    /* `extra` is reversed: push in FIFO order */
    job *fwd = NULL;
    while (extra) { job *n = extra->next; extra->next = fwd; fwd = extra; extra = n; }
    while (fwd) { job *n = fwd->next; ring_push_slow(w, &fwd->r); free(fwd); fwd = n; }
    *out = j->r;
    free(j);
    return true;
}

/* Steals halves of the other workers' rings. Run-next slots are left to
 * their owners here; only the spinner (watch_runnext) and the parked
 * watcher take one that stays full. */
static bool steal_work(worker *w, jobrec *out) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    if (n <= 1) return false;
    for (int pass = 0; pass < STEAL_PASSES; pass++) {
        uint64_t start = rng_next(w) % (uint64_t)n;
        for (int64_t i = 0; i < n; i++) {
            worker *v = workers[(start + (uint64_t)i) % (uint64_t)n];
            if (v != w && steal_ring(w, v, out)) { STAT(steal); return true; }
        }
    }
    return false;
}

/* Polls the network for w (netpoll, timeout 0): the completions go to
 * w's ring in order. Returns whether any was posted. */
__attribute__((noinline)) static bool netpoll_poll(const zell__netpoll *np) {
    t_ring_only = 1;
    bool got = np->poll();
    t_ring_only = 0;
    return got;
}

static inline bool netpoll_here(worker *w) {
    (void)w;
    const zell__netpoll *np = netpoll();
    return np && netpoll_poll(np);
}

static bool find_work(worker *w, jobrec *out) {
    w->tick++;
    if (w->tick % GLOBAL_EVERY == 0 && global_get(w, out)) { w->rn_streak = 0; return true; }
    /* Run-next first, unless it has had the worker to itself for a while
     * and the ring waits: then one ring job runs in between (run-next
     * cannot be preempted here, so this is its time slice). */
    bool ring_first = w->rn_streak >= RUNNEXT_STREAK;
    if (!ring_first && runnext_take(w, out)) { w->rn_streak++; STAT(jobs_rn); return true; }
    if (ring_pop(w, out)) { w->rn_streak = 0; STAT(jobs_ring); return true; }
    if (ring_first && runnext_take(w, out)) { w->rn_streak = 1; STAT(jobs_rn); return true; }
    if (global_get(w, out)) { w->rn_streak = 0; STAT(jobs_glob); return true; }
    /* The network before the other workers (Go's findrunnable): ready
     * waits complete here and their continuations land in our ring. */
    if (netpoll_here(w) && ring_pop(w, out)) { w->rn_streak = 0; STAT(jobs_ring); return true; }
    if (steal_work(w, out)) { w->rn_streak = 0; return true; }
    return false;
}

/* Polls the global queue and the rings until w->spin_until (the spinning
 * token is held). Returns whether work showed up somewhere: in a ring or
 * the global queue, or a run-next slot nobody took for the grace period
 * (then it is already taken, in *out). */
static bool spin_for_work(worker *w, jobrec *out, bool *taken) {
    *taken = false;
    for (uint32_t i = 1;; i++) {
        if (queued_work()) return true;
        if ((i & 63) == 0) {
            if ((i & 255) == 0 && zell__plat_mono_ns() >= w->spin_until) return false;
            if (netpoll_here(w)) return true;
            if (watch_runnext(w, out)) { *taken = true; return true; }
            /* more threads than cores (spares): let the poster run */
            if (__atomic_load_n(&q_alive, __ATOMIC_RELAXED) > q_ncpu) zell__plat_yield();
        }
        for (int k = 0; k < SPIN_RELAX; k++) zell__plat_cpu_relax();
    }
}

/* Takes the spinning token if nobody holds it. One spinner at a time:
 * Go's cap of GOMAXPROCS/2 spinners measured no faster here (ping-pong
 * 414 -> 457 ms, select_many CPU 5.8 -> 6.0 s). q_spinning is a count
 * (raised with add, dropped with sub) because a waker may raise it for a
 * parked worker while a worker takes it here. */
static bool take_spin_token(void) {
    int64_t zero = 0;
    return __atomic_compare_exchange_n(&q_spinning, &zero, 1, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
}

/* Drops the spinning token; with work still queued somewhere, hands it to
 * a parked worker (Go's resetspinning), so parallelism is kept up. */
static void stop_spinning(worker *w) {
    if (!w->spinning) return;
    w->spinning = 0;
    if (__atomic_sub_fetch(&q_spinning, 1, __ATOMIC_SEQ_CST) == 0 && (ring_len(w) != 0 || global_nonempty())) wake_one();
}

static void run_job(const jobrec *j) {
    zell_object *saved = t_ctx;
    zell_task *saved_cur = t_cur;
    t_ctx = j->ctx;   /* the job's references move to the thread */
    t_cur = j->cur;
    /* The unwind point of a task panic (ABI.md 8.1): the panic destroys
     * the job's coroutine frames and jumps back here (registers only, no
     * SEH or DWARF unwinding: platform.h zell__plat_setjmp). */
    zell__job_unwind uw;
    uw.chain0 = zell_coro_chain;
    uw.resumed = j->fn == zell_coro_resume;
    uw.prev = zell__job_unwind_tls;
    zell__job_unwind_tls = &uw;
    if (zell__plat_setjmp(uw.jb) == 0) {
        j->fn(j->arg);
    } else {
        zell_coro_chain = uw.chain0;
    }
    /* Frame-linked awaits (ABI.md 10.3): a linked coroutine that ended
     * handed over its suspended awaiter; resume it here, in a loop, so a
     * chain of N linked frames unwinds without recursion or queueing. A
     * panic in one of them jumps back to the setjmp above and the loop
     * goes on with the awaiter its unwinding handed over. */
    for (;;) {
        void *h = zell_coro_next;
        if (!h) break;
        zell_coro_next = NULL;
        uw.resumed = 1;
        zell_coro_resume(h);
    }
    zell__job_unwind_tls = uw.prev;
    orphans_clear();   /* nothing after the job can claim them */
    PROF_START();
    /* Restore the thread-locals before dropping the job's references:
     * the release may free the task (or context) and run destructors
     * (a task result's Drop) that start tasks, which must not see a
     * dying object as their parent (that resurrected and double-freed
     * it). */
    zell_object *done_ctx = t_ctx;
    zell_task *done_cur = t_cur;
    t_ctx = saved;
    t_cur = saved_cur;
    zell_release(done_ctx);
    zell_release(done_cur);
    PROF_T(jobtail);
}

static void *worker_main(void *arg) {
    worker *w = arg;
    t_w = w;
    t_is_worker = 1;
    zell__rc_thread_eligible();     /* biased refcounts (rc.c) */
    zell__plat_thread_stack_init(); /* frame walks of the CPU profiler */
    bool sentry_woke = false;   /* back from a park timeout: park again without spinning */
    for (;;) {
        jobrec j;
        PROF_START();
        if (find_work(w, &j)) {
            sentry_woke = false;
            stop_spinning(w);
            PROF_T(findwork);
            run_job(&j);
            continue;
        }
        /* Out of work. Take back pages other threads freed into (and
         * retire empty ones), so idle workers do not pin memory; merge
         * this thread's refcount queue (rc.c: destructors may run). */
        if (zell__heap_needs_collect()) { zell__heap_collect(0); continue; }
        if (zell__rc_pending()) { zell__rc_poll(); continue; }
        /* Poll before parking, if nobody else does (the token). */
        if (!w->spinning && !sentry_woke && q_nworkers > 1 && take_spin_token()) {
            w->spinning = 1;
            w->spin_until = zell__plat_mono_ns() + SPIN_NS;
            w->cand = -1;
            STAT(spin);
        }
        if (w->spinning) {
            bool taken;
            if (spin_for_work(w, &j, &taken)) {
                STAT(spin_found);
                if (taken) { stop_spinning(w); run_job(&j); }
                continue;   /* else find_work takes it; stop_spinning follows */
            }
        }
        /* Park. */
        zell__plat_mutex_lock(&q_mu);
        if (w->spinning) {
            w->spinning = 0;
            __atomic_sub_fetch(&q_spinning, 1, __ATOMIC_SEQ_CST);
        }
        if (q_stopping) break;
        /* spare threads retire once enough unblocked workers exist again */
        if (w->spare && q_alive - q_blocked > q_nworkers) break;
        __atomic_add_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
        /* A poster stores its queue's tail (or its run-next state), then
         * reads q_idle; we raised q_idle and now read the queues: one of us
         * sees the other. A run-next slot is its owner's to take, but a job
         * that never yields (a loop that keeps finding completed tasks)
         * would starve it with every other worker parked and nobody
         * posting: the timed watcher below covers that. */
        if (queued_work()) {
            STAT(recheck);
            __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
            zell__plat_mutex_unlock(&q_mu);
            continue;
        }
        /* Only a run-next slot is full: its owner is about to take it, or
         * runs a job that will not yield. One parked worker (q_watcher)
         * waits with a timeout and takes the slot if it still holds the
         * same record when it wakes; the timeout doubles while the slots
         * it sees keep moving. Everybody else parks below. */
        uint32_t wv = 0;
        int64_t wp = q_watcher ? -1 : first_runnext_full(w, &wv);
        sentry_woke = false;
        if (wp >= 0) {
            q_watcher = w;
            STAT(watch);
            zell__rc_park(0);
            zell__plat_cond_wait_ns(&q_cv, &q_mu, w->watch_ns);
            if (q_watcher == w) q_watcher = NULL;
            __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
            if (q_wake_pending > 0) {
                q_wake_pending--;
                w->spinning = 1;
                w->spin_until = zell__plat_mono_ns() + SPIN_NS;
            }
            zell__plat_mutex_unlock(&q_mu);
            zell__rc_unpark();
            if (netpoll()) netpoll()->awake();
            if (runnext_take_exact(workers[wp], wv, &j)) {
                STAT(linger);
                w->watch_ns = WATCH_MIN_NS;
                stop_spinning(w);
                run_job(&j);
            } else if (w->watch_ns < WATCH_MAX_NS) {
                w->watch_ns *= 2;
            }
            continue;
        }
        w->watch_ns = WATCH_MIN_NS;
        if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) == q_alive) zell__plat_cond_broadcast(&idle_cv);  /* drained */
        /* The last worker to go idle waits for the network (and the next
         * I/O deadline) in the hub poller instead of on q_cv; a post kicks
         * it (wake_locked). */
        const zell__netpoll *np = netpoll();
        if (np && __atomic_load_n(&q_idle, __ATOMIC_RELAXED) + q_blocked == q_alive && !q_hub_w && np->claim()) {
            q_hub_w = w;
            q_hub_kicked = 0;
            zell__plat_mutex_unlock(&q_mu);
            zell__rc_park(0);
            if (zell__cycles_park) zell__cycles_park();
            STAT(park);
            np->block();
            zell__plat_mutex_lock(&q_mu);
            q_hub_w = NULL;
            __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
            if (q_hub_kicked) {   /* the kicker reserved the spinning token for us */
                q_hub_kicked = 0;
                w->spinning = 1;
                w->spin_until = zell__plat_mono_ns() + SPIN_NS;
            }
            zell__plat_mutex_unlock(&q_mu);
            zell__rc_unpark();
            np->awake();
            t_ring_only = 1;
            np->finish();
            t_ring_only = 0;
            continue;
        }
        zell__rc_park(0);
        if (zell__cycles_park) zell__cycles_park();   /* weak: the collector is linked */
        STAT(park);
        /* Every park has a timeout. A post to a run-next slot or ring made
         * while a spinner held the token wakes nobody (wake_one leaves it
         * to the spinner), and a spinner that then takes other work leaves
         * the post behind an owner that may never yield (a mutex waiter's
         * retry behind a loop that re-takes the mutex, ABI.md 10.6). With
         * nobody watching, this worker is the sentry and looks again after
         * SENTRY_NS; the others after PARK_NS, so a sentry that went to
         * work is replaced. Both back off while the whole pool is idle. */
        bool sentry = q_watcher == NULL;
        if (sentry) q_watcher = w;
        bool timed_out = !zell__plat_cond_wait_ns(&q_cv, &q_mu, sentry ? w->sentry_ns : w->park_ns);
        if (sentry && q_watcher == w) q_watcher = NULL;
        __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
        if (q_wake_pending > 0) {   /* the waker reserved the spinning token for us */
            q_wake_pending--;
            w->spinning = 1;
            w->spin_until = zell__plat_mono_ns() + SPIN_NS;
            w->sentry_ns = SENTRY_NS;
            w->park_ns = PARK_NS;
        } else if (timed_out) {
            sentry_woke = true;
            bool all_idle = __atomic_load_n(&q_idle, __ATOMIC_RELAXED) + 1 + q_blocked >= q_alive;
            if (sentry) w->sentry_ns = !all_idle ? SENTRY_NS : w->sentry_ns < SENTRY_MAX_NS ? w->sentry_ns * 2 : SENTRY_MAX_NS;
            else w->park_ns = !all_idle ? PARK_NS : w->park_ns < PARK_MAX_NS ? w->park_ns * 2 : PARK_MAX_NS;
        }
        zell__plat_mutex_unlock(&q_mu);
        zell__rc_unpark();
        if (np) np->awake();
    }
    /* q_mu held */
    w->alive = 0;
    __atomic_sub_fetch(&q_alive, 1, __ATOMIC_SEQ_CST);
    zell__plat_cond_broadcast(&idle_cv);
    zell__plat_mutex_unlock(&q_mu);
    return NULL;
}

/* q_mu must be held. Returns false when no slot is left (MAX_WORKERS). */
static bool spawn_worker_locked(int spare) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_RELAXED);
    worker *w = NULL;
    for (int64_t i = 0; i < n; i++) {
        if (!workers[i]->alive) { w = workers[i]; break; }
    }
    if (!w) {
        if (n >= MAX_WORKERS) return false;
        w = calloc(1, sizeof *w);
        if (!w) zell_panic("out of memory (worker)");
        w->rng = 0x9E3779B97F4A7C15ull ^ ((uint64_t)n + 1) * 0xBF58476D1CE4E5B9ull;
        workers[n] = w;
        __atomic_store_n(&q_nslots, n + 1, __ATOMIC_RELEASE);
    }
    w->alive = 1;
    w->spare = spare;
    w->spinning = 0;
    w->tick = w->rn_streak = 0;
    w->watch_ns = WATCH_MIN_NS;
    w->sentry_ns = SENTRY_NS;
    w->park_ns = PARK_NS;
    w->cand = -1;
    if (!zell__plat_thread_start_detached(worker_main, w)) zell_panic("cannot create worker thread");
    __atomic_add_fetch(&q_alive, 1, __ATOMIC_SEQ_CST);
    if (q_alive > q_alive_peak) q_alive_peak = q_alive;
    return true;
}

/* How many spare workers may exist at once: 4 per core worker, at least
 * 16 (`ZELL_SPARE_WORKERS` overrides), never beyond the slots. A worker
 * that blocks in zell_task_await while the spares are all in use blocks
 * without a replacement: blocked callers of the C API queue up on fewer
 * threads instead of turning into one OS thread each (10k producers
 * blocked in zell_chan_send used to mean ~1000 threads). Compiled Zell
 * code never blocks a worker (await is a continuation), so this only
 * bounds C callers: tests and embedders. */
static int64_t spare_limit(int64_t n) {
    const char *env = getenv("ZELL_SPARE_WORKERS");
    long v = env ? strtol(env, NULL, 10) : 0;
    int64_t lim = v > 0 ? v : (n * 4 > 16 ? n * 4 : 16);
    if (lim > MAX_WORKERS - n) lim = MAX_WORKERS - n;
    return lim;
}

/* The most worker threads alive at once since the previous call (the
 * count restarts from the threads alive now). */
bool zell__sched_poke(void) {
    zell__plat_mutex_lock(&q_mu);
    bool parked = __atomic_load_n(&q_idle, __ATOMIC_RELAXED) - (q_hub_w ? 1 : 0) > 0;
    if (parked) wake_locked();
    zell__plat_mutex_unlock(&q_mu);
    return parked;
}

bool zell__sched_hub_parked(void) {
    zell__plat_mutex_lock(&q_mu);
    bool r = q_hub_w != NULL;
    zell__plat_mutex_unlock(&q_mu);
    return r;
}

int64_t zell__sched_awake(void) {
    /* q_alive and q_blocked are written under q_mu with atomic stores, so
     * this lock-free read is a snapshot (the I/O watchdog's hint) */
    return __atomic_load_n(&q_alive, __ATOMIC_SEQ_CST) - __atomic_load_n(&q_idle, __ATOMIC_SEQ_CST) - __atomic_load_n(&q_blocked, __ATOMIC_SEQ_CST);
}

int64_t zell__sched_threads_peak(void) {
    zell__plat_mutex_lock(&q_mu);
    int64_t n = q_alive_peak;
    q_alive_peak = q_alive;
    zell__plat_mutex_unlock(&q_mu);
    return n;
}

int64_t zell__sched_max_threads(void) {
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    return q_nworkers + q_max_spares;
}

static int64_t worker_count(void) {
    const char *env = getenv("ZELL_WORKERS");
    long n = env ? strtol(env, NULL, 10) : 0;
    if (n <= 0) n = zell__plat_cpu_count();
    if (n <= 0) n = 1;
    if (n > 256) n = 256;
    return n;
}

static void timers_start(void);
static void timers_stop(void);

void (*zell__io_stop_fn)(void);
void (*zell__net_stop_fn)(void);

/* Stops the resolver pool and the I/O pollers if they ever started. */
static void stop_io(void) {
    if (zell__net_stop_fn) zell__net_stop_fn();
    if (zell__io_stop_fn) zell__io_stop_fn();
}

/* Process-wide setup (SIGPIPE, open-files limit), at most once. */
void zell__process_init(void) {
    static zell__plat_once proc_once = ZELL_PLAT_ONCE_INIT;
    zell__plat_once_run(&proc_once, zell__plat_process_init);
}

/* Set by zell__sched_start, so a program that never starts the scheduler
 * does not link the drain (sched_stop_started, nor what it pulls in). */
static void sched_stop_started(void);
static void (*g_sched_stop)(void);

/* Starts the workers and the timer thread. Called lazily, by the first
 * post, await or timer: a program that never starts a task stays single-
 * threaded (cheaper startup, non-atomic refcounts, ABI.md 3.2). */
void zell__sched_start(void) {
    g_sched_stop = sched_stop_started;
    zell__process_init();
    zell__plat_mutex_lock(&q_mu);
    if (q_started) { zell__plat_mutex_unlock(&q_mu); return; }
    q_stopping = 0;
    q_nworkers = worker_count();
    q_max_spares = spare_limit(q_nworkers);
    q_ncpu = zell__plat_cpu_count();
    for (int64_t i = 0; i < q_nworkers; i++) spawn_worker_locked(0);
    __atomic_store_n(&q_started, 1, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&q_mu);
    timers_start();
}

void zell__sched_stop(void) {
    orphans_clear();   /* the calling thread's (synchronous code) */
    if (g_sched_stop) g_sched_stop();
    else stop_io();
}

static void sched_stop_started(void) {
    zell__plat_mutex_lock(&q_mu);
    if (!q_started) {
        zell__plat_mutex_unlock(&q_mu);
        stop_io();   /* I/O waits may exist without a task ever running */
        return;
    }
    /* drain: every worker parked (their queues are then empty) and nothing global */
    while (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) != q_alive || q_head) zell__plat_cond_wait(&idle_cv, &q_mu);
    q_stopping = 1;
    zell__plat_cond_broadcast(&q_cv);
    if (q_hub_w) netpoll()->kick();
    while (q_alive > 0) zell__plat_cond_wait(&idle_cv, &q_mu);
    zell__plat_mutex_unlock(&q_mu);
#ifdef ZELL_SCHED_STATS
    fprintf(stderr, "sched: wake %lld spin %lld spin_found %lld steal %lld linger %lld park %lld recheck %lld jobs rn %lld ring %lld glob %lld rn_kick %lld stall %lld watch %lld\n",
            (long long)st_wake, (long long)st_spin, (long long)st_spin_found, (long long)st_steal, (long long)st_linger, (long long)st_park, (long long)st_recheck,
            (long long)st_jobs_rn, (long long)st_jobs_ring, (long long)st_jobs_glob, (long long)st_rn_kick, (long long)st_stall, (long long)st_watch);
    fprintf(stderr, "sched: Mcycles lazy %lld addcont %lld complete %lld drop %lld findwork %lld jobtail %lld\n",
            (long long)(pr_lazy >> 20), (long long)(pr_addcont >> 20), (long long)(pr_complete >> 20), (long long)(pr_drop >> 20), (long long)(pr_findwork >> 20), (long long)(pr_jobtail >> 20));
#endif
    timers_stop();
    stop_io();
    zell__plat_mutex_lock(&q_mu);
    q_nworkers = 0;
    q_wake_pending = 0;
    __atomic_store_n(&q_spinning, 0, __ATOMIC_SEQ_CST);
    __atomic_store_n(&q_started, 0, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&q_mu);
}

/* Posts fn(arg) to run with context `ctx` as task `cur` (owned references,
 * moved). From a worker, `next` puts it in the run-next slot (a completed
 * task waking its awaiter: it runs as soon as the current job yields the
 * thread), else at the tail of the worker's ring; from any other thread
 * it goes to the global queue. */
static void sched_post_ctx(zell_resume_fn fn, void *arg, zell_object *ctx, zell_task *cur, bool next) {
    if (!fn) zell_panic_nil();
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    jobrec r = { fn, arg, ctx, cur };
    worker *w = t_w;
    if (w) {
        if (next && !t_ring_only) {
            /* Into an empty run-next slot: this worker runs it as soon as
             * the current job yields; waking another one only has it steal
             * the job back and forth. A record pushed out to the ring does
             * wake one. */
            if (runnext_put(w, &r)) wake_one();
        } else {
            ring_push_slow(w, &r);
            wake_one();
        }
        return;
    }
    job *j = job_node(&r);
    if (t_post_batch) {
        if (t_post_batch->tail) t_post_batch->tail->next = j; else t_post_batch->head = j;
        t_post_batch->tail = j;
        t_post_batch->n++;
        return;
    }
    post_batch one = { j, j, 1 };
    post_batch_flush(&one);
}

/* Pushes a batch of jobs from a non-worker thread to the global queue in
 * one lock round trip. A polling worker takes them without a wake-up;
 * otherwise one parked worker is woken (none parked: every worker is busy
 * and will look), and it hands the spinning token on while work remains,
 * so the other idle workers follow. */
static void post_batch_flush(post_batch *b) {
    if (b->n == 0) return;
    zell__plat_mutex_lock(&q_mu);
    global_push_locked(b->head, b->tail, b->n);
    wake_locked();
    zell__plat_mutex_unlock(&q_mu);
    b->head = b->tail = NULL;
    b->n = 0;
}

void zell_sched_post(zell_resume_fn fn, void *arg) {
    zell_retain(t_ctx);
    zell_retain(t_cur);
    sched_post_ctx(fn, arg, t_ctx, t_cur, false);
}

void zell__sched_post_plain(zell_resume_fn fn, void *arg, bool next) { sched_post_ctx(fn, arg, NULL, NULL, next); }

bool zell__sched_can_spin(void) {
    worker *w = t_w;
    return w && q_nworkers > 1 && __atomic_load_n(&q_alive, __ATOMIC_RELAXED) <= q_ncpu && ring_len(w) == 0 && !runnext_full(w);
}

int64_t zell_sched_workers(void) {
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    return q_nworkers;
}

bool zell_sched_is_worker(void) { return t_is_worker != 0; }

void zell_coro_resume(void *frame) {
    if (!frame) zell_panic_nil();
    /* LLVM switched-resume ABI: the first word of a coroutine frame is the resume fn. */
    void (*resume)(void *) = *(void (**)(void *))frame;
    resume(frame);
}


/* ======================================================================
 * Tasks
 * ====================================================================== */

enum { TASK_PENDING = 0, TASK_DONE = 1 };

typedef struct cont {
    zell_resume_fn fn;
    void *frame;
    zell_object *ctx;          /* owned: context of the registering code */
    zell_task *cur;            /* owned: current task of the registering code */
    struct cont *next;
} cont;

/* A wait to wake when its task is cancelled (10.11): rt_internal.h. */
typedef zell__hook hook;

/* Drops a hook that is in no list: its reference to w and, unless the
 * node is embedded in w, the node (an embedded node may die with w here,
 * so nothing reads it afterwards). */
static inline void hook_free(hook *h) {
    int32_t embedded = h->embedded;
    zell_release(h->w);
    if (!embedded) free(h);
}

struct zell_task {
    zell_object hdr;
    int32_t mu;                 /* spin lock (task_lock): short critical sections only */
    int32_t _pad0;
    int32_t state;              /* atomic */
    int32_t _pad;
    int64_t result_size;
    const zell_value_ops *ops; /* applied at slot + ops_offset */
    int64_t ops_offset;
    cont *conts;                /* LIFO; reversed when fired */
    /* The first continuation lives in the task (the common case is one
     * awaiter, so no node is allocated); cont0.fn is NULL while unused. */
    cont cont0;
    /* Task tree for cancellation (10.11), guarded by mu (children lists by
     * the parent's mu). Only tasks made by zell_task_new / scopes have it. */
    zell_task *parent;         /* owned until the task is freed */
    zell_task *child_head;     /* children (not owned) */
    zell_task *sib_prev, *sib_next;
    hook *hooks;
    int64_t hook_seq;           /* links so far (atomic store under mu): hook->seq */
    int32_t cancelled;          /* atomic */
    int32_t nhooks;
    /* Runs when the task itself is the target of zell_task_cancel (a
     * pending channel wait: task.Any cancelling a loser). Set before the
     * task is published, then read-only. */
    void (*cancel_fn)(zell_task *t);
    /* The caller's context when a coroutine started (zell_task_new; owned,
     * valid while ctx_saved): put back when the coroutine first suspends or
     * completes, so context changes made inside it (task context values,
     * diag spans) never leak into the caller (10.8). */
    zell_object *caller_ctx;
    int32_t ctx_saved;
    int32_t dbg_id;             /* debug agent record (debug.c), 0 = none */
    /* The pending zell_task_delay timer that completes this task, guarded
     * by tm_mu (NULL once it fired or was removed): lets rt_fire drop the
     * timer in O(log n) instead of scanning all timers. */
    struct timer *timer;
    /* Threads blocked in zell_task_await (guarded by mu): each is woken
     * through its own condition variable, exactly once, by task_complete. */
    struct awaiter *awaiters;
    /* A panicked task (ABI.md 8.1): done with a zero slot; the panic's
     * message and trace (owned). panic_seen: an awaiter observed it (else
     * the drop prints it); panic_filled: the task.Panic error was stored
     * into the slot for a Task<T!> reader (guarded by mu). */
    zell_string *panic_msg;
    zell_string *panic_trace;
    int32_t panic_seen;          /* atomic */
    int32_t panic_filled;
    unsigned char slot[];       /* result_size bytes (8-aligned) */
};

/* The words compiled code reads inline (ABI.md 10.2 "Task layout"): the
 * await fast path (state, panic_msg, slot) and the ramp's cancel-hook mark
 * (hook_seq). Moving any of them is an ABI change. */
_Static_assert(offsetof(struct zell_task, state) == 24 && offsetof(struct zell_task, hook_seq) == 144 && offsetof(struct zell_task, panic_msg) == 200 && offsetof(struct zell_task, slot) == 224 && TASK_DONE == 1, "ABI.md 10.2: task layout");

/* A thread blocked in zell_task_await, on its stack. It parks on its own
 * (thread-local) mutex and condition variable: a process-wide condition
 * shared by every blocked thread turned each completion into a broadcast
 * to all of them (10k producers blocking in zell_chan_send on a full
 * channel spawn ~1000 spare workers; each receive then woke all 1000,
 * which re-locked the shared mutex and went back to sleep: a thundering
 * herd that looked like a hang under TSan). Invariant: `done` is set and
 * the waiter signalled only after the task's state is TASK_DONE, by the
 * completing thread, with the awaiter still linked on the task (so the
 * waiter's stack frame is alive): a waiter that finds the task done under
 * its lock never links itself and is never signalled. */
typedef struct awaiter {
    zell__plat_mutex *mu;
    zell__plat_cond *cv;
    int32_t done;               /* under *mu */
    struct awaiter *next;
} awaiter;

static _Thread_local zell__plat_mutex t_park_mu = ZELL_PLAT_MUTEX_INIT;
static _Thread_local zell__plat_cond t_park_cv = ZELL_PLAT_COND_INIT;
static int64_t g_await_spurious;   /* diag: wake-ups of zell_task_await that found the task still pending */

static int64_t g_live_tasks; /* diag: tasks made minus dropped on non-worker threads (workers count in their own slot) */

/* Live-task bookkeeping without a shared atomic on the hot path: a worker
 * counts in its own slot, other threads in g_live_tasks. */
static inline void live_tasks_add(int64_t d) {
    if (t_w) t_w->live_tasks += d;
    else __atomic_add_fetch(&g_live_tasks, d, __ATOMIC_RELAXED);
}

/* A task's lock: held for a few loads and stores (never across a wait or a
 * call into user code that could take it again), so a spin lock with a
 * yield is cheaper than a pthread mutex, which every async call would
 * otherwise initialise, lock and destroy. Threads blocking on a task
 * (zell_task_await) link an awaiter on it and park on their own
 * condition variable (see awaiter). */
static inline void task_lock(zell_task *t) {
    while (__atomic_exchange_n(&t->mu, 1, __ATOMIC_ACQUIRE)) {
        while (__atomic_load_n(&t->mu, __ATOMIC_RELAXED)) zell__plat_yield();
    }
}

static inline void task_unlock(zell_task *t) { __atomic_store_n(&t->mu, 0, __ATOMIC_RELEASE); }

int64_t zell__live_tasks(void) {
    int64_t n = __atomic_load_n(&g_live_tasks, __ATOMIC_RELAXED);
    int64_t slots = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < slots; i++) n += __atomic_load_n(&workers[i]->live_tasks, __ATOMIC_RELAXED);
    return n;
}
int64_t zell__await_spurious_wakeups(void) { return __atomic_load_n(&g_await_spurious, __ATOMIC_RELAXED); }

int64_t zell__sched_queue_len(void) {
    int64_t n = __atomic_load_n(&q_len, __ATOMIC_SEQ_CST);
    int64_t slots = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < slots; i++) n += ring_len(workers[i]) + (runnext_full(workers[i]) ? 1 : 0);
    return n;
}

static void task_drop(void *obj) {
    zell_task *t = obj;
    PROF_START();
    live_tasks_add(-1);
    if (t->state == TASK_DONE && t->ops && t->ops->release)
        t->ops->release(t->slot + t->ops_offset);
    cont *c = t->conts;
    while (c) { cont *n = c->next; zell_release(c->ctx); zell_release(c->cur); free(c); c = n; }
    if (t->cont0.fn) { zell_release(t->cont0.ctx); zell_release(t->cont0.cur); }
    hook *h = t->hooks;
    while (h) { hook *n = h->next; hook_free(h); h = n; }
    if (t->ctx_saved) zell_release(t->caller_ctx);
    if (t->dbg_id) zell__dbg_task_drop(t->dbg_id);
    if (t->panic_msg) {
        /* Nobody awaited the panicked task (fire-and-forget): say so now. */
        if (!__atomic_load_n(&t->panic_seen, __ATOMIC_ACQUIRE)) zell__panic_print(t->panic_msg, t->panic_trace);
        zell_release(t->panic_msg);
        zell_release(t->panic_trace);
    }
    if (t->parent) {
        zell_task *p = t->parent;
        task_lock(p);
        if (t->sib_prev) t->sib_prev->sib_next = t->sib_next; else p->child_head = t->sib_next;
        if (t->sib_next) t->sib_next->sib_prev = t->sib_prev;
        task_unlock(p);
        zell_release(p);
    }
    PROF_T(drop);
}

static const zell_type_info task_type = { "Task", 0, task_drop };

static zell_task *task_new_ex(int64_t result_size, const zell_value_ops *ops, int64_t ops_off) {
    if (result_size < 0) zell__panicf("zell_task_new: negative result size");
    zell_task *t = zell_object_new(&task_type,
                                     (int64_t)sizeof(zell_task) + ZELL_ROUND8(result_size));
    /* Tasks move between threads all the time (awaiters, continuations,
     * jobs): biasing them to one thread would only queue merges (rc.c). */
    t->hdr.refcount |= ZELL_RC_NOBIAS;
    live_tasks_add(1);
    t->result_size = result_size;
    t->ops = ops;
    t->ops_offset = ops_off;
    return t;
}

/* Links t under the current task (or `parent`), inheriting cancellation. */
static void task_attach(zell_task *t, zell_task *parent) {
    if (!parent) return;
    zell_retain(parent);
    t->parent = parent;
    task_lock(parent);
    t->sib_next = parent->child_head;
    if (parent->child_head) parent->child_head->sib_prev = t;
    parent->child_head = t;
    if (__atomic_load_n(&parent->cancelled, __ATOMIC_ACQUIRE)) __atomic_store_n(&t->cancelled, 1, __ATOMIC_RELEASE);
    task_unlock(parent);
}

/* The task of the code that runs now: a child of the current task, and the
 * current task from here on (until it suspends or completes). */
zell_task *zell_task_new(int64_t result_size, const zell_value_ops *result_ops) {
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    task_attach(t, t_cur);
    zell_retain(t_ctx);
    t->caller_ctx = t_ctx;
    t->ctx_saved = 1;
    zell_retain(t);
    zell_task *old = t_cur;
    t_cur = t;
    zell_release(old);  /* t->parent holds its own reference */
    return t;
}

zell_task *zell_task_new_plain(int64_t result_size, const zell_value_ops *result_ops) {
    return task_new_ex(result_size, result_ops, 0);
}

/* task_sync.c (ABI.md 10.6, 10.7): a plain task whose slot has room for
 * slot_bytes (at least result_size) so a pending wait can keep its queue
 * links and a sender its value there; ops apply at slot + ops_off. */
zell_task *zell__task_new_slot(int64_t result_size, const zell_value_ops *ops, int64_t ops_off, int64_t slot_bytes) {
    if (result_size < 0 || slot_bytes < result_size) zell__panicf("zell__task_new_slot: bad sizes");
    zell_task *t = zell_object_new(&task_type, (int64_t)sizeof(zell_task) + ZELL_ROUND8(slot_bytes));
    t->hdr.refcount |= ZELL_RC_NOBIAS;
    live_tasks_add(1);
    t->result_size = result_size;
    t->ops = ops;
    t->ops_offset = ops_off;
    return t;
}

void *zell__task_slot(zell_task *t) { return t->slot; }
int64_t zell__task_result_size(const zell_task *t) { return t->result_size; }
void zell__task_lock(zell_task *t) { task_lock(t); }
void zell__task_unlock(zell_task *t) { task_unlock(t); }

/* Completes a task nobody else has seen yet, whose result is already in
 * its slot (no waiters, no continuations, not the current task). */
void zell__task_mark_done(zell_task *t) { __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_RELEASE); }

/* The immortal completed Task<void>: what an operation that did not have
 * to wait returns (Mutex.Lock of a free mutex, Delay(0), a Send that found
 * room); awaiting it takes the is_done fast path, retain and release are
 * no-ops. */
struct zell_task zell_task_done_task = {
    .hdr = { ZELL_RC_IMMORTAL, &task_type }, .state = TASK_DONE,
};

zell_task *zell_task_done(void) { return &zell_task_done_task; }

/* The immortal yield sentinel: what task.Yield returns. It is never done;
 * add_cont recognises it and posts the continuation straight to the ring
 * tail (behind queued work) instead of registering it, so a Yield costs no
 * task object and one job. A blocking zell_task_await of it from a
 * non-task thread waits on a fresh task completed by a job (task_wait).
 * Cancel, deadlines and fire on it are no-ops; retain/release are no-ops. */
static struct zell_task yield_task = {
    .hdr = { ZELL_RC_IMMORTAL, &task_type }, .state = TASK_PENDING,
};

static inline bool is_yield(const zell_task *t) { return t == &yield_task; }

/* The current task stops running on this thread: its parent is current. */
static void restore_parent(zell_task *t) {
    if (t_cur != t) return;
    if (t->ctx_saved) {
        /* Back on the caller's stack (first suspension or completion): the
         * caller's context too. The task's own context lives on in its
         * continuations. */
        zell_object *old = t_ctx;
        t_ctx = t->caller_ctx;
        t->caller_ctx = NULL;
        t->ctx_saved = 0;
        zell_release(old);
    }
    zell_retain(t->parent);
    t_cur = t->parent;
    zell_release(t);
}

/* Completes t; returns false (and does nothing) if it already was done. */
static bool task_complete_ex(zell_task *t, const void *result, zell_string *pmsg, zell_string *ptrace);
static bool task_complete(zell_task *t, const void *result) {
    return task_complete_ex(t, result, NULL, NULL);
}

/* pmsg/ptrace (borrowed, retained here): complete t as panicked. */
static bool task_complete_ex(zell_task *t, const void *result, zell_string *pmsg, zell_string *ptrace) {
    if (!t) zell_panic_nil();
    if (is_yield(t)) return false;
    /* A waiter that sees DONE may drop the last reference at once, while
     * this thread still writes t (conts, the lock word, t_cur): hold one
     * until the task is left (the drop then runs on whichever thread lets
     * go last, ordered by the count's acquire-release decrement). */
    bool held = t_cur == t;   /* the current-task slot's reference keeps t alive until restore_parent drops it, last */
    if (!held) zell_retain(t);
    task_lock(t);
    if (t->state == TASK_DONE) {
        task_unlock(t);
        if (!held) zell_release(t);
        return false;
    }
    if (pmsg) {
        zell_retain(pmsg);
        zell_retain(ptrace);
        t->panic_msg = pmsg;
        t->panic_trace = ptrace;
    } else if (result && t->result_size > 0) {
        memcpy(t->slot, result, (size_t)t->result_size);
        if (t->ops && t->ops->retain) t->ops->retain(t->slot + t->ops_offset);
    }
    /* Release suffices: readers take `state` with an acquire load (or
     * under the lock) and then read the slot; no reader pairs it with a
     * second flag in a store-then-load protocol. */
    __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_RELEASE);
    cont *c = t->conts, *rev = NULL;
    t->conts = NULL;
    cont first = t->cont0;   /* copied out: t may be freed once unlocked */
    t->cont0.fn = NULL;
    awaiter *aw = t->awaiters;   /* blocked threads: linked under the lock, so none can join after this */
    t->awaiters = NULL;
    task_unlock(t);
    /* Wake each blocked thread once. `next` is read first: once `done` is
     * published the waiter may return and its frame is gone. */
    while (aw) {
        awaiter *n = aw->next;
        zell__plat_mutex_lock(aw->mu);
        aw->done = 1;
        zell__plat_cond_signal(aw->cv);
        zell__plat_mutex_unlock(aw->mu);
        aw = n;
    }
    restore_parent(t);  /* a coroutine returning: the code after it is its parent's */
    if (!held) zell_release(t);
    /* After this point `t` may be freed; use only `first` and `rev`. */
    while (c) { cont *n = c->next; c->next = rev; rev = c; c = n; }
    /* The first continuation runs next on this worker (the common case is
     * one awaiter); the others queue behind the ring in registration order. */
    bool next = true;
    if (first.fn) {
        sched_post_ctx(first.fn, first.frame, first.ctx, first.cur, true);
        next = false;
    }
    while (rev) {
        cont *n = rev->next;
        sched_post_ctx(rev->fn, rev->frame, rev->ctx, rev->cur, next);
        next = false;
        free(rev);
        rev = n;
    }
    return true;
}

void zell_task_complete(zell_task *t, const void *result) {
    PROF_START();
    if (!task_complete(t, result)) zell_panic("task completed twice");
    PROF_T(complete);
}

bool zell_task_try_complete(zell_task *t, const void *result) {
    return task_complete(t, result);
}

bool zell_task_is_done(const zell_task *t) {
    if (!t) zell_panic_nil();
    return __atomic_load_n(&t->state, __ATOMIC_ACQUIRE) == TASK_DONE;
}

/* `suspend`: the registering coroutine suspends when this succeeds. Its
 * task's saved caller context is then restored on this thread *before* the
 * continuation is published: once it is, the continuation may resume on
 * another thread, whose restore_parent must not race with ours on the
 * task's ctx_saved/caller_ctx. */
/* add_cont on the yield sentinel: the continuation is posted at once to
 * the ring tail with the caller's context, as a registered continuation
 * would be by a completion (same ownership: ctx retained, cur moved when
 * suspending), never to run-next, so it runs behind already queued work. */
static bool yield_cont(zell_resume_fn fn, void *frame, bool suspend) {
    zell_object *ctx = t_ctx;
    zell_retain(ctx);
    zell_object *drop_ctx = NULL;
    zell_task *cur = t_cur;
    if (suspend && cur) {
        /* restore_parent with the current-task reference moved into the
         * job; done before posting, since the job may run elsewhere. */
        if (cur->ctx_saved) {
            drop_ctx = t_ctx;
            t_ctx = cur->caller_ctx;
            cur->caller_ctx = NULL;
            cur->ctx_saved = 0;
        }
        zell_retain(cur->parent);
        t_cur = cur->parent;
    } else {
        zell_retain(cur);
    }
    zell_release(drop_ctx);
    sched_post_ctx(fn, frame, ctx, cur, false);
    return true;
}

static bool add_cont(zell_task *t, zell_resume_fn fn, void *frame, bool suspend) {
    if (!t || !fn) zell_panic_nil();
    if (is_yield(t)) return yield_cont(fn, frame, suspend);
    if (zell_task_is_done(t)) return false;
    task_lock(t);
    if (t->state == TASK_DONE) {
        task_unlock(t);
        return false;
    }
    cont *c;
    if (!t->cont0.fn) {
        c = &t->cont0;   /* the first continuation: no allocation */
    } else {
        c = malloc(sizeof *c);
        if (!c) zell_panic("out of memory (continuation)");
        c->next = t->conts;
        t->conts = c;
    }
    c->fn = fn;
    c->frame = frame;
    c->ctx = t_ctx;
    zell_retain(c->ctx);
    zell_object *drop_ctx = NULL;
    zell_task *cur = t_cur;
    c->cur = cur;
    if (suspend && cur) {
        /* The suspending coroutine's task stops running here: what
         * restore_parent does, under the lock, with the current-task
         * reference moved into the continuation instead of retained and
         * released. Once unlocked, the completer may fire the continuation
         * and its job's release may free `cur` on another thread, so
         * nothing of `cur` is touched after the unlock. */
        if (cur->ctx_saved) {
            drop_ctx = t_ctx;   /* released after unlocking */
            t_ctx = cur->caller_ctx;
            cur->caller_ctx = NULL;
            cur->ctx_saved = 0;
        }
        zell_retain(cur->parent);
        t_cur = cur->parent;
    } else {
        zell_retain(cur);
    }
    task_unlock(t);
    zell_release(drop_ctx);
    return true;
}

/* A linked coroutine (no task of its own, ABI.md 10.3) registers: the
 * current task and context are not its own, so they stay current; the
 * continuation runs with both (retained). */
bool zell_task_add_continuation_linked(zell_task *t, zell_resume_fn fn, void *frame) {
    PROF_START();
    if (!add_cont(t, fn, frame, false)) return false;
    PROF_T(addcont);
    return true;
}

/* The registration a linked frame deferred at its first suspension
 * (ABI.md 10.3): until the outermost frame of the synchronous chain
 * publishes it with zell_coro_register, nothing can resume the chain, so
 * the frames in between link with plain stores. One per thread at most. */
static _Thread_local struct { zell_task *t; void *frame; zell_object *ctx; } t_pend;

void zell_coro_defer(zell_task *t, void *frame, zell_object *ctx0) {
    if (t_pend.frame) zell_panic("zell_coro_defer: a registration is pending already");
    t_pend.t = t;
    t_pend.frame = frame;
    t_pend.ctx = t_ctx;   /* the frame's context, moved: its continuation's */
    t_ctx = ctx0;         /* moved: the caller's context is current again */
}

/* Publishes the deferred registration: what add_cont does for the frame
 * that suspends now (`own`: it has a task of its own, which stops being
 * current), but for the pending frame and with its context; when the task
 * is done already (or is the yield sentinel), the frame's resumption is
 * posted instead, so the registering frame always suspends. */
void zell_coro_register(bool own) {
    PROF_START();
    zell_task *t = t_pend.t;
    void *frame = t_pend.frame;
    zell_object *ctx = t_pend.ctx;
    if (!frame) zell_panic("zell_coro_register: nothing pending");
    t_pend.t = NULL;
    t_pend.frame = NULL;
    t_pend.ctx = NULL;
    zell_task *cur = t_cur;
    zell_object *drop_ctx = NULL;
    bool locked = false;
    cont *c = NULL;
    if (!is_yield(t) && !zell_task_is_done(t)) {
        task_lock(t);
        locked = true;
        if (t->state != TASK_DONE) {
            if (!t->cont0.fn) {
                c = &t->cont0;
            } else {
                c = malloc(sizeof *c);
                if (!c) zell_panic("out of memory (continuation)");
                c->next = t->conts;
                t->conts = c;
            }
        }
    }
    /* The current task stops running here if it is the frame's own (as in
     * add_cont: its reference moves into the continuation or the job). */
    if (own && cur) {
        if (cur->ctx_saved) {
            drop_ctx = t_ctx;
            t_ctx = cur->caller_ctx;
            cur->caller_ctx = NULL;
            cur->ctx_saved = 0;
        }
        zell_retain(cur->parent);
        t_cur = cur->parent;
    } else {
        zell_retain(cur);
    }
    if (c) {
        c->fn = zell_coro_resume;
        c->frame = frame;
        c->ctx = ctx;
        c->cur = cur;
    }
    if (locked) task_unlock(t);
    zell_release(drop_ctx);
    if (!c) sched_post_ctx(zell_coro_resume, frame, ctx, cur, !is_yield(t));   /* done: run next, like a completion */
    PROF_T(addcont);
}

/* Public: the registering code (a coroutine) suspends when this returns
 * true, so the current task becomes its parent again (add_cont). */
bool zell_task_add_continuation(zell_task *t, zell_resume_fn fn, void *frame) {
    PROF_START();
    if (!add_cont(t, fn, frame, true)) return false;
    PROF_T(addcont);
    return true;
}

/* A panicked Task<T> read by an awaiter: the panic goes on in the reader. */
static void *checked_slot(zell_task *t) {
    if (t->panic_msg) {
        __atomic_store_n(&t->panic_seen, 1, __ATOMIC_RELEASE);
        zell__panic_traced("panic in awaited task: ", t->panic_msg, t->panic_trace, 0);
    }
    return t->slot;
}

/* A panicked Task<T!>: the task.Panic error goes into the slot's error
 * word (once; the slot's ops release it with the task). */
static void *checked_slot_err(zell_task *t, int64_t err_off) {
    if (t->panic_msg) {
        if (err_off < 0 || err_off + 8 > ZELL_ROUND8(t->result_size)) zell__panicf("zell_task_result_err: bad error offset");
        task_lock(t);
        if (!t->panic_filled) {
            zell_error *e = zell__panic_error(t->panic_msg, t->panic_trace);
            memcpy(t->slot + err_off, &e, sizeof e);
            t->panic_filled = 1;
        }
        task_unlock(t);
        __atomic_store_n(&t->panic_seen, 1, __ATOMIC_RELEASE);
    }
    return t->slot;
}

void *zell_task_result(const zell_task *t) {
    if (is_yield(t)) return yield_task.slot;   /* resumed after a Yield */
    if (!zell_task_is_done(t)) zell_panic("task result read before completion");
    return checked_slot((zell_task *)t);
}

void *zell_task_result_err(zell_task *t, int64_t err_off) {
    if (is_yield(t)) return t->slot;   /* resumed after a Yield */
    if (!zell_task_is_done(t)) zell_panic("task result read before completion");
    return checked_slot_err(t, err_off);
}

/* A worker blocked over the spare limit, STALL_NS after it blocked (or
 * after its last check): if every worker thread is blocked, none left
 * zell_task_await meanwhile and work is queued, nothing can run that work
 * (progress depends on a thread that does not exist, as in a job that
 * blocks on jobs behind it in the queue), so one more spare is started,
 * past the limit. Blocked threads released by another thread (a consumer
 * on the main thread draining a channel) count as progress: the pool then
 * stays within the limit. One thread per STALL_NS of complete stall. */
static void stall_check(worker *w, int64_t *unblocks0) {
    zell__plat_mutex_lock(&q_mu);
    if (!q_stopping && q_alive == q_blocked && q_unblocks == *unblocks0 &&
        __atomic_load_n(&q_idle, __ATOMIC_RELAXED) == 0 && (queued_work() || any_runnext_full(w))) {
        STAT(stall);
        spawn_worker_locked(1);
    }
    *unblocks0 = q_unblocks;
    zell__plat_mutex_unlock(&q_mu);
}

static void *task_wait(zell_task *t);

void *zell_task_await(zell_task *t) {
    task_wait(t);
    return checked_slot(t);
}

void *zell_task_await_err(zell_task *t, int64_t err_off) {
    task_wait(t);
    return checked_slot_err(t, err_off);
}

/* The entry's await of an async main (ABI.md 11): a panic of main's task
 * ends the process as it always did (message, trace, exit code 2). */
void *zell_task_await_main(zell_task *t) {
    task_wait(t);
    if (t->panic_msg) {
        __atomic_store_n(&t->panic_seen, 1, __ATOMIC_RELEASE);
        zell__panic_traced("", t->panic_msg, t->panic_trace, 1);
    }
    return t->slot;
}

/* The program entry of an async main (ABI.md 10.2, 11): its ramp runs as
 * a job on a worker, with no current task as on the main thread, so the
 * spawns and wake-ups before its first suspension take the worker's local
 * queues instead of the global one. The main thread blocks on a gate task
 * until the ramp returned; the caller then reads `res` as before. */
typedef struct main_ramp {
    zell_main_ramp_fn ramp;
    void *res;
    zell_task *gate;
} main_ramp;

static void main_ramp_job(void *arg) {
    main_ramp *m = arg;
    zell_task *gate = m->gate;  /* m lives on the main thread's stack */
    m->ramp(m->res);
    zell_task_complete(gate, NULL);
    zell_release(gate);         /* the job's reference */
}

void zell_task_run_main(zell_main_ramp_fn ramp, void *res) {
    if (!ramp || !res) zell_panic_nil();
    main_ramp m = { ramp, res, task_new_ex(0, NULL, 0) };
    zell_retain(m.gate);  /* held by the job */
    zell_sched_post(main_ramp_job, &m);
    task_wait(m.gate);
    zell_release(m.gate);
}

static void yield_job(void *arg) {
    zell_task *t = arg;
    zell_task_complete(t, NULL);
    zell_release(t);
}

static void *task_wait(zell_task *t) {
    if (!t) zell_panic_nil();
    if (!t_cur) orphans_clear();   /* a blocking await: no coroutine body before its first suspension runs here */
    if (is_yield(t)) {
        /* A blocking await of task.Yield: a fresh task completed by a job. */
        zell_task *y = task_new_ex(0, NULL, 0);
        zell_retain(y);  /* held by the job */
        zell_sched_post(yield_job, y);
        task_wait(y);
        zell_release(y);
        return t->slot;
    }
    if (zell_task_is_done(t)) return t->slot;
    int over = 0;            /* blocked past the spare limit: watch for a stall */
    int64_t unblocks0 = 0;
    if (t_is_worker) {
        /* Blocking a worker: its queued work must not wait with it. The
         * run-next slot goes to the ring, where thieves find it, and a
         * parked worker is woken to steal (the ring may hold the very job
         * that completes t). Then keep N runnable threads by starting a
         * spare worker if nobody is idle (it retires later), up to
         * q_max_spares of them (spare_limit). */
        worker *w = t_w;
        jobrec rn;
        if (runnext_take(w, &rn)) ring_push_slow(w, &rn);
        if (ring_len(w) != 0) wake_one();
        zell__plat_mutex_lock(&q_mu);
        __atomic_add_fetch(&q_blocked, 1, __ATOMIC_SEQ_CST);
        if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) == 0 && !q_stopping) {
            if (q_alive - q_nworkers < q_max_spares) spawn_worker_locked(1);
            else { over = 1; unblocks0 = q_unblocks; }
        }
        zell__plat_mutex_unlock(&q_mu);
    }
    int parked = __atomic_load_n(&t->state, __ATOMIC_ACQUIRE) != TASK_DONE;
    if (parked) zell__rc_park(1);   /* others may merge our refcount queue (rc.c) */
    /* Link under the task's lock: task_complete sets DONE and takes the
     * list under the same lock, so either we see DONE here or the
     * completer sees us and signals our condition variable. */
    task_lock(t);
    if (t->state != TASK_DONE) {
        awaiter a = { &t_park_mu, &t_park_cv, 0, t->awaiters };
        t->awaiters = &a;
        task_unlock(t);
        zell__plat_mutex_lock(&t_park_mu);
        while (!a.done) {
            if (!over) {
                zell__plat_cond_wait(&t_park_cv, &t_park_mu);
                if (!a.done) __atomic_add_fetch(&g_await_spurious, 1, __ATOMIC_RELAXED);
            } else if (!zell__plat_cond_wait_ns(&t_park_cv, &t_park_mu, STALL_NS) && !a.done) {
                /* q_mu is taken while t_park_mu is held: the completer
                 * takes t_park_mu without q_mu, and no q_mu holder waits
                 * for a park lock, so the order cannot invert. */
                stall_check(t_w, &unblocks0);
            } else if (!a.done) {
                __atomic_add_fetch(&g_await_spurious, 1, __ATOMIC_RELAXED);
            }
        }
        zell__plat_mutex_unlock(&t_park_mu);
    } else {
        task_unlock(t);
    }
    if (parked) zell__rc_unpark();
    if (t_is_worker) {
        zell__plat_mutex_lock(&q_mu);
        __atomic_sub_fetch(&q_blocked, 1, __ATOMIC_SEQ_CST);
        q_unblocks++;
        zell__plat_mutex_unlock(&q_mu);
    }
    return t->slot;
}

typedef struct spawn_ctx {
    zell_task_fn fn;
    zell_task *task;
    void *ctx;
} spawn_ctx;
/* (The spawned job inherits the spawner's task context through
 * zell_sched_post.) */

static void spawn_tramp(void *arg) {
    spawn_ctx *sc = arg;
    zell_task_fn fn = sc->fn;
    zell_task *t = sc->task;
    void *ctx = sc->ctx;
    free(sc);
    fn(t, ctx);
    zell_release(t);  /* the job's reference */
}

zell_task *zell_task_spawn_ex(zell_task_fn fn, void *ctx, int64_t result_size,
                                const zell_value_ops *result_ops) {
    if (!fn) zell_panic_nil();
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    spawn_ctx *sc = malloc(sizeof *sc);
    if (!sc) zell_panic("out of memory (spawn)");
    sc->fn = fn;
    sc->task = t;
    sc->ctx = ctx;
    zell_retain(t);  /* held by the job until fn returns */
    zell_sched_post(spawn_tramp, sc);
    return t;
}

zell_task *zell_task_spawn(zell_task_fn fn, void *ctx) {
    return zell_task_spawn_ex(fn, ctx, 8, NULL);
}

/* ======================================================================
 * Timers (zell_task_delay): one thread, a binary min-heap on (deadline,
 * seq) so equal deadlines fire in insertion order, monotonic clock.
 * Insert, pop and removal (rt_fire of a pending delay) are O(log n).
 * ====================================================================== */

typedef struct timer {
    int64_t deadline;   /* monotonic ns */
    uint64_t seq;       /* insertion order, breaks deadline ties */
    int64_t idx;        /* position in tm_heap */
    zell_task *task;   /* owned: completed at the deadline, or */
    zell_task *cancel; /* owned: cancelled at the deadline (zell_task_set_deadline) */
    struct timer *next; /* batch of due timers in timer_main */
} timer;

static zell__plat_mutex tm_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond tm_cv;
static timer **tm_heap;           /* guarded by tm_mu */
static int64_t tm_n, tm_cap;
static uint64_t tm_seq;
static int tm_running, tm_stop;
static zell__plat_thread tm_thread;

static bool tm_less(const timer *a, const timer *b) {
    return a->deadline < b->deadline || (a->deadline == b->deadline && a->seq < b->seq);
}

static void tm_set(int64_t i, timer *t) { tm_heap[i] = t; t->idx = i; }

static void tm_sift_up(int64_t i) {
    timer *t = tm_heap[i];
    while (i > 0) {
        int64_t p = (i - 1) / 2;
        if (!tm_less(t, tm_heap[p])) break;
        tm_set(i, tm_heap[p]);
        i = p;
    }
    tm_set(i, t);
}

static void tm_sift_down(int64_t i) {
    timer *t = tm_heap[i];
    for (;;) {
        int64_t c = 2 * i + 1;
        if (c >= tm_n) break;
        if (c + 1 < tm_n && tm_less(tm_heap[c + 1], tm_heap[c])) c++;
        if (!tm_less(tm_heap[c], t)) break;
        tm_set(i, tm_heap[c]);
        i = c;
    }
    tm_set(i, t);
}

/* tm_mu held. */
static void tm_push(timer *t) {
    if (tm_n == tm_cap) {
        int64_t cap = tm_cap ? tm_cap * 2 : 64;
        timer **h = realloc(tm_heap, sizeof *h * (size_t)cap);
        if (!h) zell_panic("out of memory (timer)");
        tm_heap = h;
        tm_cap = cap;
    }
    t->seq = tm_seq++;
    tm_set(tm_n++, t);
    tm_sift_up(t->idx);
}

/* Removes t from the heap (tm_mu held); clears its task's back pointer. */
static void tm_remove(timer *t) {
    int64_t i = t->idx;
    timer *last = tm_heap[--tm_n];
    if (last != t) {
        tm_set(i, last);
        if (i > 0 && tm_less(last, tm_heap[(i - 1) / 2])) tm_sift_up(i);
        else tm_sift_down(i);
    }
    if (t->task) __atomic_store_n(&t->task->timer, NULL, __ATOMIC_RELEASE);
}

static void *timer_main(void *unused) {
    (void)unused;
    zell__plat_mutex_lock(&tm_mu);
    while (!tm_stop) {
        if (tm_n == 0) { zell__plat_cond_wait(&tm_cv, &tm_mu); continue; }
        int64_t now = zell__plat_mono_ns();
        int64_t left = tm_heap[0]->deadline - now;
        if (left > 0) {
            zell__plat_cond_wait_ns(&tm_cv, &tm_mu, left);
            continue;
        }
        /* Take every due timer in one go (one lock round trip per batch). */
        timer *due = NULL, **tail = &due;
        while (tm_n > 0 && tm_heap[0]->deadline <= now) {
            timer *t = tm_heap[0];
            tm_remove(t);
            t->next = NULL;
            *tail = t;
            tail = &t->next;
        }
        zell__plat_mutex_unlock(&tm_mu);
        /* The continuations of the due timers are posted as one batch: one
         * global-queue lock round trip per batch instead of one per timer. */
        post_batch batch = { NULL, NULL, 0 };
        t_post_batch = &batch;
        while (due) {
            timer *t = due;
            due = t->next;
            if (t->task) zell_task_try_complete(t->task, NULL);  /* may race with rt_fire */
            if (t->cancel) zell_task_cancel(t->cancel);
            zell_release(t->task);
            zell_release(t->cancel);
            free(t);
        }
        t_post_batch = NULL;
        post_batch_flush(&batch);
        zell__plat_mutex_lock(&tm_mu);
    }
    /* Pending timers at shutdown are dropped without completing. */
    while (tm_n > 0) {
        timer *t = tm_heap[0];
        tm_remove(t);
        zell_release(t->task);
        zell_release(t->cancel);
        free(t);
    }
    free(tm_heap);
    tm_heap = NULL;
    tm_cap = 0;
    zell__plat_mutex_unlock(&tm_mu);
    return NULL;
}

static void timers_start(void) {
    zell__plat_mutex_lock(&tm_mu);
    if (!tm_running) {
        zell__plat_cond_init(&tm_cv);  /* monotonic timed waits where available */
        tm_stop = 0;
        if (!zell__plat_thread_start(&tm_thread, timer_main, NULL))
            zell_panic("cannot create timer thread");
        tm_running = 1;
    }
    zell__plat_mutex_unlock(&tm_mu);
}

static void timers_stop(void) {
    zell__plat_mutex_lock(&tm_mu);
    if (!tm_running) { zell__plat_mutex_unlock(&tm_mu); return; }
    tm_stop = 1;
    zell__plat_cond_signal(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
    zell__plat_thread_join(tm_thread);
    zell__plat_mutex_lock(&tm_mu);
    tm_running = 0;
    zell__plat_cond_destroy(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
}

static void cancel_hook(void (*fn)(zell_task *), zell_task *w);
static void timer_insert(int64_t ms, zell_task *t, zell_task *cancel);

static void delay_cancelled(zell_task *w) { zell_task_rt_fire(w); }

/* A delay completes early when the current task is cancelled (10.11), and
 * when the delay task itself is (a loser of task.Any): its timer is
 * dropped at once with the reference it holds, not when it would fire. */
zell_task *zell_task_delay(int64_t ms) {
    if (ms <= 0) return zell_task_done();
    zell_task *t = task_new_ex(0, NULL, 0);
    t->cancel_fn = delay_cancelled;
    timer_insert(ms, t, NULL);
    cancel_hook(delay_cancelled, t);
    return t;
}

void zell_task_set_deadline(zell_task *t, int64_t ms) {
    if (!t) zell_panic_nil();
    if (is_yield(t)) return;
    if (ms <= 0) { zell_task_cancel(t); return; }
    timer_insert(ms, NULL, t);
}

/* Adds a timer for t (completed) or cancel (cancelled); both retained. */
static void timer_insert(int64_t ms, zell_task *t, zell_task *cancel) {
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    timer *tm = malloc(sizeof *tm);
    if (!tm) zell_panic("out of memory (timer)");
    tm->deadline = zell__plat_mono_ns() + ms * 1000000;
    zell_retain(t);
    zell_retain(cancel);
    tm->task = t;
    tm->cancel = cancel;
    zell__plat_mutex_lock(&tm_mu);
    tm_push(tm);
    if (t) __atomic_store_n(&t->timer, tm, __ATOMIC_RELEASE);
    /* Only a new earliest deadline changes how long the timer thread sleeps. */
    if (tm->idx == 0) zell__plat_cond_signal(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
}

/* ======================================================================
 * Combinators
 * ====================================================================== */

typedef struct all_state {
    zell_task *result;    /* owned */
    int64_t remaining;     /* atomic */
} all_state;

static void all_one_done(void *arg) {
    all_state *st = arg;
    if (__atomic_sub_fetch(&st->remaining, 1, __ATOMIC_ACQ_REL) == 0) {
        zell_task_complete(st->result, NULL);
        zell_release(st->result);
        free(st);
    }
}

zell_task *zell_task_all(zell_task *const *tasks, int64_t n) {
    zell_task *r = task_new_ex(0, NULL, 0);
    all_state *st = malloc(sizeof *st);
    if (!st) zell_panic("out of memory (task_all)");
    zell_retain(r);
    st->result = r;
    st->remaining = n + 1;  /* +1 guard released below */
    for (int64_t i = 0; i < n; i++) {
        if (!tasks[i]) zell_panic_nil();
        if (!add_cont(tasks[i], all_one_done, st, false)) all_one_done(st);
    }
    all_one_done(st);
    return r;
}

/* task.All with cancellation (ABI.md 10.11): the first input to complete
 * with an error (or a panic) cancels the inputs still pending; the result
 * task, completed once every input has settled, holds that input's index
 * (-1: no failure). */
typedef struct allc_state {
    zell_task *result;    /* owned */
    int64_t refs;          /* atomic: outstanding callbacks + guard */
    int64_t winner;        /* atomic */
    int64_t n;
    zell_task **inputs;   /* owned */
    uint8_t *fallible;    /* owned */
} allc_state;

typedef struct allc_node {
    allc_state *st;
    int64_t index;
} allc_node;

static void allc_unref(allc_state *st) {
    if (__atomic_sub_fetch(&st->refs, 1, __ATOMIC_ACQ_REL) == 0) {
        int64_t w = __atomic_load_n(&st->winner, __ATOMIC_ACQUIRE);
        zell_task_complete(st->result, &w);
        zell_release(st->result);
        for (int64_t i = 0; i < st->n; i++) zell_release(st->inputs[i]);
        free(st->inputs);
        free(st->fallible);
        free(st);
    }
}

static bool allc_failed(const zell_task *t, bool fallible) {
    if (is_yield(t)) return false;
    if (t->panic_msg) return true;
    if (!fallible || t->result_size < 8) return false;
    zell_error *e;
    memcpy(&e, t->slot + ZELL_ROUND8(t->result_size) - 8, sizeof e);
    return e != NULL;
}

static void allc_one_done(void *arg) {
    allc_node *nd = arg;
    allc_state *st = nd->st;
    int64_t idx = nd->index;
    free(nd);
    if (allc_failed(st->inputs[idx], st->fallible[idx])) {
        int64_t none = -1;
        if (__atomic_compare_exchange_n(&st->winner, &none, idx, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            for (int64_t i = 0; i < st->n; i++)
                if (i != idx && !zell_task_is_done(st->inputs[i])) zell_task_cancel(st->inputs[i]);
        }
    }
    allc_unref(st);
}

zell_task *zell_task_all_cancel(zell_task *const *tasks, int64_t n, const uint8_t *fallible) {
    zell_task *r = task_new_ex(8, NULL, 0);
    if (n <= 0) {
        int64_t none = -1;
        zell_task_complete(r, &none);
        return r;
    }
    allc_state *st = malloc(sizeof *st);
    if (!st) zell_panic("out of memory (task_all)");
    zell_retain(r);
    st->result = r;
    st->refs = n + 1;
    st->winner = -1;
    st->n = n;
    st->inputs = malloc(sizeof *st->inputs * (size_t)n);
    st->fallible = malloc((size_t)n);
    if (!st->inputs || !st->fallible) zell_panic("out of memory (task_all)");
    for (int64_t i = 0; i < n; i++) {
        if (!tasks[i]) zell_panic_nil();
        zell_retain(tasks[i]);
        st->inputs[i] = tasks[i];
        st->fallible[i] = fallible ? fallible[i] : 0;
    }
    for (int64_t i = 0; i < n; i++) {
        allc_node *nd = malloc(sizeof *nd);
        if (!nd) zell_panic("out of memory (task_all)");
        nd->st = st;
        nd->index = i;
        if (!add_cont(tasks[i], allc_one_done, nd, false)) allc_one_done(nd);
    }
    allc_unref(st);
    return r;
}

typedef struct any_state {
    zell_task *result;    /* owned */
    int64_t refs;          /* atomic: outstanding callbacks + guard */
    int32_t fired;         /* atomic */
    int64_t n;
    zell_task **inputs;   /* owned: the losers are cancelled (10.11) */
} any_state;

typedef struct any_node {
    any_state *st;
    int64_t index;
} any_node;

static void any_unref(any_state *st) {
    if (__atomic_sub_fetch(&st->refs, 1, __ATOMIC_ACQ_REL) == 0) {
        zell_release(st->result);
        for (int64_t i = 0; i < st->n; i++) zell_release(st->inputs[i]);
        free(st->inputs);
        free(st);
    }
}

static void any_one_done(void *arg) {
    any_node *nd = arg;
    any_state *st = nd->st;
    int64_t idx = nd->index;
    free(nd);
    if (__atomic_exchange_n(&st->fired, 1, __ATOMIC_ACQ_REL) == 0) {
        /* The losers first: the awaiter, resumed by the completion, may at
         * once send on a channel a losing receive still waits on. */
        for (int64_t i = 0; i < st->n; i++)
            if (i != idx && !zell_task_is_done(st->inputs[i])) zell_task_cancel(st->inputs[i]);
        zell_task_complete(st->result, &idx);
    }
    any_unref(st);
}

zell_task *zell_task_any(zell_task *const *tasks, int64_t n) {
    if (n <= 0) zell_panic("task.Any called with no tasks");
    zell_task *r = task_new_ex(8, NULL, 0);
    any_state *st = malloc(sizeof *st);
    if (!st) zell_panic("out of memory (task_any)");
    zell_retain(r);
    st->result = r;
    st->refs = n + 1;
    st->fired = 0;
    st->n = n;
    st->inputs = malloc(sizeof *st->inputs * (size_t)n);
    if (!st->inputs) zell_panic("out of memory (task_any)");
    for (int64_t i = 0; i < n; i++) {
        if (!tasks[i]) zell_panic_nil();
        zell_retain(tasks[i]);
        st->inputs[i] = tasks[i];
    }
    for (int64_t i = 0; i < n; i++) {
        any_node *nd = malloc(sizeof *nd);
        if (!nd) zell_panic("out of memory (task_any)");
        nd->st = st;
        nd->index = i;
        if (!add_cont(tasks[i], any_one_done, nd, false)) any_one_done(nd);
    }
    any_unref(st);
    return r;
}

zell_task *zell_task_all_cancel_uniform(zell_task *const *tasks, int64_t n, bool fallible) {
    uint8_t *fl = n > 0 ? malloc((size_t)n) : NULL;
    if (n > 0 && !fl) zell_panic("out of memory (task_all)");
    if (fl) memset(fl, fallible ? 1 : 0, (size_t)n);
    zell_task *r = zell_task_all_cancel(tasks, n, fl);
    free(fl);
    return r;
}

/* ======================================================================
 * Task context (ABI.md 10.8): one owned zell_object pointer per thread,
 * carried by every job and continuation, so a coroutine sees the context
 * of the code that started it after every await.
 * ====================================================================== */

zell_object *zell_task_get_ctx(void) {
    zell_retain(t_ctx);
    return t_ctx;
}

void zell_task_set_ctx(zell_object *ctx) {
    zell_retain(ctx);
    zell_object *old = t_ctx;
    t_ctx = ctx;
    zell_release(old);
}

/* ======================================================================
 * Primitives for task written in Zell (Channel, Mutex, WaitGroup,
 * Timeout, Spawn): signals, cancellable timers, spin locks, yield.
 * ====================================================================== */

zell_task *zell_task_rt_waker(void) { return task_new_ex(0, NULL, 0); }

/* Completes a signal (no-op if already done). If it is a pending
 * zell_task_delay timer, the timer is cancelled first. */
void zell_task_rt_fire(zell_task *t) {
    if (!t) zell_panic_nil();
    if (is_yield(t)) return;
    timer *found = NULL;
    /* t->timer is set before a delay task is handed out and only cleared
     * afterwards, so NULL here means no pending timer: signals, wakers and
     * fired delays skip the global timer lock. */
    if (__atomic_load_n(&t->timer, __ATOMIC_ACQUIRE)) {
        zell__plat_mutex_lock(&tm_mu);
        found = __atomic_load_n(&t->timer, __ATOMIC_ACQUIRE);
        if (found) tm_remove(found);
        zell__plat_mutex_unlock(&tm_mu);
    }
    zell_task_try_complete(t, NULL);
    if (found) {
        zell_release(found->task);
        free(found);
    }
}

void zell_task_spin_lock(int64_t *slot) {
    if (!slot) zell_panic_nil();
    for (int spins = 0;; spins++) {
        if (__atomic_load_n(slot, __ATOMIC_RELAXED) == 0 &&
            __atomic_exchange_n(slot, 1, __ATOMIC_ACQUIRE) == 0)
            return;
        if (spins > 64) zell__plat_yield();
    }
}

void zell_task_spin_unlock(int64_t *slot) {
    if (!slot) zell_panic_nil();
    __atomic_store_n(slot, 0, __ATOMIC_RELEASE);
}

zell_task *zell_task_yield(void) { return &yield_task; }

/* ======================================================================
 * Cancellation (ABI.md 10.11): a tree of tasks (parent = the current task
 * when zell_task_new ran), a cancelled flag per task inherited by new
 * children, and per-task hooks that wake the waits of a cancelled task.
 * ====================================================================== */

/* Collects (moves) the hooks of t and its descendants and flags them; locks
 * are taken parent before child.
 *
 * Races with embedded hooks (channel waits, task_sync.c): a node is only
 * read or moved under its owner's lock, and each linked node owns a
 * reference to its wait, so a wait that completes (on any thread, without
 * the owner's lock) leaves a valid node behind; once moved to `out` the
 * reference moves with it, and fn(w) runs with no lock held and finds a
 * completed wait dequeued (chan_wait_cancelled is idempotent and its header
 * lies past the result, which completion overwrites). Lock order: task
 * locks parent before child; a task lock is never held while a channel
 * lock is taken (hooks run unlocked, and pruned nodes are released after
 * the owner's lock is dropped, since releasing a wait can drop its result). */
static void cancel_collect(zell_task *t, hook **out) {
    task_lock(t);
    if (__atomic_exchange_n(&t->cancelled, 1, __ATOMIC_ACQ_REL) == 0 || t->hooks) {
        hook *h = t->hooks;
        while (h) { hook *n = h->next; h->next = *out; *out = h; h = n; }
        t->hooks = NULL;
        t->nhooks = 0;
    }
    for (zell_task *c = t->child_head; c; c = c->sib_next) cancel_collect(c, out);
    task_unlock(t);
}

void zell_task_cancel(zell_task *t) {
    if (!t) zell_panic_nil();
    if (is_yield(t)) return;
    hook *hs = NULL;
    cancel_collect(t, &hs);
    while (hs) {
        hook *n = hs->next;
        if (hs->has_data) hs->fn_data(hs->w, hs->data);
        else hs->fn(hs->w);
        hook_free(hs);
        hs = n;
    }
    if (t->cancel_fn) t->cancel_fn(t);
}

void zell__task_set_cancel_fn(zell_task *t, void (*fn)(zell_task *)) { t->cancel_fn = fn; }

bool zell_task_is_cancelled(const zell_task *t) {
    return t && __atomic_load_n(&t->cancelled, __ATOMIC_ACQUIRE) != 0;
}

bool zell_task_cur_cancelled(void) { return zell_task_is_cancelled(t_cur); }

/* Adds the filled hook h (owning a reference to h->w) to t's list, or runs
 * it at once if t is already cancelled. Hooks of waits that completed
 * meanwhile are pruned once the list has 8 (released after the lock). */
static void hook_add(zell_task *t, hook *h) {
    task_lock(t);
    if (__atomic_load_n(&t->cancelled, __ATOMIC_ACQUIRE)) {
        task_unlock(t);
        if (h->has_data) h->fn_data(h->w, h->data);
        else h->fn(h->w);
        hook_free(h);
        return;
    }
    hook *dead = NULL;
    if (t->nhooks >= 8) {
        hook **pp = &t->hooks;
        while (*pp) {
            hook *x = *pp;
            if (zell_task_is_done(x->w)) {
                *pp = x->next;
                x->next = dead;
                dead = x;
                t->nhooks--;
            } else {
                pp = &x->next;
            }
        }
    }
    h->seq = t->hook_seq + 1;
    __atomic_store_n(&t->hook_seq, h->seq, __ATOMIC_RELAXED);
    h->next = t->hooks;
    t->hooks = h;
    t->nhooks++;
    task_unlock(t);
    while (dead) { hook *n = dead->next; hook_free(dead); dead = n; }
}

/* Cancel hooks of waits started with no current task once a coroutine was
 * started with none (t_orphan_on: the ramp of an async main before its
 * first suspension, an async call from synchronous code): that coroutine
 * moves those linked during its body onto its own task at its first
 * suspension (zell_task_lazy), as it moves those linked on a current task.
 * Done waits are pruned once the list has 8; the rest are dropped (and
 * recording stops) at the end of every job, at a blocking await and at
 * shutdown, where no such body can still claim them (they could never be
 * cancelled anyway). */
static _Thread_local hook *t_orphans;
static _Thread_local int32_t t_norphans;

static void hooks_free_list(hook *h) {
    while (h) { hook *n = h->next; hook_free(h); h = n; }
}

static void orphan_add(hook *h) {
    hook *dead = NULL;
    if (t_norphans >= 8) {
        for (hook **pp = &t_orphans; *pp;) {
            hook *x = *pp;
            if (zell_task_is_done(x->w)) {
                *pp = x->next;
                x->next = dead;
                dead = x;
                t_norphans--;
            } else {
                pp = &x->next;
            }
        }
    }
    h->seq = ++t_orphan_seq;
    h->next = t_orphans;
    t_orphans = h;
    t_norphans++;
    hooks_free_list(dead);
}

static void orphans_clear(void) {
    t_orphan_on = 0;
    if (!t_orphans) return;
    hook *h = t_orphans;
    t_orphans = NULL;
    t_norphans = 0;
    hooks_free_list(h);
}

/* Unlinks the hooks of *list with seq > mark, in order; *n is the list's
 * count. Returns them (NULL if none) and their number in *moved. */
static hook *hooks_take(hook **list, int32_t *n, int64_t mark, int32_t *moved) {
    hook *out = NULL, **tail = &out;
    *moved = 0;
    for (hook **pp = list; *pp;) {
        hook *x = *pp;
        if (x->seq > mark) {
            *pp = x->next;
            x->next = NULL;
            *tail = x;
            tail = &x->next;
            (*n)--;
            (*moved)++;
        } else {
            pp = &x->next;
        }
    }
    return out;
}

/* Moves to `to` the hooks linked on `from` (the orphans when NULL) after
 * link number `mark`: the waits a coroutine started before its first
 * suspension (ABI.md 10.3), or runs them if `to` is already cancelled.
 * `to` was attached under `from` first, so a cancellation of `from` that
 * races with the move reaches the moved hooks through `to`. */
static void hooks_move(zell_task *from, int64_t mark, zell_task *to) {
    hook *moved;
    int32_t n;
    if (!from) {
        moved = t_orphan_seq > mark ? hooks_take(&t_orphans, &t_norphans, mark, &n) : NULL;
    } else {
        if (__atomic_load_n(&from->hook_seq, __ATOMIC_RELAXED) <= mark) return;
        task_lock(from);
        moved = hooks_take(&from->hooks, &from->nhooks, mark, &n);
        task_unlock(from);
    }
    if (!moved) return;
    task_lock(to);
    if (!__atomic_load_n(&to->cancelled, __ATOMIC_ACQUIRE)) {
        int64_t seq = to->hook_seq;
        hook *last = moved;
        for (hook *x = moved; x; x = x->next) {
            x->seq = ++seq;
            last = x;
        }
        __atomic_store_n(&to->hook_seq, seq, __ATOMIC_RELAXED);
        last->next = to->hooks;
        to->hooks = moved;
        to->nhooks += n;
        task_unlock(to);
        return;
    }
    task_unlock(to);
    while (moved) {
        hook *x = moved->next;
        if (moved->has_data) moved->fn_data(moved->w, moved->data);
        else moved->fn(moved->w);
        hook_free(moved);
        moved = x;
    }
}

/* Registers fn(w) to run when task t is cancelled, in an allocated node. */
static void cancel_hook_on(zell_task *t, void (*fn)(zell_task *), void (*fn_data)(zell_task *, uintptr_t), uintptr_t data, zell_task *w) {
    if (!t && !t_orphan_on) return;
    hook *h = malloc(sizeof *h);
    if (!h) zell_panic("out of memory (cancel hook)");
    zell_retain(w);
    if (fn_data) h->fn_data = fn_data; else h->fn = fn;
    h->has_data = fn_data != NULL;
    h->data = data;
    h->w = w;
    h->embedded = 0;
    if (t) hook_add(t, h); else orphan_add(h);
}

void zell__hook_link(hook *h, zell_task *owner, void (*fn)(zell_task *), zell_task *w) {
    if (!owner && !t_orphan_on) return;
    zell_retain(w);
    h->fn = fn;
    h->has_data = 0;
    h->data = 0;
    /* h->w == w already: set before the wait was published (a waker reads
     * it without the owner's lock, so it is not rewritten here) */
    h->embedded = 1;
    if (owner) hook_add(owner, h); else orphan_add(h);
}

/* The same on the current task. */
static void cancel_hook_ex(void (*fn)(zell_task *), void (*fn_data)(zell_task *, uintptr_t), uintptr_t data, zell_task *w) {
    cancel_hook_on(t_cur, fn, fn_data, data, w);
}

static void cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook_ex(fn, NULL, 0, w); }

void zell__cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook(fn, w); }

void zell__cancel_hook_data(void (*fn)(zell_task *, uintptr_t), zell_task *w, uintptr_t data) {
    cancel_hook_ex(NULL, fn, data, w);
}

static void signal_cancelled(zell_task *w) { zell_task_rt_fire(w); }

zell_task *zell_task_rt_signal(void) {
    zell_task *t = task_new_ex(0, NULL, 0);
    cancel_hook(signal_cancelled, t);
    return t;
}

/* debug.c: the current task (borrowed) and its debug record id. */
zell_task *zell__task_cur(void) { return t_cur; }
int32_t zell__task_dbg_id(const zell_task *t) { return t ? __atomic_load_n(&t->dbg_id, __ATOMIC_ACQUIRE) : 0; }
void zell__task_set_dbg_id(zell_task *t, int32_t id) { __atomic_store_n(&t->dbg_id, id, __ATOMIC_RELEASE); }

/* debug.c: the task t was attached under (borrowed; NULL for none). */
zell_task *zell__task_parent(zell_task *t) {
    if (!t) return NULL;
    task_lock(t);
    zell_task *p = t->parent;
    task_unlock(t);
    return p;
}

/* debug.c: the current task of the code whose continuation was registered
 * first on pending task t (borrowed: the continuation holds it until it
 * runs, after t completes), or NULL. */
zell_task *zell__task_awaiter(zell_task *t) {
    if (!t) return NULL;
    task_lock(t);
    zell_task *w = t->cont0.fn ? t->cont0.cur : NULL;
    task_unlock(t);
    return w;
}

zell_task *zell_task_current(void) {
    zell_retain(t_cur);
    return t_cur;
}

zell_task *zell_task_new_scope(void) {
    zell_task *t = task_new_ex(0, NULL, 0);
    task_attach(t, t_cur);
    return t;
}

zell_task *zell_task_enter_scope(zell_task *scope) {
    zell_task *prev = t_cur;  /* moved to the caller */
    if (scope) {
        zell_retain(scope);
        t_cur = scope;
    } else {
        zell_retain(prev);
        t_cur = prev;
    }
    return prev;
}

void zell_task_leave_scope(zell_task *prev) {
    zell_retain(prev);
    zell_task *old = t_cur;
    t_cur = prev;
    zell_release(old);
}

/* task.ErrCancelled: an immortal sentinel. */
static ZELL_STRING_STORAGE(15) cancelled_msg = ZELL_STRING_INIT(cancelled_msg, "task cancelled");

static zell_error cancelled_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&cancelled_msg,
};

zell_error *zell_task_err_cancelled(void) { return &cancelled_error; }

/* task.Key<T>() ids: unique per process. */
int64_t zell_task_next_key(void) {
    static int64_t next;
    return __atomic_add_fetch(&next, 1, __ATOMIC_RELAXED);
}

/* Debugger support (ABI.md §20): where the lldb/gdb formatters in tools/
 * find a task's state and result slot. They read memory only (no calls into
 * the debuggee), so this works on core dumps too. */
const int64_t zell_debug_task_layout[4] = {
    1, /* version of this table */
    (int64_t)offsetof(struct zell_task, state),
    (int64_t)offsetof(struct zell_task, slot),
    TASK_DONE,
};

/* ======================================================================
 * Synchronous fast path of async functions (ABI.md 10.3)
 *
 * A coroutine ramp makes no task until its first real suspension: the body
 * runs as part of the caller's current task, and when it returns without
 * suspending the result goes to the caller through an out pointer (no
 * task, and no frame either when LLVM sees no suspend point). These three
 * functions are the runtime half of that protocol.
 * ====================================================================== */

/* The coroutine's own task, made at its first suspension: `t` when it
 * already exists, else a new pending task that is attached under the
 * current task and made current (what zell_task_new does at the start of
 * the ramp in the eager protocol), with `ctx0` (+1, the context saved when
 * the ramp started) as the context to put back when the coroutine suspends
 * or completes. Returns the task with two references: the frame's and the
 * one the ramp returns to its caller (the current-task slot holds a third,
 * dropped by restore_parent). */
zell_task *zell_task_lazy(zell_task *t, int64_t result_size, const zell_value_ops *result_ops, zell_object *ctx0, int64_t mark) {
    if (t) return t;
    PROF_START();
    t = task_new_ex(result_size, result_ops, 0);
    t->hdr.refcount += 2;   /* the frame's and the ramp's references: nobody else sees t yet, so no atomics */
    task_attach(t, t_cur);
    hooks_move(t_cur, mark, t);   /* the waits the body started belong to t */
    t->caller_ctx = ctx0;   /* moved */
    t->ctx_saved = 1;
    zell_task *old = t_cur;
    t_cur = t;
    zell_release(old);  /* t->parent holds its own reference */
    PROF_T(lazy);
    return t;
}

/* A coroutine ramp starts: the current context, +1 (what zell_task_get_ctx
 * returns), and in *mark the current task's link number, so that its
 * zell_task_lazy moves the cancel hooks of the waits the body starts
 * before its first suspension onto the coroutine's own task. */
zell_object *zell_coro_enter(int64_t *mark) {
    zell_task *c = t_cur;
    if (c) {
        *mark = __atomic_load_n(&c->hook_seq, __ATOMIC_RELAXED);
    } else {
        *mark = t_orphan_seq;
        t_orphan_on = 1;
    }
    zell_retain(t_ctx);
    return t_ctx;
}

/* A coroutine that made no task returns: the context saved when its ramp
 * started (`ctx0`, +1, consumed) is current again, so whatever the body set
 * (task.With, a diag span, log.Context.Push) does not leak into the caller. */
void zell_coro_leave(zell_object *ctx0) {
    zell_object *old = t_ctx;
    if (old == ctx0) {
        zell_release(ctx0);
        return;
    }
    t_ctx = ctx0;
    zell_release(old);
}

/* An already completed plain task holding `result` (result_size bytes
 * copied; the slot takes over its reference, nothing is retained), +1.
 * Materialises the result of an async function that completed without
 * suspending when the caller keeps the Task<T> instead of awaiting it. */
zell_task *zell_task_completed(int64_t result_size, const zell_value_ops *result_ops, const void *result) {
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    if (result && result_size > 0) memcpy(t->slot, result, (size_t)result_size);
    __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_RELEASE);
    return t;
}


/* ======================================================================
 * Task panics (ABI.md 8.1)
 * ====================================================================== */

_Thread_local void *zell_coro_chain;
_Thread_local void *zell_coro_next;
_Thread_local zell__job_unwind *zell__job_unwind_tls;

int zell__task_running(void) { return t_cur != NULL; }

typedef struct coro_node {
    struct coro_node *prev;
    void *hdl;
} coro_node;

/* A synchronous frame's node (ABI.md 8.1): hdl is ZELL_SYNC_FRAME and the
 * frame's setjmp buffer follows (offset 16). */
typedef struct sync_node {
    struct coro_node *prev;
    void *tag;
    zell__plat_jmpbuf jb;
} sync_node;
_Static_assert(offsetof(sync_node, jb) == 16, "compiled code stores the jump buffer at offset 16");

/* A synchronous frame with an outlined cleanup (ABI.md 8.1): tag is
 * ZELL_SYNC_CLEANUP, then the frame's cleanup function and its argument
 * (the frame's stack pointer). */
typedef struct cleanup_node {
    struct coro_node *prev;
    void *tag;
    void (*fn)(void *sp);
    void *sp;
} cleanup_node;

void zell__coro_unwind(void *stop) {
    coro_node *n;
    while ((n = zell_coro_chain) != NULL && (void *)n != stop) {
        zell_coro_chain = n->prev;   /* read before the frame (and n) is freed */
        void *hdl = n->hdl;
        if (hdl == ZELL_SYNC_CLEANUP) {
            /* Runs on top of the panicking stack: the frame is intact. */
            cleanup_node *c = (cleanup_node *)n;
            c->fn(c->sp);
            continue;
        }
        if (hdl == ZELL_SYNC_FRAME) {
            /* The frame's landing block runs its cleanup and calls
             * zell_unwind_resume, which goes on from the next node. */
            zell__plat_longjmp(((sync_node *)n)->jb);
        }
        if (hdl) {
            /* LLVM switched-resume frame: word 1 is the destroy function,
             * which takes the frame's unwind path (compiled code). */
            void (*destroy)(void *) = ((void (**)(void *))hdl)[1];
            destroy(hdl);
        }
    }
}

void zell_coro_unwound(zell_task *own, zell_object *ctx0) {
    if (!own) {
        zell_coro_leave(ctx0);
        return;
    }
    zell_string *msg, *trace;
    zell__panic_current(&msg, &trace);
    if (!msg) zell_panic("zell_coro_unwound outside a panic");
    task_complete_ex(own, NULL, msg, trace);
    zell_release(own);
}

/* ======================================================================
 * Frame-linked await (ABI.md 10.3)
 * ====================================================================== */

/* The destroy path of a linked frame (ABI.md 10.3): its awaiter is
 * suspended, waiting on this thread's cascade, so the panic goes into its
 * slot word as a task.Panic error and the awaiter is resumed next. */
void zell_coro_unwound_linked(void *word, void *awaiter, zell_object *ctx0) {
    zell_string *msg, *trace;
    zell__panic_current(&msg, &trace);
    if (!msg) zell_panic("zell_coro_unwound_linked outside a panic");
    zell_error *e = zell__panic_error(msg, trace);
    zell_coro_leave(ctx0);
    memcpy(word, &e, sizeof e);
    zell_coro_next = awaiter;
}

void zell_coro_link_panic(zell_error *e, void *value, int64_t size, int64_t err_off) {
    if (err_off < 0) {
        zell_string *msg = zell_task_panic_message(e);
        zell_string *trace = zell_task_panic_trace(e);
        zell_release(e);
        zell__panic_traced_owned("panic in awaited task: ", msg, trace);
    }
    if (err_off + 8 > ZELL_ROUND8(size)) zell__panicf("zell_coro_link_panic: bad error offset");
    if (size > 0) memset(value, 0, (size_t)size);
    memcpy((char *)value + err_off, &e, sizeof e);
}

/* task.ErrPanic and task.Panic errors. */
static ZELL_STRING_STORAGE(14) panic_sentinel_msg = ZELL_STRING_INIT(panic_sentinel_msg, "task panicked");

static zell_error panic_sentinel = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&panic_sentinel_msg,
};

zell_error *zell_task_err_panic(void) { return &panic_sentinel; }

typedef struct panic_payload {
    zell_object hdr;
    zell_string *msg;
    zell_string *trace;
} panic_payload;

static void panic_payload_drop(void *obj) {
    panic_payload *p = obj;
    zell_release(p->msg);
    zell_release(p->trace);
}

static const zell_type_info panic_type = { "task.Panic", 0, panic_payload_drop };

static zell_string *panic_message_fn(void *payload) {
    panic_payload *p = payload;
    static ZELL_STRING_STORAGE(8) prefix = ZELL_STRING_INIT(prefix, "panic: ");
    return zell_string_concat((zell_string *)&prefix, p->msg);
}

zell_error *zell__panic_error(zell_string *msg, zell_string *trace) {
    panic_payload *p = zell_object_new(&panic_type, (int64_t)sizeof(panic_payload));
    zell_retain(msg);
    zell_retain(trace);
    p->msg = msg;
    p->trace = trace;
    zell_error *e = zell_error_new_typed(&panic_type, p, panic_message_fn);
    zell_release(p);
    e->inner = &panic_sentinel;   /* errors.Is(err, task.ErrPanic); immortal */
    return e;
}

static const panic_payload *panic_find(const zell_error *e) {
    return zell_error_as(e, &panic_type);
}

zell_string *zell_task_panic_message(const zell_error *err) {
    const panic_payload *p = panic_find(err);
    if (!p) return zell_string_new("", 0);
    zell_retain(p->msg);
    return p->msg;
}

zell_string *zell_task_panic_trace(const zell_error *err) {
    const panic_payload *p = panic_find(err);
    if (!p) return zell_string_new("", 0);
    zell_retain(p->trace);
    return p->trace;
}
