#include "test.h"
#include "../src/rt_internal.h"   /* zell__await_spurious_wakeups (diag) */
#include <pthread.h>
#include <sched.h>
#include <time.h>

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* ---- spawn / await ---- */

static void square(zell_task *t, void *ctx) {
    int64_t v = (int64_t)(intptr_t)ctx;
    v *= v;
    zell_task_complete(t, &v);
}

TEST(spawn_await) {
    zell_task *t = zell_task_spawn(square, (void *)(intptr_t)12);
    int64_t *r = zell_task_await(t);
    assert(*r == 144);
    assert(zell_task_is_done(t));
    assert(*(int64_t *)zell_task_result(t) == 144);
    zell_release(t);
}

static int64_t counter;
static void bump(zell_task *t, void *ctx) {
    (void)ctx;
    __atomic_add_fetch(&counter, 1, __ATOMIC_RELAXED);
    zell_task_complete(t, NULL);
}

TEST(many_spawns_all) {
    enum { N = 2000 };
    static zell_task *ts[N];
    for (int i = 0; i < N; i++) ts[i] = zell_task_spawn(bump, NULL);
    zell_task *all = zell_task_all(ts, N);
    zell_task_await(all);
    assert(counter == N);
    for (int i = 0; i < N; i++) { assert(zell_task_is_done(ts[i])); zell_release(ts[i]); }
    zell_release(all);
    zell_task *empty = zell_task_all(NULL, 0);
    assert(zell_task_is_done(empty));
    zell_release(empty);
}

/* ---- nested blocking await inside a worker (help mode, no deadlock) ---- */

static void outer(zell_task *t, void *ctx) {
    (void)ctx;
    int64_t sum = 0;
    zell_task *inner[8];
    for (int i = 0; i < 8; i++) inner[i] = zell_task_spawn(square, (void *)(intptr_t)i);
    for (int i = 0; i < 8; i++) { sum += *(int64_t *)zell_task_await(inner[i]); zell_release(inner[i]); }
    zell_task_complete(t, &sum);
}

TEST(nested_await_on_worker) {
    zell_task *ts[64];
    for (int i = 0; i < 64; i++) ts[i] = zell_task_spawn(outer, NULL);
    for (int i = 0; i < 64; i++) {
        assert(*(int64_t *)zell_task_await(ts[i]) == 140);
        zell_release(ts[i]);
    }
}

/* ---- hand-lowered coroutine using the switched-resume frame layout ---- */

typedef struct frame {
    void (*resume)(void *);   /* +0: what llvm.coro.resume calls */
    void (*destroy)(void *);  /* +8 */
    int state;
    zell_task *self;
    zell_task *awaited;
    int64_t arg, acc;
} frame;

static void co_body(void *p) {
    frame *f = p;
    switch (f->state) {
    case 0:
        f->awaited = zell_task_delay(5 + f->arg % 7);
        f->state = 1;
        if (zell_task_add_continuation(f->awaited, zell_coro_resume, f)) return; /* suspend */
        __attribute__((fallthrough)); /* already done: continue without suspending */
    case 1:
        zell_release(f->awaited);
        f->awaited = zell_task_spawn(square, (void *)(intptr_t)f->arg);
        f->state = 2;
        if (zell_task_add_continuation(f->awaited, zell_coro_resume, f)) return;
        __attribute__((fallthrough));
    case 2:
        f->acc = *(int64_t *)zell_task_result(f->awaited) + 1;
        zell_release(f->awaited);
        zell_task_complete(f->self, &f->acc);  /* return value */
        zell_release(f->self);                 /* frame's reference */
        zell_free(f);                          /* final suspend + coro.free */
        return;
    }
}

/* Ramp function of `async func f(arg int) int`. */
static zell_task *async_f(int64_t arg) {
    frame *f = zell_alloc(sizeof *f);
    f->resume = co_body;
    f->self = zell_task_new(8, NULL);
    zell_retain(f->self);        /* +1 for the frame, +1 returned to caller */
    f->arg = arg;
    zell_task *t = f->self;
    co_body(f);                   /* run synchronously until the first suspend */
    return t;
}

TEST(coroutine_protocol) {
    enum { N = 200 };
    zell_task *ts[N];
    for (int i = 0; i < N; i++) ts[i] = async_f(i);
    zell_task *all = zell_task_all(ts, N);
    zell_task_await(all);
    zell_release(all);
    for (int i = 0; i < N; i++) {
        assert(*(int64_t *)zell_task_result(ts[i]) == (int64_t)i * i + 1);
        zell_release(ts[i]);
    }
}

/* ---- frame-linked await (ABI.md 10.3): a C model of a chain of linked
 * frames under a root that makes its task. The leaf defers its
 * registration, the frames above link with plain stores, the root
 * publishes it (zell_coro_register) while a plain thread races to
 * complete the awaited task; run_job resumes the awaiters (zell_coro_next) ---- */

typedef struct lk_res { uintptr_t word; int64_t value; } lk_res;   /* { ptr word; int value } */

typedef struct lk_frame {
    void (*resume)(void *);       /* word 0: what zell_coro_resume calls */
    int depth;                    /* 0: the leaf */
    lk_res *res;                  /* the caller's slot */
    void *link;                   /* the awaiter, once linked */
    zell_object *ctx0;            /* the caller's context, +1 */
    int64_t mark;                 /* zell_coro_enter's hook mark */
    zell_task *wait;              /* leaf: the awaited task */
    zell_task *own;               /* root: its task */
    lk_res sub;                   /* the callee's slot */
} lk_frame;

enum { LK_N = 3000, LK_DEPTH = 3 };
static zell_task *lk_results[LK_N];
static int64_t lk_started, lk_handoffs;
static zell_object *lk_ctx;
static zell_task *lk_q[LK_N];
static int64_t lk_qn;

static void *lk_completer(void *arg) {
    (void)arg;
    for (int64_t k = 0; k < LK_N; k++) {
        zell_task *t;
        while ((t = __atomic_load_n(&lk_q[k], __ATOMIC_ACQUIRE)) == NULL) {}
        zell_task_complete(t, NULL);
        zell_release(t);
    }
    return NULL;
}

/* A frame ends: what coro_return emits for each kind of frame. */
static void lk_return(lk_frame *f, int64_t v) {
    lk_res *res = f->res;
    if (f->own) {
        zell_task_complete(f->own, &v);
        zell_release(f->own);
    } else if (!f->link) {
        zell_coro_leave(f->ctx0);
        res->word = 0;
        res->value = v;
    } else {
        res->value = v;               /* the word stays LINKED */
        zell_coro_leave(f->ctx0);     /* the awaiter's context */
        assert(zell_coro_next == NULL);
        zell_coro_next = f->link;     /* suspended: resumed by run_job next */
        __atomic_add_fetch(&lk_handoffs, 1, __ATOMIC_RELAXED);
    }
    free(f);
}

static void lk_leaf_resumed(void *p) {
    lk_frame *f = p;
    zell_release(f->wait);
    lk_return(f, 1);
}

static void lk_after(lk_frame *f) {
    assert(f->sub.word == 0 || f->sub.word == ZELL_LINK_LINKED);   /* no panic here */
    lk_return(f, f->sub.value + 1);
}

static void lk_mid_resumed(void *p) {
    lk_frame *f = p;
    if (f->own) {
        zell_task *cur = zell_task_current();
        assert(cur == f->own);    /* the chain ran under the root's task */
        zell_release(cur);
    }
    lk_after(f);
}

/* The first-suspension link step: the awaiter from the word, LINKED. */
static zell_object *lk_link_step(lk_frame *f) {
    f->link = (void *)f->res->word;
    f->res->word = ZELL_LINK_LINKED;
    zell_retain(f->ctx0);
    return f->ctx0;
}

static void lk_start(lk_frame *f, lk_res *res, int depth, int root) {
    memset(f, 0, sizeof *f);
    f->depth = depth;
    f->res = res;
    f->ctx0 = zell_coro_enter(&f->mark);
    if (depth == 0) {
        f->resume = lk_leaf_resumed;
        zell_task *t = zell_task_new_plain(0, NULL);
        zell_retain(t);
        int64_t k = __atomic_fetch_add(&lk_qn, 1, __ATOMIC_RELAXED);
        __atomic_store_n(&lk_q[k], t, __ATOMIC_RELEASE);   /* the completer races us */
        f->wait = t;
        if (zell_task_is_done(t)) {
            lk_leaf_resumed(f);
            return;
        }
        zell_coro_defer(t, f, lk_link_step(f));   /* nothing can resume f yet */
        return;
    }
    f->resume = lk_mid_resumed;
    lk_frame *c = malloc(sizeof *c);
    f->sub.word = (uintptr_t)f;
    lk_start(c, &f->sub, depth - 1, 0);
    if (f->sub.word == 0) {
        lk_after(f);
        return;
    }
    assert(f->sub.word == ZELL_LINK_LINKED);
    if (root) {
        zell_task *own = zell_task_lazy(NULL, 8, NULL, f->ctx0, f->mark);
        f->own = own;
        res->word = (uintptr_t)own;
        zell_coro_register(true);   /* publishes the chain; posts it if the task is done */
        return;
    }
    zell_coro_leave(lk_link_step(f));
}

static void lk_root_job(void *arg) {
    int64_t i = (int64_t)(intptr_t)arg;
    if (i % 2) zell_task_set_ctx(lk_ctx);
    zell_object *before = zell_task_get_ctx();
    zell_task *cur0 = zell_task_current();
    lk_res r;
    r.word = 0;
    lk_frame *f = malloc(sizeof *f);
    lk_start(f, &r, LK_DEPTH, 1);
    zell_object *after = zell_task_get_ctx();
    zell_task *cur1 = zell_task_current();
    assert(after == before && cur1 == cur0);   /* context and task isolation across the ramp */
    zell_release(after);
    zell_release(before);
    zell_release(cur0);
    zell_release(cur1);
    lk_results[i] = r.word ? (zell_task *)r.word : zell_task_completed(8, NULL, &r.value);
    __atomic_add_fetch(&lk_started, 1, __ATOMIC_RELEASE);
}

TEST(linked_await_chain) {
    lk_ctx = (zell_object *)zell_string_new("ctx", 3);
    lk_ctx->refcount |= ZELL_RC_NOBIAS;   /* retained on every worker, like a task */
    pthread_t th;
    assert(pthread_create(&th, NULL, lk_completer, NULL) == 0);
    for (int64_t i = 0; i < LK_N; i++) zell_sched_post(lk_root_job, (void *)(intptr_t)i);
    while (__atomic_load_n(&lk_started, __ATOMIC_ACQUIRE) < LK_N) sched_yield();
    for (int i = 0; i < LK_N; i++) {
        assert(*(int64_t *)zell_task_await(lk_results[i]) == LK_DEPTH + 1);
        zell_release(lk_results[i]);
    }
    pthread_join(th, NULL);
    zell_release(lk_ctx);
    printf("    (%lld linked hand-offs)\n", (long long)__atomic_load_n(&lk_handoffs, __ATOMIC_RELAXED));
}

/* ---- delay / any ---- */

TEST(delay_any) {
    int64_t t0 = now_ms();
    zell_task *d = zell_task_delay(50);
    zell_task_await(d);
    assert(now_ms() - t0 >= 50);
    zell_release(d);
    zell_task *z = zell_task_delay(0);
    assert(zell_task_is_done(z));
    zell_release(z);
    zell_task *ds[3] = { zell_task_delay(300), zell_task_delay(10), zell_task_delay(200) };
    zell_task *any = zell_task_any(ds, 3);
    assert(*(int64_t *)zell_task_await(any) == 1);
    zell_release(any);
    zell_task *all = zell_task_all(ds, 3);
    zell_task_await(all);
    zell_release(all);
    for (int i = 0; i < 3; i++) zell_release(ds[i]);
}

/* ---- timer heap: many delays fire in deadline order; rt_fire removes a
 * pending delay from anywhere in the heap ---- */

static int64_t fired_seq, fired_done;
typedef struct { int64_t ms, order; } fire_rec;
static fire_rec fired[400];

static void on_fired(void *arg) {
    int64_t ms = (int64_t)(intptr_t)arg;
    int64_t i = __atomic_fetch_add(&fired_seq, 1, __ATOMIC_ACQ_REL);
    fired[i].ms = ms;
    fired[i].order = i;
    /* Publish the record: the test reads `fired` after `fired_done`. */
    __atomic_fetch_add(&fired_done, 1, __ATOMIC_RELEASE);
}

TEST(timer_heap_order_and_fire) {
    enum { N = 400 };
    static zell_task *ds[N];
    fired_seq = 0;
    fired_done = 0;
    for (int i = 0; i < N; i++) {
        /* Durations 20..215 ms in a scrambled order, 2 per value. */
        int64_t ms = 20 + (int64_t)((i * 37) % (N / 2));
        ds[i] = zell_task_delay(ms);
        if (i % 4 == 3) continue;  /* fired early below */
        zell_task_add_continuation(ds[i], on_fired, (void *)(intptr_t)ms);
    }
    /* Fire every 4th delay now: removal from the middle of the heap. */
    for (int i = 3; i < N; i += 4) {
        zell_task_rt_fire(ds[i]);
        assert(zell_task_is_done(ds[i]));
        zell_task_rt_fire(ds[i]);  /* idempotent */
    }
    zell_task *all = zell_task_all(ds, N);
    zell_task_await(all);
    zell_release(all);
    /* Continuations run as jobs; wait until all have recorded. */
    while (__atomic_load_n(&fired_done, __ATOMIC_ACQUIRE) < N - N / 4) sched_yield();
    /* Timers fire in deadline order; their jobs may run a little out of
     * order on 3 workers, but never by more than a few ms of deadline. */
    for (int64_t i = 1; i < N - N / 4; i++) assert(fired[i].ms + 15 >= fired[i - 1].ms);
    for (int i = 0; i < N; i++) zell_release(ds[i]);
}

/* ---- hand-offs between workers (spin-before-park path): a chain of
 * spawns where each job posts the next, while the others are idle ---- */

typedef struct { zell_task *done; int64_t left; } relay;

static void relay_step(zell_task *t, void *ctx) {
    relay *r = ctx;
    if (--r->left == 0) {
        zell_task_complete(r->done, NULL);
    } else {
        zell_task *next = zell_task_spawn(relay_step, r);
        zell_release(next);
    }
    zell_task_complete(t, NULL);
}

TEST(relay_handoffs) {
    relay r = { zell_task_new_plain(0, NULL), 20000 };
    zell_task *first = zell_task_spawn(relay_step, &r);
    zell_task_await(r.done);
    zell_task_await(first);
    zell_release(first);
    zell_release(r.done);
    /* After the burst the pool goes idle again and still wakes for work. */
    zell_task *d = zell_task_delay(5);
    zell_task_await(d);
    zell_release(d);
    zell_task *t = zell_task_spawn(square, (void *)(intptr_t)9);
    assert(*(int64_t *)zell_task_await(t) == 81);
    zell_release(t);
}

/* ---- a run-next slot behind a job that never yields: the job completes a
 * task (its awaiter lands in this worker's run-next slot) and then spins
 * until that awaiter has run, which it only can if another worker steals
 * the slot after the grace period (the select_many deadlock: a loop that
 * keeps finding completed tasks never gives its worker back). ---- */

static int64_t starved_ran;

static void starved_cont(void *arg) { (void)arg; __atomic_store_n(&starved_ran, 1, __ATOMIC_RELEASE); }

static void hog_job(zell_task *t, void *ctx) {
    (void)ctx;
    zell_task *w = zell_task_new_plain(0, NULL);
    zell_task_add_continuation(w, starved_cont, NULL);
    zell_task_complete(w, NULL);   /* the continuation goes to this worker's run-next slot */
    zell_release(w);
    int64_t t0 = now_ms();
    while (!__atomic_load_n(&starved_ran, __ATOMIC_ACQUIRE)) {
        assert(now_ms() - t0 < 5000);
        for (volatile int i = 0; i < 1000; i++) {}   /* busy, never yields */
    }
    zell_task_complete(t, NULL);
}

TEST(runnext_stolen_from_busy_worker) {
    for (int round = 0; round < 20; round++) {
        starved_ran = 0;
        zell_task *t = zell_task_spawn(hog_job, NULL);
        zell_task_await(t);
        zell_release(t);
    }
}

/* ---- the same with the pool parked: the hog busy-waits first (every
 * other worker gives up spinning and parks), then posts to its run-next
 * slot and keeps the thread for 10 ms more. A parked worker must pick the
 * record up within a few milliseconds (the timed run-next watcher), not
 * when the hog lets go. ---- */

static int64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int64_t parked_ran_ns;

static void parked_cont(void *arg) { (void)arg; __atomic_store_n(&parked_ran_ns, now_ns(), __ATOMIC_RELEASE); }

static void parked_hog(zell_task *t, void *ctx) {
    int64_t *posted = ctx;
    int64_t t0 = now_ns();
    while (now_ns() - t0 < 3000000) {}   /* 3 ms: the others park */
    zell_task *w = zell_task_new_plain(0, NULL);
    zell_task_add_continuation(w, parked_cont, NULL);
    *posted = now_ns();
    zell_task_complete(w, NULL);   /* -> this worker's run-next slot */
    zell_release(w);
    t0 = now_ns();
    while (now_ns() - t0 < 10000000) {}   /* 10 ms, never yields */
    zell_task_complete(t, NULL);
}

TEST(runnext_taken_while_pool_parks) {
    if (zell_sched_workers() < 2) return;
    int64_t worst = 0;
    for (int round = 0; round < 10; round++) {
        parked_ran_ns = 0;
        int64_t posted = 0;
        zell_task *d = zell_task_delay(2);   /* let the pool park */
        zell_task_await(d);
        zell_release(d);
        zell_task *t = zell_task_spawn(parked_hog, &posted);
        zell_task_await(t);
        zell_release(t);
        int64_t ran = __atomic_load_n(&parked_ran_ns, __ATOMIC_ACQUIRE);
        assert(ran != 0);
        if (ran - posted > worst) worst = ran - posted;
    }
    /* the hog holds its thread for 10 ms after the post: a pickup only
     * when it lets go would take >= 10 ms */
    if (getenv("ZELL_TEST_VERBOSE") || worst >= 5000000) fprintf(stderr, "run-next pickup took %lld us\n", (long long)(worst / 1000));
    assert(worst < 5000000);
}

/* ---- results with refcounted payloads ---- */

static void make_str(zell_task *t, void *ctx) {
    (void)ctx;
    zell_string *s = S("result");
    zell_task_complete(t, &s);   /* task retains */
    zell_release(s);
}

TEST(refcounted_result) {
    zell_task *t = zell_task_spawn_ex(make_str, NULL, sizeof(zell_string *), &zell_ops_ref);
    zell_string *s = *(zell_string **)zell_task_await(t);
    assert(STR_IS(s, "result"));
    zell_retain(s);              /* take our own copy before the task dies */
    zell_release(t);
    /* the worker may still be inside make_str or hold its job reference to t, so the
     * exact count is 1..3 here; leaks are checked after shutdown in main() */
    assert(STR_IS(s, "result") && zell_refcount(s) >= 1 && zell_refcount(s) <= 3);
    zell_release(s);
}

/* ---- the words compiled code reads and CASes inline (ABI.md 10.2
 * "Task layout", 10.6) ---- */

extern const int64_t zell_debug_task_layout[4];

TEST(inline_abi_words) {
    assert(zell_debug_task_layout[1] == 24 && zell_debug_task_layout[2] == 224 && zell_debug_task_layout[3] == 1);
    const unsigned char *d = (const unsigned char *)&zell_task_done_task;
    int32_t st;
    memcpy(&st, d + 24, sizeof st);
    assert(zell_task_done() == &zell_task_done_task && st == 1 && *(void *const *)(d + 200) == NULL);
    assert(zell_task_result(zell_task_done()) == d + 224);

    int64_t m[3] = {0, 0, 0};
    assert(zell_task_mutex_lock(m) == &zell_task_done_task && m[0] == 1);   /* free: 0 -> 1 */
    zell_task *w = zell_task_mutex_lock(m);
    assert(!zell_task_is_done(w) && m[0] == 5 && m[1] == (int64_t)(intptr_t)w);   /* locked | waiters */
    int64_t one = 1;
    assert(!__atomic_compare_exchange_n(&m[0], &one, 0, false, __ATOMIC_RELEASE, __ATOMIC_RELAXED));   /* inline Unlock: waiters, slow path */
    zell_task_mutex_unlock(m);   /* frees it and wakes w, which retries in a job */
    zell_task_await(w);
    assert(m[0] == 1 && m[1] == 0 && m[2] == 0);   /* w owns it, nothing else to do */
    one = 1;
    assert(__atomic_compare_exchange_n(&m[0], &one, 0, false, __ATOMIC_RELEASE, __ATOMIC_RELAXED) && m[0] == 0);   /* inline Unlock */
    assert(zell_task_mutex_try_lock(m) && m[0] == 1);
    zell_task_mutex_unlock(m);
    assert(m[0] == 0);
    zell_release(w);
}

/* ---- mutex ---- */

static zell_mutex *mu;
static int64_t guarded;

static void locker(zell_task *t, void *ctx) {
    (void)ctx;
    for (int i = 0; i < 2000; i++) {
        zell_mutex_lock(mu);
        int64_t v = guarded;
        guarded = v + 1;
        zell_mutex_unlock(mu);
    }
    zell_task_complete(t, NULL);
}

TEST(mutex) {
    mu = zell_mutex_new();
    assert(zell_mutex_try_lock(mu));
    assert(!zell_mutex_try_lock(mu));
    zell_task *pending = zell_mutex_lock_async(mu);
    assert(!zell_task_is_done(pending));
    zell_mutex_unlock(mu);                 /* hands ownership to `pending` */
    zell_task_await(pending);
    assert(!zell_mutex_try_lock(mu));
    zell_mutex_unlock(mu);
    zell_release(pending);

    zell_task *ts[8];
    for (int i = 0; i < 8; i++) ts[i] = zell_task_spawn(locker, NULL);
    for (int i = 0; i < 8; i++) { zell_task_await(ts[i]); zell_release(ts[i]); }
    assert(guarded == 8 * 2000);
    zell_release(mu);
}

/* ---- channels ---- */

typedef struct { zell_chan *ch; int64_t n; } prod_ctx;

static void producer(zell_task *t, void *ctx) {
    prod_ctx *pc = ctx;
    for (int64_t i = 1; i <= pc->n; i++) zell_chan_send(pc->ch, &i);
    zell_chan_close(pc->ch);
    zell_task_complete(t, NULL);
}

static void chan_roundtrip(int64_t cap) {
    zell_chan *ch = zell_chan_new(8, NULL, cap);
    prod_ctx pc = { ch, 5000 };
    zell_task *p = zell_task_spawn(producer, &pc);
    int64_t v, sum = 0, n = 0;
    while (zell_chan_recv(ch, &v)) { sum += v; n++; }
    assert(n == 5000 && sum == 5000 * 5001 / 2);
    assert(!zell_chan_recv(ch, &v) && v == 0);   /* closed: zero value, ok=false */
    zell_task_await(p);
    zell_release(p);
    zell_release(ch);
}

TEST(channel_unbuffered) { chan_roundtrip(0); }
TEST(channel_buffered) { chan_roundtrip(16); }

TEST(channel_async_and_strings) {
    zell_chan *ch = zell_chan_new(sizeof(zell_string *), &zell_ops_ref, 2);
    zell_task *r1 = zell_chan_recv_async(ch);         /* waits */
    assert(!zell_task_is_done(r1));
    zell_string *a = S("a"), *b = S("b"), *c = S("c");
    zell_task *s1 = zell_chan_send_async(ch, &a);      /* hands off to r1 */
    assert(zell_task_is_done(s1) && zell_task_is_done(r1));
    unsigned char *res = zell_task_result(r1);
    assert(*(int64_t *)res == 1 && *(zell_string **)(res + 8) == a);
    zell_task *s2 = zell_chan_send_async(ch, &b);      /* buffered */
    zell_task *s3 = zell_chan_send_async(ch, &c);      /* buffered */
    zell_task *s4 = zell_chan_send_async(ch, &a);      /* blocks: buffer full */
    assert(zell_task_is_done(s2) && zell_task_is_done(s3) && !zell_task_is_done(s4));
    assert(zell_chan_len(ch) == 2);
    zell_string *got;
    assert(zell_chan_recv(ch, &got) && got == b);       /* refills from s4 */
    zell_release(got);
    assert(zell_task_is_done(s4));
    zell_release(s1); zell_release(s2); zell_release(s3); zell_release(s4);
    zell_release(r1);
    zell_release(ch);                                   /* drops buffered c, a */
    assert(zell_refcount(a) == 1 && zell_refcount(b) == 1 && zell_refcount(c) == 1);
    zell_release(a); zell_release(b); zell_release(c);
}

/* ---- work stealing: one worker floods its local queue (the ring
 * overflows to the global queue), the other workers drain it ---- */

/* Every worker's first refcount update of an unowned object takes an owner
 * slot (rc.c zell__rc_thread_enable) and publishes the shutdown drain hook:
 * concurrently on all three workers here (a data race under `make tsan`
 * while that hook was a plain store). Runs before any other job. */
static int32_t rce_arrived, rce_done;

static void rc_enable_job(void *arg) {
    (void)arg;
    __atomic_add_fetch(&rce_arrived, 1, __ATOMIC_ACQ_REL);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&rce_arrived, __ATOMIC_ACQUIRE) < 3 && now_ms() - t0 < 1000) {}
    zell_string *s = zell_string_new("rc", 2);
    zell_retain(s);
    zell_release(s);
    zell_release(s);
    __atomic_add_fetch(&rce_done, 1, __ATOMIC_RELEASE);
}

TEST(rc_enable_concurrent) {
    for (int i = 0; i < 3; i++) zell_sched_post(rc_enable_job, NULL);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&rce_done, __ATOMIC_ACQUIRE) < 3) {
        assert(now_ms() - t0 < 10000);
        sched_yield();
    }
}

/* Channel waits link their embedded cancel hook after the wait is queued
 * (task_sync.c): the waker reads hook.w meanwhile without the owner's lock,
 * so linking must not write it (a data race under `make tsan` while
 * zell__hook_link rewrote it). Both sides run in a task scope, so both
 * link hooks. */
enum { CH_N = 3000 };
static int64_t ch_handle, ch_recv_done;

static void ch_receiver(void *arg) {
    (void)arg;
    for (int i = 0; i < CH_N; i++) {
        zell_task *t = zell_task_chan_recv(ch_handle);
        int64_t *slot = zell_task_await(t);
        assert(slot[0] == i && slot[1] == 0);
        zell_release(t);
    }
    __atomic_store_n(&ch_recv_done, 1, __ATOMIC_RELEASE);
}

TEST(chan_wait_hook_link) {
    ch_handle = zell_task_chan_new(0, 8, NULL, 16, NULL, 8, 16, NULL, 8);
    ch_recv_done = 0;
    zell_task *owner = zell_task_new_plain(0, NULL);
    zell_task *prev = zell_task_enter_scope(owner);
    zell_sched_post(ch_receiver, NULL);   /* runs as task `owner` too */
    for (int64_t i = 0; i < CH_N; i++) {
        zell_task *t = zell_task_chan_send(ch_handle, &i);
        zell_task_await(t);
        zell_release(t);
    }
    int64_t t0 = now_ms();
    while (!__atomic_load_n(&ch_recv_done, __ATOMIC_ACQUIRE)) {
        assert(now_ms() - t0 < 10000);
        sched_yield();
    }
    zell_task_leave_scope(prev);
    zell_release(prev);
    zell_task_complete(owner, NULL);
    zell_release(owner);
    zell_task_chan_release(ch_handle);
}

enum { FLOOD = 4000 };
static pthread_t flood_thread[FLOOD];
static int64_t flood_done;

static void flood_job(void *arg) {
    int64_t i = (int64_t)(intptr_t)arg;
    flood_thread[i] = pthread_self();
    __atomic_add_fetch(&flood_done, 1, __ATOMIC_RELEASE);
}

static void flooder(zell_task *t, void *ctx) {
    (void)ctx;
    for (int64_t i = 0; i < FLOOD; i++) zell_sched_post(flood_job, (void *)(intptr_t)i);
    zell_task_complete(t, NULL);
}

TEST(work_stealing) {
    flood_done = 0;
    zell_task *f = zell_task_spawn(flooder, NULL);
    zell_task_await(f);
    zell_release(f);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&flood_done, __ATOMIC_ACQUIRE) < FLOOD) {
        assert(now_ms() - t0 < 10000);
        sched_yield();
    }
    /* Every job ran exactly once (the counter), on more than one worker. */
    int distinct = 1;
    for (int64_t i = 1; i < FLOOD; i++) {
        int seen = 0;
        for (int64_t j = 0; j < i && !seen; j++) seen = pthread_equal(flood_thread[i], flood_thread[j]);
        if (!seen) distinct++;
    }
    assert(distinct >= 2);
}

/* ---- run-next fairness: a pair of tasks completing each other in a tight
 * loop on one worker (the continuation always lands in its run-next slot)
 * must not starve a job queued in that worker's ring. The other two
 * workers are kept busy, so nothing can steal the queued job. ---- */

static int64_t busy_running, busy_release;
static int64_t pp_steps, third_ran_at;

static void busy_job(zell_task *t, void *ctx) {
    (void)ctx;
    __atomic_add_fetch(&busy_running, 1, __ATOMIC_RELEASE);
    while (!__atomic_load_n(&busy_release, __ATOMIC_ACQUIRE)) sched_yield();
    zell_task_complete(t, NULL);
}

static void third_job(void *arg) {
    (void)arg;
    __atomic_store_n(&third_ran_at, pp_steps, __ATOMIC_RELEASE);
}

static void pp_step(void *arg) {
    zell_task *done = arg;
    if (++pp_steps >= 20000) {
        zell_task_complete(done, NULL);
        return;
    }
    /* complete a fresh task whose continuation is this function: it goes
     * to this worker's run-next slot */
    zell_task *t = zell_task_new_plain(0, NULL);
    zell_task_add_continuation(t, pp_step, done);
    zell_task_complete(t, NULL);
    zell_release(t);
}

static void pp_start(void *arg) {
    zell_sched_post(third_job, NULL);   /* into this worker's ring */
    pp_step(arg);
}

TEST(runnext_fairness) {
    busy_running = busy_release = 0;
    pp_steps = 0;
    third_ran_at = -1;
    zell_task *b1 = zell_task_spawn(busy_job, NULL), *b2 = zell_task_spawn(busy_job, NULL);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&busy_running, __ATOMIC_ACQUIRE) < 2) { assert(now_ms() - t0 < 5000); sched_yield(); }
    zell_task *done = zell_task_new_plain(0, NULL);
    zell_sched_post(pp_start, done);
    zell_task_await(done);
    int64_t at = __atomic_load_n(&third_ran_at, __ATOMIC_ACQUIRE);
    assert(at >= 0 && at < 1000);   /* ran within the first streak, not after the 20000 steps */
    __atomic_store_n(&busy_release, 1, __ATOMIC_RELEASE);
    zell_task_await(b1);
    zell_task_await(b2);
    zell_release(b1);
    zell_release(b2);
    zell_release(done);
}

/* ---- channels: cap 0, 1 and N with waiting senders and receivers; close
 * with waiters; 10k producers into one consumer, every producer completes;
 * cancellation of a pending receive and of a pending send ---- */

static void chan_cap(int64_t cap) {
    zell_chan *ch = zell_chan_new(8, NULL, cap);
    /* fill the buffer, then one more sender waits */
    zell_task *sends[4];
    for (int64_t i = 0; i < 4; i++) sends[i] = zell_chan_send_async(ch, &i);
    for (int64_t i = 0; i < 4; i++) assert(zell_task_is_done(sends[i]) == (i < cap));
    assert(zell_chan_len(ch) == (cap < 4 ? cap : 4));
    /* receives take the buffer in order, then the blocked senders in order */
    for (int64_t i = 0; i < 4; i++) {
        int64_t v;
        assert(zell_chan_recv(ch, &v) && v == i);
    }
    for (int64_t i = 0; i < 4; i++) { zell_task_await(sends[i]); zell_release(sends[i]); }
    /* receivers wait, each is woken exactly once, in order */
    zell_task *recvs[3];
    for (int i = 0; i < 3; i++) { recvs[i] = zell_chan_recv_async(ch); assert(!zell_task_is_done(recvs[i])); }
    for (int64_t i = 10; i < 13; i++) { zell_task *s = zell_chan_send_async(ch, &i); assert(zell_task_is_done(s)); zell_release(s); }
    for (int i = 0; i < 3; i++) {
        unsigned char *r = zell_task_await(recvs[i]);
        assert(*(int64_t *)r == 1 && *(int64_t *)(r + 8) == 10 + i);
        zell_release(recvs[i]);
    }
    /* close with waiting receivers: all fail at once; later receives too */
    zell_task *r1 = zell_chan_recv_async(ch), *r2 = zell_chan_recv_async(ch);
    zell_chan_close(ch);
    assert(*(int64_t *)zell_task_await(r1) == 0 && *(int64_t *)zell_task_await(r2) == 0);
    zell_release(r1);
    zell_release(r2);
    int64_t v = 7;
    assert(!zell_chan_recv(ch, &v) && v == 0);
    zell_release(ch);
}

TEST(channel_capacities) {
    chan_cap(0);
    chan_cap(1);
    chan_cap(3);
}

typedef struct { zell_chan *ch; int64_t id; int64_t each; } fan_ctx;

static void fan_producer(zell_task *t, void *ctx) {
    fan_ctx *fc = ctx;
    for (int64_t i = 0; i < fc->each; i++) {
        int64_t v = fc->id * fc->each + i;
        zell_chan_send(fc->ch, &v);
    }
    zell_task_complete(t, NULL);
}

TEST(channel_fan_in_10k) {
    enum { P = 10000, EACH = 10 };
    zell__sched_threads_peak();   /* restart the count for this test */
    int64_t t0 = now_ms();
    zell_chan *ch = zell_chan_new(8, NULL, 64);
    static fan_ctx fcs[P];
    static zell_task *ps[P];
    for (int64_t i = 0; i < P; i++) {
        fcs[i] = (fan_ctx){ ch, i, EACH };
        ps[i] = zell_task_spawn(fan_producer, &fcs[i]);
    }
    int64_t sum = 0, v;
    for (int64_t n = 0; n < P * EACH; n++) { assert(zell_chan_recv(ch, &v)); sum += v; }
    assert(sum == (int64_t)P * EACH * ((int64_t)P * EACH - 1) / 2);
    for (int64_t i = 0; i < P; i++) { zell_task_await(ps[i]); assert(zell_task_is_done(ps[i])); zell_release(ps[i]); }
    assert(zell_chan_len(ch) == 0);
    zell_release(ch);
    /* Blocked producers do not turn into threads beyond the spare limit
     * (ABI.md 10.5): the pool stays at N core workers plus the spares,
     * plus at most one thread per 10 ms of complete stall (a starved main
     * thread on a loaded machine; here normally none). */
    int64_t peak = zell__sched_threads_peak(), max = zell__sched_max_threads();
    int64_t allowed = max + (now_ms() - t0) / 10 + 1;
    if (peak > allowed) { fprintf(stderr, "  %lld worker threads alive at once, limit %lld\n", (long long)peak, (long long)allowed); assert(0); }
    assert(max == zell_sched_workers() + (zell_sched_workers() * 4 > 16 ? zell_sched_workers() * 4 : 16));
}

/* Regression: 1000 producers block in zell_chan_send on a full channel,
 * each on its own (spare) worker thread, all inside zell_task_await. Every
 * receive completes one sender and must wake only that thread. The old
 * process-wide condition variable woke all of them on each completion
 * (~P*P/2 spurious wake-ups): a thundering herd that hung the fan-in test
 * under TSan. A spurious wake-up here means a thread that found its task
 * still pending; pthread may produce a stray one, never a herd. */
TEST(await_wakes_one_thread) {
    enum { P = 1000 };
    int64_t before = zell__await_spurious_wakeups();
    zell_chan *ch = zell_chan_new(8, NULL, 1);
    static fan_ctx fcs[P];
    static zell_task *ps[P];
    for (int64_t i = 0; i < P; i++) {
        fcs[i] = (fan_ctx){ ch, i, 1 };
        ps[i] = zell_task_spawn(fan_producer, &fcs[i]);
    }
    /* let the producers pile up: each blocked send starts a spare worker */
    struct timespec ts = { 0, 300 * 1000 * 1000 };
    nanosleep(&ts, NULL);
    int64_t sum = 0, v;
    for (int64_t n = 0; n < P; n++) { assert(zell_chan_recv(ch, &v)); sum += v; }
    assert(sum == (int64_t)P * (P - 1) / 2);
    for (int64_t i = 0; i < P; i++) { zell_task_await(ps[i]); zell_release(ps[i]); }
    zell_release(ch);
    int64_t spurious = zell__await_spurious_wakeups() - before;
    if (spurious >= P) { fprintf(stderr, "  %lld spurious await wake-ups for %d completions\n", (long long)spurious, P); assert(0); }
}

/* A pending receive and a pending send are cancelled through their own
 * task (task.Any cancels losers this way) and leave the queues. */
TEST(channel_cancel_waits) {
    zell_chan *ch = zell_chan_new(8, NULL, 0);
    zell_task *r = zell_chan_recv_async(ch);
    assert(!zell_task_is_done(r));
    zell_task_cancel(r);
    unsigned char *res = zell_task_await(r);
    assert(*(int64_t *)res == 0);
    zell_release(r);
    int64_t v = 5;
    zell_task *s = zell_chan_send_async(ch, &v);
    assert(!zell_task_is_done(s));
    zell_task_cancel(s);
    zell_task_await(s);
    zell_release(s);
    /* nothing is left queued: a receiver now waits, a sender hands off */
    zell_task *r2 = zell_chan_recv_async(ch);
    assert(!zell_task_is_done(r2));
    zell_task *s2 = zell_chan_send_async(ch, &v);
    assert(zell_task_is_done(s2) && zell_task_is_done(r2) && *(int64_t *)((unsigned char *)zell_task_result(r2) + 8) == 5);
    zell_release(s2);
    zell_release(r2);
    zell_chan_close(ch);
    zell_release(ch);
}

/* Queued waits started inside a scope are cancelled through the hook on
 * the scope (a Recv in a cancelled scope); a wait that completed first is
 * left alone (its result is intact), and dropping the scope releases every
 * hook node, so nothing leaks. */
TEST(channel_cancel_from_scope) {
    int64_t live = zell_live_objects();
    zell_chan *ch = zell_chan_new(8, NULL, 0), *ch2 = zell_chan_new(8, NULL, 0);
    zell_task *scope = zell_task_new_scope();
    zell_task *prev = zell_task_enter_scope(scope);
    zell_task *done = zell_chan_recv_async(ch2);   /* completes before the cancel */
    zell_task *r = zell_chan_recv_async(ch);
    int64_t v = 7;
    zell_task *s = zell_chan_send_async(ch2, &v);
    assert(zell_task_is_done(s) && zell_task_is_done(done));
    zell_release(s);
    /* completed waits are pruned from the scope's hook list on later links:
     * the scope keeps at most a few of them alive, not all 100 */
    int64_t before = zell_live_objects();
    for (int i = 0; i < 100; i++) {
        zell_task *q = zell_chan_recv_async(ch2);
        zell_task *p = zell_chan_send_async(ch2, &v);
        assert(zell_task_is_done(q) && zell_task_is_done(p));
        zell_release(p);
        zell_release(q);
    }
    assert(zell_live_objects() - before <= 9);
    s = zell_chan_send_async(ch2, &v);           /* queued: nobody receives on ch2 */
    zell_task_leave_scope(prev);
    zell_release(prev);
    assert(!zell_task_is_done(r) && !zell_task_is_done(s));
    zell_task_cancel(scope);
    assert(*(int64_t *)zell_task_await(r) == 0);
    zell_task_await(s);
    unsigned char *dr = zell_task_await(done);   /* its header survived the result */
    assert(*(int64_t *)dr == 1 && *(int64_t *)(dr + 8) == 7);
    zell_release(r);
    zell_release(s);
    zell_release(done);
    zell_release(scope);
    /* both queues are empty again */
    zell_task *r2 = zell_chan_recv_async(ch2);
    assert(!zell_task_is_done(r2));
    zell_task *s2 = zell_chan_send_async(ch2, &v);
    assert(zell_task_is_done(s2) && zell_task_is_done(r2));
    zell_release(s2);
    zell_release(r2);
    zell_chan_close(ch);
    zell_chan_close(ch2);
    zell_release(ch);
    zell_release(ch2);
    assert(zell_live_objects() == live);
}

/* task.Any(Recv, Delay) where the Delay wins: the losing Recv is cancelled
 * through its own task (cancel_fn); its hook on the scope goes with the
 * scope, so nothing leaks. The Delay is done before the Any, so the loser is
 * cancelled on this thread: with the Any completing on a worker, the
 * worker drops the last channel reference right after this thread's
 * relaxed owner decrement (rc.c store_b), which TSan reports as a race on
 * the freed block. */
TEST(channel_any_delay_wins) {
    int64_t live = 0;
    for (int i = 0; i < 21; i++) {
        if (i == 1) {   /* after the first round's one-time setup, once the timer thread let go */
            struct timespec ts = { 0, 20000000 };
            do { live = zell_live_objects(); nanosleep(&ts, NULL); } while (zell_live_objects() != live);
        }
        zell_chan *ch = zell_chan_new(8, NULL, 0);
        zell_task *scope = zell_task_new_scope();
        zell_task *prev = zell_task_enter_scope(scope);
        zell_task *in[2] = { zell_chan_recv_async(ch), zell_task_delay(1) };
        zell_task_await(in[1]);
        zell_task *any = zell_task_any(in, 2);
        zell_task_leave_scope(prev);
        zell_release(prev);
        assert(*(int64_t *)zell_task_await(any) == 1);
        assert(*(int64_t *)zell_task_await(in[0]) == 0);
        zell_release(any);
        zell_release(in[0]);
        zell_release(in[1]);
        zell_release(scope);
        zell_chan_close(ch);
        zell_release(ch);
    }
    /* the timer thread drops its reference to a Delay just after firing it */
    int64_t t0 = now_ms();
    while (zell_live_objects() != live) {
        assert(now_ms() - t0 < 5000);
        sched_yield();
    }
}

/* A coroutine `async func recv(ch) int { return try await ch.Recv() else -1 }`
 * by hand (ABI.md 10.3): the receive starts before the first suspension, so
 * its cancel hook goes on the current task (or, with none, the orphan
 * list) and zell_task_lazy moves it onto the coroutine's task. A loser of
 * task.Any is cancelled, so its queued receive leaves the channel and the
 * next Send reaches a fresh receiver (LANGUAGE_FEEDBACK 42). */
typedef struct { zell_task *own, *w; } rw_frame;

static void rw_resumed(void *fr) {
    rw_frame *f = fr;
    int64_t *r = zell_task_result(f->w);
    int64_t v = r[0] ? r[1] : -1;
    zell_release(f->w);
    zell_task *own = f->own;
    free(f);
    zell_task_complete(own, &v);
    zell_release(own);   /* the frame's reference */
}

static zell_task *rw_start(zell_chan *ch) {
    rw_frame *f = malloc(sizeof *f);
    int64_t mark;
    zell_object *ctx0 = zell_coro_enter(&mark);
    f->w = zell_chan_recv_async(ch);
    assert(!zell_task_is_done(f->w));
    f->own = zell_task_lazy(NULL, 8, NULL, ctx0, mark);
    zell_task *own = f->own;   /* the ramp's reference, for the caller */
    if (!zell_task_add_continuation(f->w, rw_resumed, f)) rw_resumed(f);
    return own;
}

static void any_loser_round(int scoped) {
    zell_chan *ch = zell_chan_new(8, NULL, 0);
    zell_task *scope = NULL, *prev = NULL;
    if (scoped) {
        scope = zell_task_new_scope();
        prev = zell_task_enter_scope(scope);
    }
    zell_task *in[2] = { rw_start(ch), zell_task_delay(1) };
    zell_task *any = zell_task_any(in, 2);
    if (scoped) {
        zell_task_leave_scope(prev);
        zell_release(prev);
    }
    assert(*(int64_t *)zell_task_await(any) == 1);
    assert(*(int64_t *)zell_task_await(in[0]) == -1);   /* cancelled, not left queued */
    zell_task *w2 = zell_chan_recv_async(ch);
    int64_t v = 42;
    zell_task *s = zell_chan_send_async(ch, &v);
    assert(zell_task_is_done(w2) && zell_task_is_done(s));   /* handed to the fresh receiver */
    int64_t *r = zell_task_result(w2);
    assert(r[0] == 1 && r[1] == 42);
    zell_release(s);
    zell_release(w2);
    zell_release(any);
    zell_release(in[0]);
    zell_release(in[1]);
    zell_release(scope);
    zell_chan_close(ch);
    zell_release(ch);
}

TEST(channel_any_recv_loser_leaves_queue) {
    for (int i = 0; i < 1000; i++) any_loser_round(i % 2);
}

/* A cancel (of the scope or of the wait itself) racing the completion of a
 * queued receive and of a queued send, 1000 rounds each (run under tsan). */
typedef struct { zell_chan *ch; int64_t got; int recv; } race_arg;

static void *race_peer(void *p) {
    race_arg *a = p;
    int64_t v = 7;
    if (a->recv) {
        int64_t out = 0;
        a->got = zell_chan_recv(a->ch, &out) ? out : -1;
    } else {
        zell_chan_send(a->ch, &v);
    }
    return NULL;
}

TEST(channel_cancel_races_completion) {
    int64_t live = zell_live_objects();
    for (int round = 0; round < 2000; round++) {
        int sender = round & 1, self = (round >> 1) & 1;
        /* a queued Recv vs. a peer Send (buffered: the send never blocks), or
         * a queued Send vs. a peer Recv (unbuffered: Close ends the peer) */
        race_arg a = { zell_chan_new(8, NULL, sender ? 0 : 1), 0, sender };
        zell_task *scope = zell_task_new_scope();
        zell_task *prev = zell_task_enter_scope(scope);
        int64_t v = 7;
        zell_task *w = sender ? zell_chan_send_async(a.ch, &v) : zell_chan_recv_async(a.ch);
        zell_task_leave_scope(prev);
        zell_release(prev);
        pthread_t th;
        pthread_create(&th, NULL, race_peer, &a);
        zell_task_cancel(self ? w : scope);
        unsigned char *res = zell_task_await(w);
        if (!sender) assert(*(int64_t *)res == 0 || *(int64_t *)(res + 8) == 7);
        if (sender) zell_chan_close(a.ch);   /* a cancelled send leaves the peer waiting */
        pthread_join(th, NULL);
        if (sender) assert(a.got == 7 || a.got == -1);
        else zell_chan_close(a.ch);
        zell_release(w);
        zell_release(scope);
        zell_release(a.ch);
    }
    /* the peer's last releases of objects this thread owns are merged here
     * (rc.c biased counts) */
    int64_t t0 = now_ms();
    while (zell_live_objects() != live) {
        assert(now_ms() - t0 < 5000);
        zell__rc_poll();
        sched_yield();
    }
}

/* ---- mutex: 1k tasks contending, TryLock, lock of a free mutex is the
 * immortal done task, unlock without lock panics ---- */

static zell_mutex *cmu;
static int64_t cguarded, cdone;
static zell_task *call_done;

typedef struct { int64_t left; } contender;

static void contender_step(void *arg);

/* Owns the lock: the critical section, then the next iteration. */
static void contender_locked(void *arg) {
    contender *c = arg;
    int64_t v = cguarded;
    if (c->left % 7 == 0) sched_yield();
    cguarded = v + 1;
    zell_mutex_unlock(cmu);
    if (--c->left == 0) {
        if (__atomic_add_fetch(&cdone, 1, __ATOMIC_ACQ_REL) == 1000) zell_task_complete(call_done, NULL);
        return;
    }
    contender_step(arg);
}

/* Takes the lock without blocking the worker: a free mutex is owned at
 * once (the immortal done task), otherwise the continuation runs when the
 * unlocker hands the lock over (the 10.3 protocol, as compiled code does). */
static void contender_step(void *arg) {
    zell_task *t = zell_mutex_lock_async(cmu);
    bool wait = zell_task_add_continuation(t, contender_locked, arg);
    zell_release(t);   /* a pending task is kept alive by the mutex queue */
    if (!wait) contender_locked(arg);
}

TEST(mutex_contention_1k) {
    enum { N = 1000 };
    cmu = zell_mutex_new();
    cguarded = cdone = 0;
    call_done = zell_task_new_plain(0, NULL);
    static contender cs[N];
    for (int i = 0; i < N; i++) { cs[i].left = 50; zell_sched_post(contender_step, &cs[i]); }
    zell_task_await(call_done);
    zell_release(call_done);
    assert(cguarded == N * 50);
    /* free again: TryLock succeeds once, a Lock of a free mutex is done at
     * once (the shared immortal task), waiters are woken oldest first */
    zell_task *free_lock = zell_mutex_lock_async(cmu);
    assert(zell_task_is_done(free_lock));
    assert(!zell_mutex_try_lock(cmu));
    zell_task *w1 = zell_mutex_lock_async(cmu), *w2 = zell_mutex_lock_async(cmu);
    assert(!zell_task_is_done(w1) && !zell_task_is_done(w2));
    zell_mutex_unlock(cmu);
    zell_task_await(w1);   /* woken first; nobody barges here */
    assert(!zell_task_is_done(w2));
    zell_mutex_unlock(cmu);
    zell_task_await(w2);
    zell_mutex_unlock(cmu);
    assert(zell_mutex_try_lock(cmu));
    zell_mutex_unlock(cmu);
    zell_release(w1);
    zell_release(w2);
    zell_release(free_lock);
    zell_release(cmu);
}

/* ---- mutex: a slow Lock holding the queue guard of a mutex that was freed
 * meanwhile (state == guard, not locked) owns the right to take it; a fast
 * Lock/TryLock must not CAS the locked bit in under it (regression: both
 * owned the lock, the second Unlock panicked `unlock of unlocked mutex`,
 * seen under qemu-aarch64 in examples/channels.zl) ---- */

static int64_t gm[3];
static int64_t gm_returned;

static void *gm_locker(void *arg) {
    (void)arg;
    zell_task *t = zell_task_mutex_lock(gm);   /* must wait for the guard */
    __atomic_store_n(&gm_returned, 1, __ATOMIC_RELEASE);
    return t;
}

TEST(mutex_fast_path_respects_guard) {
    gm[0] = 2; gm[1] = gm[2] = 0;   /* MU_GUARD: a slow locker is mid-way */
    gm_returned = 0;
    assert(!zell_task_mutex_try_lock(gm));
    assert(gm[0] == 2);
    pthread_t th;
    assert(pthread_create(&th, NULL, gm_locker, NULL) == 0);
    struct timespec d = { 0, 20 * 1000 * 1000 };
    nanosleep(&d, NULL);
    assert(!__atomic_load_n(&gm_returned, __ATOMIC_ACQUIRE));
    assert(__atomic_load_n(&gm[0], __ATOMIC_ACQUIRE) == 2);   /* no locked bit slipped in */
    __atomic_store_n(&gm[0], 1, __ATOMIC_RELEASE);            /* the slow locker owns it, guard off */
    void *r;
    pthread_join(th, &r);
    zell_task *t = r;
    assert(!zell_task_is_done(t));   /* queued behind the owner */
    zell_task_mutex_unlock(gm);      /* wakes t, which takes it */
    zell_task_await(t);
    zell_task_mutex_unlock(gm);
    assert(gm[0] == 0 && gm[1] == 0 && gm[2] == 0);
    zell_release(t);
}

static void unlock_unlocked(void) {
    zell_mutex *m = zell_mutex_new();
    zell_mutex_unlock(m);
}

/* ---- misuse ---- */

static void double_complete(void) {
    zell_task *t = zell_task_new(0, NULL);
    zell_task_complete(t, NULL);
    zell_task_complete(t, NULL);
}

static void early_result(void) {
    zell_task *t = zell_task_new(8, NULL);
    zell_task_result(t);
}

/* ---- a destructor run by a worker's release of the job's task starts a
 * task (regression: the worker's current-task slot still named the dying
 * task, which the new task took as its parent: resurrected, double free) ---- */

static int64_t drop_job_go, drop_ran;

static void guard_drop(void *obj) {
    (void)obj;
    zell_task *n = zell_task_new(0, NULL);   /* parent = the current task */
    zell_task_complete(n, NULL);
    zell_release(n);
    __atomic_store_n(&drop_ran, 1, __ATOMIC_RELEASE);
}

static const zell_type_info guard_type = { "Guard", 0, guard_drop };

static void wait_flag(void *arg) {
    (void)arg;
    while (!__atomic_load_n(&drop_job_go, __ATOMIC_ACQUIRE)) sched_yield();
}

TEST(drop_in_job_teardown_starts_task) {
    for (int round = 0; round < 50; round++) {
        drop_job_go = drop_ran = 0;
        zell_task *t = zell_task_new_plain(8, &zell_ops_ref);
        void *g = zell_object_new(&guard_type, 16);
        zell_task_complete(t, &g);              /* the result holds the guard */
        zell_release(g);
        zell_task *prev = zell_task_enter_scope(t);
        zell_sched_post(wait_flag, NULL);       /* the job runs as task t */
        zell_task_leave_scope(prev);
        zell_release(prev);
        zell_release(t);                        /* the job's is the last reference */
        __atomic_store_n(&drop_job_go, 1, __ATOMIC_RELEASE);
        int64_t t0 = now_ms();
        while (!__atomic_load_n(&drop_ran, __ATOMIC_ACQUIRE)) {
            assert(now_ms() - t0 < 5000);
            sched_yield();
        }
    }
}

TEST(misuse_panics) {
    char buf[8192];
    assert(run_child(double_complete, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: task completed twice"));
    assert(run_child(early_result, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: task result read before completion"));
    assert(run_child(unlock_unlocked, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: unlock of unlocked mutex"));
}

/* ---- task.Yield (ABI.md 10.2): the immortal yield sentinel posts the
 * continuation to the ring tail, behind the jobs this worker already
 * queued and ahead of the ones queued after it (never run-next, which
 * would run it first). Every other worker is held busy so nobody steals
 * and the order is deterministic. ---- */

#define YQ 16
static int64_t y_order[YQ + 2], y_n, y_blockers_in, y_release, y_done;

static void y_rec(void *arg) {
    int64_t i = __atomic_fetch_add(&y_n, 1, __ATOMIC_ACQ_REL);
    y_order[i] = (int64_t)(intptr_t)arg;
    if ((intptr_t)arg == YQ + 1) __atomic_store_n(&y_done, 1, __ATOMIC_RELEASE);
}

static void y_nop(void *arg) { (void)arg; }

static void y_blocker(zell_task *t, void *ctx) {
    (void)ctx;
    __atomic_add_fetch(&y_blockers_in, 1, __ATOMIC_ACQ_REL);
    while (!__atomic_load_n(&y_release, __ATOMIC_ACQUIRE)) sched_yield();
    zell_task_complete(t, NULL);
}

static void y_poster(zell_task *t, void *ctx) {
    (void)ctx;
    for (intptr_t i = 0; i < YQ; i++) zell_sched_post(y_rec, (void *)i);
    zell_task *y = zell_task_yield();
    assert(y == zell_task_yield());          /* one immortal object */
    assert(!zell_task_is_done(y));
    assert(zell_task_add_continuation(y, y_rec, (void *)(intptr_t)YQ));
    zell_sched_post(y_rec, (void *)(intptr_t)(YQ + 1));
    zell_release(y);                         /* no-op */
    zell_task_complete(t, NULL);
}

TEST(yield_ring_tail) {
    int64_t nw = zell_sched_workers();
    zell_task *bl[256];
    assert(nw - 1 <= 256);
    y_n = y_blockers_in = y_release = y_done = 0;
    /* Spare workers of earlier tests retire when they next park: wake the
     * parked ones with no-op jobs until only the core workers are left
     * (the second peak call reports the threads alive now). */
    int64_t t0 = now_ms();
    zell__sched_threads_peak();
    while (zell__sched_threads_peak() > nw) {
        assert(now_ms() - t0 < 5000);
        for (int i = 0; i < 64; i++) zell_sched_post(y_nop, NULL);
        usleep(2000);
    }
    for (int64_t i = 0; i < nw - 1; i++) bl[i] = zell_task_spawn(y_blocker, NULL);
    while (__atomic_load_n(&y_blockers_in, __ATOMIC_ACQUIRE) < nw - 1) { assert(now_ms() - t0 < 5000); sched_yield(); }
    zell_task *p = zell_task_spawn(y_poster, NULL);
    while (!__atomic_load_n(&y_done, __ATOMIC_ACQUIRE)) { assert(now_ms() - t0 < 5000); sched_yield(); }
    for (int64_t i = 0; i <= YQ + 1; i++) assert(y_order[i] == i);
    __atomic_store_n(&y_release, 1, __ATOMIC_RELEASE);
    for (int64_t i = 0; i < nw - 1; i++) { zell_task_await(bl[i]); zell_release(bl[i]); }
    zell_task_await(p);
    zell_release(p);
    /* A blocking await of the sentinel from a non-task thread returns. */
    zell_task_await(zell_task_yield());
}

/* ---- mutex: Go's sync.Mutex rules (DESIGN.md "Асинхронность", ABI.md
 * 10.6): barging, a woken waiter that loses stays the oldest, FIFO
 * hand-off in starvation mode, races, one worker, no starvation ---- */

static void sleep_us(long us) {
    struct timespec d = { 0, us * 1000 };
    nanosleep(&d, NULL);
}

enum { MU_STARVING_BIT = 16 };   /* ABI.md 10.6 */

static int64_t bm[3];
static zell_task *bw1, *bw2, *bjob_done;
static int bbarged;

/* On a worker: Unlock wakes bw1 (its retry goes to this worker's run-next
 * slot) without handing it over, so this running job takes it first. */
static void barge_job(void *arg) {
    (void)arg;
    assert(zell_task_mutex_lock(bm) == &zell_task_done_task);
    bw1 = zell_task_mutex_lock(bm);
    bw2 = zell_task_mutex_lock(bm);
    zell_task_mutex_unlock(bm);
    bbarged = zell_task_mutex_try_lock(bm);
    if (bbarged) assert(!zell_task_is_done(bw1));
    zell_task_complete(bjob_done, NULL);
}

TEST(mutex_barging) {
    int barged = 0;
    for (int round = 0; round < 50; round++) {
        bm[0] = bm[1] = bm[2] = 0;
        bjob_done = zell_task_new_plain(0, NULL);
        zell_sched_post(barge_job, NULL);
        zell_task_await(bjob_done);
        zell_release(bjob_done);
        if (bbarged) { barged++; zell_task_mutex_unlock(bm); }
        /* bw1 lost the race (or never raced) and re-queued at the head */
        zell_task_await(bw1);
        assert(!zell_task_is_done(bw2));
        zell_task_mutex_unlock(bm);
        zell_task_await(bw2);
        zell_task_mutex_unlock(bm);
        assert(bm[0] == 0 && bm[1] == 0 && bm[2] == 0);
        zell_release(bw1);
        zell_release(bw2);
    }
    assert(barged > 0);   /* a stolen retry may win a round, never all 50 */
}

TEST(mutex_starvation_fifo) {
    int64_t m[3] = { 0, 0, 0 };
    assert(zell_task_mutex_lock(m) == &zell_task_done_task);
    zell_task *w[5];
    for (int i = 0; i < 5; i++) { w[i] = zell_task_mutex_lock(m); assert(!zell_task_is_done(w[i])); }
    sleep_us(3000);
    /* What a woken waiter that waited > 1 ms does when it loses the race. */
    __atomic_or_fetch(&m[0], MU_STARVING_BIT, __ATOMIC_SEQ_CST);
    assert(!zell_task_mutex_try_lock(m));        /* nobody barges */
    zell_task *late = zell_task_mutex_lock(m);   /* queues at the tail */
    assert(!zell_task_is_done(late));
    for (int i = 0; i < 5; i++) {
        zell_task_mutex_unlock(m);               /* hands it to the oldest, synchronously */
        assert(zell_task_is_done(w[i]));
        for (int j = i + 1; j < 5; j++) assert(!zell_task_is_done(w[j]));
        assert(!zell_task_is_done(late));
        assert((m[0] & 1) && (m[0] & MU_STARVING_BIT));
        assert(!zell_task_mutex_try_lock(m));
    }
    zell_task_mutex_unlock(m);                   /* to late: the queue drained */
    assert(zell_task_is_done(late) && m[0] == 1 && m[1] == 0 && m[2] == 0);
    zell_task_mutex_unlock(m);
    assert(m[0] == 0);
    for (int i = 0; i < 5; i++) zell_release(w[i]);
    zell_release(late);

    /* A waiter that waited less than 1 ms ends starvation mode. */
    assert(zell_task_mutex_lock(m) == &zell_task_done_task);
    zell_task *a = zell_task_mutex_lock(m), *b = zell_task_mutex_lock(m);
    __atomic_or_fetch(&m[0], MU_STARVING_BIT, __ATOMIC_SEQ_CST);
    zell_task_mutex_unlock(m);
    assert(zell_task_is_done(a) && !zell_task_is_done(b) && !(m[0] & MU_STARVING_BIT));
    zell_task_mutex_unlock(m);   /* normal mode: wakes b */
    zell_task_await(b);
    zell_task_mutex_unlock(m);
    assert(m[0] == 0);
    zell_release(a);
    zell_release(b);
}

/* Lock/unlock races: continuation-style racers (as compiled code awaits),
 * a fresh mutex per round, which must end free with an empty queue. With
 * `yield`, the critical section yields the thread in the middle. */
static int64_t rm[3];
static int64_t rcount, rdone, rracers;
static zell_task *rall;
static int ryield;

typedef struct { int64_t left, seen; } racer;

static void racer_step(void *arg);

static void racer_finish(void *arg) {
    racer *r = arg;
    assert(rcount == r->seen);   /* nobody else got in */
    rcount = r->seen + 1;
    zell_task_mutex_unlock(rm);
    if (--r->left == 0) {
        int64_t all = rracers;   /* read before the add: the last add ends the round */
        if (__atomic_add_fetch(&rdone, 1, __ATOMIC_ACQ_REL) == all) zell_task_complete(rall, NULL);
        return;
    }
    racer_step(arg);
}

static void racer_locked(void *arg) {
    racer *r = arg;
    r->seen = rcount;
    if (ryield) zell_task_add_continuation(zell_task_yield(), racer_finish, arg);
    else racer_finish(arg);
}

static void racer_step(void *arg) {
    zell_task *t = zell_task_mutex_lock(rm);
    bool wait = zell_task_add_continuation(t, racer_locked, arg);
    zell_release(t);
    if (!wait) racer_locked(arg);
}

static void race_round(int64_t racers, int64_t iters, int yield) {
    static racer rs[16];
    assert(racers <= 16);
    rm[0] = rm[1] = rm[2] = 0;
    rcount = rdone = 0;
    rracers = racers;
    ryield = yield;
    rall = zell_task_new_plain(0, NULL);
    for (int64_t i = 0; i < racers; i++) { rs[i].left = iters; zell_sched_post(racer_step, &rs[i]); }
    zell_task_await(rall);
    zell_release(rall);
    assert(rcount == racers * iters);
    assert(rm[0] == 0 && rm[1] == 0 && rm[2] == 0);
}

TEST(mutex_race_rounds) {
    for (int round = 0; round < 1000; round++) race_round(4, 8, round & 1);
}

/* ---- children with their own worker count (a fresh process: ABI.md
 * 10.5 fixes the pool size at start) ---- */

static int64_t sm[3];
static int sstop;
static int64_t shostiles_done;

/* Re-takes the mutex in a loop with TryLock, which barges ahead of every
 * woken waiter in normal mode. */
static void hostile(void *arg) {
    (void)arg;
    while (!__atomic_load_n(&sstop, __ATOMIC_ACQUIRE)) {
        if (zell_task_mutex_try_lock(sm)) {
            for (int k = 0; k < 50; k++) zell__plat_cpu_relax();
            zell_task_mutex_unlock(sm);
        }
    }
    __atomic_add_fetch(&shostiles_done, 1, __ATOMIC_ACQ_REL);
}

/* 15 hostile workers out of 16: a waiter still gets the mutex about 1 ms
 * after it queued (starvation mode), plus scheduling slack. */
static void child_starve(void) {
    int64_t n = zell_sched_workers() - 1;
    if (n > 15) n = 15;
    for (int64_t i = 0; i < n; i++) zell_sched_post(hostile, NULL);
    sleep_us(5000);
    int64_t worst = 0;
    for (int i = 0; i < 20; i++) {
        int64_t t0 = now_ns();
        zell_task *t = zell_task_mutex_lock(sm);
        while (!zell_task_is_done(t)) {
            if (now_ns() - t0 >= 2000000000) {
                fprintf(stderr, "waiter stuck: state %lld\n", (long long)__atomic_load_n(&sm[0], __ATOMIC_SEQ_CST));
                abort();
            }
            sched_yield();
        }
        int64_t waited = now_ns() - t0;
        if (waited > worst) worst = waited;
        zell_task_mutex_unlock(sm);
        zell_release(t);
        sleep_us(1000);
    }
    __atomic_store_n(&sstop, 1, __ATOMIC_RELEASE);
    while (__atomic_load_n(&shostiles_done, __ATOMIC_ACQUIRE) < n) sched_yield();
    fprintf(stderr, "workers %lld, worst wait %.2f ms\n", (long long)(n + 1), (double)worst / 1e6);
    assert(worst < 250000000);   /* 1 ms threshold + slack (sanitizers, wine, a loaded machine) */
}

/* One worker: nobody spins, the woken waiter's retry runs after the
 * unlocking job; the critical section yields the only thread. */
static void child_one_worker(void) {
    assert(zell_sched_workers() == 1);
    for (int round = 0; round < 50; round++) race_round(8, 50, 1);
}

static int mutex_child(const char *mode, int argc, char **argv) {
    bool starve = strcmp(mode, "starve") == 0;
    setenv("ZELL_WORKERS", starve ? "16" : "1", 1);
    int64_t live = zell_live_objects();
    zell_runtime_init(argc, argv);
    if (starve) child_starve(); else child_one_worker();
    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}

#if !defined(_WIN32)
static char *test_argv0;

#define MUTEX_CHILD(name, mode) static void name(void) { \
        setenv("ZELL_TEST_MUTEX", mode, 1); \
        char *a[] = { test_argv0, NULL }; \
        execv(test_argv0, a); \
        _exit(99); }
#else
#define MUTEX_CHILD(name, mode) static void name(void) { \
        char *a[] = { "test_task", NULL }; \
        _exit(mutex_child(mode, 1, a)); }
#endif
MUTEX_CHILD(spawn_child_starve, "starve")
MUTEX_CHILD(spawn_child_one_worker, "one")

TEST(mutex_no_starvation_16_workers) {
    char buf[1024];
    int rc = run_child(spawn_child_starve, buf, sizeof buf);
    printf("    %s", buf);
    assert(rc == 0);
}

TEST(mutex_one_worker_yield) {
    char buf[1024];
    int rc = run_child(spawn_child_one_worker, buf, sizeof buf);
    if (rc != 0) printf("    %s\n", buf);
    assert(rc == 0);
}

int main(int argc, char **argv) {
    const char *child = getenv("ZELL_TEST_MUTEX");
    if (child) return mutex_child(child, argc, argv);
#if !defined(_WIN32)
    test_argv0 = argv[0];
#endif
    printf("test_task\n");
    setenv("ZELL_WORKERS", "3", 1);
    int64_t live = zell_live_objects();
    zell_runtime_init(argc, argv);
    assert(zell_sched_workers() == 3);
    assert(!zell_sched_is_worker());
    zell_slice args;
    zell_os_args(&args);
    assert(args.len == argc);
    assert(strstr(zell_string_data(*(zell_string **)zell_slice_index(&args, 0, 8)), "test_task"));
    zell_slice_release(&args);

    RUN(misuse_panics);   /* forks: run before worker threads are busy */
    RUN(rc_enable_concurrent);   /* before any other job reaches a worker */
    RUN(chan_wait_hook_link);
    RUN(spawn_await);
    RUN(many_spawns_all);
    RUN(nested_await_on_worker);
    RUN(yield_ring_tail);
    RUN(coroutine_protocol);
    RUN(linked_await_chain);
    RUN(delay_any);
    RUN(timer_heap_order_and_fire);
    RUN(relay_handoffs);
    RUN(work_stealing);
    RUN(runnext_fairness);
    RUN(runnext_stolen_from_busy_worker);
    RUN(runnext_taken_while_pool_parks);
    RUN(refcounted_result);
    RUN(inline_abi_words);
    RUN(mutex);
    RUN(channel_unbuffered);
    RUN(channel_buffered);
    RUN(channel_async_and_strings);
    RUN(channel_capacities);
    RUN(channel_fan_in_10k);
    RUN(await_wakes_one_thread);
    RUN(channel_cancel_waits);
    RUN(channel_cancel_from_scope);
    RUN(channel_any_delay_wins);
    RUN(channel_any_recv_loser_leaves_queue);
    RUN(channel_cancel_races_completion);
    RUN(mutex_contention_1k);
    RUN(mutex_fast_path_respects_guard);
    RUN(mutex_barging);
    RUN(mutex_starvation_fifo);
    RUN(mutex_race_rounds);
    RUN(mutex_no_starvation_16_workers);
    RUN(mutex_one_worker_yield);
    RUN(drop_in_job_teardown_starts_task);

    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}
