#include "test.h"
#include <sched.h>

static int drops;
static void counting_drop(void *obj) { (void)obj; drops++; }
static const zell_type_info counted = { "Counted", 24, counting_drop };

TEST(alloc_zeroed) {
    unsigned char *p = zell_alloc(1000);
    for (int i = 0; i < 1000; i++) assert(p[i] == 0);
    p = zell_realloc(p, 5000);
    zell_free(p);
    zell_free(NULL);
}

TEST(refcount_basic) {
    int64_t live = zell_live_objects();
    zell_object *o = zell_object_new(&counted, 24);
    assert(zell_refcount(o) == 1 && o->type == &counted);
    assert(zell_live_objects() == live + 1);
    zell_retain(o);
    assert(zell_refcount(o) == 2);
    zell_release(o);
    assert(drops == 0);
    zell_release(o);
    assert(drops == 1);
    assert(zell_live_objects() == live);
    zell_retain(NULL);
    zell_release(NULL);
    assert(zell_refcount(NULL) == 0);
}

TEST(immortal) {
    static zell_object s = { ZELL_RC_IMMORTAL, &counted };
    zell_retain(&s);
    zell_release(&s);
    zell_release(&s);
    assert(s.refcount == ZELL_RC_IMMORTAL);
    assert(drops == 1);
}

#include <pthread.h>
static zell_object *shared;
static void *hammer(void *arg) {
    (void)arg;
    for (int i = 0; i < 100000; i++) { zell_retain(shared); zell_release(shared); }
    return NULL;
}

TEST(atomic_refcount) {
    shared = zell_object_new(&counted, 24);
    pthread_t th[8];
    for (int i = 0; i < 8; i++) pthread_create(&th[i], NULL, hammer, NULL);
    for (int i = 0; i < 8; i++) pthread_join(th[i], NULL);
    assert(zell_refcount(shared) == 1);
    zell_release(shared);
    assert(drops == 2);
}

TEST(layout) {
    assert(sizeof(zell_object) == 16);
    assert(sizeof(zell_type_info) == 24);
    assert(sizeof(zell_value_ops) == 24);
    assert(sizeof(zell_slice) == 32);
    assert(sizeof(zell_map_desc) == 48);
    assert(sizeof(zell_map_iter) == 32);
    assert(sizeof(zell_error) == 224 && sizeof(zell_error_site) == 24 && sizeof(zell_error_frame) == 16);
    assert(sizeof(zell_string) == 32);
    assert(offsetof(zell_string, len) == 16 && offsetof(zell_string, data) == 24);
    assert(offsetof(zell_slice, data) == 0 && offsetof(zell_slice, len) == 8);
    assert(offsetof(zell_slice, cap) == 16 && offsetof(zell_slice, backing) == 24);
    assert(offsetof(zell_error, message) == 16 && offsetof(zell_error, inner) == 24);
    assert(offsetof(zell_error, user_type) == 32 && offsetof(zell_error, payload) == 40);
    assert(offsetof(zell_error, message_fn) == 48 && offsetof(zell_error, origin) == 56);
    assert(offsetof(zell_error, created) == 64 && offsetof(zell_error, nframes) == 72);
    assert(offsetof(zell_error, frames) == 80 && offsetof(zell_error, overflow) == 208);
    assert(offsetof(zell_error, overflow_cap) == 216);
}

TEST(payload_pointers) {
    int64_t live = zell_live_objects();
    zell_object *o = zell_object_new(&counted, 32);
    void *payload = (char *)o + 16;
    zell_ptr_retain(payload);
    assert(zell_refcount(o) == 2);
    void *slot = payload;
    zell_ops_ptr.retain(&slot);
    assert(zell_refcount(o) == 3);
    zell_ops_ptr.release(&slot);
    zell_ptr_release(payload);
    zell_ptr_retain(NULL);
    zell_ptr_release(NULL);
    void *nil = NULL;
    zell_ops_ptr.release(&nil);
    int d = drops;
    zell_ptr_release(payload);
    assert(drops == d + 1);
    assert(zell_live_objects() == live);
}

static void *weak_seen;
static void weak_checking_drop(void *obj) {
    (void)obj;
    /* Weak refs are already cleared when the destructor runs. */
    assert(zell_weak_get(weak_seen) == NULL);
    drops++;
}
static const zell_type_info weak_counted = { "WeakCounted", 32, weak_checking_drop };

TEST(weak_refs) {
    int64_t live = zell_live_objects();
    assert(zell_weak_new(NULL) == NULL);
    assert(zell_weak_get(NULL) == NULL);
    zell_object *o = zell_object_new(&weak_counted, 32);
    void *payload = (char *)o + 16;
    void *w1 = zell_weak_new(payload);
    void *w2 = zell_weak_new(payload);
    assert(w1 && w1 == w2);              /* one shared box per object */
    assert(zell_refcount(o) == 1);      /* weak refs do not retain */
    assert(zell_refcount(w1) == 2);
    void *p = zell_weak_get(w1);
    assert(p == payload && zell_refcount(o) == 2);
    zell_ptr_release(p);
    weak_seen = w1;
    int d = drops;
    zell_release(o);                    /* last strong ref: target freed */
    assert(drops == d + 1);
    assert(zell_weak_get(w1) == NULL);
    zell_release(w1);
    zell_release(w2);
    /* Box freed first, object later: the flag is cleared again. */
    o = zell_object_new(&counted, 32);
    payload = (char *)o + 16;
    void *w = zell_weak_new(payload);
    zell_release(w);
    zell_retain(o);
    assert(zell_refcount(o) == 2);
    zell_release(o);
    zell_release(o);
    assert(zell_live_objects() == live);
}

#include <pthread.h>
static void *weak_box_shared;
static void *weak_hammer(void *arg) {
    (void)arg;
    for (int i = 0; i < 100000; i++) {
        void *p = zell_weak_get(weak_box_shared);
        if (!p) break;
        zell_ptr_release(p);
    }
    return NULL;
}

TEST(weak_threads) {
    int64_t live = zell_live_objects();
    zell_object *o = zell_object_new(&counted, 32);
    weak_box_shared = zell_weak_new((char *)o + 16);
    pthread_t th[4];
    for (int i = 0; i < 4; i++) pthread_create(&th[i], NULL, weak_hammer, NULL);
    zell_release(o);
    for (int i = 0; i < 4; i++) pthread_join(th[i], NULL);
    assert(zell_weak_get(weak_box_shared) == NULL);
    zell_release(weak_box_shared);
    assert(zell_live_objects() == live);
}

static void leaky_child(void) {
    zell_leak_check_enable(2);
    zell_runtime_init(0, NULL);
    zell_object_new(&counted, 24);
    zell_object_new(&counted, 24);
    zell_string_literal("immortal", 8); /* does not count */
    zell_runtime_shutdown();            /* reports and exits */
    _exit(0);
}

static void clean_child(void) {
    zell_leak_check_enable(2);
    zell_runtime_init(0, NULL);
    zell_release(zell_object_new(&counted, 24));
    zell_runtime_shutdown();
}

TEST(leak_report) {
    char buf[4096];
    int st = run_child_fd(leaky_child, 2, buf, sizeof buf);
    assert(st == ZELL_LEAK_EXIT_CODE);
    assert(strstr(buf, "leak: 2 objects\n"));
    assert(strstr(buf, "  2 x Counted\n"));
    st = run_child_fd(clean_child, 2, buf, sizeof buf);
    assert(st == 0 && buf[0] == 0);
}

/* ---- the object heap (heap.c) ---- */

static const zell_type_info plain = { "Plain", 0, NULL };

TEST(heap_sizes) {
    /* Every size class, medium and large objects: zeroed, writable, freed. */
    int64_t live = zell_live_objects();
    static const int64_t sizes[] = { 16, 17, 32, 100, 128, 129, 160, 161, 1000, 1024, 1025, 4096, 16384, 16385,
                                     65536, 131072, 131073, 1 << 20, 5 << 20 };
    zell_object *objs[sizeof sizes / sizeof sizes[0]];
    for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
        unsigned char *o = zell_object_new(&plain, sizes[i]);
        for (int64_t j = 16; j < sizes[i]; j++) assert(o[j] == 0);
        memset(o + 16, 0xab, (size_t)sizes[i] - 16);
        objs[i] = (zell_object *)o;
    }
    for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
        unsigned char *o = (unsigned char *)objs[i];
        assert((sizes[i] == 16 || o[sizes[i] - 1] == 0xab) && zell_refcount(o) == 1);
        zell_release(o);
    }
    assert(zell_live_objects() == live);
    /* Reused memory is zeroed again. */
    for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
        unsigned char *o = zell_object_new(&plain, sizes[i]);
        for (int64_t j = 16; j < sizes[i]; j++) assert(o[j] == 0);
        zell_release(o);
    }
}

TEST(heap_growth) {
    /* Strings grow in place through every tier (small -> medium -> large). */
    zell_string *s = NULL;
    char chunk[1000];
    memset(chunk, 'x', sizeof chunk);
    zell_string *piece = zell_string_new(chunk, sizeof chunk);
    for (int i = 0; i < 3000; i++) s = zell_string_append(s, piece);
    assert(zell_string_len(s) == 3000000);
    assert(zell_string_data(s)[2999999] == 'x');
    zell_release(s);
    zell_release(piece);
}

#define HEAP_N 20000
static void *heap_objs[HEAP_N];
static void *heap_free_half(void *arg) {
    /* Frees objects another thread allocated (remote frees). */
    int start = (int)(intptr_t)arg;
    for (int i = start; i < HEAP_N; i += 2) zell_release(heap_objs[i]);
    return NULL;
}

static void *heap_alloc_and_exit(void *arg) {
    /* Allocates and leaves objects behind for others: its pages are abandoned. */
    (void)arg;
    for (int i = 0; i < HEAP_N; i++) heap_objs[i] = zell_object_new(&plain, 16 + (i % 7) * 16);
    return NULL;
}

TEST(heap_threads) {
    int64_t live = zell_live_objects();
    for (int round = 0; round < 5; round++) {
        pthread_t a, b;
        pthread_create(&a, NULL, heap_alloc_and_exit, NULL);
        pthread_join(a, NULL);
        pthread_create(&a, NULL, heap_free_half, (void *)0);
        pthread_create(&b, NULL, heap_free_half, (void *)1);
        pthread_join(a, NULL);
        pthread_join(b, NULL);
        assert(zell_live_objects() == live);
        /* This thread allocates and others free. */
        for (int i = 0; i < HEAP_N; i++) heap_objs[i] = zell_object_new(&plain, 48);
        pthread_create(&a, NULL, heap_free_half, (void *)0);
        pthread_create(&b, NULL, heap_free_half, (void *)1);
        pthread_join(a, NULL);
        pthread_join(b, NULL);
        assert(zell_live_objects() == live);
        /* ...and reuses what came back. */
        for (int i = 0; i < HEAP_N; i++) heap_objs[i] = zell_object_new(&plain, 48);
        for (int i = 0; i < HEAP_N; i++) zell_release(heap_objs[i]);
    }
}

TEST(heap_release_to_os) {
    /* 64 MB of small objects freed: FreeOSMemory leaves nothing retained. */
    enum { N = 64 << 20 >> 6 };
    void **v = malloc(N * sizeof *v);
    for (int i = 0; i < N; i++) v[i] = zell_object_new(&plain, 64);
    int64_t in_use = zell_diag_rt_heap_in_use();
    if (in_use == 0) {   /* the heap is off (AddressSanitizer, ZELL_NO_SLAB) */
        for (int i = 0; i < N; i++) zell_release(v[i]);
        free(v);
        return;
    }
    assert(in_use >= (int64_t)60 << 20);
    for (int i = 0; i < N; i++) zell_release(v[i]);
    free(v);
    assert(zell_diag_rt_heap_in_use() < in_use - ((int64_t)60 << 20));
    zell_runtime_free_os_memory();
    assert(zell_diag_rt_heap_retained() == 0);
    assert(zell_diag_rt_heap_released() >= (int64_t)60 << 20);
}

/* 40 MB of small objects made and freed. */
static void heap_wave(void) {
    enum { N = 40 << 20 >> 6 };
    void **v = malloc(N * sizeof *v);
    for (int i = 0; i < N; i++) v[i] = zell_object_new(&plain, 64);
    for (int i = 0; i < N; i++) zell_release(v[i]);
    free(v);
}

TEST(heap_mem_limit) {
    int64_t old = zell_runtime_set_mem_limit(0);
    heap_wave();
    if (zell_diag_rt_heap_retained() == 0 && zell_diag_rt_heap_in_use() == 0) {   /* the heap is off */
        zell_runtime_set_mem_limit(old);
        return;
    }
    /* no limit: the reserve keeps up to the recent peak */
    assert(zell_diag_rt_heap_retained() >= (int64_t)30 << 20);
    /* a 24 MB limit (target 22.5 MB): the reserve shrinks at once... */
    const int64_t lim = (int64_t)24 << 20, slack = (int64_t)1 << 20;
    assert(zell_runtime_set_mem_limit(lim) == 0 && zell_runtime_mem_limit() == lim);
    assert(zell_diag_rt_heap_retained() <= lim - lim / 16 + slack);
    /* ...and a wave past the limit leaves no more than the target behind */
    heap_wave();
    assert(zell_diag_rt_heap_retained() <= lim - lim / 16 + slack);
    zell_runtime_set_mem_limit(old);
    zell_runtime_free_os_memory();
}

/* ---- biased reference counting (rc.c) ---- */

void zell__rc_thread_eligible(void);   /* runtime-internal (rt_internal.h) */
void zell__rc_poll(void);

static int brc_drops;
static void brc_drop(void *obj) { (void)obj; __atomic_add_fetch(&brc_drops, 1, __ATOMIC_RELAXED); }
static const zell_type_info brc_type = { "Brc", 24, brc_drop };
static zell_object *brc_obj;

static void *brc_reader(void *arg) {
    (void)arg;
    for (int i = 0; i < 100000; i++) {
        zell_retain(brc_obj);
        zell_retain(brc_obj);
        zell_release(brc_obj);
        zell_release(brc_obj);
    }
    return NULL;
}

static zell_object *brc_handoff;
static void *brc_owner_side(void *arg) {
    /* Owns objects: biases them, keeps some references, gives one away. */
    int mode = (int)(intptr_t)arg;
    zell__rc_thread_eligible();
    zell_object *o = zell_object_new(&brc_type, 24);
    zell_retain(o);                          /* biased to this thread: B = 2 */
    assert(o->refcount & ZELL_RC_BIASED);
    assert(zell_refcount(o) == 2);
    for (int i = 0; i < 70000; i++) zell_retain(o);   /* past the 16-bit B: overflow into S */
    assert(zell_refcount(o) == 70002);
    for (int i = 0; i < 70000; i++) zell_release(o);
    assert(zell_refcount(o) == 2);
    brc_handoff = o;                          /* one reference moves to another thread */
    if (mode == 0) {
        zell_release(o);                     /* B = 1: the other thread frees it */
    }
    return o;                                 /* mode 1: still holds one, exits holding it */
}

static void *brc_release_other(void *arg) {
    zell_release(arg);
    return NULL;
}

TEST(brc_owner_and_shared) {
    int64_t live = zell_live_objects();
    pthread_t th[9];
    /* An object biased to thread 0, read by 8 others. */
    brc_drops = 0;
    pthread_create(&th[0], NULL, brc_owner_side, (void *)0);
    pthread_join(th[0], NULL);
    pthread_create(&th[0], NULL, brc_release_other, brc_handoff);
    pthread_join(th[0], NULL);
    assert(brc_drops == 1);
    /* Owner exits holding a reference (unowned afterwards), others share it. */
    void *o;
    pthread_create(&th[0], NULL, brc_owner_side, (void *)1);
    pthread_join(th[0], &o);
    brc_obj = o;
    for (int i = 1; i < 9; i++) pthread_create(&th[i], NULL, brc_reader, NULL);
    for (int i = 1; i < 9; i++) pthread_join(th[i], NULL);
    assert(zell_refcount(o) == 2 && brc_drops == 1);
    zell_release(o);
    assert(brc_drops == 1);
    zell_release(o);                         /* the last reference, from a non-owner */
    assert(brc_drops == 2);
    assert(zell_live_objects() == live);
}

static zell_object *brc_q[1000];
static void *brc_queue_owner(void *arg) {
    /* Keeps two references to each object, hands one to the main thread,
     * which releases it (S < 0: queued), then drops its own: the count is
     * zero but only the queue knows. */
    (void)arg;
    zell__rc_thread_eligible();
    zell_object *mine[1000];
    for (int i = 0; i < 1000; i++) {
        mine[i] = zell_object_new(&brc_type, 24);
        zell_retain(mine[i]);
        zell_retain(mine[i]);                /* B = 3 */
        brc_q[i] = mine[i];
    }
    __atomic_store_n(&brc_handoff, (zell_object *)1, __ATOMIC_RELEASE);
    while (__atomic_load_n(&brc_handoff, __ATOMIC_ACQUIRE) != (zell_object *)2) sched_yield();
    for (int i = 0; i < 1000; i++) {
        zell_release(mine[i]);
        zell_release(mine[i]);
    }
    assert(brc_drops == 0);                   /* dead, not yet merged */
    zell__rc_poll();
    assert(brc_drops == 1000);
    return NULL;
}

TEST(brc_queue) {
    int64_t live = zell_live_objects();
    brc_drops = 0;
    brc_handoff = NULL;
    pthread_t t;
    pthread_create(&t, NULL, brc_queue_owner, NULL);
    while (__atomic_load_n(&brc_handoff, __ATOMIC_ACQUIRE) != (zell_object *)1) sched_yield();
    for (int i = 0; i < 1000; i++) zell_release(brc_q[i]);
    __atomic_store_n(&brc_handoff, (zell_object *)2, __ATOMIC_RELEASE);
    pthread_join(t, NULL);
    assert(brc_drops == 1000);
    assert(zell_live_objects() == live);
}

static void *brc_exit_with_queue(void *arg) {
    /* Same, but the owner exits right after dropping: its exit merges. */
    (void)arg;
    zell__rc_thread_eligible();
    zell_object *o = zell_object_new(&brc_type, 24);
    zell_retain(o);
    zell_retain(o);
    brc_q[0] = o;
    __atomic_store_n(&brc_handoff, (zell_object *)1, __ATOMIC_RELEASE);
    while (__atomic_load_n(&brc_handoff, __ATOMIC_ACQUIRE) != (zell_object *)2) sched_yield();
    zell_release(o);
    zell_release(o);
    return NULL;
}

TEST(brc_owner_exit) {
    brc_drops = 0;
    brc_handoff = NULL;
    pthread_t t;
    pthread_create(&t, NULL, brc_exit_with_queue, NULL);
    while (__atomic_load_n(&brc_handoff, __ATOMIC_ACQUIRE) != (zell_object *)1) sched_yield();
    zell_release(brc_q[0]);
    __atomic_store_n(&brc_handoff, (zell_object *)2, __ATOMIC_RELEASE);
    pthread_join(t, NULL);
    assert(brc_drops == 1);
}

static void *brc_weak_owner(void *arg) {
    zell__rc_thread_eligible();
    zell_object *o = arg;
    zell_retain(o);                          /* biased here */
    void *w = zell_weak_new((char *)o + 16);
    void *p = zell_weak_get(w);
    assert(p && zell_refcount(o) == 3);
    zell_ptr_release(p);
    zell_release(o);
    return w;
}

TEST(brc_weak) {
    brc_drops = 0;
    zell_object *o = zell_object_new(&brc_type, 32);
    pthread_t t;
    void *w;
    pthread_create(&t, NULL, brc_weak_owner, o);
    pthread_join(t, &w);
    void *p = zell_weak_get(w);
    assert(p == (char *)o + 16);
    zell_ptr_release(p);
    zell_release(o);                          /* last, non-owner: weak cleared, dropped */
    assert(brc_drops == 1);
    assert(zell_weak_get(w) == NULL);
    zell_release(w);
}

/* Coroutine frames (ABI.md 10.3): the per-thread cache hands back the
 * last freed frame of a class, zeroes for zell_coro_frame_alloc, caps,
 * takes frames freed by other threads, and is flushed at thread exit. */
static void *frames_free_all(void *arg) {
    void **v = arg;
    for (int i = 0; i < 2000; i++) zell_coro_frame_free(v[i]);
    return NULL;
}

TEST(coro_frame_cache) {
    void *a = zell_coro_frame_alloc_uninit(200);
    memset(a, 0x5A, 200);
    zell_coro_frame_free(a);
    void *b = zell_coro_frame_alloc(190);   /* same class (224) */
    for (int i = 0; i < 190; i++) assert(((unsigned char *)b)[i] == 0);
    zell_coro_frame_free(b);
    zell_coro_frame_free(NULL);
    /* past the caps, and the big path */
    static void *v[2000];
    for (int i = 0; i < 2000; i++) v[i] = zell_coro_frame_alloc_uninit(i % 2 ? 200 : 40000);
    for (int i = 0; i < 2000; i++) memset(v[i], i & 0xff, i % 2 ? 200 : 40000);
    for (int i = 0; i < 2000; i++) zell_coro_frame_free(v[i]);
    for (int i = 0; i < 2000; i++) v[i] = zell_coro_frame_alloc_uninit(200);
    for (int i = 0; i < 2000; i++)
        for (int j = i + 1; j < i + 4 && j < 2000; j++) assert(v[i] != v[j]);
    /* freed (and cached) on another thread, which then exits */
    pthread_t th;
    pthread_create(&th, NULL, frames_free_all, v);
    pthread_join(th, NULL);
    for (int i = 0; i < 2000; i++) v[i] = zell_coro_frame_alloc(200);
    for (int i = 0; i < 2000; i++) zell_coro_frame_free(v[i]);
}

int main(void) {
    printf("test_mem\n");
    RUN(alloc_zeroed);
    RUN(refcount_basic);
    RUN(immortal);
    RUN(atomic_refcount);
    RUN(layout);
    RUN(payload_pointers);
    RUN(weak_refs);
    RUN(weak_threads);
    RUN(leak_report);
    RUN(heap_sizes);
    RUN(heap_growth);
    RUN(heap_threads);
    RUN(heap_release_to_os);
    RUN(brc_owner_and_shared);
    RUN(brc_queue);
    RUN(brc_owner_exit);
    RUN(brc_weak);
    RUN(coro_frame_cache);
    RUN(heap_mem_limit);
    return 0;
}
