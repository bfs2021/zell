/* task_sync.c - task.Channel<T> and task.Mutex (ABI.md 10.6, 10.7): a
 * channel with a ring buffer and FIFO queues of waiting senders and
 * receivers, each woken exactly once, and a mutex that lives inline in the
 * Zell struct (three words) with Go's barging and starvation rules. The
 * waits are plain tasks (task.c) whose slots hold the queue links, and the
 * element a sender waits with. The C API of 10.6/10.7 (zell_chan_*,
 * zell_mutex_*) is built on the same code. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

/* ======================================================================
 * Channels
 * ====================================================================== */

/* The three layouts of a receive's result (10.7). */
enum { RES_FALLIBLE = 0, RES_TUPLE = 1, RES_C = 2 };

/* The header a waiting task keeps in its slot, after the result (at
 * ROUND8(result_size), so filling the result never overwrites it and a late
 * cancel hook still reads a valid header). */
typedef struct chan_wait {
    struct chan_wait *next;
    struct zell_chan *ch;      /* owned while queued; NULL once dequeued (guarded by the task's lock) */
    /* The cancel hook on the task that started the wait (no allocation per
     * wait; it holds a reference to this task until the starter prunes it).
     * The wait's own cancellation (task.Any losers) is the task's cancel_fn. */
    zell__hook hook;           /* hook.w: the task this header lives in (always set) */
    struct sel_state *sel;     /* a case of a select (a sel_entry), else NULL */
    int32_t kind;              /* receivers: RES_* */
    int32_t sender;
} chan_wait;
_Static_assert(sizeof(chan_wait) % 8 == 0, "a sender's element follows its header");

/* A plain sender's owned element follows its header in the task's slot. */
static inline unsigned char *wait_elem(chan_wait *w) { return (unsigned char *)(w + 1); }

/* The channel lock: a few loads and stores per operation, never held across
 * a wait or user code, with up to 16 workers contending in a fan-in, so a
 * spin lock (with a yield once it spins for a while) beats a pthread mutex,
 * which parks the contenders in the kernel. */
typedef struct chan_lock { int32_t held; } chan_lock;

static inline void chan_lock_acquire(chan_lock *l) {
    for (int spins = 0;; spins++) {
        if (__atomic_load_n(&l->held, __ATOMIC_RELAXED) == 0 && __atomic_exchange_n(&l->held, 1, __ATOMIC_ACQUIRE) == 0) return;
        if (spins > 64) zell__plat_yield();
    }
}

static inline void chan_lock_release(chan_lock *l) { __atomic_store_n(&l->held, 0, __ATOMIC_RELEASE); }

struct zell_chan {
    zell_object hdr;
    chan_lock mu;
    int64_t elem_size;
    const zell_value_ops *elem_ops;
    /* Result layouts of the compiled receives: `T!` (Recv) and `(T, bool)`
     * (Next); zero when made by zell_chan_new (C code only uses RES_C). */
    int64_t fal_size, fal_err_off;
    const zell_value_ops *fal_ops;
    int64_t tup_size, tup_ok_off;
    const zell_value_ops *tup_ops;
    int64_t capacity, count, head;   /* ring buffer of `capacity` elements */
    unsigned char *buf;
    int32_t closed;
    chan_wait *sendq, *sendq_tail;
    chan_wait *recvq, *recvq_tail;
};

/* task.Channel.Recv on a closed, drained channel: an immortal sentinel. */
static ZELL_STRING_STORAGE(15) closed_msg = ZELL_STRING_INIT(closed_msg, "channel closed");
static zell_error closed_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&closed_msg,
};

static inline void elem_retain(zell_chan *c, void *v) { if (c->elem_ops && c->elem_ops->retain) c->elem_ops->retain(v); }
static inline void elem_release(zell_chan *c, void *v) { if (c->elem_ops && c->elem_ops->release) c->elem_ops->release(v); }
static inline unsigned char *buf_at(zell_chan *c, int64_t i) { return c->buf + (i % c->capacity) * c->elem_size; }
static inline chan_wait *wait_of(zell_task *t) {
    return (chan_wait *)((unsigned char *)zell__task_slot(t) + ZELL_ROUND8(zell__task_result_size(t)));
}

static void wq_push(chan_wait **head, chan_wait **tail, chan_wait *w) {
    w->next = NULL;
    if (*tail) (*tail)->next = w; else *head = w;
    *tail = w;
}

static chan_wait *wq_pop(chan_wait **head, chan_wait **tail) {
    chan_wait *w = *head;
    if (w) { *head = w->next; if (!*head) *tail = NULL; }
    return w;
}

static bool wq_remove(chan_wait **head, chan_wait **tail, chan_wait *w) {
    chan_wait *prev = NULL;
    for (chan_wait *x = *head; x; prev = x, x = x->next) {
        if (x != w) continue;
        if (prev) prev->next = x->next; else *head = x->next;
        if (*tail == x) *tail = prev;
        return true;
    }
    return false;
}

static void chan_drop(void *obj) {
    zell_chan *c = obj;
    for (int64_t i = 0; i < c->count; i++) elem_release(c, buf_at(c, c->head + i));
    /* Waiters hold a reference to the channel, so none are queued here. */
    zell_free(c->buf);
}

static const zell_type_info chan_type = { "Channel", (int64_t)sizeof(struct zell_chan), chan_drop };

static zell_chan *chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity) {
    if (elem_size < 0 || capacity < 0) zell_panic("zell_chan_new: invalid size");
    zell_chan *c = zell_object_new(&chan_type, (int64_t)sizeof(struct zell_chan));
    c->elem_size = elem_size;
    c->elem_ops = ops;
    c->capacity = capacity;
    c->buf = capacity ? zell_alloc(capacity * (elem_size ? elem_size : 1)) : NULL;
    return c;
}

static void chan_wait_cancelled(zell_task *t);

/* A receive task of layout `kind`: with `wait`, pending with the wait
 * header set up; else only room for the result (a receive that completes
 * at once never waits, never has a header and is not cancellable). */
static zell_task *recv_task_new(zell_chan *c, int kind, bool wait) {
    int64_t rs, off = 0;
    const zell_value_ops *ops;
    switch (kind) {
    case RES_FALLIBLE: rs = c->fal_size; ops = c->fal_ops; break;
    case RES_TUPLE: rs = c->tup_size; ops = c->tup_ops; break;
    default: rs = 8 + c->elem_size; ops = c->elem_ops; off = 8; break;
    }
    if (!wait) return zell__task_new_slot(rs, ops, off, rs);
    zell_task *t = zell__task_new_slot(rs, ops, off, ZELL_ROUND8(rs) + (int64_t)sizeof(chan_wait));
    chan_wait *w = wait_of(t);
    w->hook.w = t;
    w->kind = kind;
    zell__task_set_cancel_fn(t, chan_wait_cancelled);
    return t;
}

/* Writes a receive's result into t's slot (before the wait header): the
 * value, which is MOVED in (`value` is an owned copy whose ownership passes
 * to the task; NULL for the zero value), and the status: `err` for the
 * fallible layout, ok for the others. The task is not completed here. */
static void recv_fill(zell_chan *c, zell_task *t, int kind, const void *value, zell_error *err) {
    unsigned char *s = zell__task_slot(t);
    switch (kind) {
    case RES_FALLIBLE:
        memset(s, 0, (size_t)c->fal_size);
        if (value) memcpy(s, value, (size_t)c->elem_size);
        *(zell_error **)(s + c->fal_err_off) = err;
        break;
    case RES_TUPLE:
        memset(s, 0, (size_t)c->tup_size);
        if (value) memcpy(s, value, (size_t)c->elem_size);
        s[c->tup_ok_off] = err == NULL;
        break;
    default:
        *(int64_t *)s = err == NULL;
        if (value) memcpy(s + 8, value, (size_t)c->elem_size);
        else memset(s + 8, 0, (size_t)c->elem_size);
        break;
    }
}

/* Takes a wait out of its queue (c->mu held): it no longer belongs to the
 * channel (the references it held are released by whoever dequeued it). */
static void entry_unlink(chan_wait *w) {
    zell__task_lock(w->hook.w);
    w->ch = NULL;
    zell__task_unlock(w->hook.w);
}

static void wait_unlink(zell_task *t) { entry_unlink(wait_of(t)); }

/* ---- select (ABI.md 10.7 "Select"): one wait on several operations ----
 * A select task's slot holds its result (the index of the case that fired
 * at offset 0), then a sel_state with one sel_entry per case. Every entry
 * of a pending select is queued on its channel like a plain wait; the
 * channel that dequeues one first claims the whole select (sel_claim, an
 * atomic flag), and an entry of a select another case already claimed is
 * stale: dequeued, unlinked and released without effect. The claiming
 * side finishes the case after dropping the channel lock and then takes
 * the other entries out of their queues (sel_cleanup). */
typedef struct sel_state sel_state;
typedef struct sel_entry {
    chan_wait w;               /* w.sel: the state; w.hook.w: the select task */
    zell_chan *c;              /* the case's channel (NULL: the case is off) */
    void *dst;                 /* receive: where the value is moved */
    uint8_t *ok;               /* receive: 1 for a value, 0 for a closed channel (NULL: not wanted) */
    unsigned char *val;        /* send: the owned copy of the element */
    int64_t idx;               /* the case's index */
} sel_entry;

struct sel_state {
    int32_t done;              /* atomic: 1 once a case or the cancellation claimed the select */
    int32_t n;
    zell__hook hook;           /* on the task that started the select */
    zell_chan **lk;            /* the distinct channels, by address (the lock order) */
    int64_t nlk;
    sel_entry e[];
};

static inline sel_state *sel_of(zell_task *s) {
    return (sel_state *)((unsigned char *)zell__task_slot(s) + ZELL_ROUND8(zell__task_result_size(s)));
}

/* The element a dequeued sender offers: a plain wait's, or a send case's copy. */
static inline unsigned char *wait_val(chan_wait *w) { return w->sel ? ((sel_entry *)w)->val : wait_elem(w); }

static inline bool sel_claim(sel_state *st) {
    int32_t z = 0;
    return __atomic_compare_exchange_n(&st->done, &z, 1, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE);
}

/* Pops the oldest waiter that can still take part (c->mu held): a select's
 * entry claims its select here; the entries of selects already claimed are
 * unlinked onto *stale, for sel_release_stale once the lock is dropped. */
static chan_wait *wq_pop_live(chan_wait **head, chan_wait **tail, chan_wait **stale) {
    for (;;) {
        chan_wait *w = wq_pop(head, tail);
        if (!w || !w->sel || sel_claim(w->sel)) return w;
        entry_unlink(w);
        w->next = *stale;
        *stale = w;
    }
}

/* Releases what stale entries held: the queue's reference to their select,
 * the channel's, and a send case's element. */
static void sel_release_stale(chan_wait *w) {
    while (w) {
        chan_wait *next = w->next;
        sel_entry *e = (sel_entry *)w;
        zell_chan *c = e->c;   /* read before the select (which holds e) may go */
        if (w->sender) elem_release(c, e->val);
        zell_release(w->hook.w);
        zell_release(c);
        w = next;
    }
}

/* Completes select task s with case idx (-1: none, the default or a
 * cancellation): the index word, then the task. */
static void sel_done(zell_task *s, int64_t idx) {
    memcpy(zell__task_slot(s), &idx, sizeof idx);
    zell_task_complete(s, NULL);
}

/* Takes the entries of a finished select other than `except` out of their
 * queues (one channel lock at a time) and releases what they held. */
static void sel_cleanup(zell_task *s, sel_entry *except) {
    sel_state *st = sel_of(s);
    for (int32_t i = 0; i < st->n; i++) {
        sel_entry *e = &st->e[i];
        zell_chan *c = e->c;
        if (e == except || !c) continue;
        chan_lock_acquire(&c->mu);
        bool queued = e->w.ch == c;
        if (queued) {
            if (e->w.sender) wq_remove(&c->sendq, &c->sendq_tail, &e->w);
            else wq_remove(&c->recvq, &c->recvq_tail, &e->w);
            entry_unlink(&e->w);
        }
        chan_lock_release(&c->mu);
        if (queued) {
            if (e->w.sender) elem_release(c, e->val);
            zell_release(s);
            zell_release(c);
        }
    }
}

/* A dequeued receive case got an owned value (moved in; NULL: the zero
 * value) or a closed channel (err). */
static void sel_recv_finish(zell_chan *c, chan_wait *w, const void *value, zell_error *err) {
    sel_entry *e = (sel_entry *)w;
    zell_task *s = w->hook.w;
    if (value) memcpy(e->dst, value, (size_t)c->elem_size);
    else memset(e->dst, 0, (size_t)c->elem_size);
    if (e->ok) *e->ok = err == NULL;
    sel_done(s, e->idx);
    sel_cleanup(s, e);
    zell_release(s);   /* the queue's reference */
    zell_release(c);   /* the entry's reference */
}

/* A dequeued send case whose element was taken (moved out of e->val). */
static void sel_send_finish(zell_chan *c, chan_wait *w) {
    zell_task *s = w->hook.w;
    sel_done(s, ((sel_entry *)w)->idx);
    sel_cleanup(s, (sel_entry *)w);
    zell_release(s);
    zell_release(c);
}

/* The select was cancelled (10.11: task.Any lost, or the starter's scope):
 * no case fires and the result is -1. Idempotent. */
static void sel_cancelled(zell_task *s) {
    if (!sel_claim(sel_of(s))) return;
    sel_done(s, -1);
    sel_cleanup(s, NULL);
}

/* A queued wait whose task was cancelled (10.11): a receiver completes
 * with task.ErrCancelled, a sender returns without sending. Idempotent (it
 * runs from the hook on the task that started the wait and from the wait's
 * own cancel_fn; only the caller that dequeues the wait completes it). */
static void chan_wait_cancelled(zell_task *t) {
    chan_wait *w = wait_of(t);
    zell__task_lock(t);
    zell_chan *c = w->ch;
    if (c) zell_retain(c);
    zell__task_unlock(t);
    if (!c) return;
    chan_lock_acquire(&c->mu);
    bool queued = w->ch == c;
    if (queued) {
        if (w->sender) wq_remove(&c->sendq, &c->sendq_tail, w);
        else wq_remove(&c->recvq, &c->recvq_tail, w);
        wait_unlink(t);
    }
    chan_lock_release(&c->mu);
    if (queued) {
        if (w->sender) {
            elem_release(c, wait_elem(w));
        } else {
            recv_fill(c, t, w->kind, NULL, zell_task_err_cancelled());
        }
        zell_task_complete(t, NULL);
        zell_release(t);   /* the queue's reference */
        zell_release(c);   /* the wait's reference */
    }
    zell_release(c);
}

/* Queues the wait t on c (c->mu held); wait_hook follows after the lock is
 * dropped (the wait may have completed by then: harmless). */
static void wait_enqueue(zell_chan *c, zell_task *t, bool sender) {
    chan_wait *w = wait_of(t);
    w->sender = sender;
    zell_retain(c);
    w->ch = c;
    zell_retain(t);   /* the queue's reference */
    if (sender) wq_push(&c->sendq, &c->sendq_tail, w);
    else wq_push(&c->recvq, &c->recvq_tail, w);
}

/* The hook on the task that waits (a Recv in a cancelled scope fails). */
static void wait_hook(zell_task *t) { zell__hook_link(&wait_of(t)->hook, zell__task_cur(), chan_wait_cancelled, t); }

/* Completes a dequeued receiver with an owned value (moved) or a status. */
static void recv_finish(zell_chan *c, chan_wait *rw, const void *value, zell_error *err) {
    zell_task *t = rw->hook.w;
    int kind = rw->kind;
    recv_fill(c, t, kind, value, err);
    zell_task_complete(t, NULL);
    zell_release(t);   /* the queue's reference */
    zell_release(c);   /* the wait's reference */
}

/* Completes a dequeued sender whose value was taken. */
static void send_finish(zell_chan *c, chan_wait *sw) {
    zell_task *t = sw->hook.w;
    zell_task_complete(t, NULL);
    zell_release(t);
    zell_release(c);
}

/* A dequeued waiter of either kind (a plain wait or a select's case). */
static void waiter_recv_finish(zell_chan *c, chan_wait *w, const void *value, zell_error *err) {
    if (w->sel) sel_recv_finish(c, w, value, err);
    else recv_finish(c, w, value, err);
}

static void waiter_send_finish(zell_chan *c, chan_wait *w) {
    if (w->sel) sel_send_finish(c, w);
    else send_finish(c, w);
}

static zell_task *chan_send(zell_chan *c, const void *elem) {
    chan_lock_acquire(&c->mu);
    if (c->closed) {
        chan_lock_release(&c->mu);
        zell_panic("send on closed channel");
    }
    chan_wait *stale = NULL;
    chan_wait *rw = wq_pop_live(&c->recvq, &c->recvq_tail, &stale);
    if (rw) {
        /* hand the value to a waiting receiver (a retained copy moves in) */
        entry_unlink(rw);
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        unsigned char tmp[256];
        unsigned char *v = (size_t)c->elem_size <= sizeof tmp ? tmp : zell_alloc(c->elem_size);
        memcpy(v, elem, (size_t)c->elem_size);
        elem_retain(c, v);
        waiter_recv_finish(c, rw, v, NULL);
        if (v != tmp) zell_free(v);
        return zell_task_done();
    }
    if (c->count < c->capacity) {
        unsigned char *slot = buf_at(c, c->head + c->count);
        memcpy(slot, elem, (size_t)c->elem_size);
        elem_retain(c, slot);
        c->count++;
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        return zell_task_done();
    }
    if (zell_task_cur_cancelled()) {   /* a cancelled task does not wait: nothing is sent */
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        return zell_task_done();
    }
    zell_task *t = zell__task_new_slot(0, NULL, 0, (int64_t)sizeof(chan_wait) + c->elem_size);
    chan_wait *w = wait_of(t);
    w->hook.w = t;
    zell__task_set_cancel_fn(t, chan_wait_cancelled);
    memcpy(wait_elem(w), elem, (size_t)c->elem_size);
    elem_retain(c, wait_elem(w));
    wait_enqueue(c, t, true);
    chan_lock_release(&c->mu);
    sel_release_stale(stale);
    wait_hook(t);
    return t;
}

/* The task that waits is made only once a receive has to wait (under the
 * lock: dropping it and rechecking costs more on a contended channel than
 * the allocation); a receive that completes at once gets a task with room
 * for the result only. */
static zell_task *chan_recv(zell_chan *c, int kind) {
    chan_lock_acquire(&c->mu);
    chan_wait *stale = NULL;
    if (c->count > 0) {
        unsigned char tmp[256];
        unsigned char *v = (size_t)c->elem_size <= sizeof tmp ? tmp : zell_alloc(c->elem_size);
        memcpy(v, buf_at(c, c->head), (size_t)c->elem_size);   /* moves out of the buffer */
        c->head = (c->head + 1) % c->capacity;
        c->count--;
        chan_wait *sw = wq_pop_live(&c->sendq, &c->sendq_tail, &stale);
        if (sw) {   /* refill the buffer from the oldest blocked sender */
            memcpy(buf_at(c, c->head + c->count), wait_val(sw), (size_t)c->elem_size);
            c->count++;
            entry_unlink(sw);
        }
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, v, NULL);
        zell__task_mark_done(t);
        if (v != tmp) zell_free(v);
        if (sw) waiter_send_finish(c, sw);
        return t;
    }
    chan_wait *sw = wq_pop_live(&c->sendq, &c->sendq_tail, &stale);
    if (sw) {   /* unbuffered: take the value from the oldest waiting sender */
        entry_unlink(sw);
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, wait_val(sw), NULL);
        zell__task_mark_done(t);
        waiter_send_finish(c, sw);
        return t;
    }
    zell_error *err = c->closed ? &closed_error : zell_task_cur_cancelled() ? zell_task_err_cancelled() : NULL;
    if (err) {
        chan_lock_release(&c->mu);
        sel_release_stale(stale);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, NULL, err);
        zell__task_mark_done(t);
        return t;
    }
    zell_task *wt = recv_task_new(c, kind, true);
    wait_enqueue(c, wt, false);
    chan_lock_release(&c->mu);
    sel_release_stale(stale);
    wait_hook(wt);
    return wt;
}

static void chan_close(zell_chan *c) {
    chan_lock_acquire(&c->mu);
    if (c->closed) { chan_lock_release(&c->mu); zell_panic("close of closed channel"); }
    /* a waiting sender (a send case of a select still pending included) */
    for (chan_wait *w = c->sendq; w; w = w->next) {
        if (!w->sel || !__atomic_load_n(&w->sel->done, __ATOMIC_ACQUIRE)) {
            chan_lock_release(&c->mu);
            zell_panic("send on closed channel");
        }
    }
    c->closed = 1;
    /* every pending receiver, once (cases of claimed selects are stale) */
    chan_wait *rs = NULL, *rs_tail = NULL, *stale = NULL, *w;
    while ((w = wq_pop_live(&c->recvq, &c->recvq_tail, &stale))) {
        entry_unlink(w);
        wq_push(&rs, &rs_tail, w);
    }
    chan_lock_release(&c->mu);
    sel_release_stale(stale);
    while (rs) {
        chan_wait *n = rs->next;
        waiter_recv_finish(c, rs, NULL, &closed_error);
        rs = n;
    }
}

static inline zell_chan *chan_of(int64_t h) {
    if (!h) zell_panic_nil();
    return (zell_chan *)(intptr_t)h;
}

/* A cheap per-thread generator for the start of a select's scan. */
static uint32_t sel_rand(void) {
    static _Thread_local uint32_t x;
    if (!x) x = (uint32_t)(uintptr_t)&x | 1u;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    return x;
}

static void sel_lock_all(sel_state *st) { for (int64_t i = 0; i < st->nlk; i++) chan_lock_acquire(&st->lk[i]->mu); }
static void sel_unlock_all(sel_state *st) { for (int64_t i = st->nlk; i-- > 0;) chan_lock_release(&st->lk[i]->mu); }

/* Starts select s (its entries set up: channel, kind, destinations, send
 * copies retained): with every channel locked (by address, so two selects
 * never deadlock), the first ready case from a random start fires at once;
 * none ready: -1 with `dflt` or in a cancelled task, else every case waits
 * on its channel. */
static zell_task *sel_start(zell_task *s, bool dflt) {
    sel_state *st = sel_of(s);
    int64_t n = st->n;
    st->nlk = 0;
    for (int64_t i = 0; i < n; i++) {   /* the distinct channels, sorted */
        zell_chan *c = st->e[i].c;
        if (!c) continue;
        int64_t j = st->nlk;
        while (j > 0 && st->lk[j - 1] > c) j--;
        if (j > 0 && st->lk[j - 1] == c) continue;
        memmove(&st->lk[j + 1], &st->lk[j], (size_t)(st->nlk - j) * sizeof st->lk[0]);
        st->lk[j] = c;
        st->nlk++;
    }
    sel_lock_all(st);
    chan_wait *stale = NULL, *peer = NULL;   /* peer: the waiter the fired case dequeued */
    int64_t fired = -1;
    bool closed_send = false;
    int64_t start = n ? (int64_t)(sel_rand() % (uint32_t)n) : 0;
    for (int64_t k = 0; k < n && fired < 0; k++) {
        sel_entry *e = &st->e[(start + k) % n];
        zell_chan *c = e->c;
        if (!c) continue;
        if (!e->w.sender) {
            if (c->count > 0) {
                memcpy(e->dst, buf_at(c, c->head), (size_t)c->elem_size);   /* moves out of the buffer */
                c->head = (c->head + 1) % c->capacity;
                c->count--;
                if ((peer = wq_pop_live(&c->sendq, &c->sendq_tail, &stale))) {
                    memcpy(buf_at(c, c->head + c->count), wait_val(peer), (size_t)c->elem_size);
                    c->count++;
                    entry_unlink(peer);
                }
                if (e->ok) *e->ok = 1;
                fired = e->idx;
            } else if ((peer = wq_pop_live(&c->sendq, &c->sendq_tail, &stale))) {
                entry_unlink(peer);
                memcpy(e->dst, wait_val(peer), (size_t)c->elem_size);
                if (e->ok) *e->ok = 1;
                fired = e->idx;
            } else if (c->closed) {
                memset(e->dst, 0, (size_t)c->elem_size);
                if (e->ok) *e->ok = 0;
                fired = e->idx;
            }
        } else if (c->closed) {
            closed_send = true;
            fired = e->idx;
        } else if ((peer = wq_pop_live(&c->recvq, &c->recvq_tail, &stale))) {
            entry_unlink(peer);   /* the copy in e->val moves to the receiver below */
            fired = e->idx;
        } else if (c->count < c->capacity) {
            memcpy(buf_at(c, c->head + c->count), e->val, (size_t)c->elem_size);   /* moves */
            c->count++;
            fired = e->idx;
        }
    }
    if (fired < 0 && !dflt && !zell_task_cur_cancelled()) {
        for (int64_t i = 0; i < n; i++) {
            sel_entry *e = &st->e[i];
            zell_chan *c = e->c;
            if (!c) continue;
            zell_retain(c);
            e->w.ch = c;
            zell_retain(s);   /* the queue's reference */
            if (e->w.sender) wq_push(&c->sendq, &c->sendq_tail, &e->w);
            else wq_push(&c->recvq, &c->recvq_tail, &e->w);
        }
        sel_unlock_all(st);
        sel_release_stale(stale);
        zell__task_set_cancel_fn(s, sel_cancelled);
        zell__hook_link(&st->hook, zell__task_cur(), sel_cancelled, s);
        return s;
    }
    sel_unlock_all(st);
    sel_release_stale(stale);
    __atomic_store_n(&st->done, 1, __ATOMIC_RELEASE);
    for (int64_t i = 0; i < n; i++) {   /* the send copies that did not move */
        sel_entry *e = &st->e[i];
        if (e->c && e->w.sender && (e->idx != fired || closed_send)) elem_release(e->c, e->val);
    }
    if (closed_send) zell_panic("send on closed channel");
    if (peer) {
        sel_entry *f = &st->e[fired];
        if (f->w.sender) waiter_recv_finish(f->c, peer, f->val, NULL);
        else waiter_send_finish(f->c, peer);
    }
    memcpy(zell__task_slot(s), &fired, sizeof fired);
    zell__task_mark_done(s);
    return s;
}

/* Sets up select task s (slot from sel_slot_bytes) for its n cases. */
static void sel_init(zell_task *s, int64_t n) {
    sel_state *st = sel_of(s);
    st->n = (int32_t)n;
    st->hook.w = s;
    st->lk = (zell_chan **)&st->e[n];
}

static int64_t sel_slot_bytes(int64_t result_size, int64_t n, int64_t send_bytes) {
    return ZELL_ROUND8(result_size) + (int64_t)sizeof(sel_state) + n * (int64_t)(sizeof(sel_entry) + sizeof(zell_chan *)) + send_bytes;
}

static void sel_entry_init(zell_task *s, sel_entry *e, int64_t i, zell_chan *c, bool send) {
    e->w.hook.w = s;
    e->w.sel = sel_of(s);
    e->w.sender = send;
    e->c = c;
    e->idx = i;
}

/* ---- the compiled task.Channel<T> (handles are the channel's address) ---- */

int64_t zell_task_chan_new(int64_t capacity, int64_t elem_size, const zell_value_ops *elem_ops,
                           int64_t fal_size, const zell_value_ops *fal_ops, int64_t fal_err_off,
                           int64_t tup_size, const zell_value_ops *tup_ops, int64_t tup_ok_off) {
    if (capacity < 0) zell_panic("negative channel capacity");
    zell_chan *c = chan_new(elem_size, elem_ops, capacity);
    c->fal_size = fal_size;
    c->fal_ops = fal_ops;
    c->fal_err_off = fal_err_off;
    c->tup_size = tup_size;
    c->tup_ops = tup_ops;
    c->tup_ok_off = tup_ok_off;
    return (int64_t)(intptr_t)c;
}

void zell_task_chan_release(int64_t h) { zell_release((zell_chan *)(intptr_t)h); }
zell_task *zell_task_chan_send(int64_t h, const void *elem) { return chan_send(chan_of(h), elem); }
zell_task *zell_task_chan_recv(int64_t h) { return chan_recv(chan_of(h), RES_FALLIBLE); }
zell_task *zell_task_chan_next(int64_t h) { return chan_recv(chan_of(h), RES_TUPLE); }
void zell_task_chan_close(int64_t h) { chan_close(chan_of(h)); }
int64_t zell_task_chan_cap(int64_t h) { return chan_of(h)->capacity; }

int64_t zell_task_chan_len(int64_t h) {
    zell_chan *c = chan_of(h);
    chan_lock_acquire(&c->mu);
    int64_t n = c->count;
    chan_lock_release(&c->mu);
    return n;
}

bool zell_task_chan_is_closed(int64_t h) {
    zell_chan *c = chan_of(h);
    chan_lock_acquire(&c->mu);
    bool closed = c->closed != 0;
    chan_lock_release(&c->mu);
    return closed;
}

/* ---- select (10.7 "Select") ---- */

zell_task *zell_task_select(const zell_select_case *cases, int64_t n, int64_t has_default) {
    if (n < 0) zell_panic("zell_task_select: negative case count");
    int64_t send_bytes = 0;
    for (int64_t i = 0; i < n; i++)
        if (cases[i].chan && cases[i].send) send_bytes += ZELL_ROUND8(chan_of(cases[i].chan)->elem_size);
    zell_task *s = zell__task_new_slot(8, NULL, 0, sel_slot_bytes(8, n, send_bytes));
    sel_init(s, n);
    sel_state *st = sel_of(s);
    unsigned char *vals = (unsigned char *)&st->lk[n];
    for (int64_t i = 0; i < n; i++) {
        zell_chan *c = cases[i].chan ? chan_of(cases[i].chan) : NULL;
        sel_entry *e = &st->e[i];
        sel_entry_init(s, e, i, c, cases[i].send != 0);
        if (!c) continue;
        if (cases[i].send) {
            e->val = vals;
            vals += ZELL_ROUND8(c->elem_size);
            memcpy(e->val, cases[i].elem, (size_t)c->elem_size);
            elem_retain(c, e->val);
        } else {
            e->dst = cases[i].elem;
            e->ok = cases[i].ok;
        }
    }
    return sel_start(s, has_default != 0);
}

zell_task *zell_task_chan_recv_any(const int64_t *hs, int64_t n, int64_t res_size, int64_t val_off, int64_t ok_off) {
    if (n < 0) zell_panic("zell_task_chan_recv_any: negative channel count");
    const zell_value_ops *ops = n > 0 ? chan_of(hs[0])->elem_ops : NULL;
    zell_task *s = zell__task_new_slot(res_size, ops, val_off, sel_slot_bytes(res_size, n, 0));
    sel_init(s, n);
    sel_state *st = sel_of(s);
    unsigned char *r = zell__task_slot(s);
    for (int64_t i = 0; i < n; i++) {
        sel_entry *e = &st->e[i];
        sel_entry_init(s, e, i, chan_of(hs[i]), false);
        e->dst = r + val_off;
        e->ok = r + ok_off;
    }
    return sel_start(s, n == 0);   /* no channels: -1 at once instead of waiting forever */
}

/* ---- the C API (10.7): receives have the { int64 ok; T value @8 } layout ---- */

zell_chan *zell_chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity) {
    return chan_new(elem_size, ops, capacity);
}

zell_task *zell_chan_send_async(zell_chan *c, const void *elem) {
    if (!c) zell_panic_nil();
    return chan_send(c, elem);
}

zell_task *zell_chan_recv_async(zell_chan *c) {
    if (!c) zell_panic_nil();
    return chan_recv(c, RES_C);
}

void zell_chan_send(zell_chan *c, const void *elem) {
    zell_task *t = zell_chan_send_async(c, elem);
    zell_task_await(t);
    zell_release(t);
}

bool zell_chan_recv(zell_chan *c, void *out) {
    zell_task *t = zell_chan_recv_async(c);
    unsigned char *r = zell_task_await(t);
    bool ok = *(int64_t *)r != 0;
    if (out) {
        memcpy(out, r + 8, (size_t)c->elem_size);
        elem_retain(c, out);
    }
    zell_release(t);
    return ok;
}

void zell_chan_close(zell_chan *c) {
    if (!c) zell_panic_nil();
    chan_close(c);
}

int64_t zell_chan_len(const zell_chan *c) {
    return c ? zell_task_chan_len((int64_t)(intptr_t)c) : 0;
}

/* ======================================================================
 * Mutex: three words inline in the Zell struct (10.6): m[0] is the state,
 * m[1] and m[2] the head and tail of the queue of waiting tasks (linked
 * through their slots). State bits: 0 locked, 1 the guard of the queue, 2
 * waiters queued, 3 woken (a dequeued waiter is on its way to retry, so
 * Unlock wakes nobody else), 4 starving (FIFO hand-off); higher bits are
 * reserved. 0 is free with nothing to do on Lock and 1 is locked with
 * nothing to do on Unlock: the compiler inlines Lock as a compare-and-swap
 * 0 -> 1 and Unlock as 1 -> 0 (ABI.md 10.6) and calls the functions below
 * only when the CAS fails.
 *
 * Go's sync.Mutex rules (DESIGN.md "Асинхронность"). Normal mode: Unlock
 * frees the mutex and wakes the oldest waiter without handing it over; the
 * waiter retries in a job (mu_retry) and a running task may take the
 * mutex first (barging, also after up to MU_SPINS spins in Lock); a waiter
 * that loses re-queues at the head, so it stays the oldest. A waiter that
 * has waited more than MU_STARVE_NS when it loses sets the starving bit:
 * Unlock then hands ownership to the oldest waiter (the mutex stays locked)
 * and new Locks queue at the tail, until the queue drains or the waiter
 * that receives the mutex waited less than MU_STARVE_NS.
 *
 * Invariant: the locked bit is set only by a CAS from a state with the
 * guard and starving bits clear, or by the guard's holder, and the
 * guarded word changes only by its holder's stores (every other writer
 * CASes from an unguarded state). A free mutex with the starving bit set
 * does not occur: starving is set only while locked with a waiter queued,
 * and a hand-off keeps the mutex locked.
 * ====================================================================== */

enum { MU_LOCKED = 1, MU_GUARD = 2, MU_WAITERS = 4, MU_WOKEN = 8, MU_STARVING = 16 };
#define MU_SPINS 100                 /* CAS attempts of a contended Lock before it queues */
#define MU_STARVE_NS 1000000         /* a waiter this old switches the mutex to FIFO hand-off */

/* A waiter's slot: the queue link, when it first queued, and the mutex
 * (for the retry job). */
typedef struct mu_wait {
    zell_task *next;
    int64_t since;
    int64_t *m;
} mu_wait;

static inline mu_wait *mu_w(zell_task *t) { return (mu_wait *)zell__task_slot(t); }

/* Takes the guard and returns the state with the guard set. */
static inline int64_t mu_guard(int64_t *m) {
    for (int spins = 0;; spins++) {
        int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
        if (!(s & MU_GUARD) && __atomic_compare_exchange_n(m, &s, s | MU_GUARD, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) return s | MU_GUARD;
        if (spins > 64) zell__plat_yield();
    }
}

/* Stores the new state s, which drops the guard (guard holder only). */
static inline void mu_store(int64_t *m, int64_t s) { __atomic_store_n(m, s & ~(int64_t)MU_GUARD, __ATOMIC_RELEASE); }

/* Barging: takes the mutex when it is free, unguarded and not starving. */
static inline bool mu_try_take(int64_t *m, int64_t s) {
    return !(s & (MU_LOCKED | MU_GUARD | MU_STARVING)) &&
           __atomic_compare_exchange_n(m, &s, s | MU_LOCKED, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

static zell_task *mu_pop(int64_t *m) {
    zell_task *t = (zell_task *)(intptr_t)m[1];
    m[1] = (int64_t)(intptr_t)mu_w(t)->next;
    if (!m[1]) m[2] = 0;
    return t;
}

/* The retry of a woken waiter t (a job; t carries the queue's reference). */
static void mu_retry(void *arg) {
    zell_task *t = arg;
    int64_t *m = mu_w(t)->m;
    int64_t s = mu_guard(m) & ~(int64_t)MU_WOKEN;
    if (!(s & MU_LOCKED)) {
        mu_store(m, s | MU_LOCKED);   /* starving is clear: the mutex was free */
        zell_task_complete(t, NULL);
        zell_release(t);
        return;
    }
    /* lost to a barging task: back to the head, still the oldest */
    if (zell__plat_mono_ns() - mu_w(t)->since > MU_STARVE_NS) s |= MU_STARVING;
    mu_w(t)->next = (zell_task *)(intptr_t)m[1];
    if (!m[1]) m[2] = (int64_t)(intptr_t)t;
    m[1] = (int64_t)(intptr_t)t;
    mu_store(m, s | MU_WAITERS);
}

zell_task *zell_task_mutex_lock(int64_t *m) {
    if (!m) zell_panic_nil();
    int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
    if (mu_try_take(m, s)) return zell_task_done();
    /* Spin while another worker may release it soon (Go's canSpin). */
    if (!(s & MU_STARVING) && zell__sched_can_spin()) {
        for (int i = 0; i < MU_SPINS; i++) {
            zell__plat_cpu_relax();
            s = __atomic_load_n(m, __ATOMIC_RELAXED);
            if (s & MU_STARVING) break;
            if (mu_try_take(m, s)) return zell_task_done();
        }
    }
    s = mu_guard(m);
    if (!(s & MU_LOCKED)) {   /* free (so not starving) */
        mu_store(m, s | MU_LOCKED);
        return zell_task_done();
    }
    zell_task *t = zell__task_new_slot(0, NULL, 0, (int64_t)sizeof(mu_wait));
    zell_retain(t);   /* the queue's reference */
    mu_wait *w = mu_w(t);
    w->next = NULL;
    w->since = zell__plat_mono_ns();
    w->m = m;
    zell_task *tail = (zell_task *)(intptr_t)m[2];
    if (tail) mu_w(tail)->next = t; else m[1] = (int64_t)(intptr_t)t;
    m[2] = (int64_t)(intptr_t)t;
    mu_store(m, s | MU_WAITERS);
    return t;
}

bool zell_task_mutex_try_lock(int64_t *m) {
    if (!m) zell_panic_nil();
    /* Free and not starving; a guarded state counts as taken (its holder
     * may be a slow Lock about to take it). */
    for (;;) {
        int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
        if (s & (MU_LOCKED | MU_GUARD | MU_STARVING)) return false;
        if (__atomic_compare_exchange_n(m, &s, s | MU_LOCKED, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) return true;
    }
}

void zell_task_mutex_unlock(int64_t *m) {
    if (!m) zell_panic_nil();
    int64_t s = mu_guard(m);
    if (!(s & MU_LOCKED)) {
        mu_store(m, s);
        zell_panic("unlock of unlocked mutex");
    }
    if (s & MU_STARVING) {
        /* FIFO hand-off: the oldest waiter owns it from now on */
        zell_task *t = mu_pop(m);
        if (!m[1] || zell__plat_mono_ns() - mu_w(t)->since < MU_STARVE_NS) s &= ~(int64_t)MU_STARVING;
        if (!m[1]) s &= ~(int64_t)MU_WAITERS;
        mu_store(m, s);   /* still locked */
        zell_task_complete(t, NULL);
        zell_release(t);
        return;
    }
    s &= ~(int64_t)MU_LOCKED;
    if (!m[1] || (s & MU_WOKEN)) {   /* nobody to wake, or a woken waiter will retry */
        mu_store(m, s);
        return;
    }
    zell_task *t = mu_pop(m);
    if (!m[1]) s &= ~(int64_t)MU_WAITERS;
    mu_store(m, s | MU_WOKEN);
    zell__sched_post_plain(mu_retry, t, true);
}

/* Releases the waiters of a mutex that is dropped while tasks wait on it
 * (the C API's zell_mutex; the Zell struct has no destructor and leaks). */
static void mutex_words_drop(int64_t *m) {
    zell_task *t = (zell_task *)(intptr_t)m[1];
    while (t) {
        zell_task *n = mu_w(t)->next;
        zell_release(t);
        t = n;
    }
}

/* ---- the C API (10.6) ---- */

struct zell_mutex {
    zell_object hdr;
    int64_t m[3];
};

static void mutex_drop(void *obj) { mutex_words_drop(((zell_mutex *)obj)->m); }

static const zell_type_info mutex_type = { "Mutex", (int64_t)sizeof(struct zell_mutex), mutex_drop };

zell_mutex *zell_mutex_new(void) { return zell_object_new(&mutex_type, (int64_t)sizeof(struct zell_mutex)); }

zell_task *zell_mutex_lock_async(zell_mutex *m) {
    if (!m) zell_panic_nil();
    return zell_task_mutex_lock(m->m);
}

void zell_mutex_lock(zell_mutex *m) {
    zell_task *t = zell_mutex_lock_async(m);
    zell_task_await(t);
    zell_release(t);
}

bool zell_mutex_try_lock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    return zell_task_mutex_try_lock(m->m);
}

void zell_mutex_unlock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    zell_task_mutex_unlock(m->m);
}
