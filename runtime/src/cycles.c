/* cycles.c - the bounded cycle collector (DESIGN.md "Циклы", ABI.md 3.8).
 *
 * Synchronous trial deletion in the style of Bacon and Rajan, restricted to
 * objects of potentially cyclic types (ZELL_RC_CYCLIC, set at allocation
 * by zell_cycles_new, and by slice.c/map.c for containers whose element
 * ops have a visitor). It is not a tracing collector: it never scans the
 * heap or any roots, only the objects reachable through cyclic-typed
 * references from the *possible roots* recorded since the last run, and
 * ordinary objects (strings, plain-data slices, everything not flagged)
 * never take part.
 *
 * Possible roots: a flagged object whose count is decremented to a value
 * other than zero may have become the entry of an unreachable cycle; it is
 * recorded once (ZELL_RC_BUFFERED) in its thread's candidate buffer. When
 * a buffered object's count reaches zero it is dropped at once (Drop timing
 * is unchanged) and becomes a ZOMBIE: its memory stays until the buffer's
 * owner sweeps it. Full local buffers are swept and published to a global
 * list; in auto mode a collection is due once the threads together have
 * recorded `threshold` roots since the last one was requested: the thread
 * that trips the count publishes its buffer and requests the collection,
 * the others publish theirs at their next record, and it runs at the next
 * allocation of a cyclic object by compiled code (a safe point: the same
 * place a callee could already run any Drop), on that thread. Other
 * threads keep running.
 *
 * A collection (collect):
 *  0. g_collecting = 1 (seq_cst). From now on no flagged object's memory is
 *     freed (it is deferred to the end), so every object the collector got
 *     a pointer to from a live object stays readable.
 *  1. Take the published roots and this thread's buffer, clear BUFFERED.
 *  2. Discovery: from the live roots, follow the references the type
 *     visitors report to flagged objects; each new object is MARKED with
 *     an atomic OR whose result is its count snapshot.
 *  3. After every object is marked, visit them all again and subtract each
 *     internal reference from the target's snapshot (trial deletion).
 *  4. Scan: anything with a remaining count is referenced from outside the
 *     set; it and everything reachable from it are live (black).
 *  5. Validate: any white object whose header shows DIRTY (a retain or
 *     release happened after it was marked) or whose count differs from
 *     the snapshot becomes black, with what it reaches; repeated under the
 *     weak-table lock, which then also clears the weak boxes of the whites.
 *  6. Collect white: take one reference to each, run their destructors
 *     (Drop) first, recount the references among them (a Drop that stored
 *     a member somewhere keeps that member and what it reaches alive, and
 *     leaked, so no Drop ever runs twice), then release their fields and
 *     free them.
 *  7. Unmark; a member that was DIRTY becomes a possible root again.
 *     g_collecting = 0 and the deferred frees happen.
 *
 * The invariant (why concurrent mutators are safe): while MARKED, every
 * change of an object's count is visible as DIRTY, because every retain
 * and release of a flagged object is an atomic RMW whose result is
 * checked for MARKED (mem.c, weak.c and here; single-threaded processes
 * need none: the collector then runs alone). A mutator can only create or
 * remove a reference to an object by changing its count. So if a white
 * object's count is still its snapshot at validation time, every
 * reference that count stands for was read in step 3 and still exists,
 * and all of them come from white objects (black ones would have
 * blackened it). Hence no thread holds a reference to the white set and
 * none can get one: it is garbage, and stays garbage, except through weak
 * references, which step 5 clears under the lock zell_weak_get takes.
 * Reading fields concurrently is safe because frees of flagged objects
 * are deferred (step 0, with seq_cst fences on both sides) and the
 * collector only dereferences objects that are statically flagged or
 * whose dynamic type id is registered. Maps are walked only while the
 * process is single-threaded (map.c). */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CNT(w) ((w) & ZELL_RC_COUNT_MASK)
#define THRESHOLD_MIN 1024
#define THRESHOLD_MAX ((int64_t)1 << 22)
#define THRESHOLD_START 4096

/* ---- registry: type info / type id -> visitor ----
 * An open-addressing table of {key, entry}, written under g_reg_mu and
 * read without a lock: a slot is published by its key (release) after its
 * entry; a full table is copied into one twice the size, which is then
 * published (the old one is kept, so readers never see freed memory; the
 * waste is bounded by the final size). Compiled code registers an entry
 * the first time it allocates an object of its type
 * (zell_cycles_new_entry), so only types the program really allocates
 * are known, and dead code stays dead. An entry's `alias` (a dynamic type
 * id: `ti.<*S>` for a type S, a closure's function pointer for its
 * environment) maps to the same entry; a later entry whose key is that id
 * replaces the alias mapping. */

typedef struct { const void *key; const zell_cycle_type *e; } reg_slot;
typedef struct reg_table { int64_t cap, n; struct reg_table *prev; reg_slot s[]; } reg_table;

static reg_table *g_reg;
static zell__plat_mutex g_reg_mu = ZELL_PLAT_MUTEX_INIT;

static inline uint64_t hash_ptr(const void *p) {
    uint64_t x = (uint64_t)(uintptr_t)p;
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 29;
    return x;
}

static reg_table *reg_new(int64_t cap) {
    reg_table *t = calloc(1, sizeof *t + (size_t)cap * sizeof(reg_slot));
    if (!t) zell__panicf("out of memory (cycle collector)");
    t->cap = cap;
    return t;
}

/* Inserts key -> e into t (room checked by the caller; g_reg_mu held). */
static void reg_slot_put(reg_table *t, const void *key, const zell_cycle_type *e) {
    uint64_t h = hash_ptr(key) & (uint64_t)(t->cap - 1);
    for (;;) {
        const void *k = t->s[h].key;
        if (k == key) {
            /* A real entry (its own key) replaces an alias mapping. */
            if (e->key == key && t->s[h].e->key != key) __atomic_store_n(&t->s[h].e, e, __ATOMIC_RELEASE);
            return;
        }
        if (!k) {
            t->s[h].e = e;
            __atomic_store_n(&t->s[h].key, key, __ATOMIC_RELEASE);
            t->n++;
            return;
        }
        h = (h + 1) & (uint64_t)(t->cap - 1);
    }
}

static void reg_put_key(const void *key, const zell_cycle_type *e) {
    reg_table *t = g_reg;
    if (!t || (t->n + 1) * 2 > t->cap) {
        reg_table *n = reg_new(t ? t->cap * 2 : 128);
        for (int64_t i = 0; t && i < t->cap; i++)
            if (t->s[i].key) reg_slot_put(n, t->s[i].key, t->s[i].e);
        n->prev = t;
        __atomic_store_n(&g_reg, n, __ATOMIC_RELEASE);
        t = n;
    }
    reg_slot_put(t, key, e);
}

static const zell_cycle_type g_builtin[] = {
    { &zell__backing_type, zell__backing_visit, NULL, NULL, NULL, 1 },
    { &zell__map_type, zell__map_visit, NULL, NULL, NULL, 1 },
};

/* Registers e (and its alias); g_reg_mu held. */
static void reg_put(const zell_cycle_type *e) {
    if (!g_reg) {
        reg_put_key(g_builtin[0].key, &g_builtin[0]);
        reg_put_key(g_builtin[1].key, &g_builtin[1]);
    }
    if (e->key) reg_put_key(e->key, e);
    if (e->alias) reg_put_key(e->alias, e);
}

static int32_t mode(void);

void zell_cycles_register(const zell_cycle_type *types, int64_t n) {
    mode();
    zell__plat_mutex_lock(&g_reg_mu);
    for (int64_t i = 0; i < n; i++) reg_put(&types[i]);
    if (!n) reg_put(&g_builtin[0]);
    zell__plat_mutex_unlock(&g_reg_mu);
}

/* Registers a compiled entry on first use. */
__attribute__((noinline)) static void reg_entry(zell_cycle_type *e) {
    zell__plat_mutex_lock(&g_reg_mu);
    if (!e->state) {
        reg_put(e);
        __atomic_store_n(&e->state, 1, __ATOMIC_RELEASE);
    }
    zell__plat_mutex_unlock(&g_reg_mu);
}

static const zell_cycle_type *reg_find(const void *key) {
    reg_table *t = __atomic_load_n(&g_reg, __ATOMIC_ACQUIRE);
    if (!t || !key) return NULL;
    uint64_t h = hash_ptr(key) & (uint64_t)(t->cap - 1);
    for (;;) {
        const void *k = __atomic_load_n(&t->s[h].key, __ATOMIC_ACQUIRE);
        if (!k) return NULL;
        if (k == key) return __atomic_load_n(&t->s[h].e, __ATOMIC_ACQUIRE);
        h = (h + 1) & (uint64_t)(t->cap - 1);
    }
}

/* ---- mode and statistics ---- */

static int32_t g_mode = -1;       /* -1 until read from ZELL_CYCLES */
static int64_t g_threshold = THRESHOLD_START;
static int g_fixed_threshold;
static int64_t g_max_scan = (int64_t)1 << 22;
static int64_t s_runs, s_collected, s_candidates, s_examined, s_pending, s_aborted;

static void mode_init(void) {
    if (__atomic_load_n(&g_mode, __ATOMIC_ACQUIRE) >= 0) return;
    int32_t m = ZELL_CYCLES_AUTO;
    const char *e = getenv("ZELL_CYCLES");
    if (e && *e) {
        if (!strcmp(e, "off") || !strcmp(e, "0")) m = ZELL_CYCLES_OFF;
        else if (!strcmp(e, "manual")) m = ZELL_CYCLES_MANUAL;
        else if (!strcmp(e, "auto") || !strcmp(e, "1")) m = ZELL_CYCLES_AUTO;
        else fprintf(stderr, "zell: ZELL_CYCLES=%s: expected auto, manual or off (using auto)\n", e);
    }
    const char *t = getenv("ZELL_CYCLES_THRESHOLD");
    if (t && *t && atoll(t) > 0) {
        g_threshold = atoll(t);
        g_fixed_threshold = 1;
    }
    const char *x = getenv("ZELL_CYCLES_MAX_SCAN");
    if (x && *x && atoll(x) > 0) g_max_scan = atoll(x);
    /* The runtime's own container types are always known. */
    zell__plat_mutex_lock(&g_reg_mu);
    if (!g_reg) reg_put(&g_builtin[0]);
    zell__plat_mutex_unlock(&g_reg_mu);
    int32_t unset = -1;
    __atomic_compare_exchange_n(&g_mode, &unset, m, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE);
}

static inline int32_t mode(void) {
    int32_t m = __atomic_load_n(&g_mode, __ATOMIC_RELAXED);
    if (__builtin_expect(m < 0, 0)) {
        mode_init();
        m = __atomic_load_n(&g_mode, __ATOMIC_RELAXED);
    }
    return m;
}

int32_t zell_cycles_mode(void) { return mode(); }

void zell_cycles_set_mode(int32_t m) {
    mode();
    if (m == ZELL_CYCLES_OFF && zell_live_objects() > 0) return;
    if (m >= ZELL_CYCLES_OFF && m <= ZELL_CYCLES_AUTO) __atomic_store_n(&g_mode, m, __ATOMIC_RELEASE);
}

void zell_cycles_stats(zell_cycle_stats *out) {
    out->runs = __atomic_load_n(&s_runs, __ATOMIC_RELAXED);
    out->collected = __atomic_load_n(&s_collected, __ATOMIC_RELAXED);
    out->candidates = __atomic_load_n(&s_candidates, __ATOMIC_RELAXED);
    out->examined = __atomic_load_n(&s_examined, __ATOMIC_RELAXED);
    out->pending = __atomic_load_n(&s_pending, __ATOMIC_RELAXED);
    out->threshold = __atomic_load_n(&g_threshold, __ATOMIC_RELAXED);
    out->aborted = __atomic_load_n(&s_aborted, __ATOMIC_RELAXED);
    out->mode = mode();
}

/* ---- header word helpers (plain while the process has one thread) ---- */

static inline int threaded(void) { return __atomic_load_n(&zell_threaded, __ATOMIC_RELAXED); }

static inline int64_t rc_load(zell_object *o) { return __atomic_load_n(&o->refcount, __ATOMIC_ACQUIRE); }

static inline int64_t rc_or(zell_object *o, int64_t bits) {
    if (!threaded()) {
        int64_t w = o->refcount;
        o->refcount = w | bits;
        return w;
    }
    return __atomic_fetch_or(&o->refcount, bits, __ATOMIC_ACQ_REL);
}

static inline int64_t rc_and(zell_object *o, int64_t bits) {
    if (!threaded()) {
        int64_t w = o->refcount;
        o->refcount = w & bits;
        return w;
    }
    return __atomic_fetch_and(&o->refcount, bits, __ATOMIC_ACQ_REL);
}

static inline int64_t rc_add(zell_object *o, int64_t d) {
    if (!threaded()) {
        int64_t w = o->refcount + d;
        o->refcount = w;
        return w;
    }
    return __atomic_add_fetch(&o->refcount, d, __ATOMIC_ACQ_REL);
}

/* ---- deferred frees while a collection runs ---- */

static int g_collecting;

/* Store-load ordering between a mutator's last write of a reference and
 * its check of g_collecting, and the collector's publication of
 * g_collecting and its first read of a field (see the header): seq_cst
 * fences on both sides; under ThreadSanitizer, which has no fences, RMWs
 * on g_collecting itself (same ordering, at the cost of a shared line). */
#if defined(__SANITIZE_THREAD__)
#define COLLECTING_FENCE_LOAD() __atomic_fetch_add(&g_collecting, 0, __ATOMIC_SEQ_CST)
#else
#define COLLECTING_FENCE_LOAD() (__atomic_thread_fence(__ATOMIC_SEQ_CST), __atomic_load_n(&g_collecting, __ATOMIC_RELAXED))
#endif
static zell__plat_mutex g_def_mu = ZELL_PLAT_MUTEX_INIT;
static zell_object **g_def;
static int64_t g_def_n, g_def_cap;

/* Defers the free of `o` (no longer counted as live) if a collection
 * runs; false if none does (the caller frees it). */
static int defer_free(zell_object *o) {
    if (!(threaded() ? COLLECTING_FENCE_LOAD() : __atomic_load_n(&g_collecting, __ATOMIC_RELAXED))) return 0;
    zell__plat_mutex_lock(&g_def_mu);
    if (!__atomic_load_n(&g_collecting, __ATOMIC_RELAXED)) {
        zell__plat_mutex_unlock(&g_def_mu);
        return 0;
    }
    if (g_def_n == g_def_cap) {
        int64_t cap = g_def_cap ? g_def_cap * 2 : 256;
        zell_object **n = realloc(g_def, (size_t)cap * sizeof *n);
        if (!n) zell__panicf("out of memory (cycle collector)");
        g_def = n;
        g_def_cap = cap;
    }
    g_def[g_def_n++] = o;
    zell__plat_mutex_unlock(&g_def_mu);
    return 1;
}

/* Frees a dead flagged object that is still counted as live. */
static void free_counted(zell_object *o) {
    if (__atomic_load_n(&g_collecting, __ATOMIC_RELAXED) || threaded()) {
        zell__object_uncount();
        if (defer_free(o)) return;
        zell__object_recount();
    }
    zell__object_free(o);
}

/* Frees the memory of a flagged object that is no longer counted as live
 * (its Drop ran): at once, or at the end of a running collection. */
static void free_uncounted(zell_object *o) {
    if (defer_free(o)) return;
    zell__object_recount();
    zell__object_free(o);
}

/* ---- candidate buffers ---- */

typedef struct { zell_object **v; int64_t n, cap; } vec;

static int vec_push(vec *v, zell_object *o) {
    if (v->n == v->cap) {
        int64_t cap = v->cap ? v->cap * 2 : 64;
        zell_object **n = realloc(v->v, (size_t)cap * sizeof *n);
        if (!n) return 0;
        v->v = n;
        v->cap = cap;
    }
    v->v[v->n++] = o;
    return 1;
}

static zell__plat_mutex g_pub_mu = ZELL_PLAT_MUTEX_INIT;
static vec g_pub;                 /* published possible roots */
static int64_t g_pub_swept;       /* g_pub.n after its last sweep */
static int g_want;                /* auto: a collection is due */
/* The thread's candidate buffer: entries in recording order with NULL
 * holes. A buffered object that dies on the thread that recorded it soon
 * after (one of its last UNRECORD_WINDOW entries) leaves the buffer and is
 * freed at once; any other becomes a zombie, freed by the buffer's owner
 * when it sweeps. (Releases of pins, the common temporary references,
 * do not record at all: zell_ptr_release_noroot.) */
#define UNRECORD_WINDOW 8
#define BUF_MIN 1024
#define BUF_MAX ((int64_t)1 << 18)
typedef struct { zell_object **v; int64_t n, cap, live; } lbuf;
/* The runtime is linked into the executable, so its thread-locals can use
 * the initial-exec model (no __tls_get_addr call per access under -fPIC). */
#if defined(__ELF__)
#define CYC_TLS _Thread_local __attribute__((tls_model("initial-exec")))
#else
#define CYC_TLS _Thread_local
#endif
/* The buffer itself is on the heap (the key's value): key destructors may
 * run after the thread's thread-locals are freed (macOS frees them first).
 * Until a thread records, t_bp points at an empty buffer nobody writes. */
static lbuf g_nobuf;
static CYC_TLS lbuf *t_bp = &g_nobuf;
#define t_buf (*t_bp)
static CYC_TLS int64_t t_recorded;   /* folded into s_candidates by flush_local */
static CYC_TLS int t_in_collect;
/* Publication requests: in auto mode, once the roots recorded by all
 * threads since the last request reach the threshold, g_pub_req is bumped
 * and g_want set (due_now); every other thread publishes its buffer at its
 * next record, so the collection after this one sees them too. */
static int g_pub_req;
static int64_t g_unseen;
static CYC_TLS int t_pub_req;
static zell__plat_once g_key_once = ZELL_PLAT_ONCE_INIT;
static zell__plat_tls_key g_key;
static int g_key_ok;

/* Moves the buffer's entries into `out` and empties it. */
static void buf_take(lbuf *t, vec *out) {
    for (int64_t i = 0; i < t->n; i++) {
        zell_object *e = t->v[i];
        if (!e) continue;
        if (!vec_push(out, e)) zell__panicf("out of memory (cycle collector)");
    }
    t->n = t->live = 0;
}

/* A buffered entry that is a zombie, or dying: who frees it (see dead()). */
static int sweep_entry(zell_object *o) {
    int64_t w = rc_load(o);
    if (w & ZELL_RC_ZOMBIE) {
        free_uncounted(o);
        return 0;
    }
    if (CNT(w) == 0) {
        /* Being dropped right now: whoever sees the other's bit frees it. */
        int64_t old = rc_and(o, ~ZELL_RC_BUFFERED);
        if (old & ZELL_RC_ZOMBIE) free_uncounted(o);
        return 0;
    }
    return 1;
}

static void publish(vec *live) {
    zell__plat_mutex_lock(&g_pub_mu);
    int ok = 1;
    for (int64_t i = 0; i < live->n && ok; i++) ok = vec_push(&g_pub, live->v[i]);
    if (g_pub.n > 2 * g_pub_swept + BUF_MIN) {
        /* Roots that died since they were published hold their memory as
         * zombies: sweep them (amortised O(1) per published root). */
        int64_t k = 0;
        for (int64_t i = 0; i < g_pub.n; i++)
            if (sweep_entry(g_pub.v[i])) g_pub.v[k++] = g_pub.v[i];
        g_pub.n = k;
        g_pub_swept = k;
    }
    int64_t pend = g_pub.n;
    __atomic_store_n(&s_pending, pend, __ATOMIC_RELAXED);
    zell__plat_mutex_unlock(&g_pub_mu);
    if (!ok) zell__panicf("out of memory (cycle collector)");
    if (mode() == ZELL_CYCLES_AUTO && pend >= __atomic_load_n(&g_threshold, __ATOMIC_RELAXED))
        __atomic_store_n(&g_want, 1, __ATOMIC_RELAXED);
}

static void flush_buf(lbuf *t, int all);

/* Folds this thread's record count into the statistics and, in auto mode,
 * into the roots recorded by all threads since the last publication
 * request; true when they reached the threshold, which issues the request
 * (the records are a proxy for the roots: entries that die before a
 * collection are counted too, so the published set may be smaller). */
static int count_recorded(void) {
    int64_t n = t_recorded;
    if (!n) return 0;
    t_recorded = 0;
    __atomic_add_fetch(&s_candidates, n, __ATOMIC_RELAXED);
    if (mode() != ZELL_CYCLES_AUTO) return 0;
    if (__atomic_add_fetch(&g_unseen, n, __ATOMIC_RELAXED) < __atomic_load_n(&g_threshold, __ATOMIC_RELAXED)) return 0;
    __atomic_store_n(&g_unseen, 0, __ATOMIC_RELAXED);
    __atomic_add_fetch(&g_pub_req, 1, __ATOMIC_RELEASE);
    return 1;
}

/* This thread tripped the threshold: a collection is due now. Its own
 * roots are published at once and the collection is requested, whatever
 * the published count is (the other threads' buffers reach the global
 * list at their next record, and the next collection takes them). Waiting
 * for `threshold` *published* roots instead let a collection slip a whole
 * request period or more whenever part of the recorded entries had died
 * or sat in other threads' buffers. */
static void due_now(void) {
    t_pub_req = __atomic_load_n(&g_pub_req, __ATOMIC_ACQUIRE);
    if (t_buf.cap) flush_buf(t_bp, 1);
    __atomic_store_n(&g_want, 1, __ATOMIC_RELAXED);
}

/* The buffer is full: drop its holes and zombies (flush_buf); keep it when that made
 * room, grow it while it is small, else publish its live entries (always
 * with `all`). */
static void flush_local(int all) {
    if (count_recorded()) {
        due_now();
        return;
    }
    if (!t_buf.cap) return;
    flush_buf(t_bp, all);
}

static void flush_buf(lbuf *t, int all) {
    int64_t j = 0;
    /* Close the holes left by entries that died here and free the zombies
     * (entries that died on another thread: in an async program, where
     * tasks move between workers, that is most of them). Sweeping only
     * when publishing kept every zombie's memory until the next
     * collection request, which an adaptive threshold that found no
     * garbage pushes out to THRESHOLD_MAX records: ~100 MB of dead
     * objects in an HTTP server under load. */
    for (int64_t i = 0; i < t->n; i++) {
        zell_object *e = t->v[i];
        if (!e) continue;
        if (!sweep_entry(e)) continue;
        t->v[j++] = e;
    }
    t->n = t->live = j;
    /* In auto mode a buffer holding a collection's worth of roots is
     * published, so whichever thread collects next sees them. */
    if (!all && mode() == ZELL_CYCLES_AUTO && j >= __atomic_load_n(&g_threshold, __ATOMIC_RELAXED)) all = 1;
    if (!all && j * 2 <= t->cap) return;
    if (!all && t->cap < BUF_MAX) {
        zell_object **v = realloc(t->v, (size_t)(t->cap * 2) * sizeof *v);
        if (!v) zell__panicf("out of memory (cycle collector)");
        t->v = v;
        t->cap *= 2;
        return;
    }
    if (!j) return;
    vec live = { 0 };
    buf_take(t, &live);
    publish(&live);
    free(live.v);
}

static void thread_exit(void *arg) {
    lbuf *t = arg;
    if (!t) return;
    if (t->cap) flush_buf(t, 1);
    free(t->v);
    if (t_bp == t) t_bp = &g_nobuf;
    free(t);
}

static void key_init(void) { g_key_ok = zell__plat_tls_new(&g_key, thread_exit); }

__attribute__((noinline)) static void record_slow(void) {
    if (t_bp == &g_nobuf) {
        lbuf *t = calloc(1, sizeof *t);
        if (t) t->v = malloc(BUF_MIN * sizeof *t->v);
        if (!t || !t->v) zell__panicf("out of memory (cycle collector)");
        t->cap = BUF_MIN;
        zell__plat_once_run(&g_key_once, key_init);
        if (g_key_ok) zell__plat_tls_set(g_key, t);
        t_bp = t;
    } else {
        flush_local(0);
    }
}

__attribute__((noinline)) static void record_count(void) {
    if (count_recorded()) due_now();
}

/* Out of line, so the release fast path stays a few instructions and
 * tail-calls it. */
__attribute__((noinline)) static void record(zell_object *o) {
    if (__builtin_expect(t_buf.n == t_buf.cap, 0)) record_slow();
    if (__builtin_expect(__atomic_load_n(&g_pub_req, __ATOMIC_RELAXED) != t_pub_req, 0)) {
        t_pub_req = __atomic_load_n(&g_pub_req, __ATOMIC_ACQUIRE);
        flush_local(1);
    }
    t_buf.v[t_buf.n++] = o;
    t_buf.live++;
    if (__builtin_expect(++t_recorded >= 256, 0)) record_count();
}

/* Removes `o` from this thread's buffer; false if it is not there. */
static inline int unrecord(zell_object *o) {
    for (int64_t k = t_buf.n - 1; k >= 0 && k >= t_buf.n - UNRECORD_WINDOW; k--) {
        if (t_buf.v[k] == o) {
            t_buf.v[k] = NULL;
            t_buf.live--;
            if (k == t_buf.n - 1) t_buf.n--;
            return 1;
        }
    }
    return 0;
}

/* Bytes to allocate for a flagged object of `size` bytes: no extra room is
 * needed (kept for the slice/map callers). */
int64_t zell__cycles_extra(int64_t size) { return size; }

/* ---- retain / release of flagged objects (from mem.c) ---- */

void zell__cycles_dirty(zell_object *o) { __atomic_fetch_or(&o->refcount, ZELL_RC_DIRTY, __ATOMIC_ACQ_REL); }

void zell__cycles_retain(zell_object *o) {
    if (!threaded()) {
        o->refcount = o->refcount + 1;
        return;
    }
    if (__atomic_add_fetch(&o->refcount, 1, __ATOMIC_RELAXED) & ZELL_RC_MARKED) zell__cycles_dirty(o);
}

/* The count of `o` reached zero (`w`: the word after the decrement). */
/* Frees a dropped object (`w`: the word after the decrement). */
static void dead_free(zell_object *o, int64_t w) {
    if (w & ZELL_RC_BUFFERED) {
        /* Recorded by this thread: forget it and free it now. */
        if (unrecord(o)) {
            free_counted(o);
            return;
        }
        /* In another buffer: leave the memory to its owner, unless the
         * owner dropped the entry meanwhile (it saw a count of zero). */
        zell__object_uncount();
        int64_t old = rc_or(o, ZELL_RC_ZOMBIE);
        if (!(old & ZELL_RC_BUFFERED)) free_uncounted(o);
        return;
    }
    free_counted(o);
}

/* The deferred form of dead(). */
static void dead_fin(zell_object *o, int64_t w) {
    o->type->drop(o);
    dead_free(o, w);
}

__attribute__((noinline)) static void dead(zell_object *o, int64_t w) {
    if (w & ZELL_RC_WEAK_FLAG) zell__weak_clear(o);
    if (!(o->type && o->type->drop)) {
        dead_free(o, w);
        return;
    }
    /* The drop may kill a long chain of children: bounded depth
     * (rt_internal.h). */
    if (!zell__dead_enter()) {
        zell__dead_push(dead_fin, o, w);
        return;
    }
    o->type->drop(o);
    zell__dead_leave();
    dead_free(o, w);
}

/* The finalization of the single-threaded fast path of zell__cycles_release
 * (not recorded, no weak box, no collection running when the count fell). */
static void dead_plain(zell_object *o, int64_t w) {
    (void)w;
    o->type->drop(o);
    if (!__atomic_load_n(&g_collecting, __ATOMIC_RELAXED)) {
        zell__object_free(o);
        return;
    }
    free_counted(o);
}

void zell__cycles_release(zell_object *o) {
    int64_t n = rc_load(o), nn;
    if (!threaded()) {
        /* One thread: no collection can run concurrently (MARKED is only
         * set while this thread collects, and then DIRTY is not needed). */
        nn = n - 1;
        if (CNT(nn) == 0) {
            o->refcount = nn;
            if (!(nn & (ZELL_RC_BUFFERED | ZELL_RC_WEAK_FLAG)) && !__atomic_load_n(&g_collecting, __ATOMIC_RELAXED)) {
                /* Not recorded, no weak box, no collection: as without
                 * the collector. */
                if (!(o->type && o->type->drop)) {
                    zell__object_free(o);
                    return;
                }
                if (!zell__dead_enter()) {
                    zell__dead_push(dead_plain, o, nn);
                    return;
                }
                o->type->drop(o);
                zell__dead_leave();
                if (!__atomic_load_n(&g_collecting, __ATOMIC_RELAXED)) {
                    zell__object_free(o);
                    return;
                }
                free_counted(o);
                return;
            }
            dead(o, nn);
            return;
        }
        if (n & (ZELL_RC_BUFFERED | ZELL_RC_MARKED)) {
            o->refcount = nn;
            return;
        }
        o->refcount = nn | ZELL_RC_BUFFERED;   /* a possible root */
        record(o);
        return;
    }
    int rec;
    for (;;) {
        nn = n - 1;
        rec = 0;
        if (n & ZELL_RC_MARKED) nn |= ZELL_RC_DIRTY;
        else if (CNT(nn) != 0 && !(n & ZELL_RC_BUFFERED)) {
            nn |= ZELL_RC_BUFFERED;  /* a possible root */
            rec = 1;
        }
        if (__atomic_compare_exchange_n(&o->refcount, &n, nn, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
    }
    if (rec) record(o);
    else if (CNT(nn) == 0) dead(o, nn);
}

void zell__cycles_release_noroot(zell_object *o) {
    int64_t n = rc_load(o), nn;
    if (!threaded()) {
        nn = n - 1;
        o->refcount = nn;
        if (CNT(nn) == 0) dead(o, nn);
        return;
    }
    /* (threaded) */
    for (;;) {
        nn = (n - 1) | (n & ZELL_RC_MARKED ? ZELL_RC_DIRTY : 0);
        if (__atomic_compare_exchange_n(&o->refcount, &n, nn, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
    }
    if (CNT(nn) == 0) dead(o, nn);
}

void zell__cycles_flag(zell_object *o) {
    if (mode() != ZELL_CYCLES_OFF) o->refcount |= ZELL_RC_CYCLIC;
}

static void maybe_collect(void);

void *zell_cycles_new(const zell_type_info *type, int64_t size) {
    if (__builtin_expect(__atomic_load_n(&g_want, __ATOMIC_RELAXED), 0)) maybe_collect();
    zell_object *o = zell_object_new(type, size);
    if (mode() == ZELL_CYCLES_OFF) return o;
    o->refcount |= ZELL_RC_CYCLIC;
    return o;
}

void *zell_cycles_new_entry(zell_cycle_type *e, int64_t size) {
    if (__builtin_expect(!__atomic_load_n(&e->state, __ATOMIC_ACQUIRE), 0)) reg_entry(e);
    return zell_cycles_new(e->key, size);
}

void *zell_cycles_new_dyn(const zell_type_info *type, int64_t size) {
    const zell_cycle_type *r = reg_find(type);
    if (r && r->visit) return zell_cycles_new(type, size);
    return zell_object_new(type, size);
}

/* ---- the collection ---- */

typedef struct {
    zell_object *o;
    const zell_cycle_type *r;
    int64_t snap;      /* count when marked */
    int64_t gc;        /* snapshot minus internal references */
    int64_t in;        /* references from white members (step 6) */
    uint8_t black, freed, kept;
} node;

typedef struct {
    node *v;
    int64_t n, cap;
    int64_t *idx;      /* open addressing: index + 1, 0 = empty */
    int64_t icap;
    int64_t *ef, *et;  /* internal edges (from, to) */
    int64_t en, ecap;
    int64_t cur;       /* node being visited */
    int overflow, oom;
} graph;

static int64_t g_find(const graph *g, const zell_object *o) {
    if (!g->icap) return -1;
    uint64_t h = hash_ptr(o) & (uint64_t)(g->icap - 1);
    while (g->idx[h]) {
        if (g->v[g->idx[h] - 1].o == o) return g->idx[h] - 1;
        h = (h + 1) & (uint64_t)(g->icap - 1);
    }
    return -1;
}

static int g_grow_index(graph *g) {
    int64_t cap = g->icap ? g->icap * 2 : 1024;
    int64_t *n = calloc((size_t)cap, sizeof *n);
    if (!n) return 0;
    for (int64_t i = 0; i < g->n; i++) {
        uint64_t h = hash_ptr(g->v[i].o) & (uint64_t)(cap - 1);
        while (n[h]) h = (h + 1) & (uint64_t)(cap - 1);
        n[h] = i + 1;
    }
    free(g->idx);
    g->idx = n;
    g->icap = cap;
    return 1;
}

/* The registry entry that walks `o`, or NULL. */
static const zell_cycle_type *walker(const zell_object *o) {
    const zell_cycle_type *r = reg_find(o->type);
    return r && r->visit ? r : NULL;
}

/* Adds a flagged object to the set: MARKED, count snapshot. */
static void g_add(graph *g, zell_object *o) {
    if (g->overflow || g->oom || g_find(g, o) >= 0) return;
    if (g->n >= g_max_scan) {
        g->overflow = 1;
        return;
    }
    int64_t w = rc_or(o, ZELL_RC_MARKED);
    if (CNT(w) == 0 || (w & ZELL_RC_ZOMBIE)) {
        /* Being dropped: not part of anything. */
        rc_and(o, ~(ZELL_RC_MARKED | ZELL_RC_DIRTY));
        return;
    }
    if (g->n == g->cap) {
        int64_t cap = g->cap ? g->cap * 2 : 256;
        node *n = realloc(g->v, (size_t)cap * sizeof *n);
        if (!n) {
            rc_and(o, ~ZELL_RC_MARKED);
            g->oom = 1;
            return;
        }
        g->v = n;
        g->cap = cap;
    }
    if ((g->n + 1) * 2 > g->icap && !g_grow_index(g)) {
        rc_and(o, ~ZELL_RC_MARKED);
        g->oom = 1;
        return;
    }
    g->v[g->n] = (node){ o, walker(o), CNT(w), CNT(w), 0, 0, 0, 0 };
    uint64_t h = hash_ptr(o) & (uint64_t)(g->icap - 1);
    while (g->idx[h]) h = (h + 1) & (uint64_t)(g->icap - 1);
    g->idx[h] = ++g->n;
}

/* A reference reported by a visitor: is it a flagged object we may read? */
static zell_object *child(void *obj, const void *tid) {
    if (!obj) return NULL;
    if (tid && !reg_find(tid)) return NULL;
    zell_object *c = obj;
    int64_t w = rc_load(c);
    if (w < 0 || !(w & ZELL_RC_CYCLIC)) return NULL;
    return c;
}

static void cb_discover(void *ctx, void *obj, const void *tid) {
    zell_object *c = child(obj, tid);
    if (c) g_add(ctx, c);
}

static void cb_count(void *ctx, void *obj, const void *tid) {
    graph *g = ctx;
    if (!obj || (tid && !reg_find(tid))) return;
    int64_t j = g_find(g, obj);
    if (j < 0) return;
    g->v[j].gc--;
    if (g->en == g->ecap) {
        int64_t cap = g->ecap ? g->ecap * 2 : 1024;
        int64_t *f = realloc(g->ef, (size_t)cap * sizeof *f);
        if (f) g->ef = f;
        int64_t *t = f ? realloc(g->et, (size_t)cap * sizeof *t) : NULL;
        if (t) g->et = t;
        if (!f || !t) {
            g->oom = 1;
            return;
        }
        g->ecap = cap;
    }
    g->ef[g->en] = g->cur;
    g->et[g->en] = j;
    g->en++;
}

/* Step 6: references among white members. */
static void cb_white(void *ctx, void *obj, const void *tid) {
    graph *g = ctx;
    if (!obj || (tid && !reg_find(tid))) return;
    int64_t j = g_find(g, obj);
    if (j >= 0 && !g->v[j].black) g->v[j].in++;
}

/* Blackens node i and everything reachable from it through the edges. */
static void blacken(graph *g, int64_t i, int64_t *stack, const int64_t *start, const int64_t *adj) {
    if (g->v[i].black) return;
    int64_t sp = 0;
    g->v[i].black = 1;
    stack[sp++] = i;
    while (sp) {
        int64_t k = stack[--sp];
        for (int64_t e = start[k]; e < start[k + 1]; e++) {
            int64_t j = adj[e];
            if (!g->v[j].black) {
                g->v[j].black = 1;
                stack[sp++] = j;
            }
        }
    }
}

/* Validation: a white member whose count moved is reachable. Returns the
 * number of objects blackened. */
static int64_t validate(graph *g, int64_t *stack, const int64_t *start, const int64_t *adj) {
    int64_t changed = 0;
    for (int64_t i = 0; i < g->n; i++) {
        if (g->v[i].black) continue;
        int64_t w = rc_load(g->v[i].o);
        if ((w & ZELL_RC_DIRTY) || CNT(w) != g->v[i].snap) {
            blacken(g, i, stack, start, adj);
            changed++;
        }
    }
    return changed;
}

static int g_busy;   /* one collection at a time */

static void unmark_all(graph *g) {
    for (int64_t i = 0; i < g->n; i++) {
        node *nd = &g->v[i];
        if (nd->freed) continue;
        int64_t old = rc_and(nd->o, ~(ZELL_RC_MARKED | ZELL_RC_DIRTY));
        /* A count change while examined: maybe a new root. */
        if ((old & ZELL_RC_DIRTY) && CNT(old) > 0 && !(old & ZELL_RC_BUFFERED) && !nd->kept) {
            int64_t prev = rc_or(nd->o, ZELL_RC_BUFFERED);
            if (!(prev & ZELL_RC_BUFFERED) && CNT(prev) > 0) {
                record(nd->o);
            } else if (!(prev & ZELL_RC_BUFFERED)) {
                rc_and(nd->o, ~ZELL_RC_BUFFERED);
            }
        }
    }
}

/* Runs one collection. `dry`: only count the garbage. Returns the number
 * of objects freed (or found, when dry), -1 if another one is running. */
static int64_t collect(int dry, int wait) {
    if (t_in_collect || mode() == ZELL_CYCLES_OFF) return 0;
    int expected = 0;
    while (!__atomic_compare_exchange_n(&g_busy, &expected, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        if (!wait) return -1;
        expected = 0;
        zell__plat_yield();
    }
    t_in_collect = 1;
    __atomic_store_n(&g_want, 0, __ATOMIC_RELAXED);
    /* 0. From here on frees of flagged objects are deferred. */
    __atomic_add_fetch(&zell_cycles_epoch, 1, __ATOMIC_SEQ_CST);  /* pins taken before see it */
    __atomic_exchange_n(&g_collecting, 1, __ATOMIC_SEQ_CST);
#if !defined(__SANITIZE_THREAD__)
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
#endif

    /* 1. Possible roots. */
    vec roots = { 0 };
    zell__plat_mutex_lock(&g_pub_mu);
    roots = g_pub;
    g_pub = (vec){ 0 };
    g_pub_swept = 0;
    __atomic_store_n(&s_pending, 0, __ATOMIC_RELAXED);
    zell__plat_mutex_unlock(&g_pub_mu);
    if (t_bp != &g_nobuf) buf_take(t_bp, &roots);
    graph g = { 0 };
    vec zombies = { 0 };
    for (int64_t i = 0; i < roots.n; i++) {
        zell_object *o = roots.v[i];
        int64_t old = rc_and(o, ~ZELL_RC_BUFFERED);
        if (old & ZELL_RC_ZOMBIE) {
            if (!vec_push(&zombies, o)) free_uncounted(o);
        } else if (CNT(old) != 0) {
            g_add(&g, o);
        }
        /* else: being dropped; its dropper frees it (it sees BUFFERED gone). */
    }

    /* 2. Discovery through flagged references. */
    for (int64_t i = 0; i < g.n && !g.overflow && !g.oom; i++)
        if (g.v[i].r) g.v[i].r->visit(g.v[i].o, cb_discover, &g);

    int64_t freed = 0, whites = 0;
    if (g.overflow || g.oom) {
        __atomic_add_fetch(&s_aborted, 1, __ATOMIC_RELAXED);
        if (!g_fixed_threshold) __atomic_store_n(&g_threshold, THRESHOLD_MAX, __ATOMIC_RELAXED);
        goto done;
    }

    /* 3. Trial deletion: subtract the internal references. */
    for (int64_t i = 0; i < g.n && !g.oom; i++) {
        g.cur = i;
        if (g.v[i].r) g.v[i].r->visit(g.v[i].o, cb_count, &g);
    }
    if (g.oom) goto done;
    {
        /* Adjacency (CSR) of the internal edges. */
        int64_t *start = calloc((size_t)g.n + 1, sizeof *start);
        int64_t *adj = malloc(((size_t)g.en + 1) * sizeof *adj);
        int64_t *stack = malloc(((size_t)g.n + 1) * sizeof *stack);
        int64_t *fill = calloc((size_t)g.n + 1, sizeof *fill);
        if (!start || !adj || !stack || !fill) {
            free(start), free(adj), free(stack), free(fill);
            goto done;
        }
        for (int64_t e = 0; e < g.en; e++) start[g.ef[e] + 1]++;
        for (int64_t i = 0; i < g.n; i++) start[i + 1] += start[i];
        for (int64_t e = 0; e < g.en; e++) adj[start[g.ef[e]] + fill[g.ef[e]]++] = g.et[e];
        free(fill);

        /* 4. Scan: referenced from outside the set -> live with all it reaches. */
        for (int64_t i = 0; i < g.n; i++)
            if (g.v[i].gc != 0) blacken(&g, i, stack, start, adj);
        /* 5. Validate (again under the weak lock, which also clears the boxes). */
        while (validate(&g, stack, start, adj)) {}
        if (!dry) {
            zell__weak_lock();
            while (validate(&g, stack, start, adj)) {}
            for (int64_t i = 0; i < g.n; i++)
                if (!g.v[i].black && (rc_load(g.v[i].o) & ZELL_RC_WEAK_FLAG)) zell__weak_clear_locked(g.v[i].o);
            zell__weak_unlock();
        }
        for (int64_t i = 0; i < g.n; i++) whites += !g.v[i].black;
        if (dry || whites == 0) {
            free(start), free(adj), free(stack);
            freed = dry ? whites : 0;
            goto done;
        }

        /* 6. Collect white: hold each, run the destructors, recheck. */
        for (int64_t i = 0; i < g.n; i++)
            if (!g.v[i].black) rc_add(g.v[i].o, 1);
        for (int64_t i = 0; i < g.n; i++)
            if (!g.v[i].black && g.v[i].r && g.v[i].r->fin) g.v[i].r->fin(g.v[i].o);
        int resurrected = 1;
        while (resurrected) {
            resurrected = 0;
            for (int64_t i = 0; i < g.n; i++) g.v[i].in = 0;
            for (int64_t i = 0; i < g.n; i++)
                if (!g.v[i].black && g.v[i].r) g.v[i].r->visit(g.v[i].o, cb_white, &g);
            for (int64_t i = 0; i < g.n; i++) {
                node *nd = &g.v[i];
                if (nd->black) continue;
                if (CNT(rc_load(nd->o)) != 1 + nd->in) {
                    /* Referenced from outside the white set again: a
                     * destructor stored it (or a member reaching it)
                     * somewhere. It stays alive and keeps our reference:
                     * its Drop already ran, so it must never run again.
                     * The next round also keeps what it references. */
                    nd->black = 1;
                    nd->kept = 1;
                    resurrected = 1;
                }
            }
        }
        /* Release the fields (destructors are not run again), then free. */
        for (int64_t i = 0; i < g.n; i++) {
            node *nd = &g.v[i];
            if (nd->black) continue;
            if (nd->r && nd->r->clear) nd->r->clear(nd->o);
            else if (nd->o->type && nd->o->type->drop) nd->o->type->drop(nd->o);
        }
        for (int64_t i = 0; i < g.n; i++) {
            node *nd = &g.v[i];
            if (nd->black) continue;
            int64_t w = rc_load(nd->o);
            if (CNT(w) != 1) {
                nd->kept = 1;  /* reached again while its fields were released */
                continue;
            }
            int64_t nw;
            for (;;) {
                nw = ((w - 1) & ~(ZELL_RC_MARKED | ZELL_RC_DIRTY)) | (w & ZELL_RC_BUFFERED ? ZELL_RC_ZOMBIE : 0);
                if (!threaded()) {
                    nd->o->refcount = nw;
                    break;
                }
                if (__atomic_compare_exchange_n(&nd->o->refcount, &w, nw, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
            }
            nd->freed = 1;
            freed++;
            if (nw & ZELL_RC_ZOMBIE) {
                /* In another thread's unpublished buffer: it frees the memory. */
                zell__object_uncount();
            } else {
                zell__object_free(nd->o);
            }
        }
        free(start), free(adj), free(stack);
    }

done:
    unmark_all(&g);
    for (int64_t i = 0; i < zombies.n; i++) free_uncounted(zombies.v[i]);
    __atomic_add_fetch(&s_runs, 1, __ATOMIC_RELAXED);
    __atomic_add_fetch(&s_examined, g.n, __ATOMIC_RELAXED);
    if (!dry) __atomic_add_fetch(&s_collected, freed, __ATOMIC_RELAXED);
    if (!dry && !g_fixed_threshold && !g.overflow) {
        /* Adaptive: collections that find little garbage come less often. */
        int64_t t = __atomic_load_n(&g_threshold, __ATOMIC_RELAXED);
        if (freed * 8 < roots.n) t = t * 2 > THRESHOLD_MAX ? THRESHOLD_MAX : t * 2;
        else if (freed * 2 > roots.n) t = t / 2 < THRESHOLD_MIN ? THRESHOLD_MIN : t / 2;
        __atomic_store_n(&g_threshold, t, __ATOMIC_RELAXED);
    }
    free(g.v), free(g.idx), free(g.ef), free(g.et), free(roots.v), free(zombies.v);
    /* 7. Deferred frees. */
    zell__plat_mutex_lock(&g_def_mu);
    __atomic_store_n(&g_collecting, 0, __ATOMIC_SEQ_CST);
    zell_object **def = g_def;
    int64_t ndef = g_def_n;
    g_def = NULL;
    g_def_n = g_def_cap = 0;
    zell__plat_mutex_unlock(&g_def_mu);
    for (int64_t i = 0; i < ndef; i++) {
        zell__object_recount();
        zell__object_free(def[i]);
    }
    free(def);
    __atomic_add_fetch(&zell_cycles_epoch, 1, __ATOMIC_SEQ_CST);
    t_in_collect = 0;
    __atomic_store_n(&g_busy, 0, __ATOMIC_RELEASE);
    return freed;
}

static void maybe_collect(void) {
    if (mode() == ZELL_CYCLES_AUTO && !t_in_collect) collect(0, 0);
}

int64_t zell_cycles_collect(void) {
    if (t_in_collect) return 0;
    flush_local(1);
    int64_t n = collect(0, 1);
    return n < 0 ? 0 : n;
}

/* A worker about to park (task.c) publishes its roots: an idle thread
 * makes no record, so they would stay out of every collection until it
 * runs again (and an explicit CollectCycles() would miss them). */
void zell__cycles_park(void) {
    if (t_bp == &g_nobuf || t_in_collect) return;
    flush_local(1);
}

int64_t zell__cycles_at_exit(void) {
    int32_t m = mode();
    if (m == ZELL_CYCLES_OFF || t_in_collect) return -1;
    flush_local(1);
    if (m == ZELL_CYCLES_AUTO) {
        /* Freeing one cycle can make others garbage only through Drop
         * effects; a few rounds settle it. */
        for (int i = 0; i < 4 && collect(0, 1) > 0; i++) {}
        return 0;
    }
    int64_t n = collect(1, 1);
    return n < 0 ? 0 : n;
}

/* ---- runtime (compiler/src/sema/stdlib_src/runtime_cycles.zl) ---- */

int64_t zell_runtime_rt_collect_cycles(void) { return zell_cycles_collect(); }

int64_t zell_runtime_rt_cycle_stat(int64_t which) {
    zell_cycle_stats s;
    zell_cycles_stats(&s);
    switch (which) {
    case 0: return s.runs;
    case 1: return s.collected;
    case 2: return s.candidates + t_recorded;
    case 3: return s.examined;
    case 4: return s.pending + t_buf.live;
    case 5: return s.threshold;
    case 6: return s.aborted;
    case 7: return s.mode;
    default: return 0;
    }
}
