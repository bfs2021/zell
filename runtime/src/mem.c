/* mem.c - raw allocation, refcounted object header, value ops, leak check. */
#include "heap.h"
#include "platform/platform.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- object counts ----
 * Live and total object counts are kept per thread (no shared atomic on
 * the allocation path) and summed on demand: every thread that allocates
 * registers its counters once; an exiting thread folds them into g_dead. */
typedef struct obj_stats {
    int64_t live, total;           /* written by the owner thread, read by others */
    struct obj_stats *next, *prev;
    int reg;
} obj_stats;
/* Per-thread counters live on the heap, not in the thread-local itself:
 * key destructors may run after a thread's thread-locals are freed (macOS
 * frees them first), and other threads read the list. */
static _Thread_local obj_stats *t_stats;
static obj_stats g_stats_head = { 0, 0, &g_stats_head, &g_stats_head, 1 };
static int64_t g_dead_live, g_dead_total;   /* exited threads (g_stats_mu) */
static zell__plat_mutex g_stats_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_once g_stats_once = ZELL_PLAT_ONCE_INIT;
static zell__plat_tls_key g_stats_key;
static int g_stats_keyed;

/* Set before the process gets a second thread (zell_threaded, ABI.md 3.2). */
uint8_t zell_threaded;

static void stats_exit(void *arg) {
    obj_stats *st = arg;
    zell__plat_mutex_lock(&g_stats_mu);
    st->prev->next = st->next;
    st->next->prev = st->prev;
    g_dead_live += st->live;
    g_dead_total += st->total;
    zell__plat_mutex_unlock(&g_stats_mu);
    if (t_stats == st) t_stats = NULL;   /* a later destructor that allocates registers again */
    free(st);
}

static void stats_key_init(void) { g_stats_keyed = zell__plat_tls_new(&g_stats_key, stats_exit); }

__attribute__((noinline, cold)) static obj_stats *stats_register(void) {
    zell__plat_once_run(&g_stats_once, stats_key_init);
    obj_stats *st = calloc(1, sizeof *st);
    if (!st) zell__panicf("out of memory (object statistics)");
    zell__plat_mutex_lock(&g_stats_mu);
    st->next = g_stats_head.next;
    st->prev = &g_stats_head;
    g_stats_head.next->prev = st;
    g_stats_head.next = st;
    st->reg = 1;
    zell__plat_mutex_unlock(&g_stats_mu);
    if (g_stats_keyed) zell__plat_tls_set(g_stats_key, st);
    t_stats = st;
    return st;
}

static inline void stats_add(int64_t live, int64_t total) {
    obj_stats *st = t_stats;
    if (__builtin_expect(!st, 0)) st = stats_register();
    __atomic_store_n(&st->live, st->live + live, __ATOMIC_RELAXED);
    if (total) __atomic_store_n(&st->total, st->total + total, __ATOMIC_RELAXED);
}

static int64_t stats_sum(int which) {
    zell__plat_mutex_lock(&g_stats_mu);
    int64_t n = which ? g_dead_total : g_dead_live;
    for (obj_stats *st = g_stats_head.next; st != &g_stats_head; st = st->next)
        n += __atomic_load_n(which ? &st->total : &st->live, __ATOMIC_RELAXED);
    zell__plat_mutex_unlock(&g_stats_mu);
    return n;
}

/* ---- leak tracking ----
 * Mode 0: off. Mode 1: count live objects and report the count at shutdown.
 * Mode 2: additionally keep every live object in a doubly linked list (a
 * 16-byte prefix in front of the header) so the report can name the types. */
typedef struct track {
    struct track *prev, *next;
} track;

static int g_leak_mode;
static int g_track; /* prefix in use (fixed before the first allocation) */
static track g_track_head = { &g_track_head, &g_track_head };
static zell__plat_mutex g_track_mu = ZELL_PLAT_MUTEX_INIT;

static void leak_report(void);
/* Set by zell_leak_check_enable, so programs built without --leak-check do
 * not link the report. */
static void (*g_leak_report)(void);

void zell_leak_check_enable(int32_t mode) {
    g_leak_mode = mode;
    g_leak_report = leak_report;
    /* The prefix can only be used if no object exists yet. */
    if (mode >= 2 && stats_sum(0) == 0) g_track = 1;
}

static void track_add(zell_object *o) {
    track *t = (track *)o - 1;
    zell__plat_mutex_lock(&g_track_mu);
    t->next = g_track_head.next;
    t->prev = &g_track_head;
    g_track_head.next->prev = t;
    g_track_head.next = t;
    zell__plat_mutex_unlock(&g_track_mu);
}

static void track_remove(zell_object *o) {
    track *t = (track *)o - 1;
    zell__plat_mutex_lock(&g_track_mu);
    t->prev->next = t->next;
    t->next->prev = t->prev;
    t->prev = t->next = t;
    zell__plat_mutex_unlock(&g_track_mu);
}

/* ---- raw memory ---- */

void *zell_alloc(int64_t size) {
    if (size < 0) zell__panicf("zell_alloc: negative size %lld", (long long)size);
    void *p = calloc(1, size == 0 ? 1 : (size_t)size);
    if (!p) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    return p;
}

void *zell_realloc(void *p, int64_t size) {
    if (size < 0) zell__panicf("zell_realloc: negative size %lld", (long long)size);
    void *q = realloc(p, size == 0 ? 1 : (size_t)size);
    if (!q) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    return q;
}

void zell_free(void *p) { free(p); }

/* Coroutine frames (ABI.md 10.3): memory from the object heap's size
 * classes (frames are made and freed on every async call, often on
 * different threads), or calloc when the heap is off. The compiler
 * allocates with zell_coro_frame_alloc_uninit (no memset: the lowering
 * stores every slot the destroy path can read before it can run) and keeps
 * the zeroing zell_coro_frame_alloc for a coroutine whose destroy function
 * reads LLVM's suspend index. ZELL_FRAME_POISON=1 fills uninitialised
 * frames with 0xA5, to catch a read of a slot nobody wrote. */
/* Per-thread frame cache: a LIFO free list per heap size class, so a deep
 * chain of frames made and freed over and over (every async call that
 * suspends) neither walks the heap's page lists nor empties and retires
 * pages. No atomics: a frame freed on another thread goes into that
 * thread's cache (frames have no owner; the heap takes a block back from
 * any thread). Capped per class (FC_MAX_N frames, FC_MAX_BYTES) and per
 * thread (FC_MAX_TOTAL: a 1000-deep chain of 200-byte frames fits; more
 * would keep remotely freed frames from their owners' heaps, which then
 * take fresh pages); past the cap a frame goes back to the heap. Only
 * heap blocks are cached (the heap is off under ASan, so ASan sees every
 * frame), and only small ones. Flushed at thread exit (heap.c calls
 * zell__frame_cache_flush first) and at shutdown before the leak report. */
/* The compiler inlines both ends of the cache (ABI.md 10.3 "Frame cache"):
 * the layout, the caps and the size classes are part of the ABI. `lim` is 0
 * until this thread may cache (heap on, no ZELL_FRAME_POISON), so inlined
 * code finds the cache empty and full until then and calls the runtime. */
#define FC_MAX_N 1024
#define FC_MAX_BYTES ((int64_t)256 << 10)
#define FC_MAX_TOTAL ((int64_t)256 << 10)

typedef struct {
    void *head[ZELL__HP_CLASSES];
    uint32_t n[ZELL__HP_CLASSES];
    int64_t bytes;
    uint32_t lim;
} zell_frame_cache_t;

_Thread_local zell_frame_cache_t zell_frame_cache;
_Static_assert(ZELL__HP_CLASSES == 36 && offsetof(zell_frame_cache_t, n) == 288 && offsetof(zell_frame_cache_t, bytes) == 432
               && offsetof(zell_frame_cache_t, lim) == 440, "ABI.md 10.3: zell_frame_cache layout");
#define t_fc zell_frame_cache

void zell__frame_cache_flush(void) {
    for (uint32_t c = 0; c < ZELL__HP_CLASSES; c++) {
        void *p = t_fc.head[c];
        t_fc.head[c] = NULL;
        t_fc.n[c] = 0;
        while (p) {
            void *next = *(void **)p;
            zell__heap_free(p);
            p = next;
        }
    }
    t_fc.bytes = 0;
}

static inline __attribute__((always_inline)) void *frame_alloc(int64_t size, int zero) {
    if (__builtin_expect(!zell__heap_on, 0) && !zell__hp_start()) return NULL;
    if (__builtin_expect(size > ZELL__HP_SMALL_MAX, 0)) return zell__hp_alloc_big(size, zero);
    uint32_t c = zell__hp_cls(size);
    void *p = t_fc.head[c];
    if (p) {
        t_fc.head[c] = *(void **)p;
        t_fc.n[c]--;
        t_fc.bytes -= zell__hp_sizes[c];
    } else {
        p = zell__heap_alloc_cls(c);
    }
    if (zero) memset(p, 0, (size_t)size);
    return p;
}

void *zell_coro_frame_alloc(int64_t size) {
    if (size < 16) size = 16;
    void *p = frame_alloc(size, 1);
    return p ? p : zell_alloc(size);
}

static int g_frame_poison = -1;   /* -1: ZELL_FRAME_POISON not read yet */

static __attribute__((noinline)) int frame_poison_init(void) {
    const char *e = getenv("ZELL_FRAME_POISON");
    int on = e && *e && *e != '0';
    __atomic_store_n(&g_frame_poison, on, __ATOMIC_RELAXED);
    return on;
}

void *zell_coro_frame_alloc_uninit(int64_t size) {
    if (size < 16) size = 16;
    void *p = frame_alloc(size, 0);
    if (__builtin_expect(!p, 0)) {
        p = malloc((size_t)size);
        if (!p) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    }
    int poison = __atomic_load_n(&g_frame_poison, __ATOMIC_RELAXED);
    if (__builtin_expect(poison != 0, 0) && (poison > 0 || frame_poison_init())) memset(p, 0xA5, (size_t)size);
    return p;
}

/* Puts small block p of class c (block size sz) into this thread's frame
 * cache, or back to the heap when the cache is full. */
static inline __attribute__((always_inline)) void frame_cache_put(void *p, uint32_t c, int64_t sz) {
    uint32_t n = t_fc.n[c];
    if (__builtin_expect(!t_fc.lim, 0)) {
        int poison = __atomic_load_n(&g_frame_poison, __ATOMIC_RELAXED);
        if (poison < 0) poison = frame_poison_init();
        if (!poison) t_fc.lim = FC_MAX_N;
    }
    if (n < t_fc.lim &&(int64_t)(n + 1) * sz <= FC_MAX_BYTES && t_fc.bytes + sz <= FC_MAX_TOTAL) {
        *(void **)p = t_fc.head[c];
        t_fc.head[c] = p;
        t_fc.n[c] = n + 1;
        t_fc.bytes += sz;
        return;
    }
    zell__heap_free_small(p);
}

void zell_coro_frame_free(void *p) {
    if (!p) return;
    if (!zell__heap_on) {
        free(p);
        return;
    }
    if (__builtin_expect(!zell__heap_is_small(p), 0)) {
        zell__hp_free_big(p);
        return;
    }
    hp_page *pg = zell__hp_page_of(p);
    frame_cache_put(p, pg->cls, pg->bsize);
}

/* The size the frame was allocated with gives its class directly: no
 * segment-map lookup, no page header read (ABI.md 10.3). */
void zell_coro_frame_free_sized(void *p, int64_t size) {
    if (!p) return;
    if (size < 16) size = 16;
    if (__builtin_expect(!zell__heap_on || size > ZELL__HP_SMALL_MAX, 0)) {
        zell_coro_frame_free(p);
        return;
    }
    uint32_t c = zell__hp_cls(size);
    frame_cache_put(p, c, zell__hp_sizes[c]);
}

/* ---- objects ---- */

/* Allocates an object of `size` bytes (header included) with the header set
 * and the payload zeroed when `zero`. Objects come from the object heap
 * (heap.c) unless it is off or leak check mode 2 tracks them. */
static inline __attribute__((always_inline)) zell_object *object_alloc(const zell_type_info *type, int64_t size, int zero) {
    if (size < (int64_t)sizeof(zell_object))
        zell__panicf("zell_object_new: size %lld smaller than header", (long long)size);
    zell_object *o = NULL;
    if (!g_track) {
        /* Small blocks: zero only the payload; fresh large mappings are zero. */
        int small = size <= ZELL__HP_SMALL_MAX;
        o = zell__heap_alloc(size, zero && !small);
        if (o && zero && small) memset(o + 1, 0, (size_t)size - sizeof(zell_object));
    }
    if (!o) {
        if (g_track) {
            o = (zell_object *)((track *)zell_alloc(size + (int64_t)sizeof(track)) + 1);
            track_add(o);
        } else {
            o = malloc((size_t)size);
            if (!o) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
            if (zero) memset(o + 1, 0, (size_t)size - sizeof(zell_object));
        }
    }
    o->refcount = 1;
    o->type = type;
    stats_add(1, 1);
    if (ZELL_DIAG_HEAP_ON()) zell__diag_heap_count(type, 1);  /* ZELL_DIAG_HEAP */
    return o;
}

void *zell_object_new(const zell_type_info *type, int64_t size) {
    return object_alloc(type, size, 1);
}

zell_object *zell__object_new_uninit(const zell_type_info *type, int64_t size) {
    return object_alloc(type, size, 0);
}

/* Memory of a heap object comes from the object heap when it is on and
 * leak tracking is off (both fixed before the first object exists). */
static inline int from_heap(void) { return !g_track && zell__heap_on; }

static inline __attribute__((always_inline)) void object_free(zell_object *o) {
    stats_add(-1, 0);
    if (ZELL_DIAG_HEAP_ON()) zell__diag_heap_count(o->type, -1);  /* ZELL_DIAG_HEAP */
    if (from_heap()) {
        zell__heap_free(o);
    } else if (g_track) {
        track_remove(o);
        free((track *)o - 1);
    } else {
        free(o);
    }
}

int64_t zell__object_capacity(zell_object *o) {
    if (from_heap()) return zell__heap_capacity(o);
    if (g_track) return (int64_t)zell__plat_alloc_size((track *)o - 1) - (int64_t)sizeof(track);
    return (int64_t)zell__plat_alloc_size(o);
}

zell_object *zell__object_resize(zell_object *o, int64_t size) {
    if (from_heap()) return zell__hp_resize(o, size);
    if (!g_track) return zell_realloc(o, size);
    zell__plat_mutex_lock(&g_track_mu);
    track *t = (track *)o - 1;
    track *n = realloc(t, (size_t)size + sizeof(track));
    if (!n) {
        zell__plat_mutex_unlock(&g_track_mu);
        zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    }
    n->prev->next = n;
    n->next->prev = n;
    zell__plat_mutex_unlock(&g_track_mu);
    return (zell_object *)(n + 1);
}

void zell__object_immortalize(zell_object *o) {
    if (o->refcount < 0) return;
    o->refcount = ZELL_RC_IMMORTAL;
    stats_add(-1, 0);
    if (ZELL_DIAG_HEAP_ON()) zell__diag_heap_count(o->type, -1);
    if (g_track) track_remove(o);
}

/* ---- bounded release depth (rt_internal.h, ABI.md 3.2 "deferred drop") ---- */

typedef struct { zell__dead_fn fn; zell_object *o; int64_t w; } dead_item;
_Thread_local int64_t zell__dead_state;
static _Thread_local dead_item *t_dead_v;
static _Thread_local int64_t t_dead_n, t_dead_cap, t_dead_mark;

int zell__dead_enter_slow(void) {
    int64_t s = zell__dead_state;
    if ((int32_t)s >= ZELL_DEAD_DEPTH) return 0;
    t_dead_mark = t_dead_n;   /* entries above it are this frame's children */
    zell__dead_state = s + 1;
    return 1;
}

void zell__dead_push(zell__dead_fn fn, zell_object *o, int64_t w) {
    zell__dead_state |= ZELL_DEAD_PENDING;
    if (t_dead_n == t_dead_cap) {
        int64_t cap = t_dead_cap ? t_dead_cap * 2 : 256;
        dead_item *v = realloc(t_dead_v, (size_t)cap * sizeof *v);
        if (!v) zell__panicf("out of memory (requested %lld bytes)", (long long)(cap * (int64_t)sizeof *v));
        t_dead_v = v;
        t_dead_cap = cap;
    }
    t_dead_v[t_dead_n++] = (dead_item){ fn, o, w };
}

void zell__dead_settle(int64_t depth) {
    if (depth == ZELL_DEAD_DEPTH) {
        /* The children this frame's drop queued, in release order: reverse
         * them so the stack pops the first one first (recursive order). */
        for (int64_t i = t_dead_mark, j = t_dead_n - 1; i < j; i++, j--) {
            dead_item t = t_dead_v[i];
            t_dead_v[i] = t_dead_v[j];
            t_dead_v[j] = t;
        }
        return;
    }
    if (depth != 1) return;   /* an inner frame: the outermost one drains */
    /* Each popped finalizer runs at depth 1 and may recurse to the bound
     * again, queueing its own deep children on top: depth-first, iterative. */
    while (t_dead_n) {
        dead_item it = t_dead_v[--t_dead_n];
        it.fn(it.o, it.w);
    }
    zell__dead_state &= ~ZELL_DEAD_PENDING;
    free(t_dead_v);
    t_dead_v = NULL;
    t_dead_cap = 0;
}

/* The view block a view's refcount word forwards to (ABI.md 4.1). */
static inline zell_object *view_block(int64_t rc) {
    return (zell_object *)(uintptr_t)(rc & ~ZELL_RC_VIEW);
}

void zell_retain(void *obj) {
    if (!obj) return;
    zell_object *o = obj;
    int64_t n = __atomic_load_n(&o->refcount, __ATOMIC_RELAXED);
    if (n < 0) {
        if (!zell_rc_is_view(n)) return;
        o = view_block(n);
        n = __atomic_load_n(&o->refcount, __ATOMIC_RELAXED);
    }
    if (!__atomic_load_n(&zell_threaded, __ATOMIC_RELAXED))
        __atomic_store_n(&o->refcount, n + 1, __ATOMIC_RELAXED);   /* one thread: no RMW (and nothing is biased) */
    else if ((n & (ZELL_RC_BIASED | ZELL_RC_CYCLIC)) == ZELL_RC_BIASED)
        zell__rc_retain_biased(o, n);    /* biased counts (rc.c, ABI.md 3.1) */
    else if ((n & (ZELL_RC_CYCLIC | ZELL_RC_NOBIAS)) == ZELL_RC_NOBIAS)
        __atomic_add_fetch(&o->refcount, 1, __ATOMIC_RELAXED);   /* never biased (tasks): no CAS */
    else if (!(n & ZELL_RC_CYCLIC))
        zell__rc_retain_unowned(o, n);   /* may bias it (rc.c) */
    else if (__atomic_add_fetch(&o->refcount, 1, __ATOMIC_RELAXED) & ZELL_RC_MARKED)
        zell__cycles_dirty(o);   /* examined by the cycle collector (ABI.md 3.8) */
}


/* The deferred form of the plain release's finalization. */
static void dead_plain(zell_object *o, int64_t w) {
    (void)w;
    o->type->drop(o);
    object_free(o);
}

void zell_release(void *obj) {
    if (!obj) return;
    zell_object *o = obj;
    int64_t n = __atomic_load_n(&o->refcount, __ATOMIC_ACQUIRE);
    if (n < 0) {
        if (!zell_rc_is_view(n)) return;
        o = view_block(n);   /* blocks are never views or immortal */
        n = __atomic_load_n(&o->refcount, __ATOMIC_ACQUIRE);
    }
    if (n != 1) {
        /* One thread, a plain count of at least 2 below every flag bit: a
         * plain store (nothing is biased while single-threaded). */
        if (!__atomic_load_n(&zell_threaded, __ATOMIC_RELAXED) && (uint64_t)(n - 2) < ((uint64_t)1 << 57) - 2) {
            __atomic_store_n(&o->refcount, n - 1, __ATOMIC_RELAXED);
            return;
        }
        if ((n & (ZELL_RC_BIASED | ZELL_RC_CYCLIC)) == ZELL_RC_BIASED) {
            zell__rc_release_biased(o, n);   /* rc.c, ABI.md 3.1 */
            return;
        }
        /* Objects of cyclic types record possible cycle roots (ABI.md 3.8). */
        if (__builtin_expect((n & ZELL_RC_CYCLIC) != 0, 0)) {
            zell__cycles_release(o);
            return;
        }
        /* Shared (or has a weak box): the decrement must be atomic unless
         * the process has one thread. A count of exactly 1 with no weak
         * flag means the caller holds the only reference and nobody else
         * can retain it, so the RMW is skipped. */
        if (!__atomic_load_n(&zell_threaded, __ATOMIC_RELAXED) && n > 1 && !(n & (ZELL_RC_WEAK_FLAG | ZELL_RC_NOBIAS))) {
            __atomic_store_n(&o->refcount, n - 1, __ATOMIC_RELAXED);
            return;
        }
        /* Unowned and threaded: a CAS, since another thread may bias it
         * (a NOBIAS object never is: a plain atomic decrement). */
        if (__atomic_load_n(&zell_threaded, __ATOMIC_RELAXED) && !(n & ZELL_RC_NOBIAS)) {
            zell__rc_release_unowned(o, n);
            return;
        }
        n = __atomic_sub_fetch(&o->refcount, 1, __ATOMIC_ACQ_REL);
        if ((n & ZELL_RC_COUNT_MASK) != 0) return;
        /* Weak references are cleared before the destructor runs. */
        if (n & ZELL_RC_WEAK_FLAG) zell__weak_clear(o);
    } else {
        o->refcount = 0;
    }
    if (o->type && o->type->drop) {
        /* Children may die in the drop: bound the recursion
         * (rt_internal.h). An object without a drop frees nothing else,
         * so it needs no depth accounting. */
        if (!zell__dead_enter()) {
            zell__dead_push(dead_plain, o, 0);
            return;
        }
        o->type->drop(o);
        zell__dead_leave();
    }
    object_free(o);
}

int64_t zell_refcount(const void *obj) {
    if (!obj) return 0;
    int64_t rc = __atomic_load_n(&((const zell_object *)obj)->refcount, __ATOMIC_RELAXED);
    if (zell_rc_is_view(rc)) rc = __atomic_load_n(&view_block(rc)->refcount, __ATOMIC_RELAXED);
    return zell__rc_count(rc);
}

int64_t zell_live_objects(void) { return stats_sum(0); }

int64_t zell__total_objects(void) { return stats_sum(1); }

bool zell__heap_each(void (*cb)(const char *type, int64_t bytes, void *arg), void *arg) {
    if (!g_track) return false;
    zell__plat_mutex_lock(&g_track_mu);
    for (track *t = g_track_head.next; t != &g_track_head; t = t->next) {
        zell_object *o = (zell_object *)(t + 1);
        const char *name = (o->type && o->type->name) ? o->type->name : "<untyped>";
        int64_t bytes = (int64_t)zell__plat_alloc_size(t) - (int64_t)sizeof(track);
        cb(name, bytes, arg);
    }
    zell__plat_mutex_unlock(&g_track_mu);
    return true;
}

void zell__live_each(void (*cb)(zell_object *obj, void *arg), void *arg) {
    if (!g_track) return;
    zell__plat_mutex_lock(&g_track_mu);
    for (track *t = g_track_head.next; t != &g_track_head; t = t->next) cb((zell_object *)(t + 1), arg);
    zell__plat_mutex_unlock(&g_track_mu);
}

/* ---- pointers to payloads (`*T` values) ---- */

void zell_ptr_retain(void *payload) {
    if (payload) zell_retain((char *)payload - sizeof(zell_object));
}

void zell_ptr_release(void *payload) {
    if (payload) zell_release((char *)payload - sizeof(zell_object));
}

/* ---- leak report ---- */

typedef struct { const char *name; int64_t n; } type_count;

static int cmp_count(const void *a, const void *b) {
    const type_count *x = a, *y = b;
    if (x->n != y->n) return x->n < y->n ? 1 : -1;
    return strcmp(x->name, y->name);
}

void zell__leak_report(void) {
    if (g_leak_report) g_leak_report();
}

static void leak_report(void) {
    if (!g_leak_mode) return;
    /* The cycle collector's last word: a final collection (auto mode) or a
     * count of the cyclic garbage it could free (manual mode). */
    int64_t collectable = zell__cycles_at_exit ? zell__cycles_at_exit() : -1;
    int64_t live = zell_live_objects();
    if (live <= 0) return;
    fflush(stdout);
    fprintf(stderr, "leak: %lld objects\n", (long long)live);
    if (g_track) {
        zell__plat_mutex_lock(&g_track_mu);
        int64_t cap = 16, len = 0;
        type_count *counts = calloc((size_t)cap, sizeof *counts);
        for (track *t = g_track_head.next; t != &g_track_head && counts; t = t->next) {
            zell_object *o = (zell_object *)(t + 1);
            const char *name = (o->type && o->type->name) ? o->type->name : "<untyped>";
            int64_t i = 0;
            while (i < len && strcmp(counts[i].name, name) != 0) i++;
            if (i == len) {
                if (len == cap) {
                    type_count *nc = realloc(counts, (size_t)(cap * 2) * sizeof *counts);
                    if (!nc) break;
                    counts = nc;
                    cap *= 2;
                }
                counts[len].name = name;
                counts[len].n = 0;
                len++;
            }
            counts[i].n++;
        }
        zell__plat_mutex_unlock(&g_track_mu);
        if (counts) {
            qsort(counts, (size_t)len, sizeof *counts, cmp_count);
            for (int64_t i = 0; i < len; i++)
                fprintf(stderr, "  %lld x %s\n", (long long)counts[i].n, counts[i].name);
            free(counts);
        }
        zell__leak_cycles();
    }
    if (collectable > 0)
        fprintf(stderr, "  %lld of them in unreachable reference cycles the cycle collector can free (ZELL_CYCLES=auto or runtime.CollectCycles())\n",
                (long long)collectable);
    fflush(stderr);
    _Exit(ZELL_LEAK_EXIT_CODE);
}

/* ---- value ops ---- */

static void ops_ref_retain(void *slot) { zell_retain(*(void **)slot); }
static void ops_ref_release(void *slot) { zell_release(*(void **)slot); }
const zell_value_ops zell_ops_ref = { ops_ref_retain, ops_ref_release, NULL };

static void ops_ptr_retain(void *slot) { zell_ptr_retain(*(void **)slot); }
static void ops_ptr_release(void *slot) { zell_ptr_release(*(void **)slot); }
const zell_value_ops zell_ops_ptr = { ops_ptr_retain, ops_ptr_release, NULL };

static void ops_slice_retain(void *slot) { zell_slice_retain((zell_slice *)slot); }
static void ops_slice_release(void *slot) { zell_slice_release((zell_slice *)slot); }
const zell_value_ops zell_ops_slice = { ops_slice_retain, ops_slice_release, NULL };

/* ---- for the cycle collector (cycles.c) ---- */

void zell__object_free(zell_object *o) { object_free(o); }
void zell__object_uncount(void) { stats_add(-1, 0); }
void zell__object_recount(void) { stats_add(1, 0); }

/* Collection epoch (ABI.md 3.8): bumped when a cycle collection starts and
 * when it ends. Compiled code reads it when it pins a `*T` of a cyclic type
 * for a call and passes it back on the paired release: if no collection
 * overlapped the pin, that release cannot leave an unrecorded garbage
 * cycle behind (every other reference dropped meanwhile was a recorded
 * decrement), so it skips recording a possible root. */
int64_t zell_cycles_epoch;

void zell_ptr_release_noroot(void *payload, int64_t epoch) {
    if (!payload) return;
    zell_object *o = (zell_object *)((char *)payload - sizeof(zell_object));
    int64_t n = __atomic_load_n(&o->refcount, __ATOMIC_RELAXED);
    if (n >= 0 && (n & ZELL_RC_CYCLIC) && epoch == __atomic_load_n(&zell_cycles_epoch, __ATOMIC_ACQUIRE)) {
        /* One thread and not the last reference: a plain decrement (no
         * collection runs concurrently, so DIRTY is not needed). */
        if (!__atomic_load_n(&zell_threaded, __ATOMIC_RELAXED) && (n & ZELL_RC_COUNT_MASK) > 1) {
            __atomic_store_n(&o->refcount, n - 1, __ATOMIC_RELAXED);
            return;
        }
        zell__cycles_release_noroot(o);
        return;
    }
    zell_release(o);
}
