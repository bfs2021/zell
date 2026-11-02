/* heap.c - the object heap (ABI.md 3.2, "Allocation"): small objects in
 * 64 KB pages owned by one thread each, large objects in mappings of their
 * own, and the return of free memory to the OS.
 *
 * Small objects (at most HP_SMALL_MAX bytes, header included) are rounded
 * up to one of HP_CLASSES size classes (16-byte steps up to 128, then four
 * classes per power of two). Memory comes from 4 MB *segments* (aligned
 * mappings, never unmapped) cut into 64 *pages* of 64 KB; the descriptors
 * of a segment's pages sit at its base, so the page of a block is found by
 * masking its address, and a two-level bitmap over the address space tells
 * whether an address lies in a segment at all.
 *
 * A page serves one size class and is owned by one thread's heap. The
 * owner allocates from the page's `free` list and frees into `local_free`
 * with plain loads and stores, and counts the blocks handed out (`used`).
 * Other threads push the blocks they free onto the page's atomic `xfree`
 * list; the owner takes that list over (and subtracts its length from
 * `used`) when it runs out of blocks. A page that runs completely out of
 * blocks moves to its heap's `full` list; the first remote free into such a
 * page bumps the heap's `hint`, and the owner then sweeps its full pages in
 * its next slow path (or when its worker thread goes idle,
 * zell__heap_collect). Setting `in_full` before the last check of
 * `xfree`, against a remote free's push before its read of `in_full`
 * (both sequentially consistent), ensures a wake-up cannot be lost.
 *
 * A page whose `used` drops to 0 is *retired* to the global pool (unless it
 * is the only page of its class in the heap), where any thread can take it
 * for any class. Free pages and cached large mappings form the *reserve*:
 * synchronously it may hold up to the recent peak of the heap in use minus
 * what is in use now (at least ZELL_HEAP_RESERVE, 16 MB), so rebuilding
 * data up to the size it had lately reuses the same memory without page
 * faults and in use + reserve stays within that peak; beyond it the oldest
 * free pages are released at once. A background scavenger thread (below)
 * gives back what stayed unused for 0.3 s beyond ZELL_HEAP_RESERVE. A
 * page is released with zell__plat_mem_release (MADV_DONTNEED / MADV_FREE
 * / DiscardVirtualMemory: the address range stays, the memory goes back to
 * the OS), a cached mapping with munmap. zell__heap_collect(1)
 * (runtime.FreeOSMemory) releases the whole reserve.
 *
 * When a thread exits, its pages without live blocks are retired and the
 * others are *abandoned* (owner NULL): remote frees keep going to `xfree`,
 * and a thread that needs a page of that class adopts one; abandoned pages
 * that become free are swept back to the pool now and then.
 *
 * Objects over 16 KB and under 128 KB are *medium*: malloc'd with a 16-byte
 * prefix {kind, usable size}. Larger ones are *large*: a mapping of their
 * own (the same prefix with its length), grown by mremap where the OS has
 * it. Freed mappings
 * are cached (within the reserve) for reuse by allocations of a similar
 * size, so a program that allocates and frees large buffers in a loop does
 * not pay a system call and page faults each time.
 *
 * The heap is off (callers use malloc) under AddressSanitizer (so it sees
 * every object), when ZELL_NO_SLAB=1 is set (Valgrind) or when the TLS
 * key cannot be created; the choice is made once, at the first allocation.
 * Leak check mode 2 bypasses it (mem.c). */
#include "heap.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

#if defined(__SANITIZE_ADDRESS__)
#define HP_ENABLED 0
#elif defined(__has_feature)
#if __has_feature(address_sanitizer)
#define HP_ENABLED 0
#endif
#endif
#ifndef HP_ENABLED
#define HP_ENABLED 1
#endif

#define HP_EXTEND 16384                          /* bytes carved into a free list at a time */
#define HP_BIG_HDR 16                            /* prefix of medium and large objects */
#define HP_LARGE_MIN ((int64_t)128 << 10)        /* mapping of its own from here (prefix included) */
#define HP_LARGE_SLOTS 64                        /* cached large mappings */

enum { PG_FREE = 1, PG_RELEASED, PG_OWNED, PG_ABANDONED };

const uint32_t zell__hp_sizes[ZELL__HP_CLASSES] = {
    16, 32, 48, 64, 80, 96, 112, 128,
    160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024,
    1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168, 8192,
    10240, 12288, 14336, 16384,
};
_Static_assert(ZELL__HP_SMALL_MAX == 16384, "last class");

typedef struct hp_segment {
    hp_page pages[HP_PAGES];
    struct hp_segment *next;     /* all segments (keeps them reachable for leak checkers) */
    int carved;                  /* pages handed out at least once */
} hp_segment;
#define HP_SEG_HDR ((sizeof(hp_segment) + 4095) & ~(size_t)4095)
_Static_assert(HP_SEG_HDR < HP_PAGE / 2, "segment header fits in the first page");

static hp_page hp_empty_page;          /* never has blocks */
static hp_heap hp_empty_heap = { .cur = { [0 ... ZELL__HP_CLASSES - 1] = &hp_empty_page } };
_Thread_local hp_heap *zell__hp_tls = &hp_empty_heap;
#define t_heap zell__hp_tls

uint8_t zell__heap_on;                /* 1 once the heap is in use (rt_internal.h) */
static int g_mode;                     /* 0 undecided, 1 on, 2 off */
static zell__plat_once g_once = ZELL_PLAT_ONCE_INIT;
static zell__plat_tls_key g_key;

/* The pool (g_mu): free pages (committed, then released), abandoned pages,
 * cached large mappings, segments, heaps of exited threads. */
static zell__plat_mutex g_mu = ZELL_PLAT_MUTEX_INIT;
static hp_list g_free, g_released;
static hp_list g_abandoned[ZELL__HP_CLASSES];
static int64_t g_nabandoned;
static hp_segment *g_segments, *g_seg_cur;
static hp_heap *g_heaps_free;
static struct { char *base; size_t len; int64_t freed_at; } g_cache[HP_LARGE_SLOTS];
static int g_ncache;
static int64_t g_acquired;             /* page acquisitions (sweep abandoned pages now and then) */
static uint32_t g_epoch;               /* atomic: bumped by a forced collection */

/* Byte counts (atomic). */
static int64_t g_in_use;               /* owned pages + live large mappings */
static int64_t g_free_bytes;           /* committed free pages (g_mu) */
static int64_t g_cache_bytes;          /* cached large mappings (g_mu) */
static int64_t g_released_bytes;       /* released free pages (g_mu) */
static int64_t g_returned_total;       /* bytes ever given back to the OS */
static int64_t g_reserve_min = (int64_t)16 << 20;

uint64_t *zell__hp_map[HP_TOP];        /* segment bitmap leaves, set once */

/* ---- lists ---- */

static inline void list_push(hp_list *l, hp_page *p) {  /* at the head */
    p->prev = NULL;
    p->next = l->head;
    if (l->head) l->head->prev = p; else l->tail = p;
    l->head = p;
}

static inline void list_append(hp_list *l, hp_page *p) {
    p->next = NULL;
    p->prev = l->tail;
    if (l->tail) l->tail->next = p; else l->head = p;
    l->tail = p;
}

static inline void list_remove(hp_list *l, hp_page *p) {
    if (p->prev) p->prev->next = p->next; else l->head = p->next;
    if (p->next) p->next->prev = p->prev; else l->tail = p->prev;
    p->next = p->prev = NULL;
}

/* ---- segments and pages ---- */

static inline char *page_base(hp_page *pg) {
    hp_segment *s = (hp_segment *)((uintptr_t)pg & ~(uintptr_t)(HP_SEG - 1));
    return (char *)s + (size_t)(pg - s->pages) * HP_PAGE;
}

/* Marks a segment in the bitmap (g_mu held). */
static int seg_mark(char *base) {
    uintptr_t r = (uintptr_t)base >> HP_SEG_SHIFT;
    if ((uintptr_t)base >> HP_ADDR_BITS) return 0;
    uint64_t **slot = &zell__hp_map[r >> HP_LEAF_BITS];
    uint64_t *leaf = *slot;
    if (!leaf) {
        leaf = calloc((1u << HP_LEAF_BITS) / 64, sizeof(uint64_t));
        if (!leaf) return 0;
        __atomic_store_n(slot, leaf, __ATOMIC_RELEASE);
    }
    uintptr_t bit = r & ((1u << HP_LEAF_BITS) - 1);
    __atomic_or_fetch(&leaf[bit >> 6], (uint64_t)1 << (bit & 63), __ATOMIC_RELEASE);
    return 1;
}

/* The range of a free page that can go back to the OS (not the segment header). */
static void page_release_range(hp_page *pg, char **lo, size_t *len) {
    char *b = page_base(pg);
    size_t ps = zell__plat_page_size();
    char *s = b;
    if (pg == &((hp_segment *)b)->pages[0]) s = (char *)(((uintptr_t)b + HP_SEG_HDR + ps - 1) & ~(uintptr_t)(ps - 1));
    *lo = s;
    *len = (size_t)(b + HP_PAGE - s);
}

static void page_init(hp_page *pg, uint32_t c, hp_heap *h) {
    char *b = page_base(pg);
    char *s = b;
    if (pg == &((hp_segment *)b)->pages[0]) s = b + HP_SEG_HDR;
    uint32_t bs = zell__hp_sizes[c];
    size_t n = (size_t)(b + HP_PAGE - s) / bs;
    pg->free = pg->local_free = NULL;
    __atomic_store_n(&pg->xfree, (hp_block *)NULL, __ATOMIC_RELAXED);
    pg->start = pg->bump = s;
    pg->end = s + n * bs;
    pg->bsize = bs;
    pg->used = 0;
    pg->cls = (uint16_t)c;
    pg->state = PG_OWNED;
    __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
    __atomic_store_n(&pg->heap, h, __ATOMIC_RELEASE);
}

/* ---- the reserve ---- */

/* What free memory stays (the reserve): synchronously, up to the recent
 * peak of the heap in use minus what is in use now (so a program that
 * frees and rebuilds its data does not fault the same pages in again, and
 * in use + kept stays within that peak); the scavenger thread gives back
 * what stayed unused for HP_SCAV_AGE_NS beyond g_reserve_min. */
static int64_t g_peak_cur, g_peak_prev;   /* atomic: max in use this / last scavenger period */

static inline void note_peak(void) {
    int64_t u = __atomic_load_n(&g_in_use, __ATOMIC_RELAXED);
    int64_t p = __atomic_load_n(&g_peak_cur, __ATOMIC_RELAXED);
    while (u > p && !__atomic_compare_exchange_n(&g_peak_cur, &p, u, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {
    }
}

/* The memory limit (DESIGN.md "Память"): ZELL_MEMLIMIT, else the
 * container's (zell__plat_mem_limit), else none (0); runtime.SetMemLimit
 * changes it. The heap aims at 15/16 of it (the rest is for thread stacks,
 * code and malloc) by keeping no reserve that would pass the target, so
 * memory freed near the limit goes back to the OS at once. */
static int64_t g_mem_limit;               /* atomic, bytes; 0: none */
static const char *g_mem_limit_src;
static zell__plat_once g_limit_once = ZELL_PLAT_ONCE_INIT;

static inline int64_t mem_target(int64_t lim) { return lim - lim / 16; }

/* What a reserve may hold under the limit: up to the target, 0 past it. */
static inline int64_t limit_room(void) {
    int64_t lim = __atomic_load_n(&g_mem_limit, __ATOMIC_RELAXED);
    if (!lim) return INT64_MAX;
    int64_t r = mem_target(lim) - __atomic_load_n(&g_in_use, __ATOMIC_RELAXED);
    return r > 0 ? r : 0;
}

/* The reserve the scavenger keeps: g_reserve_min, within the limit. */
static inline int64_t reserve_floor(void) {
    int64_t m = __atomic_load_n(&g_reserve_min, __ATOMIC_RELAXED), r = limit_room();
    return m < r ? m : r;
}

/* Free memory kept synchronously: what keeps in use + kept within the
 * recent peak (so the process does not grow past what it needed lately),
 * and at least g_reserve_min; never past the memory limit's target. */
static inline int64_t reserve_limit(void) {
    int64_t m = __atomic_load_n(&g_reserve_min, __ATOMIC_RELAXED);
    int64_t a = __atomic_load_n(&g_peak_cur, __ATOMIC_RELAXED), b = __atomic_load_n(&g_peak_prev, __ATOMIC_RELAXED);
    if (a < b) a = b;
    a -= __atomic_load_n(&g_in_use, __ATOMIC_RELAXED);
    if (a < m) a = m;
    int64_t r = limit_room();
    return a < r ? a : r;
}

typedef struct { void *p; size_t len; hp_page *pg; } victim;   /* pg NULL: unmap p */
#define TRIM_BATCH 32

/* Picks what to give back while the reserve is over `limit` (g_mu held),
 * oldest first and only what was freed at or before `before` (monotonic
 * ns); the caller makes the system calls after unlocking (trim_apply). A
 * page being released is in no list, so nobody reuses it before its
 * release. */
static int trim_pick(int64_t limit, int64_t before, victim *v) {
    int n = 0;
    while (n < TRIM_BATCH && g_free_bytes + g_cache_bytes > limit) {
        int cache_ok = g_ncache && g_cache[0].freed_at <= before;
        int page_ok = g_free.tail && g_free.tail->freed_at <= before;
        if (cache_ok && (g_cache_bytes >= g_free_bytes || !page_ok)) {
            /* the oldest cached mapping */
            v[n].p = g_cache[0].base;
            v[n].len = g_cache[0].len;
            v[n].pg = NULL;
            g_cache_bytes -= (int64_t)g_cache[0].len;
            memmove(&g_cache[0], &g_cache[1], (size_t)(g_ncache - 1) * sizeof g_cache[0]);
            __atomic_store_n(&g_ncache, g_ncache - 1, __ATOMIC_RELAXED);
            n++;
        } else if (page_ok) {
            hp_page *pg = g_free.tail;   /* the least recently freed page */
            list_remove(&g_free, pg);
            pg->state = PG_RELEASED;
            g_free_bytes -= (int64_t)HP_PAGE;
            page_release_range(pg, (char **)&v[n].p, &v[n].len);
            v[n].pg = pg;
            n++;
        } else {
            break;
        }
    }
    return n;
}

static void trim_apply(victim *v, int n) {
    int pages = 0;
    for (int i = 0; i < n; i++) {
        if (v[i].pg) {
            zell__plat_mem_release(v[i].p, v[i].len);
            pages++;
        } else {
            zell__plat_mem_unmap(v[i].p, v[i].len);
        }
        __atomic_add_fetch(&g_returned_total, (int64_t)v[i].len, __ATOMIC_RELAXED);
    }
    if (!pages) return;
    zell__plat_mutex_lock(&g_mu);
    for (int i = 0; i < n; i++) {
        if (!v[i].pg) continue;
        list_push(&g_released, v[i].pg);
        g_released_bytes += (int64_t)HP_PAGE;
    }
    zell__plat_mutex_unlock(&g_mu);
}

/* Gives memory back until the reserve is within `limit` (g_mu NOT held). */
static void trim(int64_t limit) {
    victim v[TRIM_BATCH];
    for (;;) {
        zell__plat_mutex_lock(&g_mu);
        int n = trim_pick(limit, INT64_MAX, v);
        zell__plat_mutex_unlock(&g_mu);
        if (!n) return;
        trim_apply(v, n);
    }
}

/* Under a memory limit, growth shrinks what the reserve may hold: each
 * time the heap in use grew by 1 MB since the last look, the reserve over
 * the new room goes back to the OS (wave B of objects of other sizes does
 * not stack on wave A's freed pages). g_mu NOT held. */
static int64_t g_limit_mark;   /* atomic: in use at the last look */

static inline void limit_grow(void) {
    if (!__atomic_load_n(&g_mem_limit, __ATOMIC_RELAXED)) return;
    int64_t u = __atomic_load_n(&g_in_use, __ATOMIC_RELAXED), m = __atomic_load_n(&g_limit_mark, __ATOMIC_RELAXED);
    if (u - m < ((int64_t)1 << 20)) {
        if (u < m) __atomic_store_n(&g_limit_mark, u, __ATOMIC_RELAXED);
        return;
    }
    __atomic_store_n(&g_limit_mark, u, __ATOMIC_RELAXED);
    trim(reserve_limit());
}

/* ---- the scavenger ----
 * A background thread (started the first time free memory exceeds
 * g_reserve_min; it never touches objects, so zell_threaded stays 0):
 * every HP_SCAV_PERIOD_NS it gives back the free pages and cached mappings
 * that stayed unused for HP_SCAV_AGE_NS, down to g_reserve_min, and starts
 * a new peak period. It sleeps on g_scav_cv while there is nothing over the
 * minimum. So freed memory beyond 16 MB is back with the OS about 0.2-0.5 s
 * after it was freed, while a program that reuses it within that time
 * never faults it in again. */
#define HP_SCAV_PERIOD_NS ((int64_t)200000000)
#define HP_SCAV_AGE_NS ((int64_t)300000000)

static zell__plat_cond g_scav_cv = ZELL_PLAT_COND_INIT;
static int g_scav_state;              /* 0 not started, 1 working, 2 waiting for work (g_mu) */

/* Whether the scavenger must be started or woken (g_mu held). */
static inline int scav_wanted_locked(void) {
    return g_scav_state != 1 && g_free_bytes + g_cache_bytes > reserve_floor();
}

static void *scav_main(void *arg) {
    (void)arg;
    victim v[TRIM_BATCH];
    zell__plat_mutex_lock(&g_mu);
    for (;;) {
        int64_t min = reserve_floor();
        if (g_free_bytes + g_cache_bytes <= min) {
            g_scav_state = 2;
            zell__plat_cond_wait(&g_scav_cv, &g_mu);
            g_scav_state = 1;
            continue;
        }
        zell__plat_mutex_unlock(&g_mu);
        zell__plat_sleep_ns(HP_SCAV_PERIOD_NS);
        __atomic_store_n(&g_peak_prev, __atomic_load_n(&g_peak_cur, __ATOMIC_RELAXED), __ATOMIC_RELAXED);
        __atomic_store_n(&g_peak_cur, __atomic_load_n(&g_in_use, __ATOMIC_RELAXED), __ATOMIC_RELAXED);
        int64_t before = zell__plat_mono_ns() - HP_SCAV_AGE_NS;
        for (;;) {
            zell__plat_mutex_lock(&g_mu);
            int n = trim_pick(min, before, v);
            zell__plat_mutex_unlock(&g_mu);
            if (!n) break;
            trim_apply(v, n);
        }
        /* under a memory limit, malloc's free memory (medium objects) goes too */
        if (__atomic_load_n(&g_mem_limit, __ATOMIC_RELAXED)) zell__plat_malloc_trim();
        zell__plat_mutex_lock(&g_mu);
    }
    return NULL;
}

/* Starts or wakes the scavenger (g_mu NOT held). */
static void scav_poke(void) {
    zell__plat_mutex_lock(&g_mu);
    int st = g_scav_state;
    if (st == 0) g_scav_state = 1;
    else if (st == 2) zell__plat_cond_signal(&g_scav_cv);
    zell__plat_mutex_unlock(&g_mu);
    if (st == 0 && !zell__plat_thread_start_background(scav_main, NULL)) {
        zell__plat_mutex_lock(&g_mu);
        g_scav_state = 0;
        zell__plat_mutex_unlock(&g_mu);
    }
}

/* ---- the pool ---- */

/* A page for class c owned by h (g_mu NOT held), or NULL (out of memory). */
static hp_page *page_acquire(hp_heap *h, uint32_t c) {
    zell__plat_mutex_lock(&g_mu);
    hp_page *pg = g_free.head;
    if (pg) {
        list_remove(&g_free, pg);
        g_free_bytes -= (int64_t)HP_PAGE;
    } else if ((pg = g_released.head)) {
        list_remove(&g_released, pg);
        g_released_bytes -= (int64_t)HP_PAGE;
    } else {
        if (!g_seg_cur || g_seg_cur->carved == HP_PAGES) {
            hp_segment *s = zell__plat_mem_map(HP_SEG, HP_SEG);
            if (!s || !seg_mark((char *)s)) {
                zell__plat_mutex_unlock(&g_mu);
                if (s) zell__plat_mem_unmap(s, HP_SEG);
                return NULL;
            }
            /* From the second segment on, huge pages: a heap that grew past
             * 4 MB takes far fewer page faults, a small program keeps its
             * small RSS. */
            if (g_segments && limit_room() > __atomic_load_n(&g_mem_limit, __ATOMIC_RELAXED) / 2) zell__plat_mem_hint_huge(s, HP_SEG);
            s->next = g_segments;
            g_segments = s;
            g_seg_cur = s;
        }
        pg = &g_seg_cur->pages[g_seg_cur->carved++];
    }
    __atomic_add_fetch(&g_acquired, 1, __ATOMIC_RELAXED);
    zell__plat_mutex_unlock(&g_mu);
    __atomic_add_fetch(&g_in_use, (int64_t)HP_PAGE, __ATOMIC_RELAXED);
    note_peak();
    limit_grow();
    page_init(pg, c, h);
    return pg;
}

/* Returns a page without live blocks to the pool (g_mu held). */
static void page_retire_locked(hp_page *pg) {
    pg->freed_at = zell__plat_mono_ns();
    __atomic_store_n(&pg->heap, (hp_heap *)NULL, __ATOMIC_RELAXED);
    __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
    pg->state = PG_FREE;
    pg->free = pg->local_free = NULL;
    list_push(&g_free, pg);
    g_free_bytes += (int64_t)HP_PAGE;
    __atomic_sub_fetch(&g_in_use, (int64_t)HP_PAGE, __ATOMIC_RELAXED);
}

static void page_retire(hp_page *pg) {
    victim v[TRIM_BATCH];
    zell__plat_mutex_lock(&g_mu);
    page_retire_locked(pg);
    int n = g_free_bytes + g_cache_bytes > reserve_limit() ? trim_pick(reserve_limit(), INT64_MAX, v) : 0;
    int poke = scav_wanted_locked();
    zell__plat_mutex_unlock(&g_mu);
    if (n) trim_apply(v, n);
    if (poke) scav_poke();
}

/* Takes over the remote frees of pg (owner or g_mu for abandoned pages):
 * they go to `free` if it is empty, else to local_free. */
static inline int page_collect(hp_page *pg) {
    if (!__atomic_load_n(&pg->xfree, __ATOMIC_RELAXED)) return 0;
    hp_block *x = __atomic_exchange_n(&pg->xfree, (hp_block *)NULL, __ATOMIC_ACQUIRE);
    if (!x) return 0;
    uint32_t n = 1;
    hp_block *last = x;
    while (last->next) {
        last = last->next;
        n++;
    }
    pg->used -= n;
    if (!pg->free) {
        last->next = NULL;
        pg->free = x;
    } else {
        last->next = pg->local_free;
        pg->local_free = x;
    }
    return 1;
}

/* Sweeps the abandoned pages: retires those whose blocks are all free. */
static void sweep_abandoned(void) {
    victim v[TRIM_BATCH];
    zell__plat_mutex_lock(&g_mu);
    for (uint32_t c = 0; c < ZELL__HP_CLASSES; c++) {
        hp_page *pg = g_abandoned[c].head;
        while (pg) {
            hp_page *next = pg->next;
            page_collect(pg);
            if (pg->used == 0) {
                list_remove(&g_abandoned[c], pg);
                __atomic_sub_fetch(&g_nabandoned, 1, __ATOMIC_RELAXED);
                /* abandoned pages still count as in use */
                page_retire_locked(pg);
            }
            pg = next;
        }
    }
    int n = trim_pick(reserve_limit(), INT64_MAX, v);
    int poke = scav_wanted_locked();
    zell__plat_mutex_unlock(&g_mu);
    if (n) trim_apply(v, n);
    if (poke) scav_poke();
}

/* ---- heaps ---- */

static void heap_thread_exit(void *arg);

static void mode_init(void);

int zell__hp_start(void) {
    if (__atomic_load_n(&g_mode, __ATOMIC_ACQUIRE) == 0) zell__plat_once_run(&g_once, mode_init);
    return __atomic_load_n(&g_mode, __ATOMIC_ACQUIRE) == 1;
}

/* "512MiB", "1G", "800000000", "off" (k/m/g and ki/mi/gi are both 1024-based). -1: unparsable. */
static int64_t parse_limit(const char *e) {
    if (strcmp(e, "off") == 0 || strcmp(e, "0") == 0) return 0;
    char *end;
    long long v = strtoll(e, &end, 10);
    if (v <= 0 || end == e) return -1;
    switch (*end) {
    case 'k': case 'K': v <<= 10; break;
    case 'm': case 'M': v <<= 20; break;
    case 'g': case 'G': v <<= 30; break;
    case 't': case 'T': v <<= 40; break;
    case 0: break;
    default: return -1;
    }
    return (int64_t)v;
}

static void limit_init(void) {
    const char *e = getenv("ZELL_MEMLIMIT");
    int64_t v = e && *e ? parse_limit(e) : -1;
    if (v >= 0) {
        __atomic_store_n(&g_mem_limit, v, __ATOMIC_RELAXED);
        g_mem_limit_src = "ZELL_MEMLIMIT";
        return;
    }
    const char *src = NULL;
    __atomic_store_n(&g_mem_limit, zell__plat_mem_limit(&src), __ATOMIC_RELAXED);
    g_mem_limit_src = src;
}

int64_t zell__heap_mem_limit(const char **src) {
    zell__plat_once_run(&g_limit_once, limit_init);
    if (src) *src = g_mem_limit_src;
    return __atomic_load_n(&g_mem_limit, __ATOMIC_RELAXED);
}

int64_t zell_runtime_mem_limit(void) { return zell__heap_mem_limit(NULL); }

int64_t zell_runtime_set_mem_limit(int64_t n) {
    int64_t old = zell__heap_mem_limit(NULL);
    __atomic_store_n(&g_mem_limit, n > 0 ? n : 0, __ATOMIC_RELAXED);
    g_mem_limit_src = "runtime.SetMemLimit";
    trim(reserve_limit());   /* a lower limit takes effect now */
    if (n > 0) zell__plat_malloc_trim();
    return old;
}

static void mode_init(void) {
    int on = HP_ENABLED;
    zell__plat_once_run(&g_limit_once, limit_init);
    const char *e = getenv("ZELL_NO_SLAB");
    if (e && *e && *e != '0') on = 0;
    if (on && !zell__plat_tls_new(&g_key, heap_thread_exit)) on = 0;
    const char *r = getenv("ZELL_HEAP_RESERVE");
    if (r && *r) {
        char *end;
        long long v = strtoll(r, &end, 10);
        if (*end == 'k' || *end == 'K') v <<= 10;
        else if (*end == 'm' || *end == 'M') v <<= 20;
        else if (*end == 'g' || *end == 'G') v <<= 30;
        if (v >= 0) __atomic_store_n(&g_reserve_min, (int64_t)v, __ATOMIC_RELAXED);
    }
    __atomic_store_n(&zell__heap_on, (uint8_t)on, __ATOMIC_RELAXED);
    __atomic_store_n(&g_mode, on ? 1 : 2, __ATOMIC_RELEASE);
}

static hp_heap *heap_new(void) {
    zell__plat_mutex_lock(&g_mu);
    hp_heap *h = g_heaps_free;
    if (h) g_heaps_free = h->next_free;
    zell__plat_mutex_unlock(&g_mu);
    if (!h) {
        h = calloc(1, sizeof *h);   /* never freed: remote frees may still bump a stale heap's hint */
        if (!h) zell__panicf("out of memory (thread heap)");
    }
    for (int c = 0; c < ZELL__HP_CLASSES; c++) {
        h->cur[c] = &hp_empty_page;
        h->queue[c].head = h->queue[c].tail = NULL;
        h->full[c].head = h->full[c].tail = NULL;
    }
    h->seen = __atomic_load_n(&h->hint, __ATOMIC_RELAXED);
    h->epoch = __atomic_load_n(&g_epoch, __ATOMIC_RELAXED);
    h->next_free = NULL;
    zell__plat_tls_set(g_key, h);
    t_heap = h;
    return h;
}

/* Moves full pages that got remote frees back to the queue (retiring
 * those without live blocks); with `all`, also retires the empty pages of
 * the queue (a forced collection). */
static void heap_sweep(hp_heap *h, int all) {
    h->seen = __atomic_load_n(&h->hint, __ATOMIC_SEQ_CST);
    for (uint32_t c = 0; c < ZELL__HP_CLASSES; c++) {
        hp_page *pg = h->full[c].head;
        while (pg) {
            hp_page *next = pg->next;
            if (page_collect(pg)) {
                list_remove(&h->full[c], pg);
                __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
                if (pg->used == 0) page_retire(pg);
                else list_append(&h->queue[c], pg);
            }
            pg = next;
        }
        if (all) {
            pg = h->queue[c].head;
            while (pg) {
                hp_page *next = pg->next;
                page_collect(pg);
                if (pg->used == 0) {
                    list_remove(&h->queue[c], pg);
                    page_retire(pg);
                }
                pg = next;
            }
        }
        h->cur[c] = h->queue[c].head ? h->queue[c].head : &hp_empty_page;
    }
}

static void heap_thread_exit(void *arg) {
    hp_heap *h = arg;
    if (!h || h == &hp_empty_heap) return;
    zell__frame_cache_flush();   /* while the frames can still go back to this heap */
    t_heap = &hp_empty_heap;   /* frees by later TLS destructors take the remote path */
    for (uint32_t c = 0; c < ZELL__HP_CLASSES; c++) {
        hp_list *ls[2] = { &h->queue[c], &h->full[c] };
        for (int k = 0; k < 2; k++) {
            hp_page *pg;
            while ((pg = ls[k]->head)) {
                list_remove(ls[k], pg);
                page_collect(pg);
                if (pg->used == 0) {
                    page_retire(pg);
                    continue;
                }
                zell__plat_mutex_lock(&g_mu);
                __atomic_store_n(&pg->heap, (hp_heap *)NULL, __ATOMIC_RELEASE);
                __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
                pg->state = PG_ABANDONED;
                list_append(&g_abandoned[c], pg);
                __atomic_add_fetch(&g_nabandoned, 1, __ATOMIC_RELAXED);
                zell__plat_mutex_unlock(&g_mu);
            }
        }
        h->cur[c] = &hp_empty_page;
    }
    zell__plat_mutex_lock(&g_mu);
    h->next_free = g_heaps_free;
    g_heaps_free = h;
    zell__plat_mutex_unlock(&g_mu);
}

/* ---- small objects ---- */

/* Makes blocks available in pg (owner): 1 if pg->free is now non-empty. */
static int page_refill(hp_page *pg) {
    if (pg->free) return 1;
    if (pg->local_free) {
        pg->free = pg->local_free;
        pg->local_free = NULL;
        return 1;
    }
    if (page_collect(pg)) return 1;
    if (pg->bump < pg->end) {
        uint32_t bs = pg->bsize;
        size_t n = (size_t)(pg->end - pg->bump) / bs;
        size_t want = HP_EXTEND / bs;
        if (want < 1) want = 1;
        if (n > want) n = want;
        char *p = pg->bump;
        for (size_t i = 0; i + 1 < n; i++) ((hp_block *)(p + i * bs))->next = (hp_block *)(p + (i + 1) * bs);
        ((hp_block *)(p + (n - 1) * bs))->next = NULL;
        pg->bump = p + n * bs;
        pg->free = (hp_block *)p;
        return 1;
    }
    return 0;
}

void *zell__hp_alloc_slow(uint32_t c) {
    zell__rc_poll();   /* a periodic point to merge this thread's refcount queue (rc.c) */
    hp_heap *h = t_heap;
    if (h == &hp_empty_heap) h = heap_new();
    if (__atomic_load_n(&h->hint, __ATOMIC_RELAXED) != h->seen) heap_sweep(h, 0);
    for (;;) {
        hp_page *pg = h->queue[c].head;
        while (pg) {
            if (page_refill(pg)) {
                if (pg != h->queue[c].head) {
                    list_remove(&h->queue[c], pg);
                    list_push(&h->queue[c], pg);
                }
                h->cur[c] = pg;
                hp_block *b = pg->free;
                pg->free = b->next;
                pg->used++;
                return b;
            }
            /* No blocks: to the full list. Announce it, then look at xfree
             * once more (a remote free either sees in_full or is seen here). */
            __atomic_store_n(&pg->in_full, 1, __ATOMIC_SEQ_CST);
            if (__atomic_load_n(&pg->xfree, __ATOMIC_SEQ_CST)) {
                __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
                continue;   /* refill again */
            }
            hp_page *next = pg->next;
            list_remove(&h->queue[c], pg);
            list_append(&h->full[c], pg);
            pg = next;
        }
        if (__atomic_load_n(&h->hint, __ATOMIC_SEQ_CST) != h->seen) {
            heap_sweep(h, 0);
            if (h->queue[c].head) continue;
        }
        /* Adopt an abandoned page of this class, or take a fresh one. */
        pg = NULL;
        if (__atomic_load_n(&g_nabandoned, __ATOMIC_RELAXED)) {
            zell__plat_mutex_lock(&g_mu);
            if ((pg = g_abandoned[c].head)) {
                list_remove(&g_abandoned[c], pg);
                __atomic_sub_fetch(&g_nabandoned, 1, __ATOMIC_RELAXED);
                pg->state = PG_OWNED;
                __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
                __atomic_store_n(&pg->heap, h, __ATOMIC_RELEASE);
            }
            zell__plat_mutex_unlock(&g_mu);
        }
        if (!pg) {
            if ((__atomic_load_n(&g_acquired, __ATOMIC_RELAXED) & 63) == 63 && __atomic_load_n(&g_nabandoned, __ATOMIC_RELAXED))
                sweep_abandoned();
            pg = page_acquire(h, c);
            if (!pg) zell__panicf("out of memory (heap page)");
        }
        list_push(&h->queue[c], pg);
    }
}

/* A free by the owner that emptied the page or hit a page in the full list. */
void zell__hp_free_local_slow(hp_heap *h, hp_page *pg) {
    uint32_t c = pg->cls;
    if (__atomic_load_n(&pg->in_full, __ATOMIC_RELAXED)) {
        list_remove(&h->full[c], pg);
        __atomic_store_n(&pg->in_full, 0, __ATOMIC_RELAXED);
        list_append(&h->queue[c], pg);
        if (h->cur[c] == &hp_empty_page) h->cur[c] = h->queue[c].head;
    }
    if (pg->used != 0) return;
    /* Keep the only page of a class, so one object allocated and freed in a
     * loop does not move a page in and out of the pool. */
    if (h->queue[c].head == pg && !pg->next) return;
    list_remove(&h->queue[c], pg);
    if (h->cur[c] == pg) h->cur[c] = h->queue[c].head ? h->queue[c].head : &hp_empty_page;
    page_retire(pg);
}

void zell__hp_free_remote(hp_page *pg, hp_block *b) {
    hp_block *old = __atomic_load_n(&pg->xfree, __ATOMIC_RELAXED);
    do {
        b->next = old;
    } while (!__atomic_compare_exchange_n(&pg->xfree, &old, b, 1, __ATOMIC_SEQ_CST, __ATOMIC_RELAXED));
    if (!old && __atomic_load_n(&pg->in_full, __ATOMIC_SEQ_CST)) {
        hp_heap *h = __atomic_load_n(&pg->heap, __ATOMIC_ACQUIRE);
        if (h) __atomic_add_fetch(&h->hint, 1, __ATOMIC_SEQ_CST);
    }
}

/* ---- medium and large objects ----
 * A 16-byte prefix {kind, len}: medium objects (up to HP_LARGE_MIN) are
 * malloc'd (len = the usable size of the malloc block), large ones have a
 * mapping of their own (len = its length). */

enum { BIG_MEDIUM = 0x6d656469, BIG_LARGE = 0x6c617267 };
typedef struct { uint32_t kind, pad; size_t len; } hp_big;
_Static_assert(sizeof(hp_big) == HP_BIG_HDR, "big prefix");

static inline size_t large_len(int64_t size) {
    size_t ps = zell__plat_page_size();
    return ((size_t)size + HP_BIG_HDR + ps - 1) & ~(ps - 1);
}

static void *alloc_large(int64_t size, int zero) {
    size_t need = large_len(size);
    char *base = NULL;
    size_t len = 0;
    if (__atomic_load_n(&g_ncache, __ATOMIC_RELAXED)) {
        size_t slack = need / 4 > ((size_t)64 << 10) ? need / 4 : ((size_t)64 << 10);
        zell__plat_mutex_lock(&g_mu);
        int best = -1;
        for (int i = g_ncache - 1; i >= 0; i--) {   /* newest first */
            if (g_cache[i].len >= need && g_cache[i].len - need <= slack && (best < 0 || g_cache[i].len < g_cache[best].len)) best = i;
        }
        if (best >= 0) {
            base = g_cache[best].base;
            len = g_cache[best].len;
            g_cache_bytes -= (int64_t)len;
            memmove(&g_cache[best], &g_cache[best + 1], (size_t)(g_ncache - best - 1) * sizeof g_cache[0]);
            __atomic_store_n(&g_ncache, g_ncache - 1, __ATOMIC_RELAXED);
        }
        zell__plat_mutex_unlock(&g_mu);
        if (base && zero) memset(base + HP_BIG_HDR, 0, (size_t)size);
    }
    if (!base) {
        len = need;
        base = zell__plat_mem_map(len, zell__plat_page_size());   /* fresh: already zero */
        if (!base) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    }
    ((hp_big *)base)->kind = BIG_LARGE;
    ((hp_big *)base)->len = len;
    __atomic_add_fetch(&g_in_use, (int64_t)len, __ATOMIC_RELAXED);
    note_peak();
    limit_grow();
    return base + HP_BIG_HDR;
}

static void free_large(char *base, size_t len) {
    __atomic_sub_fetch(&g_in_use, (int64_t)len, __ATOMIC_RELAXED);
    victim v[TRIM_BATCH + 1];
    int n = 0;
    zell__plat_mutex_lock(&g_mu);
    int64_t limit = reserve_limit();
    if ((int64_t)len <= limit / 2) {
        if (g_ncache == HP_LARGE_SLOTS) {
            v[n].p = g_cache[0].base;
            v[n].len = g_cache[0].len;
            v[n].pg = NULL;
            n++;
            g_cache_bytes -= (int64_t)g_cache[0].len;
            memmove(&g_cache[0], &g_cache[1], (size_t)(g_ncache - 1) * sizeof g_cache[0]);
            g_ncache--;
        }
        g_cache[g_ncache].base = base;
        g_cache[g_ncache].len = len;
        g_cache[g_ncache].freed_at = zell__plat_mono_ns();
        __atomic_store_n(&g_ncache, g_ncache + 1, __ATOMIC_RELAXED);
        g_cache_bytes += (int64_t)len;
        n += trim_pick(limit, INT64_MAX, v + n);
        base = NULL;
    }
    int poke = scav_wanted_locked();
    zell__plat_mutex_unlock(&g_mu);
    if (poke) scav_poke();
    if (base) {
        zell__plat_mem_unmap(base, len);
        __atomic_add_fetch(&g_returned_total, (int64_t)len, __ATOMIC_RELAXED);
    }
    if (n) trim_apply(v, n);
}

void *zell__hp_alloc_big(int64_t size, int zero) {
    if (size + HP_BIG_HDR >= HP_LARGE_MIN) return alloc_large(size, zero);
    char *base = zero ? calloc(1, (size_t)size + HP_BIG_HDR) : malloc((size_t)size + HP_BIG_HDR);
    if (!base) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
    size_t len = zell__plat_alloc_size(base);
    ((hp_big *)base)->kind = BIG_MEDIUM;
    ((hp_big *)base)->len = len;
    __atomic_add_fetch(&g_in_use, (int64_t)len, __ATOMIC_RELAXED);
    limit_grow();
    return base + HP_BIG_HDR;
}

void zell__hp_free_big(void *p) {
    hp_big *h = (hp_big *)((char *)p - HP_BIG_HDR);
    if (h->kind == BIG_LARGE) {
        free_large((char *)h, h->len);
        return;
    }
    __atomic_sub_fetch(&g_in_use, (int64_t)h->len, __ATOMIC_RELAXED);
    free(h);
}

int64_t zell__hp_big_capacity(const void *p) {
    return (int64_t)((const hp_big *)((const char *)p - HP_BIG_HDR))->len - HP_BIG_HDR;
}

void *zell__hp_resize(void *p, int64_t size) {
    int64_t have = zell__heap_capacity(p);
    if (size <= have) return p;
    if (!zell__heap_is_small(p)) {
        hp_big *h = (hp_big *)((char *)p - HP_BIG_HDR);
        if (h->kind == BIG_LARGE) {
            size_t len = h->len, need = large_len(size);
            hp_big *n = zell__plat_mem_remap(h, len, need);
            if (n) {
                n->len = need;
                __atomic_add_fetch(&g_in_use, (int64_t)(need - len), __ATOMIC_RELAXED);
                return (char *)n + HP_BIG_HDR;
            }
        } else if (size + HP_BIG_HDR < HP_LARGE_MIN) {
            size_t len = h->len;
            hp_big *n = realloc(h, (size_t)size + HP_BIG_HDR);
            if (!n) zell__panicf("out of memory (requested %lld bytes)", (long long)size);
            n->len = zell__plat_alloc_size(n);
            __atomic_add_fetch(&g_in_use, (int64_t)n->len - (int64_t)len, __ATOMIC_RELAXED);
            return (char *)n + HP_BIG_HDR;
        }
    }
    void *n = zell__heap_alloc(size, 0);
    memcpy(n, p, (size_t)have);
    zell__heap_free(p);
    return n;
}

int zell__heap_needs_collect(void) {
    hp_heap *h = t_heap;
    if (h == &hp_empty_heap) return 0;
    return __atomic_load_n(&h->hint, __ATOMIC_RELAXED) != h->seen || __atomic_load_n(&g_epoch, __ATOMIC_RELAXED) != h->epoch;
}

void zell__heap_collect(int force) {
    if (!__atomic_load_n(&zell__heap_on, __ATOMIC_RELAXED)) {
        if (force) zell__plat_malloc_trim();
        return;
    }
    hp_heap *h = t_heap;
    uint32_t ep = __atomic_load_n(&g_epoch, __ATOMIC_RELAXED);
    if (force) ep = __atomic_add_fetch(&g_epoch, 1, __ATOMIC_RELAXED);
    if (h != &hp_empty_heap) {
        int all = force || h->epoch != ep;
        if (all || __atomic_load_n(&h->hint, __ATOMIC_RELAXED) != h->seen) heap_sweep(h, all);
        h->epoch = ep;
    }
    if (!force) return;
    if (__atomic_load_n(&g_nabandoned, __ATOMIC_RELAXED)) sweep_abandoned();
    trim(0);
    zell__plat_malloc_trim();
}

void zell__heap_stats(int64_t *in_use, int64_t *retained, int64_t *released, int64_t *returned) {
    zell__plat_mutex_lock(&g_mu);
    if (retained) *retained = g_free_bytes + g_cache_bytes;
    if (released) *released = g_released_bytes;
    zell__plat_mutex_unlock(&g_mu);
    if (in_use) *in_use = __atomic_load_n(&g_in_use, __ATOMIC_RELAXED);
    if (returned) *returned = __atomic_load_n(&g_returned_total, __ATOMIC_RELAXED);
}
