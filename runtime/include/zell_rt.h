/*
 * zell_rt.h - public ABI of the Zell runtime library (libzell_rt.a), v0.1
 *
 * This is the single public header. ABI.md is the normative description of
 * every symbol and layout declared here; keep the two in sync.
 *
 * General conventions (see ABI.md "Conventions"):
 *   - Target: 64-bit (LP64). Pointers and int64_t are 8 bytes.
 *   - Arguments that are heap objects are BORROWED: the runtime never
 *     consumes a caller's reference. If the runtime stores a value it retains
 *     it itself.
 *   - Heap objects returned by a function are OWNED (+1): the caller must
 *     release them, unless the function is documented as [borrowed].
 *   - NULL is accepted everywhere a heap object is accepted, with the
 *     documented meaning (NULL string == "", NULL error == no error, ...).
 *   - Value types larger than 8 bytes (zell_slice, iterators) are always
 *     passed by pointer and returned through an out-pointer, so the compiler
 *     never has to implement C struct-by-value ABI rules.
 */
#ifndef ZELL_RT_H
#define ZELL_RT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(__GNUC__) || defined(__clang__)
#define ZELL_NORETURN __attribute__((noreturn))
#else
#define ZELL_NORETURN _Noreturn
#endif

#define ZELL_RT_VERSION_MAJOR 0
#define ZELL_RT_VERSION_MINOR 1

/* ======================================================================
 * Memory: object header, type info, value ops
 * ====================================================================== */

/* Refcount value marking a static/immortal object (literals, sentinels).
 * retain/release are no-ops on such objects. */
#define ZELL_RC_IMMORTAL ((int64_t)-1)
/* Refcount value of an object allocated in an arena (arena, ABI.md
 * 3.6): retain/release are no-ops, the arena frees it. */
#define ZELL_RC_ARENA ((int64_t)-2)
/* Bit 62 of a (non-immortal) refcount word: the object has a weak box.
 * Bits 57..61 belong to the cycle collector (ABI.md 3.8): CYCLIC marks an
 * object of a potentially cyclic type (set at allocation while the
 * collector is on; such objects always take the runtime's retain/release
 * paths), BUFFERED one recorded as a possible cycle root, MARKED one in the
 * set a running collection examines, DIRTY one whose count changed while
 * MARKED, ZOMBIE one whose count reached zero while BUFFERED (dropped; its
 * memory is freed by the buffer's owner). The count itself is
 * refcount & ZELL_RC_COUNT_MASK (the low 57 bits). */
#define ZELL_RC_WEAK_FLAG ((int64_t)1 << 62)
#define ZELL_RC_CYCLIC ((int64_t)1 << 61)
#define ZELL_RC_BUFFERED ((int64_t)1 << 60)
#define ZELL_RC_MARKED ((int64_t)1 << 59)
#define ZELL_RC_DIRTY ((int64_t)1 << 58)
#define ZELL_RC_ZOMBIE ((int64_t)1 << 57)
#define ZELL_RC_COUNT_MASK (ZELL_RC_ZOMBIE - 1)
/* Biased reference counting (ABI.md 3.1): an object without CYCLIC whose
 * word has BIASED is owned by one thread. Its word is then
 *   bits 0..15  B: the owner's count (plain 16-bit loads/stores by the owner)
 *   bits 16..46 S: the shared count, signed, stored + ZELL_RC_S_EXCESS
 *               (atomic 64-bit RMW of +-1 << 16 by other threads)
 *   bits 47..58 the owner's thread slot (1..4095)
 *   bit 59      QUEUED: S went negative; the object waits in its owner's queue
 *   bit 60      BIASED, bit 62 WEAK_FLAG
 * and the count is B + S. A word without BIASED (and without CYCLIC) is an
 * unowned count in the low 57 bits, as above. Bits 47..60 have these
 * meanings only in BIASED words; CYCLIC words keep the cycle collector's. */
#define ZELL_RC_BIASED ((int64_t)1 << 60)
#define ZELL_RC_QUEUED ((int64_t)1 << 59)
/* The same bit in an unowned (not BIASED, not CYCLIC) word: never bias this
 * object (the runtime sets it on objects that are shared between threads
 * by design, such as tasks). Not part of the count. */
#define ZELL_RC_NOBIAS ZELL_RC_QUEUED
#define ZELL_RC_B_MASK ((int64_t)0xFFFF)
#define ZELL_RC_S_SHIFT 16
#define ZELL_RC_S_FIELD ((int64_t)0x7FFFFFFF)     /* after >> ZELL_RC_S_SHIFT */
#define ZELL_RC_S_EXCESS ((int64_t)1 << 30)
#define ZELL_RC_OWNER_SHIFT 47
#define ZELL_RC_OWNER_SLOTS 4096
/* ((uint64_t)word >> ZELL_RC_OWNER_SHIFT) & ZELL_RC_OWNER_MASK equals the
 * calling thread's zell_rc_owner exactly when that thread owns the
 * (biased, non-cyclic, non-special) object. */
#define ZELL_RC_OWNER_MASK ((int64_t)0x16FFF)
#define ZELL_RC_NO_OWNER ((int64_t)0x1FFFF)          /* never matches */
/* Exit status of a program whose leak check found live objects. */
#define ZELL_LEAK_EXIT_CODE 66

typedef struct zell_type_info {
    const char *name;          /* +0  human-readable type name (may be NULL) */
    int64_t size;              /* +8  informational object size, 0 if unknown */
    void (*drop)(void *obj);   /* +16 releases children; must NOT free obj. May be NULL. */
} zell_type_info;             /* 24 bytes */

typedef struct zell_object {
    int64_t refcount;              /* +0 accessed atomically; ZELL_RC_IMMORTAL = static */
    const zell_type_info *type;   /* +8 may be NULL (no drop) */
} zell_object;                    /* 16 bytes; payload starts at +16 */

/* Ownership hooks for a value stored inline in a container slot
 * (slice element, map key/value, task result, channel element).
 * NULL ops pointer == plain data, nothing to retain/release. */
/* Cycle-collector visitor callback (ABI.md 3.8): `obj` is the header of a
 * referenced object; `tid` is NULL when the object is statically of a
 * cyclic type, else a dynamic type id (interface/any type word, closure
 * function pointer) the collector checks against its registry first. */
typedef void (*zell_cycle_cb)(void *ctx, void *obj, const void *tid);
typedef struct zell_value_ops {
    void (*retain)(void *slot);    /* +0 add the references held by *slot */
    void (*release)(void *slot);   /* +8 drop the references held by *slot */
    /* +16 cycle collector: cb(ctx, obj, tid) for each reference in *slot
     * that may lie on a cycle, or NULL. Containers created with ops whose
     * visit is non-NULL are flagged ZELL_RC_CYCLIC (ABI.md 3.8). */
    void (*visit)(void *slot, zell_cycle_cb cb, void *ctx);
} zell_value_ops;                 /* 24 bytes */

/* Slot holds exactly one heap-object pointer (string, *T, map, error, task...). */
extern const zell_value_ops zell_ops_ref;
/* Slot holds one `*T` pointer to an object payload (object + 16), or NULL. */
extern const zell_value_ops zell_ops_ptr;
/* Slot holds a zell_slice (32 bytes). */
extern const zell_value_ops zell_ops_slice;

void *zell_alloc(int64_t size);              /* zeroed raw memory, OOM -> panic */
void *zell_realloc(void *p, int64_t size);   /* raw; new bytes NOT zeroed */
void  zell_free(void *p);                    /* raw; NULL ok */
void *zell_coro_frame_alloc(int64_t size);   /* zeroed coroutine frame (10.3) */
void *zell_coro_frame_alloc_uninit(int64_t size);   /* the same, not zeroed (10.3) */
void  zell_coro_frame_free(void *p);         /* frees it; NULL ok */
void  zell_coro_frame_free_sized(void *p, int64_t size); /* frees a frame allocated with `size`; NULL ok */

void *zell_object_new(const zell_type_info *type, int64_t size); /* rc=1, zeroed, size includes header */
void  zell_retain(void *obj);                /* NULL / immortal: no-op */
void  zell_release(void *obj);               /* NULL / immortal: no-op; at 0: drop + free */
/* 0 while the process has only one thread (refcounts may then be updated
 * without atomic RMW); set for good before the second thread starts. */
extern uint8_t zell_threaded;
/* The calling thread's owner tag for biased words: (BIASED >> 47) | slot,
 * or ZELL_RC_NO_OWNER for a thread without a slot (ABI.md 3.1). */
#ifdef __cplusplus
extern thread_local int64_t zell_rc_owner;
#else
extern _Thread_local int64_t zell_rc_owner;
#endif
int64_t zell_refcount(const void *obj);      /* debugging / tests; 0 for NULL */
int64_t zell_live_objects(void);             /* number of live zell_object_new objects */

/* Cycle collector (ABI.md 3.8). A registry entry describes objects whose
 * type info is `key` (visit/fin/clear take the object header; fin runs the
 * destructors only, clear releases the references without them; NULL
 * clear means type->drop), or, with visit == NULL, is a dynamic type id
 * (key only) whose referenced objects may be flagged. `alias` is a
 * dynamic type id (ti.<*S> of a type S, a closure's function pointer for
 * its environment) registered with the entry, or NULL; `state` is the
 * runtime's (0 until zell_cycles_new_entry registered it). */
typedef struct zell_cycle_type {
    const void *key;                                        /* +0  */
    void (*visit)(void *obj, zell_cycle_cb cb, void *ctx); /* +8  */
    void (*fin)(void *obj);                                 /* +16 */
    void (*clear)(void *obj);                               /* +24 */
    const void *alias;                                      /* +32 */
    int64_t state;                                          /* +40 */
} zell_cycle_type;                                         /* 48 bytes */
typedef struct zell_cycle_stats {
    int64_t runs;          /* collections run (dry runs included) */
    int64_t collected;     /* objects freed by the collector so far */
    int64_t candidates;    /* possible roots recorded so far */
    int64_t examined;      /* objects examined by all collections */
    int64_t pending;       /* published possible roots waiting for a collection */
    int64_t threshold;     /* auto mode: published roots that trigger one */
    int64_t aborted;       /* collections abandoned (set over ZELL_CYCLES_MAX_SCAN) */
    int64_t mode;          /* 0 off, 1 manual, 2 auto */
} zell_cycle_stats;
#define ZELL_CYCLES_OFF 0
#define ZELL_CYCLES_MANUAL 1
#define ZELL_CYCLES_AUTO 2
/* Registers `n` entries (kept by pointer; call before objects of those types exist). */
void zell_cycles_register(const zell_cycle_type *types, int64_t n);
/* zell_object_new for a cyclic type: sets ZELL_RC_CYCLIC unless the
 * collector is off; may run a pending automatic collection first. */
void *zell_cycles_new(const zell_type_info *type, int64_t size);
/* The same for the entry's type (e->key), registering the entry (and its
 * alias) first if needed: what compiled code calls. */
void *zell_cycles_new_entry(zell_cycle_type *e, int64_t size);
/* The same for a dynamic type: flagged when `type` is a registered object type. */
void *zell_cycles_new_dyn(const zell_type_info *type, int64_t size);
/* Runs a collection now on the calling thread; returns the objects freed. */
int64_t zell_cycles_collect(void);
void zell_cycles_stats(zell_cycle_stats *out);
int32_t zell_cycles_mode(void);
/* ZELL_CYCLES_MANUAL <-> ZELL_CYCLES_AUTO (tests); OFF only before the first flagged object. */
void zell_cycles_set_mode(int32_t mode);
/* runtime.CollectCycles / CycleStats (stdlib_src/runtime_cycles.zl):
 * stat 0 runs, 1 collected, 2 candidates, 3 examined, 4 pending,
 * 5 threshold, 6 aborted, 7 mode. */
int64_t zell_runtime_rt_collect_cycles(void);
/* The collection epoch and the release paired with a pin (ABI.md 3.8). */
extern int64_t zell_cycles_epoch;
void zell_ptr_release_noroot(void *payload, int64_t epoch);
/* diag.HeapByType (diag_heap.c): "live\ttotal\tbytes\ttype" lines. */
struct zell_string *zell_diag_rt_heap_types(void);
bool zell_diag_rt_heap_counting(void);
int64_t zell_runtime_rt_cycle_stat(int64_t which);

/* Leak check (ABI.md 3.5): pointer fields of a struct type, so the report can
 * name the fields that close a cycle. `fields` ends with offset -1. */
typedef struct zell_leak_field {
    int64_t offset;            /* +0 byte offset of a `*T` field in the payload */
    const char *name;          /* +8 "Type.Field" */
} zell_leak_field;
/* Leak check mode 2: `type` is @acyclic (`name` its short name); a leaked
 * object of it on a cycle is reported (ABI.md 3.5). */
void zell_leak_acyclic(const zell_type_info *type, const char *name);
void zell_leak_fields(const zell_type_info *type, const zell_leak_field *fields);

/* Arenas (arena, ABI.md 3.6). Handles are raw pointers passed as int64. */
int64_t zell_arena_create(void);
void zell_arena_destroy(int64_t arena);       /* frees everything and the arena */
void zell_arena_free(int64_t arena);          /* frees everything; the arena stays usable */
void *zell_arena_object(int64_t arena, const zell_type_info *type, int64_t size); /* zeroed, rc ZELL_RC_ARENA */
struct zell_slice;
struct zell_string;
void zell_arena_slice(struct zell_slice *out, int64_t arena, int64_t elem_size, int64_t n,
                       const zell_value_ops *ops);
struct zell_string *zell_arena_string(int64_t arena, const struct zell_string *s); /* borrowed in, arena-owned out */
int64_t zell_arena_objects(int64_t arena);    /* objects currently allocated in the arena */
void  zell_ptr_retain(void *payload);        /* retain the object whose payload (obj+16) is given; NULL ok */
void  zell_ptr_release(void *payload);       /* release likewise; NULL ok */
/* Leak check (`zell run --leak-check`): call before anything else. Mode 1
 * counts, mode 2 also records type names. zell_runtime_shutdown then prints
 * "leak: N objects" (+ "  K x Type" lines) to stderr and exits with
 * ZELL_LEAK_EXIT_CODE if objects are still alive. */
void  zell_leak_check_enable(int32_t mode);

/* Weak references: a weak box shared by all weak refs to one object. */
void *zell_weak_new(void *payload);          /* +1 weak box for the object of `payload`; NULL -> NULL */
void *zell_weak_get(void *box);              /* +1 payload pointer, or NULL if freed (or box NULL) */

/* ======================================================================
 * Strings: pointer to immutable heap object; NULL == ""
 * ====================================================================== */

typedef struct zell_string {
    zell_object hdr;   /* +0  */
    int64_t len;        /* +16 length in bytes */
    char *data;         /* +24 the bytes. data[len] is always readable and is
                         *     '\0' except in a view (ABI.md 4.1): use the
                         *     length, or zell_string_cstr() for C APIs */
} zell_string;         /* 32 bytes; an owned string's bytes follow at +32 */

extern const zell_type_info zell_type_string;
/* Type of a standalone view {hdr, len, data, owner @32}: its drop releases
 * the owner (ABI.md 4.1). */
extern const zell_type_info zell_type_string_view;

/* Refcount word of a view stored inside a view block (strings.Split...):
 * ZELL_RC_VIEW | address of the block; retain/release go to the block.
 * Every such word is < ZELL_RC_ARENA. */
#define ZELL_RC_VIEW ((int64_t)INT64_MIN)
static inline int zell_rc_is_view(int64_t rc) { return rc < ZELL_RC_ARENA; }

/* Static storage of an immortal string literal of N bytes (N includes the
 * NUL) and its initializer; `self` names the variable being defined. */
#define ZELL_STRING_STORAGE(N) struct { zell_object hdr; int64_t len; char *data; char bytes[N]; }
#define ZELL_STRING_INIT(self, lit) \
    { { ZELL_RC_IMMORTAL, &zell_type_string }, (int64_t)(sizeof(lit) - 1), (self).bytes, lit }

/* Static literal for C code. The compiler emits the equivalent LLVM global. */
#define ZELL_STATIC_STRING(name, lit)                                          \
    static ZELL_STRING_STORAGE(sizeof(lit)) name##_storage =                   \
        ZELL_STRING_INIT(name##_storage, lit);                                 \
    static zell_string *const name = (zell_string *)&name##_storage

zell_string *zell_string_new(const char *bytes, int64_t len);   /* owned copy */
zell_string *zell_string_from_cstr(const char *cstr);           /* owned copy */
zell_string *zell_string_literal(const char *bytes, int64_t len); /* immortal copy, never freed */
const char   *zell_string_data(const zell_string *s);  /* [borrowed] bytes (NOT NUL-terminated for a view), "" for NULL */
/* [borrowed-ish] NUL-terminated bytes for C APIs: s->data when it is
 * terminated, else a copy in a per-thread ring of 16 buffers, valid until
 * the 16th next call on this thread (use it within one C call). */
const char   *zell_string_cstr(const zell_string *s);
int64_t zell_string_len(const zell_string *s);         /* bytes */
int64_t zell_string_rune_count(const zell_string *s);  /* code points; invalid byte = 1 rune */
zell_string *zell_string_concat(const zell_string *a, const zell_string *b);
/* a + b, CONSUMING the caller's reference to `a` (result +1): when `a` is
 * uniquely referenced it is extended in place (amortised O(len(b))). */
zell_string *zell_string_append(zell_string *a, const zell_string *b);
int32_t zell_string_compare(const zell_string *a, const zell_string *b); /* -1, 0, 1 bytewise */
bool    zell_string_eq(const zell_string *a, const zell_string *b);
uint64_t zell_string_hash(const zell_string *s);        /* FNV-1a 64 of the bytes */
zell_string *zell_string_slice(const zell_string *s, int64_t lo, int64_t hi); /* bytes [lo,hi) */
uint8_t zell_string_byte_at(const zell_string *s, int64_t i);   /* bounds-checked */
int32_t zell_string_decode_rune(const zell_string *s, int64_t i, int64_t *width); /* U+FFFD on error */
int64_t zell_string_index(const zell_string *s, const zell_string *sub); /* byte index or -1 */
bool    zell_string_contains(const zell_string *s, const zell_string *sub);
bool    zell_string_has_prefix(const zell_string *s, const zell_string *prefix);
bool    zell_string_has_suffix(const zell_string *s, const zell_string *suffix);

zell_string *zell_string_from_int(int64_t v);
zell_string *zell_string_from_uint(uint64_t v);
zell_string *zell_string_from_float(double v);
zell_string *zell_string_from_bool(bool v);
zell_string *zell_string_from_rune(int32_t r);

struct zell_error;
int64_t zell_string_to_int(const zell_string *s, struct zell_error **err);
double  zell_string_to_float(const zell_string *s, struct zell_error **err);

/* ======================================================================
 * Slices: 32-byte value, always handled through pointers
 * ====================================================================== */

typedef struct zell_slice {
    void *data;            /* +0  first element (NULL when backing is NULL) */
    int64_t len;           /* +8  */
    int64_t cap;           /* +16 elements available from data */
    zell_object *backing; /* +24 owned reference to the backing array, or NULL */
} zell_slice;             /* 32 bytes; all-zero == nil/empty slice */

void zell_slice_new(zell_slice *out, int64_t elem_size, int64_t len, int64_t cap);
void zell_slice_new_ops(zell_slice *out, int64_t elem_size, int64_t len, int64_t cap,
                         const zell_value_ops *ops);
void zell_slice_from(zell_slice *out, int64_t elem_size, const zell_value_ops *ops,
                      const void *elems, int64_t n);
void zell_slice_retain(const zell_slice *s);
void zell_slice_release(zell_slice *s);           /* also zeroes *s */
void zell_slice_assign(zell_slice *dst, const zell_slice *src); /* retain src, release old dst */
void *zell_slice_index(const zell_slice *s, int64_t i, int64_t elem_size); /* bounds-checked */
void zell_slice_append(zell_slice *s, int64_t elem_size, const zell_value_ops *ops,
                        const void *elem);
void zell_slice_append_slice(zell_slice *s, int64_t elem_size, const zell_value_ops *ops,
                              const zell_slice *other);
void zell_slice_sub(zell_slice *out, const zell_slice *s, int64_t lo, int64_t hi,
                     int64_t elem_size);
int64_t zell_slice_copy(zell_slice *dst, const zell_slice *src, int64_t elem_size,
                         const zell_value_ops *ops);
zell_string *zell_string_from_bytes(const zell_slice *bytes); /* []byte -> string */
void zell_string_to_bytes(zell_slice *out, const zell_string *s); /* string -> []byte */

/* ======================================================================
 * Maps: refcounted heap object; NULL == nil map
 * ====================================================================== */

typedef uint64_t (*zell_hash_fn)(const void *key);
typedef bool (*zell_eq_fn)(const void *a, const void *b);

typedef struct zell_map_desc {
    int64_t key_size;                 /* +0  */
    int64_t val_size;                 /* +8  */
    zell_hash_fn hash;               /* +16 */
    zell_eq_fn eq;                   /* +24 */
    const zell_value_ops *key_ops;   /* +32 may be NULL */
    const zell_value_ops *val_ops;   /* +40 may be NULL */
} zell_map_desc;                     /* 48 bytes; copied by zell_map_new */

typedef struct zell_map zell_map;   /* opaque */

typedef struct zell_map_iter {
    zell_map *map;    /* +0  borrowed */
    int64_t bucket;    /* +8  */
    void *next;        /* +16 */
    int64_t reserved;  /* +24 */
} zell_map_iter;      /* 32 bytes, contents private */

uint64_t zell_hash_bytes(const void *p, int64_t n);
uint64_t zell_hash_int(const void *key);   /* key -> int64_t */
bool     zell_eq_int(const void *a, const void *b);
uint64_t zell_hash_str(const void *key);   /* key -> zell_string* */
bool     zell_eq_str(const void *a, const void *b);

zell_map *zell_map_new(const zell_map_desc *desc, int64_t hint);
zell_map *zell_map_new_str(int64_t val_size, const zell_value_ops *val_ops);
zell_map *zell_map_new_int(int64_t val_size, const zell_value_ops *val_ops);
int64_t zell_map_len(const zell_map *m);
void   *zell_map_get(const zell_map *m, const void *key);      /* [borrowed] value slot or NULL */
void    zell_map_set(zell_map *m, const void *key, const void *val);
void   *zell_map_get_or_insert(zell_map *m, const void *key);  /* zeroed slot if new */
bool    zell_map_delete(zell_map *m, const void *key);
void   *zell_map_get_str(const zell_map *m, const zell_string *key);
void    zell_map_set_str(zell_map *m, const zell_string *key, const void *val);
bool    zell_map_delete_str(zell_map *m, const zell_string *key);
void   *zell_map_get_or_insert_str(zell_map *m, const zell_string *key); /* zeroed slot if new */
void   *zell_map_get_int(const zell_map *m, int64_t key);
void    zell_map_set_int(zell_map *m, int64_t key, const void *val);
bool    zell_map_delete_int(zell_map *m, int64_t key);
void   *zell_map_get_or_insert_int(zell_map *m, int64_t key);            /* zeroed slot if new */
void    zell_map_iter_init(zell_map_iter *it, zell_map *m);
bool    zell_map_iter_next(zell_map_iter *it, void **key, void **val);

/* ======================================================================
 * Errors
 * ====================================================================== */

typedef zell_string *(*zell_message_fn)(void *payload); /* returns OWNED string */

/* Static record of a source location, emitted by the compiler in read-only
 * data (one per error-creation site, `?` and catching `try`). */
typedef struct zell_error_site {
    const char *file;                 /* +0  path as given to the compiler */
    const char *func;                 /* +8  function name, e.g. "Load", "Cache.Get" */
    int64_t line;                     /* +16 1-based */
} zell_error_site;                   /* 24 bytes */

/* One `returned via` entry of an error trace. */
typedef struct zell_error_frame {
    const zell_error_site *site;
    zell_string *ctx;                /* `?("ctx")` context (owned ref) or NULL */
} zell_error_frame;                  /* 16 bytes */

#define ZELL_ERROR_INLINE_FRAMES 8

typedef struct zell_error {
    zell_object hdr;                 /* +0  */
    zell_string *message;            /* +16 own message / wrap context (owned ref) */
    struct zell_error *inner;        /* +24 wrapped error (owned ref) or NULL */
    const zell_type_info *user_type; /* +32 type of boxed user error or NULL */
    void *payload;                    /* +40 boxed user value: zell object (owned ref) or NULL */
    zell_message_fn message_fn;      /* +48 user Message() thunk or NULL */
    /* --- error trace (ABI.md 7.4); all zero for an error without one --- */
    struct zell_error *origin;       /* +56 the error this is a copy-on-write copy of (owned ref) or NULL */
    const zell_error_site *created;  /* +64 creation site or NULL */
    int64_t nframes;                  /* +72 number of `returned via` frames */
    zell_error_frame frames[ZELL_ERROR_INLINE_FRAMES]; /* +80 the first 8 frames */
    zell_error_frame *overflow;      /* +208 frames 8.. (raw heap array) or NULL */
    int64_t overflow_cap;             /* +216 capacity of overflow */
} zell_error;                        /* 224 bytes */

extern const zell_type_info zell_type_error;
extern zell_error *const zell_err_eof;   /* sentinel returned by stdin reads at EOF */

zell_error *zell_error_new(const zell_string *msg);
zell_error *zell_error_new_cstr(const char *msg);
zell_error *zell_error_sentinel(const zell_string *msg);  /* immortal, for `const E = error(..)` */
zell_error *zell_error_new_typed(const zell_type_info *type, void *payload,
                                   zell_message_fn message_fn);
zell_error *zell_error_wrap(zell_error *inner, const zell_string *ctx); /* NULL inner -> NULL */
zell_error *zell_error_unwrap(const zell_error *err);   /* owned inner or NULL */
zell_string *zell_error_message(const zell_error *err); /* owned; full chain text */
bool  zell_error_is(const zell_error *err, const zell_error *target);
void *zell_error_as(const zell_error *err, const zell_type_info *type); /* [borrowed] payload */
/* Error traces (ABI.md 7.4). All take an OWNED err and return it (or its
 * copy-on-write replacement) OWNED; NULL err -> NULL. A shared error (a
 * sentinel) keeps its pending trace in a thread-local record until it is
 * caught: `zell_error_caught` appends the frame `site` (if not NULL) and
 * then attaches the record to a copy of the error (`keep`) or drops it. */
zell_error *zell_error_created(zell_error *err, const zell_error_site *site);
zell_error *zell_error_returned(zell_error *err, const zell_error_site *site,
                                  const zell_string *ctx);
zell_error *zell_error_caught(zell_error *err, const zell_error_site *site, int64_t keep);
void zell_error_discarded(zell_error *err);  /* drops the record, releases err */
zell_string *zell_error_trace(const zell_error *err);              /* owned; "" if none */
void zell_error_frames(const zell_error *err, zell_slice *out);    /* owned []errors.Frame */
extern const zell_value_ops zell_ops_error_frame;  /* ops of a 32-byte errors.Frame */
/* The double nearest to the shortest decimal that round-trips as float32
 * `f`; printing it with the float64 rules gives Go's float32 output. */
double zell_float32_shortest(float f);
/* "<op> <path>: <strerror(e)>"; for ENOENT the error wraps the immortal
 * sentinel fs.ErrNotFound (zell_fs_err_not_found), so errors.Is works. */
zell_error *zell_error_errno(const char *op, const char *path, int e);
zell_error *zell_fs_err_not_found(void);           /* fs.ErrNotFound, immortal */
/* fmt.Sprintf: Go verbs over the compiler's per-argument lowering
 * (kind letters, int values, float values, println texts; std_fmt_sprintf.c). */
zell_string *zell_fmt_sprintf(const zell_string *format, const zell_string *kinds, const zell_slice *ints,
                                const zell_slice *floats, const zell_slice *texts);
/* `main() void!` failed: "error: <message>" and the trace on stderr. */
void zell_error_report(const zell_error *err);

/* ======================================================================
 * Panic
 * ====================================================================== */

ZELL_NORETURN void zell_panic(const char *msg);
ZELL_NORETURN void zell_panic_str(const zell_string *msg);
ZELL_NORETURN void zell_panic_error(const zell_error *err);
ZELL_NORETURN void zell_panic_nil(void);
ZELL_NORETURN void zell_panic_bounds(int64_t idx, int64_t len);
ZELL_NORETURN void zell_panic_slice_bounds(int64_t lo, int64_t hi, int64_t cap);

/* ======================================================================
 * I/O
 * ====================================================================== */

void zell_print_str(const zell_string *s);
void zell_print_cstr(const char *s);
void zell_print_int(int64_t v);
void zell_print_uint(uint64_t v);
void zell_print_float(double v);
void zell_print_bool(bool v);
void zell_print_rune(int32_t r);
void zell_print_space(void);
void zell_print_newline(void);
void zell_println_str(const zell_string *s);
void zell_println_cstr(const char *s);
void zell_println_int(int64_t v);
void zell_println_uint(uint64_t v);
void zell_println_float(double v);
void zell_println_bool(bool v);
void zell_println_rune(int32_t r);
void zell_eprint_str(const zell_string *s);   /* to stderr */
void zell_eprintln_str(const zell_string *s);
void zell_flush(void);

zell_string *zell_read_file(const zell_string *path, zell_error **err);
zell_error  *zell_write_file(const zell_string *path, const zell_string *data);
zell_error  *zell_write_file_bytes(const zell_string *path, const zell_slice *data);
zell_string *zell_read_line(zell_error **err);  /* without '\n'; EOF -> NULL + zell_err_eof */

/* ======================================================================
 * Tasks / scheduler
 * ====================================================================== */

typedef struct zell_task zell_task;     /* opaque refcounted object */
typedef struct zell_mutex zell_mutex;   /* opaque refcounted object */
typedef struct zell_chan zell_chan;     /* opaque refcounted object */

typedef void (*zell_resume_fn)(void *frame);
typedef void (*zell_task_fn)(zell_task *task, void *ctx);

void    zell_sched_post(zell_resume_fn fn, void *arg);  /* run fn(arg) on a worker */
int64_t zell_sched_workers(void);
bool    zell_sched_is_worker(void);
void    zell_coro_resume(void *frame);   /* llvm.coro.resume for switched-resume frames */

zell_task *zell_task_new(int64_t result_size, const zell_value_ops *result_ops);
zell_task *zell_task_spawn(zell_task_fn fn, void *ctx);
zell_task *zell_task_spawn_ex(zell_task_fn fn, void *ctx, int64_t result_size,
                                const zell_value_ops *result_ops);
void  zell_task_complete(zell_task *task, const void *result);
bool  zell_task_is_done(const zell_task *task);
bool  zell_task_add_continuation(zell_task *task, zell_resume_fn fn, void *frame);
void *zell_task_result(const zell_task *task);  /* [borrowed] slot; panics if pending */
void *zell_task_await(zell_task *task);         /* blocking; [borrowed] slot */
/* Task panics (ABI.md 8.1, 10.3). A panicked task is done with a zero slot:
 * zell_task_result / zell_task_await re-panic in the reader ("panic in
 * awaited task: ..."); the _err forms (a Task<T!>) store the task.Panic
 * error at slot + err_off instead (once) and return the slot. */
void *zell_task_result_err(zell_task *task, int64_t err_off);
void *zell_task_await_err(zell_task *task, int64_t err_off);
/* The entry's await of an async main: a panic ends the process (exit 2). */
void *zell_task_await_main(zell_task *task);
/* The entry of an async main: runs ramp(res) as a job on a worker (no
 * current task) and blocks until it returned; `res` then holds main's
 * task or, if it never suspended, its result (ABI.md 11). */
typedef void (*zell_main_ramp_fn)(void *res);
void zell_task_run_main(zell_main_ramp_fn ramp, void *res);
/* The destroy path of an unwound coroutine frame: completes `own` as
 * panicked and drops the frame's reference, or (own NULL, never
 * suspended) restores the caller's context ctx0 like zell_coro_leave. */
void zell_coro_unwound(zell_task *own, zell_object *ctx0);
/* The running coroutine frames of this thread, innermost first: nodes
 * { prev, hdl } in the frames (hdl NULL = no destroy), and the nodes of
 * synchronous frames { prev, ZELL_SYNC_FRAME, jmpbuf } (ABI.md 8.1). */
extern _Thread_local void *zell_coro_chain;
#define ZELL_SYNC_FRAME ((void *)1)
#define ZELL_SYNC_CLEANUP ((void *)2)
/* The end of a synchronous frame's landing block (ABI.md 8.1): goes on
 * unwinding the panic with the next frame on the chain. */
ZELL_NORETURN void zell_unwind_resume(void);
/* This thread's task state, read and written inline by compiled ramps
 * (ABI.md 10.3, the inlined zell_coro_enter / zell_coro_leave): the
 * current context (owned, 10.8), the current task (owned, 10.11), and the
 * orphan cancel-hook link number and recording flag (10.11). Layout fixed:
 * ctx +0, cur +8, orphan_seq +16, orphan_on +24 (int32). */
typedef struct zell_coro_tls_t {
    zell_object *ctx;
    zell_task *cur;
    int64_t orphan_seq;
    int32_t orphan_on;
    int32_t _pad;
} zell_coro_tls_t;
extern _Thread_local zell_coro_tls_t zell_coro_tls;
/* Frame-linked await (ABI.md 10.3): a directly awaited coroutine that
 * suspends with its awaiter's handle in the first word of its result slot
 * `{ ptr word; T value }` makes no task; it leaves ZELL_LINK_LINKED in the
 * word, and once it ends the word holds ZELL_LINK_LINKED (returned, value
 * moved into the slot) or the task.Panic error (+1) of its panic. */
#define ZELL_LINK_LINKED 1
/* The awaiter a linked callee hands over when it ends: run_job resumes
 * it (and whatever it hands over in turn) before the job ends. */
extern _Thread_local void *zell_coro_next;
/* A linked coroutine (no task of its own) that was resumed registers
 * `frame` on `task`: the continuation runs under the current task and
 * context, which stay current here. Returns false when `task` is done. */
bool zell_task_add_continuation_linked(zell_task *task, zell_resume_fn fn, void *frame);
/* A linked coroutine's first suspension on `task`: its registration is
 * deferred (one per thread) with the current context; ctx0 (+1, consumed)
 * is current again. zell_coro_register publishes it from the frame that
 * suspends last on this stack (`own`: that frame has a task of its own,
 * which stops being current), posting the resumption if `task` is done. */
void zell_coro_defer(zell_task *task, void *frame, zell_object *ctx0);
void zell_coro_register(bool own);
/* The destroy path of a linked frame: the panic, as a task.Panic error,
 * goes into the awaiter's slot word, ctx0 is current again and the
 * awaiter is handed over (zell_coro_next). */
void zell_coro_unwound_linked(void *word, void *awaiter, zell_object *ctx0);
/* An awaiter that found the task.Panic error `e` (consumed) in the word:
 * err_off < 0 (a T result) re-panics "panic in awaited task: ...";
 * otherwise (T!) value gets `size` zero bytes and `e` at err_off. */
void zell_coro_link_panic(zell_error *e, void *value, int64_t size, int64_t err_off);
/* The program's line table (ABI.md 8.2), registered by the constructor of
 * the line-table object the compiler links into every program. */
void zell_lines_register(const void *table, int64_t len);
zell_task *zell_task_delay(int64_t ms);
zell_task *zell_task_all(zell_task *const *tasks, int64_t n);
/* task.All with cancellation (ABI.md 10.11): 8-byte result = index of the first input that failed, -1 if none. fallible[i] != 0: input i is a Task<T!>; fallible may be NULL. +1 */
zell_task *zell_task_all_cancel(zell_task *const *tasks, int64_t n, const uint8_t *fallible);
/* Same with one flag for every input (the slice forms). */
zell_task *zell_task_all_cancel_uniform(zell_task *const *tasks, int64_t n, bool fallible);
zell_task *zell_task_any(zell_task *const *tasks, int64_t n); /* result: int64_t index */

zell_mutex *zell_mutex_new(void);
zell_task  *zell_mutex_lock_async(zell_mutex *m);  /* completes when acquired */
void zell_mutex_lock(zell_mutex *m);                /* blocking */
bool zell_mutex_try_lock(zell_mutex *m);
void zell_mutex_unlock(zell_mutex *m);

zell_chan  *zell_chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity);
zell_task  *zell_chan_send_async(zell_chan *ch, const void *elem);  /* void result */
zell_task  *zell_chan_recv_async(zell_chan *ch); /* result: {int64 ok; elem @ +8} */
void zell_chan_send(zell_chan *ch, const void *elem);          /* blocking */
bool zell_chan_recv(zell_chan *ch, void *out);                 /* blocking; out gets owned copy */
void zell_chan_close(zell_chan *ch);
int64_t zell_chan_len(const zell_chan *ch);

/* Completes `task` unless it is already done; returns whether it did. */
bool zell_task_try_complete(zell_task *task, const void *result);

/* Task context (ABI.md 10.8): an opaque object per running task, inherited
 * by continuations, spawned jobs and tasks started from it. */
zell_object *zell_task_get_ctx(void);          /* +1 or NULL */
void zell_task_set_ctx(zell_object *ctx);      /* borrowed; retained */

/* A task for callback-style producers; unlike zell_task_new it does not
 * become the current task (ABI.md 10.11). */
zell_task *zell_task_new_plain(int64_t result_size, const zell_value_ops *result_ops);
/* The immortal completed Task<void> (ABI.md 10.2): retain/release no-ops. */
zell_task *zell_task_done(void);
/* The object zell_task_done() returns: compiled code uses its address on
 * the inline fast path of Mutex.Lock (ABI.md 10.6). */
extern struct zell_task zell_task_done_task;

/* Synchronous fast path of async functions (ABI.md 10.3): the coroutine's
 * task, made at its first suspension (`task` if it exists already; returns
 * it with the frame's and the caller's reference; `ctx0`, +1, is the
 * context to restore; the cancel hooks linked on the current task after
 * `mark` move to it); the return of a coroutine that made no task
 * (`ctx0`, +1 consumed, is current again); a done plain task holding a
 * moved result, +1. */
zell_task *zell_task_lazy(zell_task *task, int64_t result_size, const zell_value_ops *result_ops, zell_object *ctx0, int64_t mark);
/* The ramp's first call: the current context (+1) and the mark for
 * zell_task_lazy (the current task's cancel-hook link number). */
zell_object *zell_coro_enter(int64_t *mark);
void zell_coro_leave(zell_object *ctx0);
zell_task *zell_task_completed(int64_t result_size, const zell_value_ops *result_ops, const void *result);

/* Cancellation (ABI.md 10.11). */
void zell_task_cancel(zell_task *task);               /* cascades to descendants */
bool zell_task_is_cancelled(const zell_task *task);   /* NULL -> false */
bool zell_task_cur_cancelled(void);                    /* the current task */
void zell_task_set_deadline(zell_task *task, int64_t ms);  /* cancel after ms */
zell_task *zell_task_current(void);                   /* +1 or NULL */
zell_task *zell_task_new_scope(void);                 /* cancellation node under the current task, +1 */
zell_task *zell_task_enter_scope(zell_task *scope);  /* returns the previous current (+1) */
void zell_task_leave_scope(zell_task *prev);          /* borrowed */
zell_error *zell_task_err_cancelled(void);            /* task.ErrCancelled, immortal */
zell_error *zell_task_err_panic(void);                /* task.ErrPanic, immortal */
zell_string *zell_task_panic_message(const zell_error *err);  /* +1, "" if not a panic */
zell_string *zell_task_panic_trace(const zell_error *err);    /* +1, "" if not a panic */
int64_t zell_task_next_key(void);

/* task primitives used by the Zell-source parts of task. */
zell_task *zell_task_rt_signal(void);          /* pending Task<void>, +1; fired when the current task is cancelled */
zell_task *zell_task_rt_waker(void);           /* the same, never fired by cancellation */
void zell_task_rt_fire(zell_task *signal);     /* complete (idempotent), cancels a delay */
void zell_task_spin_lock(int64_t *slot);        /* 0 = free, 1 = held */
void zell_task_spin_unlock(int64_t *slot);
zell_task *zell_task_yield(void);              /* completes from a fresh job, +1 */

/* task.Channel<T> (task_sync.c, ABI.md 10.7): the handle is the channel
 * object's address, owned (+1) by the Zell chanRef box until
 * zell_task_chan_release. The receive tasks' results have the compiled
 * layouts of `T!` (recv) and `(T, bool)` (next): their sizes, ops and the
 * offsets of the error / ok word are given at creation. */
int64_t zell_task_chan_new(int64_t capacity, int64_t elem_size, const zell_value_ops *elem_ops,
                           int64_t fal_size, const zell_value_ops *fal_ops, int64_t fal_err_off,
                           int64_t tup_size, const zell_value_ops *tup_ops, int64_t tup_ok_off);
void zell_task_chan_release(int64_t h);
zell_task *zell_task_chan_send(int64_t h, const void *elem);  /* elem borrowed; Task<void>, +1 */
zell_task *zell_task_chan_recv(int64_t h);                    /* Task<T!>, +1 */
zell_task *zell_task_chan_next(int64_t h);                    /* Task<(T, bool)>, +1 */
void zell_task_chan_close(int64_t h);
int64_t zell_task_chan_len(int64_t h);
int64_t zell_task_chan_cap(int64_t h);
bool zell_task_chan_is_closed(int64_t h);
/* select (10.7 "Select"): one wait on several channel operations. A case
 * with chan == 0 is off. Receive: the value is moved to *elem and *ok (may
 * be NULL) is 1, or 0 with the zero value when the channel is closed and
 * drained. Send: *elem is copied and retained (the caller keeps its own).
 * Task<int>, +1: the index of the case that fired, -1 with has_default
 * when none is ready or when the waiting task is cancelled. */
typedef struct zell_select_case { int64_t chan; int64_t send; void *elem; uint8_t *ok; } zell_select_case;
zell_task *zell_task_select(const zell_select_case *cases, int64_t n, int64_t has_default);
/* task.RecvAny: the first of n channels (handles) to deliver; the result
 * (res_size bytes) is (index int @0, value T @val_off, ok bool @ok_off),
 * index -1 for no channels or a cancelled wait. +1. */
zell_task *zell_task_chan_recv_any(const int64_t *hs, int64_t n, int64_t res_size, int64_t val_off, int64_t ok_off);

/* task.Mutex (task_sync.c, ABI.md 10.6): m points at the struct's three
 * int words { state, waitHead, waitTail }, all zero when unlocked. */
zell_task *zell_task_mutex_lock(int64_t *m);    /* Task<void> (done at once if free), +1 */
bool zell_task_mutex_try_lock(int64_t *m);
void zell_task_mutex_unlock(int64_t *m);        /* panics `unlock of unlocked mutex` */

/* Readiness waits (task_io.c, ABI.md 10.9). Result: int64_t
 * 0 = ready, 1 = timed out, 2 = the fd was released with zell_io_forget,
 * 3 = the waiting task was cancelled. */
zell_task *zell_io_wait_readable(int64_t fd, int64_t timeout_ms);  /* +1; timeout < 0 = none */
zell_task *zell_io_wait_writable(int64_t fd, int64_t timeout_ms);
void zell_io_forget(int64_t fd);   /* call before close(fd): wakes waiters with 2 */
void zell_io_register(int64_t fd); /* optional: assigns fd to a poller shard now (sockets do at creation) */

/* ======================================================================
 * Entry / process
 * ====================================================================== */

void zell_runtime_init(int argc, char **argv);
void zell_runtime_shutdown(void);
/* Waits for all queued and running jobs and stops the scheduler (it restarts
 * lazily if used again); zell_runtime_shutdown does the same first. The
 * compiler calls it before releasing the program's globals. */
void zell_runtime_drain(void);
void zell_os_args(zell_slice *out);   /* owned []string */
ZELL_NORETURN void zell_exit(int32_t code);

/* ======================================================================
 * Standard library (std.*) - symbols named by compiler/src/sema/stdlib.rs.
 * Mapping: string -> zell_string* (NULL == ""), int -> int64_t,
 * float -> double, bool -> bool, []T param -> const zell_slice*,
 * []T result -> trailing zell_slice *out (owned), T! -> T returned plus a
 * trailing zell_error **err (NULL on success, owned error on failure),
 * void! -> void with a trailing zell_error **err. Arguments are borrowed,
 * returned objects are owned (+1). See ABI.md section 14.
 * ====================================================================== */

/* strings ([]string: elem_size 8, ops &zell_ops_ref) */
void          zell_strings_split(const zell_string *s, const zell_string *sep, zell_slice *out);
zell_string *zell_strings_join(const zell_slice *parts, const zell_string *sep);
zell_string *zell_strings_trim(const zell_string *s);   /* Go strings.TrimSpace */
bool          zell_strings_contains(const zell_string *s, const zell_string *substr);
zell_string *zell_strings_replace(const zell_string *s, const zell_string *old,
                                    const zell_string *new_);  /* all occurrences */
zell_string *zell_strings_to_upper(const zell_string *s);
zell_string *zell_strings_to_lower(const zell_string *s);
bool          zell_strings_has_prefix(const zell_string *s, const zell_string *prefix);
bool          zell_strings_has_suffix(const zell_string *s, const zell_string *suffix);
int64_t       zell_strings_index(const zell_string *s, const zell_string *substr);
zell_string *zell_strings_repeat(const zell_string *s, int64_t count); /* count < 0 panics */
int64_t       zell_strings_to_int(const zell_string *s, zell_error **err);
double        zell_strings_to_float(const zell_string *s, zell_error **err);

/* math */
double zell_math_sqrt(double x);
double zell_math_abs(double x);
double zell_math_pow(double x, double y);
double zell_math_floor(double x);
double zell_math_ceil(double x);
double zell_math_min(double x, double y);
double zell_math_max(double x, double y);
double zell_math_sin(double x);
double zell_math_cos(double x);
double zell_math_tan(double x);
double zell_math_log(double x);
double zell_math_exp(double x);

/* fmt: args are the already-formatted []string */
zell_string *zell_fmt_format(const zell_string *tmpl, const zell_slice *args);

/* io */
zell_string *zell_io_read_file(const zell_string *path, zell_error **err);
void          zell_io_write_file(const zell_string *path, const zell_string *data,
                                  zell_error **err);
zell_string *zell_io_read_line(zell_error **err);  /* EOF: NULL + zell_err_eof */

/* errors (New -> zell_error_new, As -> zell_error_as) */
zell_error *zell_errors_wrap(const zell_error *err, const zell_string *msg); /* NULL err -> NULL */
zell_error *zell_errors_unwrap(const zell_error *err);  /* owned cause or NULL */
bool         zell_errors_is(const zell_error *err, const zell_error *target);
zell_string *zell_errors_trace(const zell_error *err);             /* errors.Trace */
void         zell_errors_frames(const zell_error *err, zell_slice *out); /* errors.Frames */

/* task.Delay is zell_task_delay (declared above). */

/* strings (more; std_strings_ext.c). []char: elem_size 4 (int32_t code
 * points), []byte: elem_size 1, both with NULL ops. */
void          zell_strings_fields(const zell_string *s, zell_slice *out);
int64_t       zell_strings_count(const zell_string *s, const zell_string *substr);
int64_t       zell_strings_last_index(const zell_string *s, const zell_string *substr);
zell_string *zell_strings_trim_prefix(const zell_string *s, const zell_string *prefix);
zell_string *zell_strings_trim_suffix(const zell_string *s, const zell_string *suffix);
zell_string *zell_strings_trim_left(const zell_string *s, const zell_string *cutset);
zell_string *zell_strings_trim_right(const zell_string *s, const zell_string *cutset);
void          zell_strings_split_n(const zell_string *s, const zell_string *sep, int64_t n,
                                    zell_slice *out);
bool          zell_strings_equal_fold(const zell_string *a, const zell_string *b);
zell_string *zell_strings_reverse(const zell_string *s);   /* by code point */
zell_string *zell_strings_pad_left(const zell_string *s, int64_t width, const zell_string *pad);
zell_string *zell_strings_pad_right(const zell_string *s, int64_t width, const zell_string *pad);
void          zell_strings_runes(const zell_string *s, zell_slice *out);
zell_string *zell_strings_from_runes(const zell_slice *runes);
void          zell_strings_bytes(const zell_string *s, zell_slice *out);
zell_string *zell_strings_from_bytes(const zell_slice *bytes);
int64_t       zell_strings_compare(const zell_string *a, const zell_string *b);
zell_string *zell_strings_title(const zell_string *s);

/* math (more; std_math_ext.c) */
double  zell_math_round(double x);
double  zell_math_trunc(double x);
double  zell_math_mod(double x, double y);
double  zell_math_hypot(double x, double y);
double  zell_math_atan2(double y, double x);
double  zell_math_log2(double x);
double  zell_math_log10(double x);
bool    zell_math_is_nan(double x);
bool    zell_math_is_inf(double x, int64_t sign);
double  zell_math_inf(int64_t sign);
double  zell_math_nan(void);
int64_t zell_math_imin(int64_t a, int64_t b);
int64_t zell_math_imax(int64_t a, int64_t b);
int64_t zell_math_iabs(int64_t a);

/* io (more; std_io_ext.c) */
zell_string *zell_io_read_all(zell_error **err);    /* stdin to EOF */
void          zell_io_read_lines(const zell_string *path, zell_slice *out, zell_error **err);
void          zell_io_write_lines(const zell_string *path, const zell_slice *lines,
                                   zell_error **err);
void          zell_io_eprint(const zell_string *s);
void          zell_io_eprintln(const zell_string *s);

/* time (std_time.c): times are int64 Unix milliseconds, calendar in UTC */
int64_t       zell_time_now(void);
int64_t       zell_time_now_nanos(void);
void          zell_time_sleep(int64_t ms);
int64_t       zell_time_since(int64_t start_ms);
zell_string *zell_time_format(int64_t ms, const zell_string *layout);  /* strftime + %L */
int64_t       zell_time_parse(const zell_string *s, const zell_string *layout,
                               zell_error **err);                       /* strptime */
int64_t       zell_time_year(int64_t ms);
int64_t       zell_time_month(int64_t ms);    /* 1..12 */
int64_t       zell_time_day(int64_t ms);      /* 1..31 */
int64_t       zell_time_hour(int64_t ms);
int64_t       zell_time_minute(int64_t ms);
int64_t       zell_time_second(int64_t ms);
int64_t       zell_time_weekday(int64_t ms);  /* 0 = Sunday */

/* os (std_os.c; Args is zell_os_args above) */
zell_string *zell_os_getenv(const zell_string *name);   /* "" if unset */
void          zell_os_setenv(const zell_string *name, const zell_string *value,
                              zell_error **err);
void          zell_os_environ(zell_slice *out);          /* owned []string "K=V" */
ZELL_NORETURN void zell_os_exit(int64_t code);
zell_string *zell_os_hostname(zell_error **err);
int64_t       zell_os_pid(void);
zell_string *zell_os_cwd(zell_error **err);
void          zell_os_chdir(const zell_string *path, zell_error **err);

/* fs (std_fs.c) */
bool          zell_fs_exists(const zell_string *path);
bool          zell_fs_is_dir(const zell_string *path);
void          zell_fs_read_dir(const zell_string *path, zell_slice *out, zell_error **err);
void          zell_fs_mkdir(const zell_string *path, zell_error **err);
void          zell_fs_mkdir_all(const zell_string *path, zell_error **err);
void          zell_fs_remove(const zell_string *path, zell_error **err);
void          zell_fs_remove_all(const zell_string *path, zell_error **err);
void          zell_fs_rename(const zell_string *from, const zell_string *to, zell_error **err);
int64_t       zell_fs_size(const zell_string *path, zell_error **err);
int64_t       zell_fs_mod_time(const zell_string *path, zell_error **err);
void          zell_fs_copy(const zell_string *from, const zell_string *to, zell_error **err);
zell_string *zell_fs_join(const zell_string *a, const zell_string *b);
zell_string *zell_fs_base(const zell_string *path);
zell_string *zell_fs_dir(const zell_string *path);
zell_string *zell_fs_ext(const zell_string *path);
zell_string *zell_fs_abs(const zell_string *path, zell_error **err);
zell_string *zell_fs_temp_dir(void);
void          zell_fs_append_file(const zell_string *path, const zell_string *data,
                                   zell_error **err);

/* random (std_random.c): xoshiro256**, process-wide state with a mutex */
void    zell_random_seed(int64_t seed);
int64_t zell_random_int(void);              /* [0, 2^63) */
int64_t zell_random_int_n(int64_t n);       /* [0, n); n <= 0 panics */
double  zell_random_float(void);            /* [0, 1) */
void    zell_random_bytes(int64_t n, zell_slice *out);  /* n < 0 panics */
void    zell_random_shuffle(const zell_slice *s);       /* []int, in place */

/* crypto.hash / crypto.hmac / crypto.rand (std_crypto.c).
 * []byte results: elem_size 1, NULL ops. Hex output is lowercase. */
void          zell_crypto_hash_sha256(const zell_string *data, zell_slice *out);
zell_string *zell_crypto_hash_sha256_hex(const zell_string *data);
void          zell_crypto_hash_sha512(const zell_string *data, zell_slice *out);
zell_string *zell_crypto_hash_sha512_hex(const zell_string *data);
void          zell_crypto_hash_sha1(const zell_string *data, zell_slice *out);
zell_string *zell_crypto_hash_sha1_hex(const zell_string *data);
void          zell_crypto_hash_md5(const zell_string *data, zell_slice *out);
zell_string *zell_crypto_hash_md5_hex(const zell_string *data);
int64_t       zell_crypto_hash_crc32(const zell_string *data);   /* IEEE, 0..2^32-1 */
void          zell_crypto_hmac_sha256(const zell_string *key, const zell_string *data,
                                       zell_slice *out);
zell_string *zell_crypto_hmac_sha256_hex(const zell_string *key, const zell_string *data);
bool          zell_crypto_hmac_equal(const zell_slice *a, const zell_slice *b); /* constant time */
void          zell_crypto_rand_bytes(int64_t n, zell_slice *out, zell_error **err);
int64_t       zell_crypto_rand_int(zell_error **err);             /* [0, 2^63) */
zell_string *zell_crypto_rand_hex(int64_t n, zell_error **err);  /* n bytes -> 2n chars */

/* encoding.base64 / encoding.hex (std_encoding.c) */
zell_string *zell_encoding_base64_encode(const zell_slice *data);       /* padded */
zell_string *zell_encoding_base64_encode_string(const zell_string *s);
void          zell_encoding_base64_decode(const zell_string *s, zell_slice *out,
                                           zell_error **err);
zell_string *zell_encoding_base64_decode_string(const zell_string *s, zell_error **err);
zell_string *zell_encoding_base64_encode_url(const zell_slice *data);   /* unpadded */
void          zell_encoding_base64_decode_url(const zell_string *s, zell_slice *out,
                                               zell_error **err);
zell_string *zell_encoding_hex_encode(const zell_slice *data);
zell_string *zell_encoding_hex_encode_string(const zell_string *s);
void          zell_encoding_hex_decode(const zell_string *s, zell_slice *out, zell_error **err);
zell_string *zell_encoding_hex_decode_string(const zell_string *s, zell_error **err);

/* uuid (std_uuid.c) */
zell_string *zell_uuid_new(void);                  /* random v4, lowercase */
bool          zell_uuid_is_valid(const zell_string *s);

/* net internals (std_net.c, ABI.md 16): used by the Zell-source net.
 * Sockets are handles (int64_t, never 0 or -1) that own the descriptor;
 * -1 / empty + zell_net_rt_again() mean "would block". Errors are the
 * causes (net sentinels or the system text); net.zl adds the context. */
zell_error *zell_net_err_closed(void);       /* net.ErrClosed etc.: immortal sentinels */
zell_error *zell_net_err_timeout(void);
zell_error *zell_net_err_refused(void);
zell_error *zell_net_err_reset(void);
zell_error *zell_net_err_unreachable(void);
zell_error *zell_net_err_addr_in_use(void);
zell_error *zell_net_err_not_found(void);
zell_error *zell_net_err_eof(void);          /* = zell_err_eof */
zell_error *zell_net_rt_op_error(const zell_error *boxed, const zell_string *prefix, const zell_error *inner);
int64_t zell_net_rt_dial(const zell_string *ip, int64_t port, const zell_string *lip, int64_t lport, zell_error **err);
int64_t zell_net_rt_dial_unix(const zell_string *path, zell_error **err);
void    zell_net_rt_connect_result(int64_t h, zell_error **err);
int64_t zell_net_rt_listen(const zell_string *ip, int64_t port, int64_t backlog, int64_t flags, zell_error **err);
int64_t zell_net_rt_listen_unix(const zell_string *path, int64_t backlog, zell_error **err);
int64_t zell_net_rt_accept(int64_t h, zell_error **err);
void    zell_net_rt_read(int64_t h, int64_t n, zell_slice *out, zell_error **err);
int64_t zell_net_rt_read_into(int64_t h, const zell_slice *buf, int64_t off, zell_error **err);
bool    zell_net_rt_again(void);
int64_t zell_net_rt_write(int64_t h, const zell_slice *data, int64_t off, zell_error **err);
int64_t zell_net_rt_write_str(int64_t h, const zell_string *s, int64_t off, zell_error **err);
int64_t zell_net_rt_write_v(int64_t h, const zell_slice *bufs, int64_t off, zell_error **err);
void    zell_net_rt_shutdown(int64_t h, int64_t how, zell_error **err);
void    zell_net_rt_close(int64_t h);
void    zell_net_rt_free(int64_t h);
bool    zell_net_rt_is_closed(int64_t h);
int64_t zell_net_rt_fd(int64_t h);
zell_task *zell_net_rt_wait_readable(int64_t h, int64_t ms);
zell_task *zell_net_rt_wait_writable(int64_t h, int64_t ms);
zell_string *zell_net_rt_local_addr(int64_t h);
zell_string *zell_net_rt_remote_addr(int64_t h);
int64_t zell_net_rt_port(int64_t h);
void    zell_net_rt_set_opt(int64_t h, int64_t opt, int64_t value, zell_error **err);
int64_t zell_net_rt_get_opt(int64_t h, int64_t opt, zell_error **err);
void    zell_net_rt_mcast(int64_t h, bool join, const zell_string *group, const zell_string *ifname, zell_error **err);
int64_t zell_net_rt_open_file(const zell_string *path, zell_error **err);
int64_t zell_net_rt_file_size(int64_t fd);
void    zell_net_rt_close_file(int64_t fd);
int64_t zell_net_rt_send_file(int64_t h, int64_t ffd, int64_t off, int64_t n, zell_error **err);
void    zell_net_rt_parse_ip(const zell_string *s, zell_slice *out);
zell_string *zell_net_rt_format_ip(const zell_slice *b);
bool    zell_net_rt_is_ip(const zell_string *s);
int64_t zell_net_rt_if_index(const zell_string *name);
zell_string *zell_net_rt_if_name(int64_t index);
void    zell_net_rt_interfaces(zell_slice *out, zell_error **err);
int64_t zell_net_rt_lookup_port(const zell_string *network, const zell_string *service);
zell_task *zell_net_rt_resolve(const zell_string *host, int64_t flags);   /* result: handle */
zell_task *zell_net_rt_reverse(const zell_string *ip);
void    zell_net_rt_resolve_take(int64_t handle, zell_slice *out, zell_error **err);
void    zell_net_rt_resolve_sync(const zell_string *host, int64_t flags, zell_slice *out, zell_error **err);
int64_t zell_net_rt_udp(const zell_string *ip, int64_t port, int64_t flags, zell_error **err);
int64_t zell_net_rt_udp_dial(const zell_string *ip, int64_t port, const zell_string *lip, int64_t lport, zell_error **err);
void    zell_net_rt_recv_from(int64_t h, int64_t n, zell_slice *out, zell_error **err);
int64_t zell_net_rt_read_from(int64_t h, const zell_slice *buf, zell_error **err);
zell_string *zell_net_rt_last_from(void);
int64_t zell_net_rt_last_from_port(void);
bool    zell_net_rt_last_trunc(void);
int64_t zell_net_rt_send_to(int64_t h, const zell_slice *data, const zell_string *ip, int64_t port,
                             zell_error **err);
zell_task *zell_net_rt_wait_any_writable(const zell_slice *hs, int64_t ms);
int64_t zell_net_rt_connect_check(int64_t h, zell_error **err);
void    zell_net_rt_bytes(int64_t n, zell_slice *out);
int64_t zell_net_rt_copy_bytes(const zell_slice *dst, int64_t doff, const zell_slice *src, int64_t soff, int64_t n);
int64_t zell_net_rt_index_byte(const zell_slice *b, int64_t from, int64_t c);
void    zell_net_rt_join(const zell_slice *a, const zell_slice *b, int64_t bn, zell_slice *out);

/* ---- reflection tables (ABI.md section 17, std_reflect.c) ----------------
 * Only programs that use reflect have them; then every zell_type_info
 * a compiled program emits is the first part of a zell_reflect_type, and
 * every table object is an immortal heap object (header refcount -1), so
 * the payload pointers below are valid Zell pointers. Slices of tables
 * have a NULL backing. Layouts are reflect.TypeInfo etc. in field
 * order with natural alignment (checked by _Static_assert in std_reflect.c). */
typedef struct zell_reflect_type zell_reflect_type;

typedef enum zell_reflect_kind {
    ZELL_KIND_INVALID, ZELL_KIND_VOID, ZELL_KIND_BOOL, ZELL_KIND_INT, ZELL_KIND_INT8,
    ZELL_KIND_INT16, ZELL_KIND_INT32, ZELL_KIND_UINT, ZELL_KIND_UINT8, ZELL_KIND_UINT16,
    ZELL_KIND_UINT32, ZELL_KIND_FLOAT, ZELL_KIND_FLOAT32, ZELL_KIND_CHAR, ZELL_KIND_STRING,
    ZELL_KIND_ERROR, ZELL_KIND_ANY, ZELL_KIND_INTERFACE, ZELL_KIND_STRUCT, ZELL_KIND_ENUM,
    ZELL_KIND_POINTER, ZELL_KIND_WEAK, ZELL_KIND_SLICE, ZELL_KIND_ARRAY, ZELL_KIND_MAP,
    ZELL_KIND_TUPLE, ZELL_KIND_FUNC, ZELL_KIND_FALLIBLE, ZELL_KIND_TASK,
    ZELL_KIND_COUNT
} zell_reflect_kind;

typedef struct zell_reflect_any {      /* a Zell `any`: 16 bytes */
    void *data;                         /* +0  payload of the box */
    const zell_reflect_type *type;     /* +8  dynamic type, NULL = nil */
} zell_reflect_any;

typedef struct zell_reflect_field {    /* reflect.FieldInfo, 72 bytes */
    zell_string *name;                 /* +0 */
    const zell_reflect_type *type;     /* +8 */
    int64_t offset;                     /* +16 byte offset in the struct (enum value for variants) */
    int64_t index;                      /* +24 */
    bool embedded;                      /* +32 */
    bool exported;                      /* +33 */
    zell_slice attrs;                  /* +40 []any, each holding a *A */
} zell_reflect_field;

typedef struct zell_reflect_param {    /* reflect.ParamInfo, 56 bytes */
    zell_string *name;                 /* +0 */
    const zell_reflect_type *type;     /* +8 */
    int64_t index;                      /* +16 (receiver excluded) */
    zell_slice attrs;                  /* +24 []any */
} zell_reflect_param;

typedef struct zell_reflect_method {   /* reflect.MethodInfo, 104 bytes */
    zell_string *name;                 /* +0 */
    const zell_reflect_type *type;     /* +8  func(*S, params) ret (no receiver if static) */
    bool is_static;                     /* +16 */
    void *invoke;                       /* +24 packed thunk: {any, error*} (any recv, []any args) */
    void *fnval;                        /* +32 closure-convention thunk fn(env, recv, args...) */
    zell_slice attrs;                  /* +40 */
    zell_slice params;                 /* +72 []*ParamInfo (receiver excluded) */
} zell_reflect_method;

typedef struct zell_reflect_variant {  /* reflect.VariantInfo, 80 bytes */
    zell_string *name;                 /* +0 */
    int64_t index;                      /* +8 */
    zell_slice fields;                 /* +16 []*FieldInfo */
    zell_slice attrs;                  /* +48 */
} zell_reflect_variant;

struct zell_reflect_type {             /* reflect.TypeInfo, 240 bytes */
    zell_type_info rt;                 /* +0  name, size (16 + sizeof T), drop */
    int32_t kind;                       /* +24 zell_reflect_kind */
    zell_string *name;                 /* +32 short name: User, *User, []int */
    zell_string *full_name;            /* +40 app.models.User */
    int64_t size;                       /* +48 sizeof T */
    int64_t align;                      /* +56 */
    zell_slice fields;                 /* +64  []*FieldInfo (structs) */
    zell_slice methods;                /* +96  []*MethodInfo (full types only) */
    zell_slice variants;               /* +128 []*VariantInfo (enums) */
    const zell_reflect_type *elem;     /* +160 pointer/slice/array/weak/T!/Task element, map value */
    const zell_reflect_type *key;      /* +168 map key */
    int64_t len;                        /* +176 array length */
    zell_slice attrs;                  /* +184 []any */
    const zell_reflect_type *ptr_to;   /* +216 *T (full types only; TypeInfo.New) */
    const zell_reflect_type *canon;    /* +224 for a spelling alias (int64): the int table */
    zell_string *package;              /* +232 namespace path of the declaration (NULL = "") */
};

/* The table of a box (a heap object made by compiled code), or NULL. */
const zell_reflect_type *zell_reflect_type_of(const void *obj);
/* "Struct", "Int", ... (the reflect.Kind variant name). */
const char *zell_reflect_kind_name(int32_t kind);
/* Field / method by name ([borrowed] table entries), NULL if absent. */
const zell_reflect_field *zell_reflect_field_by_name(const zell_reflect_type *t, const char *name);
const zell_reflect_method *zell_reflect_method_by_name(const zell_reflect_type *t, const char *name);
/* The canonical table (follows `canon`); NULL for NULL. */
const zell_reflect_type *zell_reflect_canon(const zell_reflect_type *t);

/* encoding.json (std_json.c, ABI.md 17.4): a JSON text parsed into an
 * arena document of nodes (handles are addresses passed as int64_t; node
 * 0 is null), a growing text writer, and a quoted JSON string literal. */
int64_t       zell_encoding_json_rt_parse(const zell_string *text, zell_error **err); /* document, or 0 + error */
void          zell_encoding_json_rt_free(int64_t doc);
int64_t       zell_encoding_json_rt_root(int64_t doc);                 /* root node */
int64_t       zell_encoding_json_rt_kind(int64_t node);                /* 0 null 1 bool 2 number 3 string 4 array 5 object */
bool          zell_encoding_json_rt_is_int(int64_t node);              /* number written as an int64 */
bool          zell_encoding_json_rt_bool(int64_t node);
int64_t       zell_encoding_json_rt_int(int64_t node);
double        zell_encoding_json_rt_float(int64_t node);
zell_string *zell_encoding_json_rt_str(int64_t node);                 /* +1 copy (NULL for "") */
int64_t       zell_encoding_json_rt_len(int64_t node);                 /* elements / pairs; 0 for scalars */
int64_t       zell_encoding_json_rt_first(int64_t node);               /* first element / first key, 0 if none */
int64_t       zell_encoding_json_rt_next(int64_t node);                /* next sibling (after a key: its value) */
int64_t       zell_encoding_json_rt_get(int64_t obj, const zell_string *key); /* member value, 0 if absent */
int64_t       zell_encoding_json_rt_get_fold(int64_t obj, const zell_string *key); /* the same, ASCII case-insensitive */
int64_t       zell_encoding_json_rt_index(int64_t doc, int64_t arr, int64_t i); /* element i, 0 if out of range */
int64_t       zell_encoding_json_rt_w_new(void);                        /* writer */
zell_string *zell_encoding_json_rt_w_finish(int64_t w);               /* +1 the text; frees the writer */
void          zell_encoding_json_rt_w_raw(int64_t w, const zell_string *s);
void          zell_encoding_json_rt_w_key(int64_t w, const zell_string *name, bool first);
void          zell_encoding_json_rt_w_str(int64_t w, const zell_string *s);
void          zell_encoding_json_rt_w_int(int64_t w, int64_t v);
void          zell_encoding_json_rt_w_uint(int64_t w, uint64_t v);
void          zell_encoding_json_rt_w_float(int64_t w, double f);
void          zell_encoding_json_rt_w_bool(int64_t w, bool b);
void          zell_encoding_json_rt_w_node(int64_t w, int64_t node);
void          zell_encoding_json_rt_w_key_int(int64_t w, const zell_string *name, bool first, int64_t v);   /* key + value */
void          zell_encoding_json_rt_w_key_str(int64_t w, const zell_string *name, bool first, const zell_string *s);
void          zell_encoding_json_rt_w_key_float(int64_t w, const zell_string *name, bool first, double f);
void          zell_encoding_json_rt_w_key_bool(int64_t w, const zell_string *name, bool first, bool b);
void          zell_encoding_json_rt_w_strs(int64_t w, const zell_slice *strs);  /* ["a","b"] of a []string */
zell_string *zell_encoding_json_quote(const zell_string *s);
zell_string *zell_encoding_json_rt_indent(const zell_string *src, const zell_string *prefix, const zell_string *indent); /* +1 */

/* diag / log runtime halves (diag.c, profile.c; ABI.md section 18). */
zell_object *zell_diag_rt_slots(void);                  /* map[string]any, +1 or NULL */
void          zell_diag_rt_set_slots(zell_object *m);
void          zell_diag_rt_lock(void);
void          zell_diag_rt_unlock(void);
int64_t       zell_diag_rt_live_objects(void);
int64_t       zell_diag_rt_allocations(void);
int64_t       zell_diag_rt_live_tasks(void);
int64_t       zell_diag_rt_queue_len(void);
int64_t       zell_diag_rt_workers(void);
int64_t       zell_diag_rt_mono_nanos(void);
int64_t       zell_diag_rt_heap_in_use(void);              /* object heap bytes (heap.c, ABI.md 3.2) */
int64_t       zell_diag_rt_heap_retained(void);
int64_t       zell_diag_rt_heap_released(void);
void          zell_runtime_free_os_memory(void);           /* runtime.FreeOSMemory */
/* testing.Panics: runs the closure on a thread in a panic capture
 * scope; 1 panicked, 0 returned, -1 unsupported (std_testing.c); the
 * message of the last caught panic of the calling thread (+1). */
int64_t       zell_testing_rt_capture(void *fn, void *env);
zell_string *zell_testing_rt_capture_message(void);
void          zell_diag_rt_start_cpu_profile(const zell_string *path, int64_t hz, zell_error **err);
int64_t       zell_diag_rt_stop_cpu_profile(zell_error **err);
zell_string *zell_diag_rt_heap_text(void);
void          zell_diag_rt_write_heap_profile(const zell_string *path, zell_error **err);
int64_t       zell_log_rt_open(const zell_string *path, zell_error **err);
void          zell_log_rt_write(int64_t fd, const zell_string *s, zell_error **err);
void          zell_log_rt_close(int64_t fd);
bool          zell_log_rt_isatty(int64_t fd);

/* ======================================================================
 * Debug agent (debug.c, std_unsafe.c, std_thread.c, std_plugin.c;
 * ABI.md section 22). The tables and frame records are described there;
 * compiled code passes them as opaque pointers.
 * ====================================================================== */

struct zell_dbg_frame;
struct zell_dbg_func;
/* Hooks emitted by `--debug-agent` builds, each behind `if (on)`, where
 * `on` is the hidden uint8_t `zell_dbg_on` loaded at function entry. */
void zell_dbg_enter(struct zell_dbg_frame *f, const struct zell_dbg_func *func, void *const *locals, int32_t entry_stmt);
void zell_dbg_stmt(struct zell_dbg_frame *f, int32_t stmt);
void zell_dbg_leave(struct zell_dbg_frame *f);
void zell_dbg_leave_linked(struct zell_dbg_frame *f);  /* a frame-linked coroutine (ABI.md 10.3) */
void zell_dbg_suspend(struct zell_dbg_frame *f);
void zell_dbg_resume(struct zell_dbg_frame *f);
/* C main: after zell_runtime_init (starts the agent when ZELL_DEBUG is
 * set) and before returning (reports the exit to the client). */
void zell_dbg_boot(const void *tables, void (*entry)(void));
void zell_dbg_exit(int32_t code);

/* debug internals (rt*). Closures are passed as their two words. */
int64_t       zell_debug_rt_tables(void);
void          zell_debug_rt_set_hook(void *fn, void *env);
void          zell_debug_rt_set_exit_hook(void *fn, void *env);
void          zell_debug_rt_set_bp(int64_t stmt, bool on);
void          zell_debug_rt_on_error(bool on);
void          zell_debug_rt_stop_next(void);
void          zell_debug_rt_freeze(bool on);
bool          zell_debug_rt_resume(int64_t task, int64_t mode, bool single);
void          zell_debug_rt_release_all(void);
void          zell_debug_rt_tasks(zell_slice *out);
int64_t       zell_debug_rt_task_state(int64_t task);
int64_t       zell_debug_rt_task_frame(int64_t task);
int64_t       zell_debug_rt_task_func(int64_t task);
int64_t       zell_debug_rt_task_reason(int64_t task);
void          zell_debug_rt_lock(int64_t n);
void          zell_debug_rt_unlock(int64_t n);
void          zell_debug_rt_wait_ready(void);
void          zell_debug_rt_signal_ready(void);
void          zell_debug_rt_ignore_hooks(void);
zell_string *zell_debug_rt_panic_text(void);
int64_t       zell_debug_rt_cur_error(void);
zell_string *zell_debug_rt_error_text(int64_t addr);
zell_string *zell_debug_rt_type_name(int64_t addr);
int64_t       zell_debug_rt_await_int(zell_task *t);
bool          zell_debug_rt_task_done(int64_t addr);
int64_t       zell_debug_rt_task_slot(int64_t addr);
zell_string *zell_debug_rt_sym_name(int64_t addr);
ZELL_NORETURN void zell_debug_rt_exit(int64_t code);
int64_t       zell_debug_rt_now_ms(void);
zell_string *zell_debug_rt_string(int64_t addr);
int64_t       zell_debug_rt_map_len(int64_t m);
void          zell_debug_rt_map_entries(int64_t m, int64_t start, int64_t n, zell_slice *out);
zell_string *zell_debug_rt_error_type(int64_t addr);
int64_t       zell_debug_rt_listen(const zell_string *host, int64_t port, zell_error **err);
int64_t       zell_debug_rt_sock_port(int64_t fd);
int64_t       zell_debug_rt_accept(int64_t fd);
void          zell_debug_rt_recv(int64_t fd, int64_t max, zell_slice *out);
bool          zell_debug_rt_send(int64_t fd, const zell_string *s);
void          zell_debug_rt_close(int64_t fd);
void          zell_debug_rt_eprint(const zell_string *s);

/* unsafe (capability `unsafe`). */
int64_t       zell_unsafe_read_int(int64_t addr, int64_t size);
int64_t       zell_unsafe_read_uint(int64_t addr, int64_t size);
double        zell_unsafe_read_float(int64_t addr, int64_t size);
void          zell_unsafe_write_int(int64_t addr, int64_t size, int64_t v);
void          zell_unsafe_read_bytes(int64_t addr, int64_t n, zell_slice *out);
void          zell_unsafe_write_bytes(int64_t addr, const zell_slice *b);
zell_string *zell_unsafe_c_string(int64_t addr);
int64_t       zell_unsafe_addr_of(void *data, void *type);
int64_t       zell_unsafe_type_of(void *data, void *type);
int64_t       zell_unsafe_call_func(int64_t addr, const zell_slice *args);

/* thread. */
int64_t       zell_thread_spawn(void *fn, void *env);
void          zell_thread_join(int64_t h);
void          zell_thread_sleep(int64_t ms);

/* plugin (capability `plugin`). */
int64_t       zell_plugin_rt_open(const zell_string *path, zell_error **err);
int64_t       zell_plugin_rt_sym(int64_t h, const zell_string *name, zell_error **err);
void          zell_plugin_rt_close(int64_t h);

#ifdef __cplusplus
}
#endif

#endif /* ZELL_RT_H */
