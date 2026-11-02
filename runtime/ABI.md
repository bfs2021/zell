# Zell runtime ABI, v0.1

This document is the contract between the Zell compiler (Rust + LLVM, which emits
LLVM IR) and the C runtime library `libzell_rt.a`. Everything listed here is
stable for 0.1. Anything not listed (files under `src/`, symbols starting with
`zell__`, opaque struct contents) is private and may change.

The public header is `runtime/include/zell_rt.h`. If this document and the
header disagree, that is a bug; report it.

Contents

1. [Building and linking](#1-building-and-linking)
2. [General conventions](#2-general-conventions)
3. [Memory and reference counting](#3-memory-and-reference-counting)
4. [Strings](#4-strings)
5. [Slices](#5-slices)
6. [Maps](#6-maps)
7. [Errors](#7-errors)
8. [Panics](#8-panics)
9. [I/O](#9-io)
10. [Tasks, async/await and the scheduler](#10-tasks-asyncawait-and-the-scheduler)
11. [Program entry](#11-program-entry)
12. [Environment variables](#12-environment-variables)
13. [Known limits in v0.1](#13-known-limits-in-v01)
14. [Standard library](#14-standard-library)
15. [Platform layer](#15-platform-layer)
16. [Networking: net and net.http](#16-networking-stdnet-and-stdnethttp)
17. [Reflection tables and encoding.json](#17-reflection-tables-and-stdencodingjson)
19. [Cryptography and TLS](#19-cryptography-and-tls)

---

## 1. Building and linking

```
make            # -> runtime/libzell_rt.a (also runtime/build/libzell_rt.a)
make test       # C unit tests + a real llvm.coro example (tests/coro_lowering.ll)
make clean
make asan       # clean rebuild + all tests under AddressSanitizer/LeakSanitizer
```

`CFLAGS_EXTRA=...` is appended to the compile flags (and to the link of the
tests), e.g. `make clean && make CFLAGS_EXTRA=-fsanitize=address test`. Prefer
`make asan`: it also installs `tests/lsan.supp` (immortal literals and sentinels
are never freed by design) and works around a fork-under-ASan deadlock in
`test_task`.

Link a Zell program with:

```
clang prog.o runtime/libzell_rt.a -lpthread -lm -rdynamic -o prog
```

`-rdynamic` is optional. It only makes function names appear in panic backtraces.
`-lm` is required since the `math` functions (section 14) call libm.

Target: x86-64 / AArch64 Linux, LP64, glibc (the panic backtrace uses
`<execinfo.h>`). The library is built with `-fPIC`, so you can also link it into
shared objects.

---

## 2. General conventions

### 2.1 Type mapping

| C type in header | LLVM IR type | Notes |
|---|---|---|
| `int64_t`, `uint64_t` | `i64` | Zell `int`, `uint`, lengths and indices are all `i64` |
| `int32_t` | `i32` | Zell `char` (rune) is `i32` |
| `uint8_t` | `i8` | Zell `byte` |
| `double` | `double` | Zell `float` |
| `bool` | `zeroext i1` | C `_Bool`. Declare both parameters **and** return values `zeroext i1`. In memory it is 1 byte |
| `T *`, any object pointer | `ptr` | |
| function pointers | `ptr` | always C calling convention (`ccc`) |

All runtime functions use the platform C calling convention. **No runtime function
takes or returns a struct by value.** Aggregates (`zell_slice`, `zell_map_iter`)
are always passed as `ptr`, and functions that produce one write it through an
out-pointer (`zell_slice *out`). That way the compiler never has to reproduce the
SysV/AAPCS rules for passing structs.

### 2.2 Ownership (the most important rule)

The runtime uses automatic reference counting. Every heap object carries an atomic
refcount (section 3).

1. **Arguments are borrowed.** A runtime function never consumes a reference the
   caller passed in. If it keeps the value (it stores it in a slice, map, error,
   task result or channel), it calls retain itself.
2. **Returned heap objects are owned (+1).** The caller must eventually call
   `zell_release` on them. The exceptions are marked **[borrowed]** in this
   document. Those return a pointer that stays valid only while the object it was
   read from stays alive and unmodified.
3. **Out-pointer aggregates are owned.** A `zell_slice` written to `*out` holds a
   reference to its backing array. The caller releases it with
   `zell_slice_release`.
4. **In-place mutators** (`zell_slice_append*`, and `zell_slice_sub` with
   `out == s`) update a slice variable that the caller owns. The old reference
   held by that variable is transferred or released internally, so the caller has
   nothing to fix up.
5. **Pointers into containers are borrowed.** This covers `zell_slice_index`,
   `zell_map_get*`, `zell_task_result` and `zell_task_await`. Writing through
   them is allowed, and it is an ordinary store into a slot: see 2.3.

### 2.3 What the compiler must do for its own variables and fields

For any location (local, struct field, slice element, map value slot) whose type
is a reference type (string, `*T` to a heap object, map, error, Task, closure,
Mutex, Channel):

- **Store** `loc = v`: `zell_retain(v)` first, then `zell_release(old)`, then
  write. Retaining first makes `x = x` safe.
- **Copy into a new variable or argument passed by value**: retain.
- **End of scope / overwrite / drop of the containing object**: release.
- For a slice-typed location, use `zell_slice_retain` / `zell_slice_release` /
  `zell_slice_assign` instead. A slice value owns exactly one reference to
  `backing`.
- For struct values that contain references, emit per-type "retain fields" and
  "release fields" helpers and use them as `zell_value_ops` (section 3.3).

Returning a value from a function transfers +1 to the caller. This is the same
rule as the runtime's own functions.

### 2.4 NULL

NULL is a valid value wherever the header takes a heap object, and it means:

| Type | NULL means |
|---|---|
| `zell_string *` | the empty string `""`. Every string function accepts NULL. The zero value of `string` is NULL |
| `zell_error *` | no error (`nil`) |
| `zell_map *` | nil map: `len` is 0, `get` returns NULL, `delete` is a no-op, iteration is empty, **`set` panics** |
| `zell_slice` | the all-zero struct is the nil or empty slice |
| `zell_task *`, `zell_mutex *`, `zell_chan *` | invalid: `zell_panic_nil()` |
| `zell_retain(NULL)`, `zell_release(NULL)` | no-op |

The compiler checks dereferences of user pointers itself:
`if (p == null) call zell_panic_nil()`.

### 2.5 Thread safety

- Refcount operations are thread-safe (atomic, or plain stores by the
  thread a biased count belongs to, 3.1), so objects can be shared between
  tasks or threads.
- Strings and errors are immutable, so they are freely shareable.
- Slices and maps are **not** internally synchronised, the same as in Go. Concurrent
  mutation needs a `Mutex` or a channel.
- Tasks, mutexes, channels and the scheduler are fully thread-safe.
- `println_*` writes one whole line under the stdio lock, so lines from
  different threads never interleave.

---

## 3. Memory and reference counting

### 3.1 Layouts

```c
typedef struct zell_type_info {      // 24 bytes, usually a constant global
    const char *name;                 // +0   type name (may be NULL)
    int64_t     size;                 // +8   informational object size, 0 if unknown
    void      (*drop)(void *obj);     // +16  may be NULL
} zell_type_info;

typedef struct zell_object {         // 16 bytes: header of EVERY heap object
    int64_t                refcount;  // +0   atomic; -1 = immortal
    const zell_type_info *type;      // +8   may be NULL
} zell_object;                       // payload starts at +16
```

```llvm
%zell_type_info = type { ptr, i64, ptr }
%zell_object    = type { i64, ptr }
```

Runtime functions always take and return pointers to the **header** (offset 0)
of a heap object. Compiled code represents a Zell `*T` as a pointer to the
**payload** (header + 16): for `new Point{X: 1, Y: 2}` the object is
`{ %zell_object, double, double }` and the `*Point` points at field `X`.
Use `zell_ptr_retain` / `zell_ptr_release` / `zell_ops_ptr` for such
values; a non-nil `*T` always points at the payload of a whole object (the
compiler never retains interior pointers, see DESIGN.md "Память").

The refcount word holds the count in its low 57 bits
(`ZELL_RC_COUNT_MASK`); bit 62 (`ZELL_RC_WEAK_FLAG`) marks an object that
has a weak box (3.4); bits 57..61 belong to the cycle collector (3.8):
`ZELL_RC_CYCLIC` (61) marks an object of a potentially cyclic type, and
only in such a word do `ZELL_RC_BUFFERED` (60), `ZELL_RC_MARKED` (59),
`ZELL_RC_DIRTY` (58) and `ZELL_RC_ZOMBIE` (57) have their collector
meaning. `-1` (`ZELL_RC_IMMORTAL`) is immortal; `-2`
(`ZELL_RC_ARENA`) marks an object owned by an arena (3.6). Retain and release
ignore these two; a word below `-2` is a string view inside a view block
(`ZELL_RC_VIEW | block`, 4.1), and retain and release forward to the block.
Inline fast paths in compiled code therefore treat every negative word as
"call the runtime" (see `codegen/rc.rs`).

**Biased counts** (`src/rc.c`). A non-negative word without `CYCLIC` is in
one of two forms, told apart by bit 60 (`ZELL_RC_BIASED`):

```
unowned  bit 62 WEAK | bit 59 NOBIAS | bits 0..56 count
biased   bit 62 WEAK | bit 60 BIASED | bit 59 QUEUED | bits 47..58 owner slot
         | bits 16..46 S + 2^30 (ZELL_RC_S_EXCESS) | bits 0..15 B
```

A biased object belongs to the thread holding owner slot 1..4095; its
count is `B + S`. **B** (the owner's part, the 16-bit half-word at the
object's address on these little-endian targets) is changed only by the
owner, with plain (relaxed atomic) 16-bit loads and stores (release stores when B drops); **S** (the shared
part, signed, stored with an excess of 2^30) is changed by everyone else
with 64-bit atomic RMW of `±1 << 16`, which never touches B. The thread-local
`zell_rc_owner` (`int64_t`, exported by the runtime) is the owner's tag
`(ZELL_RC_BIASED >> 47) | slot` of the calling thread, or
`ZELL_RC_NO_OWNER` (0x1FFFF); a thread owns a word exactly when
`((uint64_t)word >> 47) & ZELL_RC_OWNER_MASK (0x16FFF) == zell_rc_owner`
(the mask keeps the sign, `CYCLIC`, `BIASED` and slot bits).

- Objects are created unowned with count 1. The first retain of an unowned
  object whose word is exactly 1 (no flags but WEAK) by a thread with a
  slot (scheduler workers and the thread that called `zell_runtime_init`)
  while `zell_threaded == 1` biases it to that thread in one CAS
  (`B = 2, S = 0`). Words with `NOBIAS` (set by the runtime on objects shared
  by design, such as tasks) and `CYCLIC` words are never biased; neither is
  anything while `zell_threaded == 0` (one thread: unowned counts are
  already plain stores) or with `ZELL_NO_BIAS=1`. While threaded, unowned
  counts change by CAS (another thread may bias the object meanwhile).
- Owner: retain `B += 1` (at `B == 0xFFFF` one CAS moves 0x8000 from B to S
  first); release with `B >= 2` is `B -= 1`. Release with `B == 1`: if
  `S == 0` and neither `QUEUED` nor `WEAK` is set the owner held the only
  reference and frees the object without an atomic; otherwise a CAS sets
  `B = 0`, and the object is freed if then `S == 0` and `QUEUED` is clear.
  An owner release with `B == 0` is a release on S.
- Others: retain is one atomic add of `1 << 16`. Release is a CAS of
  `S -= 1` that, when S becomes negative and `B + S` does not reach zero,
  also sets `QUEUED`: the object then goes on its owner slot's merge queue
  (`B + S` may later reach zero by a plain store of the owner, which does
  not look at S). The thread whose CAS makes `B + S` zero frees the object,
  unless `QUEUED` is set.
- Merge: the owner, at the slow path of its allocations, before its worker
  parks and when it exits, or any thread while the owner is parked (an idle
  worker, a blocking `zell_task_await`) or gone (it takes the slot's lock,
  which the owner holds while it runs), takes the queue and for each object
  reads `B + S`: zero means dead (freed there); otherwise one CAS folds a
  negative S into B (`B += S, S = 0`) and clears `QUEUED`.
- **Deallocation condition**: exactly one thread frees an object, the one
  that observes `B + S == 0` in a snapshot its own atomic step produced
  (the owner's CAS, another thread's CAS, or the merge of a `QUEUED`
  object, which only the queue frees), or a holder that read
  `B + S == 1` with neither `QUEUED` nor `WEAK` set (it held the only
  reference; `zell_weak_get` resurrects by CAS of `S += 1` only while
  `B + S > 0`).
- **Owner exit**: the thread merges its queue and gives up its slot (TLS
  destructor). Its objects stay biased with the free slot: every thread
  updates them with the atomic S operations (the global *unowned* state),
  and a thread that queues one merges it at once (it takes the free slot's
  lock). A new thread that takes the slot later becomes their owner;
  taking the slot lock waits for a merge in progress, so its plain stores
  never race with one.
- `S` must stay within ±2^30 (minus a margin for adds in flight; beyond it
  the runtime panics with `reference count overflow`). At shutdown
  `zell_runtime_shutdown` merges every queue before the leak report.

`zell_refcount` decodes both forms. Code outside the runtime must not
modify a refcount word except as documented here (immortal/arena words of
its own objects); compiled code only does the inline fast paths below.

### 3.2 Functions

| Function | Semantics |
|---|---|
| `void *zell_alloc(int64_t size)` | `calloc`-style **zeroed** raw memory, not refcounted. Use it for closure environments without a header and scratch memory (coroutine frames use `zell_coro_frame_alloc`, 10.3). Out of memory or a negative size panics. `size == 0` returns a unique pointer |
| `void *zell_realloc(void *p, int64_t size)` | raw `realloc`. New bytes are **not** zeroed. Panics on out of memory |
| `void zell_free(void *p)` | frees raw memory. NULL is fine |
| `void *zell_object_new(const zell_type_info *type, int64_t size)` | allocates a zeroed object of `size` bytes, **header included** (so `size >= 16`), sets `refcount = 1` and `type = type`. Returns it +1 |
| `void zell_retain(void *obj)` | adds one reference (atomically unless the owner of a biased count or single-threaded, 3.1). No-op for NULL and for immortal objects |
| `void zell_release(void *obj)` | drops one reference (3.1). When the count reaches 0: calls `type->drop(obj)` if both are non-NULL, then frees the memory. No-op for NULL and immortal objects |
| `int64_t zell_refcount(const void *obj)` | current count (debugging and tests): the unowned count, or `B + S` of a biased one, without flags. 0 for NULL, -1 for immortal objects |
| `int64_t zell_live_objects(void)` | number of objects allocated with `zell_object_new` and not yet freed (leak checks in tests). Objects made immortal (`zell_string_literal`, `zell_error_sentinel`) do not count |
| `void zell_ptr_retain(void *payload)` / `void zell_ptr_release(void *payload)` | retain / release the object whose payload (object + 16) is given: the operations on a `*T`. No-op for NULL |
| `void zell_leak_check_enable(int32_t mode)` | leak check (3.5); call it before anything else, even before `zell_runtime_init` |
| `uint8_t zell_threaded` (global) | 0 while the process has a single thread, 1 for good once a second thread was started (`zell__plat_thread_start*` sets it before creating the thread; C code that starts threads of its own that touch Zell objects must set it first; the heap's scavenger thread, which never touches objects, leaves it alone). While it is 0 refcount updates of unowned words may be plain loads and stores, and nothing is biased |
| `_Thread_local int64_t zell_rc_owner` | the calling thread's owner tag for biased counts (3.1), `ZELL_RC_NO_OWNER` without a slot. Read by the inline fast paths |
| `void zell_runtime_free_os_memory(void)` | `runtime.FreeOSMemory`: the calling thread's empty heap pages, the free-page reserve and the cached large mappings go back to the OS (below), and `malloc_trim` |

**The `drop` contract.** `drop(obj)` must release every reference the object holds:
strings, pointers, slices (`zell_slice_release`) and so on. It must **not** free
`obj`, because the runtime does that after `drop` returns. The compiler emits one
`drop` per heap-allocated struct type that has reference-typed fields. If there
are no such fields, `type` may point to a type info whose `drop` is NULL, or
`type` itself may be NULL.

**Deferred drop (bounded release depth; `src/mem.c`, `src/rt_internal.h`).**
A `drop` releases the object's children, and a child whose count reaches
zero is dropped in turn, so releasing the head of a 2M-node linked list
would recurse `zell_release -> drop -> zell_release ...` two million
frames deep and overflow the stack. Every finalizer of a dead object (the
plain release here, the biased and unowned ones in `rc.c`, the collector's
`dead()` and the single-threaded fast path of `zell__cycles_release` in
`cycles.c`) brackets its `drop` with `zell__dead_enter` / `zell__dead_leave`
(inline, one thread-local word: the nesting depth and a "worklist not
empty" bit). Fewer than 64 finalizations nested on the thread run on the
stack as before; the one that would nest deeper is not run but *queued*:
`(finalizer, object, refcount word)` goes onto a thread-local worklist
(`malloc`ed, freed when drained), and the outermost finalization (depth 1)
drains it after its own `drop` has returned, each popped entry recursing
up to the bound again. The frame at the bound reverses the entries its
children queued, so the stack pops them first-released first and the order
of `Drop()` calls is exactly the recursive one: a parent before its
children, siblings and their subtrees in field order (DESIGN.md "Память").
The only visible difference is when memory is returned: an object queued
this way is freed after its ancestors' memory, before the outermost release
returns. Weak references are cleared, as before, at the moment the count
reaches zero (before queueing). Objects whose type has no `drop` need no
depth accounting: they free nothing else.

**Allocation (implementation note, not ABI; `src/heap.c`, fast paths in
`src/heap.h`).** Objects of at most 16 KB (header included) are *small*:
rounded up to one of 36 size classes (16-byte steps to 128, then four per
power of two) and carved from 64 KB *pages*, 64 to a 4 MB-aligned
*segment* whose base holds the page descriptors (a bitmap over the address
space tells segment memory from the rest on free). A page serves one class
and is owned by one thread's heap: the owner allocates and frees with plain
stores and counts its live blocks; other threads push what they free onto
the page's atomic list, which the owner takes back when it runs out
(full pages are revisited when a remote free signals them, at the owner's
next slow path or before its worker parks). A page whose blocks are all
free is *retired* to a global pool (unless it is the only page of its
class in the heap); pages of exited threads are adopted or retired when
they become free. Objects up to 128 KB are *medium* (`malloc` with a
16-byte prefix), larger ones *large*: a mapping of their own grown with
`mremap` where available. **Returning memory**: freed pages and large
mappings stay as a reserve while in use + reserve stays within the recent
peak of the heap (so rebuilding data does not fault pages in again) and
16 MB (`ZELL_HEAP_RESERVE=<bytes>[KMG]`); a background *scavenger* thread
(started the first time the reserve exceeds the minimum; it never touches
objects, so `zell_threaded` stays 0) gives back, every 0.2 s, what stayed
unused for 0.3 s beyond the minimum: free pages with
`zell__plat_mem_release` (Linux `MADV_DONTNEED`, macOS `MADV_FREE`,
Windows `DiscardVirtualMemory` / `MEM_RESET`; the address range stays
reserved) and cached mappings with `munmap`. Under a **memory limit**
(`zell_runtime_mem_limit`, DESIGN.md "Память") the reserve never holds
more than 15/16 of the limit minus the heap in use: checked when memory is
freed and, for growth, each time the heap in use grew by 1 MB; the
scavenger then also calls `malloc_trim`, and segments get no huge-page
hint once the heap passes half the target. **Huge pages** are opt-in
(`ZELL_HUGEPAGES=1`): segments after the first get `MADV_HUGEPAGE`, far
fewer page faults for a big heap, but with THP defrag on `madvise` (the
usual setting) the first touch of a huge page may compact memory for
milliseconds, a latency spike (100k timers on 32 workers: 39 ms mean
lateness with it, 2.4 ms without). `zell_runtime_free_os_memory`
releases everything at once. `diag.Runtime()` reports `HeapInUse`
(owned pages, medium and large objects), `HeapRetained` (the reserve) and
`HeapReleased` (pages given back whose address range is kept). The heap is
off (plain `malloc`) under AddressSanitizer, with leak check mode 2 and
with `ZELL_NO_SLAB=1`. Nothing outside the runtime may free or `realloc`
object memory itself. The live and total object counts are per-thread
counters summed on demand (no shared atomic per allocation). The
scheduler's worker and timer threads start with the first task, not in
`zell_runtime_init`, so a program that never starts a task stays
single-threaded.

**Inline fast paths (compiler-side, `codegen/rc.rs`).** By default (at
every `-O` level; a build with `ZELL_NO_INLINE_RC=1` emits the plain calls
instead) compiled code does not call `zell_retain`/`zell_release`
(or the `zell_ptr_*` variants) for the common cases: for a non-NULL object
it loads the word (relaxed) and `zell_threaded`; if the latter is 0 it
stores `word + 1` (retain, `word >= 0`) or `word - 1` (release,
`2 <= word < 2^57`, below every flag bit). Otherwise, if the word is a
biased count owned by this thread (`((uint64_t)word >> 47) & 0x16FFF ==
zell_rc_owner`, a thread-local load), it stores the 16-bit B `+ 1` (retain,
`B != 0xFFFF`, relaxed) or `- 1` (release, `B >= 2`, a release store: the
thread that later frees the object acquires it) at the object's address. Every
other case calls the runtime function, which stays the reference
behaviour. The layout the inline code relies on is exactly the one above
(`zell_rt.h`): the 64-bit word at the object's address, `B` in its low 16
bits (little-endian: a 16-bit store at the object's address), the owner
tag at bits 47.., `zell_threaded` and `zell_rc_owner` as declared in 3.2;
a change to any of them is an ABI change for compiled code, not only for
the runtime. The price is code size: it grows large programs by up to a
third and their optimisation time by half (benchmarks/README.md).

**Immortal objects** (`refcount == -1`, `ZELL_RC_IMMORTAL`) are never modified or
freed. Use them for string literals and sentinel errors emitted as LLVM globals.

When the count of an object with a weak box reaches zero, the weak box is
cleared **before** `drop` runs, so weak references read as NULL from then on
(including inside the destructor). Use weak references for back pointers;
reference cycles that remain are freed by the cycle collector (3.8).

### 3.3 Value ops (ownership of inline values in containers)

Containers (slices, maps, task results, channels) store values inline as raw bytes.
To know how to retain or release those bytes they take an optional
`zell_value_ops *`:

```c
typedef struct zell_value_ops {   // 24 bytes
    void (*retain)(void *slot);    // +0  add the references contained in *slot
    void (*release)(void *slot);   // +8  drop the references contained in *slot
    void (*visit)(void *slot, zell_cycle_cb cb, void *ctx); // +16 cycle collector (3.8), or NULL
} zell_value_ops;
```

`visit` reports the references in `*slot` that may lie on a reference
cycle (3.8). A slice backing or map created with ops whose `visit` (key or
value ops for a map) is non-NULL is flagged `ZELL_RC_CYCLIC`; the
runtime's own `zell_ops_*` have none. Compiled code emits a non-NULL
`visit` exactly for element types that hold references to objects of
cyclic types, so all containers of one element type agree.

- `NULL` ops means plain data (ints, floats, bools, structs with no references).
- `&zell_ops_ref`: the slot is one 8-byte heap-object pointer (string, map,
  error, Task, weak box...).
- `&zell_ops_ptr`: the slot is one `*T` (a payload pointer, object + 16).
- `&zell_ops_slice`: the slot is one 32-byte `zell_slice`.
- For struct values containing references, the compiler emits a constant
  `zell_value_ops` whose functions retain or release each field.

`release` must handle an all-zero slot (all release functions treat NULL as a
no-op), because containers zero-initialise their storage.

### 3.4 Weak references

A `weak *T` is a pointer to a **weak box**, a small refcounted object
`{hdr, zell_object *target}` shared by all weak references to the same
object, or NULL. Boxes are kept in a side table (object -> box) protected by a
mutex; an object with a box has `ZELL_RC_WEAK_FLAG` set in its refcount word.

| Function | Semantics |
|---|---|
| `void *zell_weak_new(void *payload)` | the weak box of the object whose payload is given, created on first use; returns it **+1** (the box, not the object, is retained). NULL gives NULL. Panics for immortal objects |
| `void *zell_weak_get(void *box)` | the target's payload **+1** (retained), or NULL if the target was freed (or `box` is NULL). Never resurrects an object whose count already reached zero |

Weak boxes are ordinary objects: retain and release them with
`zell_retain`/`zell_release` (`zell_ops_ref` in containers). When the
target dies the box's `target` becomes NULL; when the last weak reference
goes away the box removes itself from the table and clears the flag.

### 3.5 Leak check

`zell_leak_check_enable(mode)`: mode 1 counts live objects; mode 2 also
links every object into a list (a 16-byte prefix in front of the header, so
it must be enabled before the first allocation). At the end of
`zell_runtime_shutdown`, if `zell_live_objects() > 0`, the runtime prints

```
leak: N objects
  K x TypeName        (mode 2 only, by count, then name)
  cycle through Type.Field (make one field of the cycle `weak *T` to break it)
  M of them in unreachable reference cycles the cycle collector can free (...)
```

to stderr and exits with status `ZELL_LEAK_EXIT_CODE` (66). Before the
count is taken, `zell__cycles_at_exit` runs: in `ZELL_CYCLES=auto` mode a
final collection (up to four rounds), so only what the collector cannot
free is reported; in `manual` mode a dry run whose garbage count is the
`M of them ...` line (omitted when 0, and in `off` mode). The compiler
enables it for `--leak-check` (mode 2 in debug builds, 1 with `--release`)
and releases the program's globals before calling the shutdown.

`void zell_leak_fields(const zell_type_info *type, const zell_leak_field *fields)`
registers the `*T` fields of objects of `type` (`zell_leak_field` = `{ i64
offset, const char *name }`, 16 bytes; `offset` is from the payload, the
array ends with `offset == -1`; both must stay valid until exit). The
compiler calls it at startup in mode 2 for every struct type info with
pointer fields, naming them `"Type.Field"`. In mode 2 the report then
lists, once per field name, every registered field of a leaked object that
points at another leaked object from which the first is reachable again
(the field lies on a reference cycle). `void zell_leak_acyclic(const zell_type_info *type, const
char *name)` (after `zell_leak_fields` of the type) marks it `@acyclic`
(DESIGN.md "Циклы"); a leaked object of it on such a cycle adds `type
<name> is @acyclic but was found in a cycle`. Only the first 100000 leaked objects
are examined.

### 3.6 Arenas (`arena`)

An arena allocates objects whose refcount word is `ZELL_RC_ARENA` (`-2`):
like immortal objects, `zell_retain`/`zell_release` ignore them (every
runtime check for "immortal" is `refcount < 0`), so compiled code handles
arena pointers, strings and slices exactly like others. Arena objects are
not counted by `zell_live_objects` and are not in the mode-2 leak list.
Handles are raw pointers passed as `int64_t`; arenas are not thread-safe.

| Function | Semantics |
|---|---|
| `int64_t zell_arena_create(void)` | a new empty arena |
| `void *zell_arena_object(int64_t a, const zell_type_info *type, int64_t size)` | a zeroed object of `size` bytes (header included) in the arena, header `{ZELL_RC_ARENA, type}`; returns the header (borrowed) |
| `void zell_arena_slice(zell_slice *out, int64_t a, int64_t elem_size, int64_t n, const zell_value_ops *ops)` | `*out` = `n` zeroed elements in a backing array in the arena (layout of 5.1); `ops` releases elements at `Free`. `n == 0` gives the nil slice. Growing it by `append` copies it to the heap (the count is never 1) |
| `zell_string *zell_arena_string(int64_t a, const zell_string *s)` | a copy of `s` (borrowed) in the arena; NULL for `""` |
| `void zell_arena_free(int64_t a)` | runs `type->drop` of every object in allocation order (releasing what they reference outside the arena; objects allocated by a drop are freed too), then keeps the emptied chunks as spares for the next allocations (spares left unused since the previous `free` go back to the system); the arena stays usable |
| `void zell_arena_destroy(int64_t a)` | `zell_arena_free`, then frees the spares and the arena itself |
| `int64_t zell_arena_objects(int64_t a)` | number of objects allocated since the last free |

Memory: 64 KB chunks (an object larger than 16 KB gets a chunk of its own);
each object is preceded by a 16-byte record prefix. After `free` every
pointer into the arena is dangling (not checked) for use, but until the
arena allocates again its headers still read `ZELL_RC_ARENA`, so
releasing such a pointer (or a slice of them) is a no-op: locals that go
out of scope after `defer a.Free()` or after the `Free` at the end of a
loop body are safe. After `destroy` nothing may touch them: compiled code
releases a function's `for` sequence temporaries at the end of the loop,
and the remaining scoped temporaries before the locals (`codegen/funcs.rs`
`release_locals`), so an `Arena` local outlives them. `arena.Arena` (a
Zell source, `compiler/src/sema/stdlib_src/arena.zl`) holds the handle in
a refcounted object whose `Drop` destroys the arena.

### 3.7 Interface and `any` values (compiler-side)

An interface value is `{ptr data, ptr vtable}` with vtable
`{ptr typeid, i64 boxed, ptr method0, ...}`; an `any` is
`{ptr data, ptr typeid}`. `data` is the payload of a heap box holding the
value (or, for an interface holding a `*S`, the pointer itself). **Exception:
an `any` holding a `bool`, an integer type, `float`, `float32` or `char`
keeps the value in the data word itself** (zero-extended, floats as their
bit pattern, little-endian, so the value can be read at the address of the
word), with no box: boxing a scalar into `any` allocates nothing. Code that
releases an `any` asks `any.inline(typeid)` (a per-module function) whether
the data word is a box; `reflect.ValueOf` copies an inline scalar into
a box (`anyBox`) so that a `Value` has an address; `unsafe.AddrOf(x)`
returns the word as is (the value bits for a scalar). The debug agent and
the lldb/gdb formatters read an inline scalar at the address of the data
word.

Inside the arm `case T x:` of a type switch on an interface local that is
never reassigned, method calls through that local use `T`'s vtable (a
constant global), so LLVM calls (and can inline) the method directly.

### 3.8 Cycle collector (`src/cycles.c`)

Bounded synchronous trial deletion (Bacon–Rajan) over the objects of
potentially cyclic types only (DESIGN.md "Циклы"). It never scans the
heap, stacks or globals: only the objects reachable through cyclic-typed
references from the *possible roots* recorded since the previous run.

**Flags.** `ZELL_RC_CYCLIC` is set at allocation, unless `ZELL_CYCLES=off`,
by `zell_cycles_new_entry` (compiled code uses it instead of
`zell_object_new` for boxes, cells and `new T` of a cyclic type, and for
closure environments that capture a cell of one), `zell_cycles_new` (C
callers with registered types), `zell_cycles_new_dyn` (a
dynamic type: flagged when the type info is registered with a visitor;
`reflect` allocation) and by `slice.c`/`map.c` for containers whose ops
have a `visit` (3.3). A flagged object never takes the biased refcount
paths or the inline release fast path (its count word is >= 2^57): its
release goes to `zell__cycles_release`, and in a multi-threaded process
its retain is an RMW whose result is checked for `MARKED`. Flagged objects
have the ordinary layout and size.

**Linking.** Outside `cycles.c` the runtime refers to the collector only
through weak symbols (`zell__cycles_*`, and `zell__diag_heap_*` of
`diag_heap.c`): a program that never allocates an object of a cyclic type
does not link it (no flagged object can exist then; the container and
leak-report hooks test the weak symbol for NULL). Compiled code links it
by calling `zell_cycles_new_entry`.

| Function | Semantics |
|---|---|
| `void *zell_cycles_new_entry(zell_cycle_type *e, int64_t size)` | `zell_cycles_new(e->key, size)`, registering `e` first if its `state` is 0. Compiled code emits one non-constant entry per cyclic boxed type (`cycles.entry.N`) and per environment of a lambda capturing a cyclic cell, so only the types a program allocates are registered and unreachable code is not kept alive |
| `void zell_cycles_register(const zell_cycle_type *t, int64_t n)` | registers `n` entries `{key, visit, fin, clear, alias, state}` (48 bytes, kept by pointer) at once. With `visit`: objects whose type info is `key`; `visit(obj, cb, ctx)` calls `cb(ctx, ref, tid)` for each reference (an object header) the object holds that may lie on a cycle; `fin(obj)` runs the destructors only (NULL: none); `clear(obj)` releases the references without destructors (NULL: `type->drop`). Without `visit`: `key` is a dynamic type id whose objects are flagged. `alias` (or NULL) is a dynamic type id registered with the entry: `ti.<*S>` (an interface holding a `*S` names the S object) or a lambda's function pointer (a closure's environment). The registry is a lock-free-read table (insertions under a mutex, grown by copying); the runtime registers its backing and map types itself |
| `void *zell_cycles_new(const zell_type_info *type, int64_t size)` | `zell_object_new` plus the flag; runs a due automatic collection first (a safe point) |
| `extern int64_t zell_cycles_epoch` / `void zell_ptr_release_noroot(void *payload, int64_t epoch)` | (in `mem.c`) the collection epoch, incremented when a collection starts and when it ends; the release paired with a *pin*: compiled code that retains a borrowed `*T` of a cyclic type for the duration of a call or a field access (`expr.rs` `arg`/`keep`) reads the epoch then and releases with `zell_ptr_release_noroot`, which decrements without recording a possible root when the epoch is unchanged (no collection overlapped the pin) and is `zell_ptr_release` otherwise. Sound because the pin added no reference anywhere: every other reference to the object or its cycle that went away during the pin was an ordinary, recorded decrement, and no collection can have dropped such a root meanwhile. Early exits release pins with the ordinary release |
| `void *zell_cycles_new_dyn(const zell_type_info *type, int64_t size)` | the same when `type` is registered with a visitor, else `zell_object_new` |
| `int64_t zell_cycles_collect(void)` | a collection now, on the calling thread (waits for a running one); returns the objects freed. 0 when off |
| `void zell_cycles_stats(zell_cycle_stats *out)` / `int32_t zell_cycles_mode(void)` / `void zell_cycles_set_mode(int32_t)` | statistics (`runs, collected, candidates, examined, pending, threshold, aborted, mode`), the mode (0 off, 1 manual, 2 auto); `set_mode` switches manual/auto (off only before any object exists) |

`cb`'s `tid` is NULL when the reference is statically to a flagged type
(`*T` of a cyclic `T`, a slice backing or map whose ops visit), else the
dynamic type id from an interface's vtable word 0, an `any`'s type word or
a closure's function pointer: the collector follows the reference only if
`tid` is registered. A visitor reads its fields with plain word loads
while mutators may write them.

**Possible roots.** A flagged object whose count is decremented to a
value other than zero, not `BUFFERED` and not `MARKED`, gets `BUFFERED`
(in the same RMW) and is appended to its thread's candidate buffer. When a
`BUFFERED` object's count reaches zero, the weak boxes are cleared and its
`drop` runs at once; if it is among the last 8 entries of the dying
thread's buffer it leaves it and is freed, else it gets
`ZOMBIE` (one RMW decides with the buffer owner's `BUFFERED`-clearing RMW
who frees the memory) and the buffer owner frees it when it sweeps. A
full buffer is swept (zombies freed) and grows up to 2^18 entries; it is
*published* to a global list when it holds a collection's worth of roots,
when the thread exits or its scheduler worker parks, before a manual
collection, and when another thread requests it (in auto mode, after
`threshold` roots were recorded process-wide: the thread that trips the
count publishes at once, the others at their next record). Published
roots that die become zombies; the list is swept when it doubles.

**Triggers.** `ZELL_CYCLES=auto` (default): once `threshold` roots were
recorded process-wide since the last request (or `threshold` published
roots are pending), the next `zell_cycles_new` on any thread runs a
collection (one at a time; a thread that finds one running skips).
`threshold` starts at 4096 and doubles (max 2^22) after a collection that
freed fewer than 1/8 of its roots, halves (min 1024) after one that freed
more than half; `ZELL_CYCLES_THRESHOLD=N` fixes it. `manual`: only
`zell_cycles_collect` (`runtime.CollectCycles`) and the leak report.
`off`: nothing is flagged, recorded or collected.

**A collection.** (0) `g_collecting = 1` with a seq_cst RMW and fence; from
then on no flagged object's memory is freed until the end (frees are
deferred; `drop` still runs on time). (1) Take the published roots and
the own buffer, clearing `BUFFERED` (zombies are freed at the end; dying
roots are left to their dropper). (2) Discovery: from the live roots,
follow the visitors' references to flagged objects; each new object gets
`MARKED` by an RMW whose result is its count snapshot (objects being
dropped are skipped). More than `ZELL_CYCLES_MAX_SCAN` (default 2^22)
objects abort the collection. (3) After all are marked, visit them again
and subtract every internal reference from the target's snapshot. (4)
Every object with a non-zero remainder, and everything reachable from it
through internal references, is live (black). (5) Validation: a white
object whose word shows `DIRTY` or a count different from its snapshot
becomes black with what it reaches; repeated until stable, and again
under the weak-table lock, which then clears the weak boxes of the white
objects. (6) The white objects are garbage: the collector takes a
reference to each, runs every `fin` (registration order of the roots,
then discovery order), recounts the references among the white objects
(a white object with any other reference — a destructor stored it
somewhere — becomes black and keeps the collector's reference forever,
so no destructor runs twice), runs every `clear`, and frees each object
whose count is then 1 (one still in another thread's unpublished buffer
becomes a zombie instead). (7) Unmark (`MARKED`/`DIRTY` cleared; a member
that was `DIRTY` is recorded as a possible root again), `g_collecting =
0`, deferred frees.

**The invariant.** While an object is `MARKED`, every change of its count
sets `DIRTY`: in a multi-threaded process every retain and release of a
flagged object (`zell_retain`, `zell__cycles_release`,
`zell_weak_get`) is an atomic RMW whose result is checked for `MARKED`;
in a single-threaded process the collector runs alone, so nothing changes
behind it (destructors only run in step 6). A thread can create or drop a
reference to an object only by changing its count, and compiled code holds
a counted reference behind every borrowed one. Hence when validation finds
a white object's count equal to its snapshot, every reference that count
stands for is one step 3 read (field reads happen after all marks; a
reference added or removed after its target was marked dirties it) and
still exists, and all of them come from white objects. No thread holds
or can obtain a reference to the white set except through a weak
reference, and step 5 clears those under the lock `zell_weak_get` takes.
Memory safety of the concurrent reads: the collector dereferences a
reference only when its target is statically flagged or its `tid` is
registered; a flagged object freed after step 0 is deferred (the
mutator's free path does a seq_cst fence before reading `g_collecting`,
the collector one after setting it; under ThreadSanitizer both are RMWs
on `g_collecting`), so every object the collector reached from a live one
stays readable. Flagged backings never move (`append` resizes in place
only at a count of exactly 1). Map tables are freed by a resize, so a map
is walked only while the process is single-threaded; in a threaded
process its entries count as outside references and cycles through maps
are not collected. Not walked at all: `error` values, tasks and coroutine
frames, arena (`-2`) and stack (`-1`) objects.

---

## 4. Strings

### 4.1 Representation

A string is a **single pointer** to an immutable heap object. **NULL is `""`.**

```c
typedef struct zell_string {
    zell_object hdr;   // +0   type = &zell_type_string (or _view, below)
    int64_t      len;   // +16  length in BYTES
    char        *data;  // +24  the bytes
} zell_string;         // 32 bytes; an owned string's bytes follow inline at +32
```

The bytes are UTF-8 but not validated. A string's length and bytes never change
after creation (`zell_string_append` may extend a uniquely referenced owned
string in place; see 4.2).

**Owned strings** keep their `len` bytes inline at +32 followed by a NUL, and
`data` points there (`data == (char *)s + 32`).

**Views** share the bytes of another string, so substrings cost no copy (Go's
`s[a:b]` semantics; like Go, a view keeps its parent's bytes alive). Their
`data` points into the parent's bytes and is **not NUL-terminated**. The byte
`data[len]` is always readable (it is the next byte of the parent, or the
parent's NUL), so `data[len] == '\0'` tells whether `data` can be passed to C
as is; C APIs that need a terminated string call `zell_string_cstr(s)`. There
are two kinds:

- a **standalone view** `{hdr, len, data, zell_object *owner @32}` (40 bytes),
  an ordinary refcounted object whose type is `zell_type_string_view`; its
  `drop` releases `owner`. `s[lo:hi]` (`zell_string_slice`), `TrimSpace`,
  `Trim*`, `TrimPrefix/Suffix`, `SplitN` pieces, and everything written in
  Zell on top of `s[a:b]` (`Cut`, regex match texts, ...) return one when
  the piece is at least 64 bytes long; shorter pieces are copied (a small copy
  is cheaper than the owner's refcount traffic and does not pin a large
  parent);
- a **block view**: bulk results (`Split`, `Fields`) put all their views in
  one *view block* object `{hdr, owner, n, cap, zell_string views[cap]}`; a
  block view's refcount word is `ZELL_RC_VIEW | (int64_t)block`
  (`ZELL_RC_VIEW` = `INT64_MIN`), so it is negative and below
  `ZELL_RC_ARENA` (`zell_rc_is_view(rc)` = `rc < ZELL_RC_ARENA`), and
  `zell_retain`/`zell_release` apply to the block instead. The block holds
  one count per live view handed out, and releases its owner when it dies.
  Empty pieces are NULL.

`owner` is always the object that holds the bytes inline (views of views are
flattened), or NULL for the bytes of an immortal string. Strings in an arena
(3.6) are never viewed (a view would outlive `Free`); slicing them copies.
Code that tests "uniquely referenced" (`refcount == 1`) before mutating a
string in place must also check `data == (char *)s + 32`.

**Literals.** The compiler emits each literal as an immortal constant global and
uses its address directly. There is no runtime call and nothing to release:

```llvm
@.str.hello = private unnamed_addr constant { i64, ptr, i64, ptr, [6 x i8] }
    { i64 -1, ptr @zell_type_string, i64 5,
      ptr getelementptr inbounds ({ i64, ptr, i64, ptr, [6 x i8] }, ptr @.str.hello, i32 0, i32 4),
      [6 x i8] c"hello\00" }, align 8
; use: ptr @.str.hello
```

(retain and release on it are no-ops). `zell_string_literal(bytes, len)` creates
the same kind of immortal string at run time. Call it at most once per literal, for
example in a module initialiser, because each call leaks one object by design. In
C, `ZELL_STATIC_STRING(name, "lit")` declares a static literal, and
`static ZELL_STRING_STORAGE(N) x = ZELL_STRING_INIT(x, "lit");` one with
storage of its own (N = bytes + 1).

The runtime's test build `make CFLAGS_EXTRA=-DZELL_STRESS_NONUL` writes a
non-NUL byte at `data[len]` of every owned string it creates, so tests catch
code that relies on the terminator.

### 4.2 Functions

All `zell_string *` returns are **owned (+1)**. All string arguments are borrowed
and may be NULL.

| Function | Semantics |
|---|---|
| `zell_string_new(const char *bytes, int64_t len)` | copy of `len` bytes |
| `zell_string_from_cstr(const char *s)` | copy of a NUL-terminated C string (NULL gives `""`) |
| `zell_string_literal(const char *bytes, int64_t len)` | immortal copy (see above) |
| `const char *zell_string_data(s)` | **[borrowed]** pointer to the bytes (`s->data`), **not** NUL-terminated for a view. `""` for NULL |
| `const char *zell_string_cstr(s)` | the bytes NUL-terminated, for C APIs (paths, `strtod`, `getaddrinfo`, `%s`): `s->data` itself when `data[len] == 0`, otherwise a copy in a per-thread ring of 16 buffers, valid until the 16th next call on the same thread (use it within one C call; do not keep it). `""` for NULL |
| `int64_t zell_string_len(s)` | length in bytes (`len(s)`) |
| `int64_t zell_string_rune_count(s)` | number of code points. Each invalid byte counts as 1 |
| `zell_string_concat(a, b)` | `a + b`. Always returns a new object |
| `zell_string_append(a, b)` | `a + b`, **consuming** the caller's reference to `a`. If `a` is uniquely referenced it is extended in place (with spare capacity, so `s += x` in a loop is linear); otherwise like `concat` followed by `release(a)`. `b` may be `a` |
| `int32_t zell_string_compare(a, b)` | bytewise comparison, returns -1, 0 or 1 (for `<`, `<=` and so on) |
| `bool zell_string_eq(a, b)` | byte equality (`==`). `NULL == ""` |
| `uint64_t zell_string_hash(s)` | 64-bit FNV-1a of the bytes. Stable within a process |
| `zell_string_slice(s, lo, hi)` | `s[lo:hi]` by **bytes**: `s` itself (retained) for the whole string, a view for pieces of at least 64 bytes, else a copy (4.1). Panics unless `0 <= lo <= hi <= len` |
| `uint8_t zell_string_byte_at(s, i)` | `s[i]`. Panics on out-of-bounds |
| `int32_t zell_string_decode_rune(s, i, int64_t *width)` | decodes the UTF-8 rune starting at byte `i` and stores its byte width (1..4) in `*width` (which may be NULL). An invalid sequence gives `U+FFFD` with width 1. Panics if `i` is out of range. Use it to lower `for i, c in str`: advance `i += width` |
| `int64_t zell_string_index(s, sub)` | byte offset of the first occurrence, or -1. An empty `sub` gives 0 |
| `bool zell_string_contains / has_prefix / has_suffix(s, x)` | as the names say |
| `zell_string_from_int(int64_t)` / `from_uint(uint64_t)` | decimal |
| `zell_string_from_float(double)` | shortest decimal that round-trips, `%g` style: `0.5`, `3`, `0.1`, `1e+21`, `NaN`, `+Inf`, `-Inf`. Note that `3.0` prints as `3`, as in Go |
| `zell_string_from_bool(bool)` | `"true"` or `"false"` |
| `zell_string_from_rune(int32_t)` | UTF-8 encoding. An invalid code point gives U+FFFD |
| `int64_t zell_string_to_int(s, zell_error **err)` | base-10 with an optional sign (`+` or `-`). No surrounding whitespace (any `isspace` byte) is allowed. On success `*err = NULL`. On failure it returns 0 and sets `*err` to an owned error (`invalid integer: "..."` or `integer out of range: "..."`). `err` may be NULL |
| `double zell_string_to_float(s, zell_error **err)` | `strtod` syntax (it also accepts `inf`, `nan` and hex floats). The error convention is the same |

Conversions between strings and `[]byte` are in section 5.

---

## 5. Slices

### 5.1 Representation

```c
typedef struct zell_slice {   // 32 bytes, 8-aligned
    void         *data;        // +0   pointer to element 0 of this view
    int64_t       len;         // +8
    int64_t       cap;         // +16  elements available from data
    zell_object *backing;     // +24  OWNED reference to the backing array, or NULL
} zell_slice;
```

```llvm
%zell_slice = type { ptr, i64, i64, ptr }
```

- `{null, 0, 0, null}` is the nil or empty slice, and it is valid everywhere.
- A slice is a **value type** held in an `alloca` or a struct field. Every live
  slice value owns one reference to `backing`.
- **Copy** with `zell_slice_assign(&dst, &src)`, or memcpy followed by
  `zell_slice_retain`. **Drop** with `zell_slice_release(&s)`, which also zeroes it.
- Sub-slices share the backing array, as in Go, so writes through one are visible
  through the other.
- `elem_size` and `ops` are **not stored in the slice**. The compiler passes them to
  every call (it knows them statically). The backing array keeps its own copies so
  it can release the elements when it dies.

The backing array is an object with this layout (for debugging only, do not use it
directly):
`{ zell_object hdr; int64 elem_size @16; int64 cap @24; const zell_value_ops *ops @32; pad @40; elements @48 }`.

**Element ownership invariant.** The backing array owns one reference for every
non-zero element slot in `[0, cap)`, and it releases all of them when freed.
Writing an element through `zell_slice_index` is therefore a normal store: retain
the new value, release the old one, write.

### 5.2 Functions

`elem_size` is `sizeof(T)` in bytes. Elements are guaranteed 8-byte alignment.

| Function | Semantics |
|---|---|
| `zell_slice_new(out, elem_size, len, cap)` | `make([]T, len, cap)` with POD elements. Zeroed. Panics unless `0 <= len <= cap`. `cap == 0` produces the nil slice. `*out` is overwritten, not released |
| `zell_slice_new_ops(out, elem_size, len, cap, ops)` | the same, with element ownership `ops` |
| `zell_slice_from(out, elem_size, ops, const void *elems, n)` | slice literal `[]T{...}`: copies `n` elements from a C array and **retains** each one (the source stays owned by the caller) |
| `zell_slice_retain(const zell_slice *s)` | +1 on `s->backing` |
| `zell_slice_release(zell_slice *s)` | -1 on `s->backing`, then `*s = {0}` |
| `zell_slice_assign(dst, src)` | `dst = src` (retains src, releases the old dst, copies). `dst == src` is safe |
| `void *zell_slice_index(s, i, elem_size)` | **[borrowed]** address of element `i`. Panics `index out of range [i] with length len` unless `0 <= i < len`. The compiler may instead inline `i <u len` and call `zell_panic_bounds` |
| `zell_slice_append(zell_slice *s, elem_size, ops, const void *elem)` | `s = append(s, elem)` **in place**. The element is copied and retained. If `len < cap` it writes into the shared backing, like Go. Otherwise it allocates a new backing array (growth: at least 4, doubling below 1024 elements, then x1.5), copies and retains the elements, and releases the old backing; if `s` is the only reference to its backing (and starts at its first element) the backing is instead resized in place and the elements keep their references. `elem` may point into `s` itself. For `t = append(s, x)` with `t != s`, first do `zell_slice_assign(&t, &s)`, then append to `t`. The compiler inlines the `len < cap` case (store the element by the 2.3 store rule, `len += 1`) and calls this function only when `len == cap` |
| `zell_slice_append_slice(s, elem_size, ops, const zell_slice *other)` | `s = append(s, other...)` in place. `other` may alias `s` (memmove semantics) |
| `zell_slice_sub(out, s, lo, hi, elem_size)` | `s[lo:hi]`, with `cap = s.cap - lo`. Panics unless `0 <= lo <= hi <= cap`. Returns a +1 reference in `*out`. If `out == s` (`s = s[lo:hi]`), the old reference is released. Otherwise `*out` is overwritten without being released, so release it first if it held a slice |
| `int64_t zell_slice_copy(dst, src, elem_size, ops)` | Go `copy(dst, src)`. Copies `min(len)` elements with memmove semantics (overlap is fine), retains the copied values and releases the overwritten ones. Returns the count |
| `zell_string *zell_string_from_bytes(const zell_slice *b)` | `string(b)`, a copy (`elem_size` 1) |
| `zell_string_to_bytes(zell_slice *out, const zell_string *s)` | `[]byte(s)`, a copy |

---

## 6. Maps

### 6.1 Representation

`map[K]V` is a pointer to an opaque refcounted object (`zell_map *`). Assigning a
map variable copies the pointer (retain/release), giving reference semantics as in
Go. NULL is the nil map (see 2.4).

Keys and values are stored inline as `key_size` and `val_size` bytes. The map
**retains** the keys and values it stores (using `key_ops` and `val_ops`) and
releases them when it overwrites them, deletes them or is destroyed.

```c
typedef uint64_t (*zell_hash_fn)(const void *key);          // key points to key_size bytes
typedef bool     (*zell_eq_fn)(const void *a, const void *b);

typedef struct zell_map_desc {       // 48 bytes; copied by zell_map_new
    int64_t key_size;                 // +0   > 0
    int64_t val_size;                 // +8   >= 0 (0 for a set)
    zell_hash_fn hash;               // +16
    zell_eq_fn   eq;                 // +24
    const zell_value_ops *key_ops;   // +32  may be NULL
    const zell_value_ops *val_ops;   // +40  may be NULL
} zell_map_desc;
```

Built-in hash and eq functions: `zell_hash_int` / `zell_eq_int` (the key is an
`int64_t`), `zell_hash_str` / `zell_eq_str` (the key is a `zell_string *` slot,
compared by content), and `zell_hash_bytes(p, n)` for building your own. For
struct keys, the compiler generates `hash` (for example by combining
`zell_hash_bytes` over the fields, or calling `zell_string_hash` for string
fields) and a field-wise `eq`. Do not memcmp a struct that has padding unless the
padding is zeroed.

### 6.2 Functions

`key` and `val` arguments point to `key_size` and `val_size` bytes. They are
borrowed.

| Function | Semantics |
|---|---|
| `zell_map *zell_map_new(const zell_map_desc *d, int64_t hint)` | new empty map, +1. `hint` pre-sizes the table for that many entries |
| `zell_map_new_str(val_size, val_ops)` | `map[string]V`. The key slot is `zell_string *` (with `zell_ops_ref`) |
| `zell_map_new_int(val_size, val_ops)` | `map[int]V`. The key slot is `int64_t` |
| `int64_t zell_map_len(m)` | number of entries (0 for nil) |
| `void *zell_map_get(m, key)` | **[borrowed]** pointer to the value slot, or NULL if absent. The pointer is valid until the next `set`, `get_or_insert` or `delete` on the map. `v, ok = m[k]`: `ok = p != NULL`, and `v` is `*p` (retain it) or the zero value |
| `void zell_map_set(m, key, val)` | `m[k] = v`. Inserts (retaining key and value) or overwrites (retains the new value, releases the old one). Panics `assignment to entry in nil map` if `m` is NULL |
| `void *zell_map_get_or_insert(m, key)` | **[borrowed]** value slot for `key`, inserting a zeroed value if absent. Use it for `m[k] += 1` and `m[k].f = x`. Writing to the slot follows the store rule in 2.3. Panics on nil |
| `bool zell_map_delete(m, key)` | removes the entry (releasing the key and value). Returns whether it existed. A nil map gives false |
| `zell_map_get_str / set_str / delete_str(m, zell_string *key, ...)` | fast paths for string-keyed maps. The key is passed directly, not by pointer |
| `zell_map_get_int / set_int / delete_int(m, int64_t key, ...)` | fast paths for int-keyed maps |
| `void *zell_map_get_or_insert_str(m, zell_string *key)` / `zell_map_get_or_insert_int(m, int64_t key)` | **[borrowed]** `get_or_insert` fast paths (added in v0.1.1; the compiler uses them for `m[k] op= v` and `m[k].f = x` on string/int keys) |

The `_str`/`_int` entry points require a map created for that key type
(`zell_map_new_str`/`_int`, or `zell_map_new` with `zell_hash_str`/`zell_eq_str`
or `zell_hash_int`/`zell_eq_int`, which select the same specialised paths).

**Implementation (not ABI):** open addressing in SwissTable style: one
control byte per slot (empty, deleted or 7 hash bits), groups of 8 matched
with SWAR, triangular probing over groups, power-of-two capacity, growth at
7/8 load. Entries live inline in one table and move only when it is resized,
so a value pointer from `get`/`get_or_insert` stays valid until the next
insertion of a new key or `delete`. String keys are hashed internally with a
word-at-a-time hash (`zell_string_hash` itself stays FNV-1a); int keys with a
64-bit mixer.

### 6.3 Iteration

```c
typedef struct zell_map_iter { zell_map *map; int64_t bucket; void *next; int64_t reserved; } zell_map_iter; // 32 bytes, private
void zell_map_iter_init(zell_map_iter *it, zell_map *m);
bool zell_map_iter_next(zell_map_iter *it, void **key, void **val);  // [borrowed] slots
```

To lower `for k, v in m`: `alloca` an iterator, call `init`, then loop on `next`.
`key` and `val` receive pointers into the map (either may be NULL if not wanted).
Copy and retain them into the loop variables.

- The iterator does **not** retain the map. The compiler must keep the map alive
  for the duration of the loop. It usually already holds it in a variable;
  otherwise retain it before the loop and release it after.
- The order is unspecified (it is table-slot order, not randomised).
- **Deleting the entry just returned is allowed.** Inserting into the map, or
  deleting any other entry, during iteration is undefined behaviour (entries may
  be skipped or visited twice, and the iterator may read freed memory).

---

## 7. Errors

### 7.1 Representation

`error` values are pointers to immutable refcounted `zell_error` objects. NULL
means no error.

```c
typedef zell_string *(*zell_message_fn)(void *payload);  // returns an OWNED string

typedef struct zell_error {            // 224 bytes
    zell_object hdr;                   // +0   type = &zell_type_error
    zell_string *message;              // +16  own message, or the wrap context (owned)
    struct zell_error *inner;          // +24  wrapped error (owned) or NULL; for a typed error
                                       //      whose struct has `Unwrap() error`, the compiler
                                       //      stores that result here right after boxing
    const zell_type_info *user_type;   // +32  type of a boxed user error, or NULL
    void *payload;                      // +40  boxed user value (a zell object, owned) or NULL
    zell_message_fn message_fn;        // +48  user Message() thunk or NULL
    // error trace (7.4), all zero for an error without one:
    struct zell_error *origin;         // +56  error this is a copy-on-write copy of (owned) or NULL
    const zell_error_site *created;    // +64  creation site or NULL
    int64_t nframes;                    // +72  number of `returned via` frames
    zell_error_frame frames[8];        // +80  first 8 frames {site, ctx}
    zell_error_frame *overflow;        // +208 frames 8.. (raw heap array) or NULL
    int64_t overflow_cap;               // +216
} zell_error;
```

Treat the fields as read-only and use the functions below. The layout is
documented so the compiler can emit **static sentinel errors** directly:

```llvm
; const ErrNotFound = error("not found")
@ErrNotFound = constant { i64, ptr, ptr, ptr, ptr, ptr, ptr, [168 x i8] }
    { i64 -1, ptr @zell_type_error, ptr @.str.not_found, ptr null, ptr null, ptr null, ptr null,
      [168 x i8] zeroinitializer }   ; the trace fields stay zero
```

`zell_error_sentinel(msg)` creates an immortal error at run time instead (for
example in a module initialiser).

### 7.2 Functions

| Function | Semantics |
|---|---|
| `zell_error_new(const zell_string *msg)` | `error(msg)`, +1. Retains `msg` |
| `zell_error_new_cstr(const char *msg)` | the same, from a C string |
| `zell_error_sentinel(msg)` | immortal error (for `const E = error("...")`). Never freed |
| `zell_error_new_typed(const zell_type_info *type, void *payload, zell_message_fn fn)` | boxes a user error (any struct with `Message()`). `payload` must be a zell heap object holding the struct value (for example `zell_object_new(&NotFound_type, 16 + sizeof(NotFound))` with the fields copied in). It is **retained**. `fn(payload)` is the compiler-generated thunk that calls the user's `Message()` and returns an owned string |
| `zell_error_wrap(zell_error *inner, const zell_string *ctx)` | `err?("ctx")` and `errors.Wrap(err, ctx)`: a new error whose own text is `ctx`, wrapping `inner` (retained). **`inner == NULL` returns NULL** |
| `zell_error_unwrap(const zell_error *e)` | `errors.Unwrap`: the inner error, +1, or NULL |
| `zell_string *zell_error_message(const zell_error *e)` | owned full text: own text, then `": "` plus the text of each inner error down the chain, for example `startup: loading config: file missing`. The own text of a typed error is `message_fn(payload)`, and the chain stops after a link with a `message_fn` (a user `Message()` is the whole text; its `inner` is the `Unwrap()` cause). NULL gives `"<nil>"` |
| `bool zell_error_is(const zell_error *err, const zell_error *target)` | `errors.Is`: walks `err`, `err->inner`, ... and returns true if any link **is the same object** (pointer identity) as `target`, where a copy-on-write copy (7.4) counts as its `origin`. Sentinels work because they are unique objects. `is(NULL, NULL)` is true and `is(x, NULL)` is false for non-NULL `x` |
| `void *zell_error_as(const zell_error *err, const zell_type_info *type)` | `errors.As`: the **[borrowed]** payload of the first link with `user_type == type`, or NULL. The compiler copies (and retains) the struct out of `payload + 16` into the target |
| `zell_err_eof` | immortal sentinel `"EOF"`, returned by `zell_read_line` |

Note: DESIGN.md says `errors.Is` compares "by value". In 0.1 the runtime compares
identity, which matches sentinel usage. Value comparison of typed errors, if
wanted, can be added by the compiler before calling `zell_error_is`.

### 7.3 Recommended lowering of `T!` (compiler-side, not enforced)

- `T!` is the pair `{ T value; ptr err }`. It is returned through an sret
  out-pointer or as an LLVM first-class aggregate between Zell functions; this
  never crosses the runtime ABI. `err == null` means success.
- `return error("x")` becomes `{zero, zell_error_new(@.str.x)}`, and `return v`
  becomes `{v, null}`.
- `e?` becomes: if `err != null`, release any partially built locals and return
  `{zero, err}`, moving the reference.
- `e?("ctx")` becomes: `w = zell_error_wrap(err, @.str.ctx)`, then
  `zell_release(err)`, then return `w` (with traces on, both steps are
  `w = zell_error_returned(err, @site, @.str.ctx)`, see 7.4).
- Inside a block `try { } catch err { }`, `e?` does not return: it releases
  the temporaries and block locals of the `try` body, stores the error and
  branches to the `catch` block.
- `try e catch err { ... }` binds `err` (owned) and releases it at the end of the
  catch scope. `try e else d` releases `err` and uses `d`.
- Runtime functions that can fail use `zell_error **err` out-parameters
  (`*err = NULL` on success, an owned error on failure), or they return
  `zell_error *` directly (NULL = ok).

### 7.4 Error traces

Every error can carry a *return trace* (DESIGN.md "Трасса ошибки"): its
creation site plus one frame per `?` it passed and per `try ... catch` that
caught it. Sites are static, read-only records emitted by the compiler:

```c
typedef struct zell_error_site {   // 24 bytes
    const char *file;               // +0  source path as given to the compiler
    const char *func;               // +8  "Open", "Cache.Get", "main$lambda1"
    int64_t line;                   // +16 1-based
} zell_error_site;

typedef struct zell_error_frame {  // 16 bytes
    const zell_error_site *site;
    zell_string *ctx;              // the `?("ctx")` context (owned) or NULL
} zell_error_frame;
```

| Function | Semantics |
|---|---|
| `zell_error *zell_error_created(zell_error *err, const zell_error_site *site)` | **consumes** `err`, returns it (or its copy) owned. Sets the creation site unless one is already set. NULL err -> NULL. The compiler calls it after `error(..)` / `errors.New`, after boxing a struct as `error`, on the result of `errors.Wrap`, on errors returned by fallible stdlib externs (the call is the creation site), and on the value of `return err` (so a sentinel gets the site of its `return`) |
| `zell_error *zell_error_returned(zell_error *err, const zell_error_site *site, const zell_string *ctx)` | **consumes** `err`, returns the error to propagate (owned). With `ctx` it first wraps (`zell_error_wrap`) and moves the trace (in the object or pending) into the wrapper; then it appends the frame `{site, ctx}`. Used by `e?` / `e?("ctx")` (also inside a block `try`) |
| `zell_error *zell_error_caught(zell_error *err, const zell_error_site *site, int64_t keep)` | **consumes** `err`, returns the error to bind (owned): the end of a propagation. With `site` it first appends the frame `{site, NULL}` (like `zell_error_returned`). Then it *settles* a pending trace (below): `keep != 0` attaches it to the error (to `err` itself when uniquely owned, else to a copy-on-write copy), `keep == 0` drops it. Called by `try e catch err` before `err` is bound (`site` = the `try`, `keep` = the compiler's judgement whether the handler can observe the trace: any use of `err` other than `err.Message()` and `errors.Is(err, ..)`), by a block `try`'s catch binding (`site` NULL) and, with `site` NULL and `keep` 1, on the error of an async function's result before `zell_task_complete` publishes it (the record is thread-local) |
| `void zell_error_discarded(zell_error *err)` | drops the pending trace of `err` (if any) and releases it: `try e else d`, `try e catch _ { }` |
| `zell_string *zell_error_trace(const zell_error *err)` | owned trace text, `""` for NULL or no trace. Format below |
| `void zell_error_frames(const zell_error *err, zell_slice *out)` | owned `[]errors.Frame`: one 32-byte record `{string File @0, int64 Line @8, string Func @16, string Context @24}` per trace line (`Context` is the `?("...")` text or NULL), elements owned via `zell_ops_error_frame` |
| `zell_errors_trace` / `zell_errors_frames` | the `errors.Trace` / `Frames` externs (same functions) |
| `void zell_error_report(const zell_error *err)` | writes `error: <message>`, then the trace (if any), to stderr. The C `main` calls it when `func main() void!` (or `async func main() void!`) returned an error, before `zell_runtime_shutdown` (then releases the error), and then exits with code 1 |

- **Storage**: the first 8 frames live inline in the error object; more go to
  a growable raw array (`overflow`, doubling from 8). One `?` costs storing
  two pointers.
- **Shared errors, pending traces**: the trace of a *shared* error
  (`refcount != 1`, including immortal sentinels) is never written into the
  object. `zell_error_created` on a shared error without a trace pushes a
  thread-local *pending record* `{err, created, sites[24]}` (a stack of 8
  per thread: records nest when a defer or a handler propagates another
  error while one is in flight); `zell_error_returned` of a shared error
  with a record appends to it and returns `err` unchanged, so propagating a
  sentinel allocates nothing. `zell_error_caught` settles the record (see
  the table), `zell_error_trace`/`zell_error_frames` read it when the
  object has no trace of its own. When the record stack is full or a trace
  outgrows a record, the functions fall back to **copy-on-write**: a
  shallow copy (message, inner, payload retained; `origin` = the original's
  root, retained; the trace copied or moved from the record) replaces the
  shared error. The same copy is made for a shared error without a record
  (one stored elsewhere and returned again). Other holders never see a
  changed trace; `zell_error_is` treats a copy as its `origin`. A unique
  error (`refcount == 1`) that still has a record (its other holders went
  away) adopts the record into the object first.
- **Text** (`errors.Trace`): lines joined by `\n`, no trailing newline:
  `  created at   <loc>  <func>` (if there is a creation site), then for each
  frame, oldest first, `  returned via <loc>  <func>[  ?("<ctx>")]`, where
  `<loc>` = `<file>:<line>` is padded to the longest location, `<func>` is
  padded to the longest function name when a context follows, and trailing
  spaces are trimmed.
- `--no-error-trace` builds emit no sites and no calls (`?("ctx")` calls
  `zell_error_wrap` directly), so traces are empty.

---

## 8. Panics

All panic functions are `noreturn`. Declare them with the LLVM `noreturn`
attribute and follow the call with `unreachable`.

| Function | Message (after `panic: `) |
|---|---|
| `zell_panic(const char *msg)` | `msg` (a C string; NULL gives an empty message) |
| `zell_panic_str(const zell_string *msg)` | the string |
| `zell_panic_error(const zell_error *err)` | `zell_error_message(err)` (for `panic(err)`) |
| `zell_panic_nil()` | `runtime error: invalid memory address or nil pointer dereference` |
| `zell_panic_bounds(int64 idx, int64 len)` | `runtime error: index out of range [idx] with length len` |
| `zell_panic_slice_bounds(int64 lo, int64 hi, int64 cap)` | `runtime error: slice bounds out of range [lo:hi] with capacity cap` |

Behaviour of a panic that ends the process (on the main thread, on a
thread outside the scheduler, in a job without a current task, or a
panic while unwinding another one):

1. `stdout` is flushed, so output printed before the panic is not lost.
2. `panic: <msg>\n` is written to stderr.
3. Unless `ZELL_NO_BACKTRACE` is set, a blank line, then
   `backtrace (most recent call first):`, then one line per frame: Zell
   code found in the line table (8.2) as `  <file>:<line> <function>`,
   anything else as `module(symbol+0xoff)[0xaddr]` (glibc
   `backtrace_symbols_fd` format; `module(+0xoff)[0xaddr]` without symbols;
   Windows: `module(symbol+0xoff) [0xaddr]`). The runtime's own panic
   entry points (`zell_panic*`, `zell__panic*`) are never listed: the
   first line is the frame that panicked.
4. The process exits immediately with **exit code 2** (`_Exit`). No `defer`, no
   destructors, no atexit handlers.

If two threads end the process with a panic at the same time, only the
first message is printed. Out-of-memory in any runtime allocation panics
with `out of memory (...)`.

### 8.1 Task panics (DESIGN.md "Паника")

A panic in a scheduler job whose thread has a current task (10.11) ends
that task only, on every platform:

1. **Unwind point.** `run_job` (task.c) runs every job under a jump
   buffer (`zell__job_unwind`, thread-local, with the value of
   `zell_coro_chain` at the job's start). The jump restores registers
   only, without SEH or DWARF unwinding and without touching the signal
   mask (`zell__plat_setjmp` / `zell__plat_longjmp`, platform.h):
   `__builtin_setjmp` on x86-64 POSIX, `_setjmp` / `_longjmp` on arm64
   (which has no `__builtin_longjmp`), and on Windows `_setjmp(buf, NULL)`
   with `longjmp`, which for a NULL frame restores the registers without
   an SEH unwind (`__builtin_setjmp` is miscompiled for Win64 frames,
   whose frame pointer is offset from the frame address).
2. **Frame chain.** `_Thread_local void *zell_coro_chain` (exported, read
   and written by compiled code) lists the coroutine frames and the
   synchronous frames running on the thread, innermost first. A
   coroutine's node is `{ ptr prev; ptr hdl }` in the frame (an alloca of
   the coroutine; `hdl` is the frame handle, or NULL when no frame was
   allocated: `coro.alloc` false, so there is no destroy function). A
   synchronous frame's node (item 4a) has a tag in place of `hdl`:
   `ZELL_SYNC_CLEANUP` (`(void *)2`) for `{ prev, tag, fn, sp }`,
   `ZELL_SYNC_FRAME` (`(void *)1`) for `{ prev, tag, jmpbuf }` (the jump
   buffer at offset 16). The coroutine pushes its node after `coro.begin`
   and again whenever it continues after an `await` that suspended or
   tried to (the resume edge of `coro.suspend` and the
   `zell_task_add_continuation`-returned-false edge); it pops it right
   before `coro.save` (so before `zell_task_add_continuation`: the frame
   may resume elsewhere at once) and at the start of the return path
   (after the defers and the release of the locals). Emitted for every
   target; on Windows the compiled code reaches the runtime's
   `_Thread_local` through native TLS (`_tls_index`), which is why the
   MinGW runtime is built by clang, not GCC (emulated TLS).
3. **The panic** (`die` in panic.c, after the debug agent's panic stop):
   the message and the backtrace as text (`zell__plat_backtrace_buf`) are
   kept thread-locally, then every frame on the chain down to the job's
   start is popped, innermost first: a coroutine frame is **destroyed**
   by calling its LLVM destroy function (word 1 of a switched-resume
   frame); a synchronous frame's cleanup is **called** (`fn(sp)`, a
   `ZELL_SYNC_CLEANUP` node) or the frame is **entered** by
   `zell__plat_longjmp` into its node's buffer (a `ZELL_SYNC_FRAME` node,
   item 4a), whose landing code ends with `ZELL_NORETURN void
   zell_unwind_resume(void)`, which goes on with the next node (only
   thread-locals carry the state: the frames above it are gone). At the job's start the runtime `zell__plat_longjmp`s to
   `run_job`, which restores the chain and the thread's context and
   current task as after any job. A message string the panic entry point
   made itself (`zell_panic_error`) is released once copied.
4. **The destroy path** (compiler side). Every suspend point's destroy
   edge (`coro.suspend` result 1) goes to one `coro.unwind` block; it is
   only ever reached through the runtime's destroy call, from any
   suspend index (also index 0 for a ramp that has not suspended yet: the
   frame is zeroed). It runs the armed open-coded defers and the defer
   array (each open-coded defer is disarmed before it runs, so a defer
   that panics at the normal exit is not run again), releases every
   local like the normal exit (`release_locals`), releases the task the
   frame was awaiting while it owned it (frame slot `coro.awaiting`), then
   calls `zell_coro_unwound(load task.slot, load ctx.slot)` and frees the
   frame (`coro.free`). So that this path sees the frame as it is *now*
   and not as of the last suspension, a coroutine that has defers or
   locals that own something makes the defer flags, the defer array and
   every local slot escape at its entry (an empty `asm sideeffect` with
   each address and a memory clobber): their stores stay in memory and
   are never dropped as dead. Other coroutines escape only
   `coro.awaiting` and skip the defers and locals.
4a. **Synchronous frames** (compiler side, `codegen/unwind.rs`,
   `codegen/unwind_lower.rs`). A function that is not a coroutine (a
   plain `func`, a closure, a plain never-suspending `async func`, 10.3)
   emits every call that may panic, while it has something to clean up
   (a `defer`, a local that may own something, a held temporary, a saved
   context), as an LLVM `invoke` whose cleanup landing pad releases the
   held temporaries live at that call (the cleanup stack,
   `codegen/cleanup.rs`, also the operand of `panic(...)`) and branches
   to the function's unwind block: the armed defers (each disarmed before
   it runs), the locals as at the normal exit (`Drop` in reverse
   declaration order), the saved context of a plain async function (as
   `zell_coro_leave`), `resume`. Releases and the runtime's
   retain/release, `zell_dbg_*` and string comparisons are plain calls.
   Arguments moved into a callee leave the caller's cleanup before the
   call. The optimiser sees the real control flow (values stay in
   registers, an `invoke` of a callee that cannot panic becomes a call and
   its landing pad goes); no DWARF or SEH unwinder is used: after
   optimisation every function that still has an `invoke` is lowered.
   - Its node is pushed in a block that dominates every `invoke` and lies
     on no cycle (an early return that reaches no `invoke` pays nothing)
     and popped before every `ret` when it is the innermost one; a
     call-site slot (`i32`) holds the landing pad's index, stored before
     each former `invoke`, and -1 before every other call that may
     unwind (a panic there is not this frame's to clean up). Values of
     the body that the landing pads use are stored to stack slots (before
     the panicking call when only `noreturn`/`cold` calls need them,
     else after their definition) and the stack slots the landing pads
     read are accessed volatile.
   - **x86-64** (Linux, macOS, Windows): the landing pads are outlined
     into `<f>.unwind(ptr sp)`, which recovers the frame's slots with
     `llvm.localrecover` (the frame escapes them with `llvm.localescape`
     and is marked `"stackrealign"`, so the offsets are relative to its
     stack pointer after the prologue) and switches on the call-site
     slot. The node is `{ ptr prev; ptr tag = ZELL_SYNC_CLEANUP ((void
     *)2); ptr fn; ptr sp }` (`fn` = `<f>.unwind`, `sp` =
     `llvm.stacksave()`); the runtime calls `fn(sp)` on top of the
     panicking stack, where the frame is still intact, and goes on with
     the next node.
   - **Elsewhere** (arm64), or when other code enters a landing region:
     the node is `{ ptr prev; ptr tag = ZELL_SYNC_FRAME ((void *)1);
     jmpbuf }` (buffer at offset 16) and the push block ends in the
     target's setjmp (`llvm.eh.sjlj.setjmp` on x86-64 POSIX: word 0
     `llvm.frameaddress(0)`, word 2 `llvm.stacksave()`, word 1 written by
     the intrinsic, what the runtime's `__builtin_longjmp` reads;
     `_setjmp(buf, NULL)` on Windows x64, `_setjmp(buf)` elsewhere,
     `returns_twice`, as `zell__plat_setjmp`), whose second return
     switches on the call-site slot; the landing pads end in
     `ZELL_NORETURN void zell_unwind_resume(void)` (item 3).
     `ZELL_UNWIND_SETJMP=1` at build time forces this lowering on x86-64
     (tests).
   - Cost on the path that does not panic, per call of a function that
     keeps a landing pad: the push (a TLS load, four stores) and the pop
     (a TLS load, a compare, a store), one store per former `invoke` and
     per other call that may unwind, the stores of the demoted values;
     state the landing pads read stays in memory (LLVM does not keep it
     in registers across a loop that may panic). Coroutines keep plain
     calls: their held temporaries are stored to frame slots (zeroed or
     flagged off when consumed) that the destroy path releases, and they
     carry the personality `zell.personality.coro`, different from the
     `zell.personality` of functions with landing pads, so that the
     inliner never moves a landing pad into a coroutine (both are
     removed by the lowering).
5. `void zell_coro_unwound(zell_task *own, zell_object *ctx0)`: `own`
   NULL (the body never suspended, so it made no task) is
   `zell_coro_leave(ctx0)`; otherwise `own` is completed **as panicked**
   (below) and the frame's reference is released.
6. **A panicked task** is done, its result slot stays zero, and it keeps
   the panic's message and trace (owned strings). Readers:
   - `zell_task_result(t)` and `zell_task_await(t)` re-panic in the reader
     with `panic in awaited task: <msg>` (not repeated if the message
     already starts with it) and the original trace;
   - `void *zell_task_result_err(zell_task *t, int64_t err_off)` and
     `void *zell_task_await_err(zell_task *t, int64_t err_off)` (the
     compiler's reads of a `Task<T!>`; `err_off` is the byte offset of the
     error word, 0 for `void!`) store, once under the task lock, a +1
     error into the slot at `err_off` (released with the slot by its ops)
     and return the slot;
   - `void *zell_task_await_main(zell_task *t)` (program entry, 11) ends
     the process with the panic's message and trace, exit code 2.
   A panicked task dropped while no reader observed the panic prints it
   to stderr (`panic: <msg>` and the trace) from its drop.
7. **The error** of a panicked `Task<T!>`: a typed error (C payload type
   `task.Panic`, `Message()` = `panic: <msg>`) whose `inner` is the
   immortal sentinel `zell_task_err_panic()` (`task.ErrPanic`, message
   `task panicked`), so `errors.Is(err, task.ErrPanic)` holds.
   `zell_string *zell_task_panic_message(const zell_error *)` and
   `zell_task_panic_trace` (+1, `""` when the chain has no task panic)
   are `task.PanicMessage` / `task.PanicTrace`.
8. **Not unwound**: C frames of runtime callbacks between the job and
   the panic (locks they hold stay held), coroutine frames elided into
   their caller's frame (`coro.alloc` false), a synchronous frame whose
   plain release ran the `Drop` that panicked (item 4a: releases have no
   landing pad, so that nothing is released twice), and owned values in
   flight inside one runtime call (an allocation that fails while a
   container is built). Their defers do not run and what they own is
   leaked; nothing is released twice. A job with a current task whose
   chain holds no task-owning frame (a C callback running Zell code)
   prints the panic and ends the job; its task is not completed.
9. A panic while unwinding (in a deferred call or a destructor run by the
   destroy path) ends the process: the first panic's message and trace,
   `panic while unwinding the panic above:`, then the second one, exit
   code 2.
10. The debug agent's panic stop (`zell__dbg_panic_fn`, 22) happens
    before anything is unwound; the re-panic of an awaiter stops again,
    `zell_task_await_main` does not.

**Panic capture** (`src/std_testing.c`, for `testing.Panics`; not on
Windows). `int64_t zell_testing_rt_capture(void *fn, void *env)` runs the
closure `func()` (its two words) on a new OS thread inside a *capture
scope* and returns 1 if it panicked, 0 if it returned, -1 where capture is
not available (`ZELL_PLAT_HAS_PANIC_CAPTURE` is 0; then testing
re-runs the test in a child process). A panic on that thread while the
scope is active prints nothing and does not exit: the message is saved and
`longjmp` returns to the scope's start, ending the thread. None of
the steps above happen, **no `defer` and no destructor of the abandoned
frames runs** (what they owned is leaked; locks they held stay held), and
panics on other threads (tasks the function started) still end the process.
`zell_string *zell_testing_rt_capture_message(void)` returns the message
of the calling thread's last captured panic (+1, `""` if none).

---

### 8.2 Line table (`zell_lines`)

Panic backtraces (8, 8.1 `PanicTrace`) print `file:line` from a table in
the executable, also in stripped `--release` builds, the same way on ELF
(Linux, FreeBSD), Mach-O (macOS) and COFF (MinGW Windows); no DWARF is
read at run time. Like Go's pclntab, the table is ordinary read-only data
that goes through the linker.

**Producer** (`compiler/src/lines.rs`, `build.rs` `link`). Codegen always
emits DWARF line tables (`LineTablesOnly` without `-g`; every function,
helpers included, has a subprogram, so Zell code inlined into C `main` or
a helper keeps its lines). After optimisation, every internal or private
function definition gets external linkage with hidden visibility
(`lines::expose`; package objects append `.<package>` to those names), so
another object can refer to it; the symbols stay out of the dynamic symbol
table and stripped builds drop them. Before the link, the compiler reads
`.debug_line` (`__debug_line` on Mach-O) of each program object, resolves
its relocations to (section, offset) addresses, maps every row to the
global function symbol holding it, and writes one more object for the same
target (an LLVM module of its own):

- `zell_lines` (hidden, constant, 4-byte aligned, 8 for `ZLN3`): the
  table below, in an allocated read-only section (`.rodata` /
  `.data.rel.ro` on ELF, `__TEXT,__const` or `__DATA,__const` on Mach-O,
  `.rdata` on COFF);
- `zell_lines_len` (`i64`, hidden): its size in bytes;
- a constructor (`llvm.global_ctors`: `.init_array`, `__mod_init_func`,
  `.ctors`) calling `void zell_lines_register(const void *table, int64_t
  len)` (lines.c), which keeps the pointer (a table without the `ZLN2` or
  `ZLN3` magic is ignored). The constructor is what keeps the table alive under
  `--gc-sections` / `-dead_strip`; programs without it (C tests, ThinLTO
  builds, whose bitcode objects have no DWARF to read) simply have no
  table.

The table is data, not symbols or debug sections, so every `strip`
(`-s` / `-S -x` passed to the linker again, `llvm-strip --strip-all`) keeps
it. Each function's address is stored as a 32-bit offset from the field
itself (`ptrtoint(@f) - ptrtoint(&field)`), which the static linker
resolves (`R_X86_64_PC32` / `R_AARCH64_PREL32`, `IMAGE_REL_AMD64_REL32`,
`ARM64_RELOC_SUBTRACTOR` + `UNSIGNED`): the values are run-time addresses
in a PIE or ASLR image too, so the runtime applies **no load slide** and
reads no file. x86_64 Mach-O has no relocation for a difference whose
symbol is undefined in the object (the functions live in the program's
object), so on `x86_64-apple-darwin` the table has magic `ZLN3` and every
code entry holds the function's absolute 64-bit address
(`X86_64_RELOC_UNSIGNED`), which dyld rebases like any data pointer: again
a run-time address, no slide applied. Every other target emits `ZLN2`.

**Layout** (little-endian, 4-byte fields):

| Offset | Field |
|---|---|
| 0 | magic `"ZLN2"` (`"ZLN3"`: x86_64 Mach-O) |
| 4 | `u32 nfiles`, `u32 nlines`, `u32 nfuncs`, `u32 strsize`, `u32 ncode` |
| 24 | `{i32 rel; u32 size; u32 first} code[ncode]`: one per function with lines; its code is `[&rel + rel, &rel + rel + size)`; its line entries are `lines[first .. next code entry's first)` (`nlines` for the last). `ZLN3`: `{u64 addr; u32 size; u32 first}` (16 bytes, native-endian pointer), code `[addr, addr + size)` |
| | `u32 files[nfiles]`: offsets into `str` of the source paths, spelled as the compiler was given them (like error-trace sites, 7) |
| | `{u32 off; u32 fl} lines[nlines]`: per function, sorted by `off` (address − the function's start); `fl = file << 20 \| line`; an entry covers addresses up to the next one of its function; `file` `0xfff` = no line (non-Zell code, line 0) |
| | `{u32 fl; u32 last; u32 name} funcs[nfuncs]`: source range of each Zell function (`file << 20 \| first line`, last line) and its name as in error traces (namespace stripped) |
| | `char str[strsize]`: NUL-terminated strings |

One line entry per location in DWARF's line program (one per statement,
call and `await`), consecutive duplicates merged. At most 4095 files and
lines up to 2^20 − 1 are described.

**Consumer** (`runtime/src/lines.c` `zell__lines_find`, called by the
backtrace printers of `platform/posix.c` and `platform/prof_win32.c`): on
the first lookup the runtime sorts the code ranges by address; for each
return address `pc` it looks up `pc - 1`: the code range holding it, the
last line entry of that function at or below the offset gives file and
line, the innermost `funcs` range holding that line gives the function
(exact with inlining; without one, the symbol name). An absolute file
under the working directory prints relative to it. `ZELL_NO_LINES=1`
falls back to symbol+offset frames, as does an address outside the table.

Inlined calls have no frame of their own: in an optimised build a frame
shows the innermost inlined line only.

### 8.3 Fatal signals and exceptions

A real fault (not a compiled `nil`/bounds check) is not a panic: no task
containment, no `defer`, the process ends. `zell__plat_runtime_init`
(called from `zell_runtime_init`) installs the handler unless
`ZELL_NO_FATAL_HANDLER` is set (then the default action, e.g. a core dump):

- POSIX (`platform/posix.c`): `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE` with
  `SA_SIGINFO | SA_ONSTACK | SA_RESETHAND`, on a 64 KiB alternate signal
  stack per thread (the main thread at init, other threads from
  `zell__plat_thread_stack_init`, unmapped at thread exit; an alternate
  stack someone else installed, e.g. a sanitizer's, is kept). The frames
  come from the interrupted context's frame-pointer chain.
- Windows (`platform/win32.c`): a first vectored exception handler for
  faults whose PC is in the executable (access violation, in-page error,
  misalignment, illegal/privileged instruction, integer divide by zero;
  a stack overflow from anywhere), an unhandled-exception filter for the
  rest (a system DLL may probe memory under its own `__try`), and
  `SetThreadStackGuarantee(64 KiB)` per thread. Exception codes map to the
  POSIX names (`SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`). The frames come
  from unwinding the exception's `CONTEXT`.

Both report through `zell__fatal_signal` (`panic.c`):

1. `stdout` is flushed.
2. stderr gets `fatal error: unexpected signal <NAME>\n` and
   `[signal <NAME>: <what> addr=0x<fault address> pc=0x<pc>]\n` (`what`:
   `segmentation violation`, `stack overflow`, `access violation`,
   `bus error`, ...).
3. Unless `ZELL_NO_BACKTRACE` is set, the backtrace in the format of 8
   (faulting frame first).
4. Exit code **2** (`_Exit`). A second fault while reporting exits at once.

## 9. I/O

All console output goes through C stdio `stdout`, which is buffered (line-buffered
on a terminal). Output is flushed at `zell_runtime_shutdown`, `zell_exit`, on
panic, and before `zell_read_line`.

| Function | Output |
|---|---|
| `zell_print_str(s)` / `zell_print_cstr(const char *)` | the bytes as-is (NULL gives nothing) |
| `zell_print_int(int64)` / `zell_print_uint(uint64)` | decimal |
| `zell_print_float(double)` | same format as `zell_string_from_float` |
| `double zell_float32_shortest(float f)` | the double nearest to the shortest decimal that round-trips as the float32 `f` (NaN/Inf unchanged). The compiler passes a `float32` through it before `zell_print_float` / `zell_string_from_float`, so `float32(0.1)` prints `0.1` and `float32(1)/3` prints `0.33333334` (Go) |
| `zell_print_bool(bool)` | `true` or `false` |
| `zell_print_rune(int32)` | UTF-8 encoding |
| `zell_print_space()` / `zell_print_newline()` | `' '` / `'\n'` |
| `zell_println_{str,cstr,int,uint,float,bool,rune}(v)` | value plus `'\n'`, written atomically with respect to other threads |
| `zell_eprint_str(s)` / `zell_eprintln_str(s)` | to stderr |
| `zell_flush()` | `fflush(stdout)` |

To lower a variadic `println(a, b, c)`, emit `print_X(a)`, `print_space()`,
`print_X(b)`, `print_space()`, `print_X(c)`, `print_newline()`. For a single
argument, use `println_X`. An `error` argument prints `zell_error_message(err)`
(release it afterwards).

Files and stdin:

| Function | Semantics |
|---|---|
| `zell_string *zell_read_file(const zell_string *path, zell_error **err)` | whole file as an owned string. On failure it returns NULL and sets `*err` to an owned error such as `open <path>: No such file or directory`. On success `*err = NULL` |
| `zell_error *zell_write_file(path, const zell_string *data)` | creates or truncates the file and writes it. Returns NULL on success or an owned error |
| `zell_error *zell_write_file_bytes(path, const zell_slice *data)` | the same, for `[]byte` |
| `zell_string *zell_read_line(zell_error **err)` | next line of stdin without `\n` (a trailing `\r` is also removed). A last line with no newline is still returned. At EOF it returns NULL and sets `*err = zell_err_eof` (immortal, so releasing it is harmless). On a read error it returns an owned error |

These calls are blocking. DESIGN.md's asynchronous I/O is future work: it would be
built as `*_async` variants that return a `zell_task *`.

---

## 10. Tasks, async/await and the scheduler

### 10.1 Model

- **Scheduler**: N worker threads (N = the CPUs the process may use,
  `zell__plat_cpu_limit`: online CPUs capped by the affinity mask and by a
  CPU quota rounded up, Linux cgroup v2 `cpu.max` of the group and its
  ancestors or v1 `cpu.cfs_quota_us`/`cpu.cfs_period_us`, Windows a
  hard-capped job object's CPU rate; or the `ZELL_WORKERS` environment
  variable). A job is a `(fn, arg)` pair plus the task context
  and current task it runs with (10.8, 10.11). The queues are Go's:
  - every worker owns a **local run queue**, a ring of 256 jobs that only
    its owner pushes to (posts made from that worker: `zell_sched_post`,
    spawns, `task.Yield`) and that the owner and thieves pop from the head
    with a compare-and-swap; a full ring moves half of itself and the new
    job to the global queue;
  - a **run-next slot** per worker: the first continuation of a task
    completed on a worker is put there (the others go behind the ring) and
    runs as soon as the current job yields the thread, with no queue in
    between. The slot is fair: after `RUNNEXT_STREAK` (32) consecutive
    jobs taken from it while the ring holds work, one ring job runs in
    between; a slot whose owner has not taken it for `RUNNEXT_GRACE_NS`
    (20 us; a long job) is stolen;
  - one **global FIFO** for posts from other threads (the main thread, the
    timer thread, the I/O pollers, resolver threads) and ring overflow. A
    worker checks it every 61 jobs even while its own queues are busy;
  - an out-of-work worker looks in its run-next slot, its ring, the global
    queue, then **steals** half of another worker's ring (four passes over
    the workers in a random order; never a run-next slot), then polls every
    queue for up to 50 us if no other worker is polling (at most one polls
    at a time: the "spinning" token), then parks on a condition variable.
    The spinner waits with the CPU's spin hint (`pause` / arm64 `yield`)
    between polls, reads the clock every 256 polls and calls `sched_yield`
    only when more worker threads than CPUs are alive (spares). A post
    wakes one parked worker only when nobody is polling, and the woken
    worker inherits the token (Go's `wakep`), so a burst of posts wakes one
    worker per 50 us at most; a worker that found work while holding the
    token hands it to another parked worker if work is still queued.
    Posters and parkers use a store-then-load protocol (sequentially
    consistent) so a post is never missed by a worker about to park.
  - **Run-next slots of busy workers.** A run-next slot is its owner's,
    but a job that never yields (a loop that keeps finding completed
    tasks) must not starve the continuation it posted. The slot state
    carries a sequence number bumped on every fill, so a watcher can tell
    a slot that stayed full from one refilled. Only two threads watch: the
    spinner looks at the other workers' slots once per 64 polls (each look
    pulls a cache line the owner writes on every hand-off) and takes a
    slot it has seen unchanged for 20 us; and a worker about to park that
    finds only a full run-next slot (no queued work) parks with a timeout
    if no other parked worker does so already (the run-next watcher): it
    takes the slot when it wakes if the slot still holds the record it
    saw, and doubles its timeout (20 us up to 1 ms) while the slots it
    sees keep moving. A run-next post wakes a parked worker by the rule
    above, which then spins and watches, so a stuck slot is taken within
    ~20 us when a worker was parked plainly, and within ~2 ms in the worst
    case. A post made while a spinner held the token wakes nobody, and a
    spinner that then takes other work leaves it behind (e.g. a mutex
    waiter's retry, 10.6, behind a job that keeps re-taking the mutex), so
    every park has a timeout: with no watcher, the parking worker is the
    sentry and looks again after 1 ms; the others after 10 ms, which
    replaces a sentry that went to work. Both double their timeout while
    the whole pool is idle (up to 64 ms and 1 s).
  - **Network polling (netpoll, 10.9)**, when the I/O pollers are shared
    (epoll): an out-of-work worker polls the network with timeout 0 after
    the global queue and before stealing (and the spinning worker every 64
    rounds); the waits it completes post their continuations to its own
    ring, in order, never to run-next. The worker that makes the pool idle
    (`q_idle + q_blocked == q_alive`) blocks in the hub poller instead of on
    the condition variable, with the earliest I/O deadline as timeout. It
    counts as parked: a poster that sees `q_idle > 0` takes the lock and,
    with no worker on the condition variable, wakes the hub through its
    wake-up channel and reserves the spinning token for that worker, so the
    store-then-load protocol above is unchanged and covers it. The right to
    block in the hub is claimed under the scheduler lock before the worker
    is recorded as its blocker, and only the claim holder waits on the hub,
    so the wake-up channel is never drained by another thread first: no
    lost wake-up.
  - A worker that blocks inside `zell_task_await` first moves its run-next
    job to its ring and wakes a worker to steal from it; a spare worker is
    started if nobody is idle (10.5), up to the spare limit: 4 per core
    worker, at least 16 (`ZELL_SPARE_WORKERS` overrides), so a pool of N
    workers never has more than N + max(16, 4N) threads (and never more
    than 1024). Blocking is a C-API affair: compiled code awaits through
    continuations and never blocks a worker.
  Compile the runtime with `-DZELL_SCHED_STATS` to print wake-up, spin,
  steal and queue counters at shutdown. The scheduler starts in
  `zell_runtime_init`, or lazily on the first `zell_sched_post`.
- **Task** (`zell_task *`, an opaque refcounted object): a state (pending or done),
  a result slot of `result_size` bytes with optional `result_ops`, and a list of
  continuations. A task completes **exactly once**.
- **Continuation**: `(resume_fn, frame)`. When the task completes, every registered
  continuation is **posted to the scheduler** (the first one to the completing
  worker's run-next slot, the others to its ring in registration order; from
  a non-worker thread, all to the global queue). It is
  never run synchronously inside `zell_task_complete` or
  `zell_task_add_continuation`. As a result, resumption never happens on the
  completer's stack, and it may happen on any worker thread.
- `async` functions are LLVM coroutines (switched-resume ABI). When a coroutine
  awaits a pending task, it registers its own handle as a continuation and
  suspends. A worker later calls `zell_coro_resume(handle)`. A coroutine has
  **no task until it first suspends**, and none at all when another
  coroutine awaits it directly (frame-linked await, 10.3): a body that returns without suspending
  hands its result to the caller directly (10.3), so an `await` that completes
  synchronously allocates no task (and no frame when the body has no suspend
  point).

### 10.2 Functions

| Function | Semantics |
|---|---|
| `void zell_sched_post(zell_resume_fn fn, void *arg)` | runs `fn(arg)` on some worker, FIFO. `typedef void (*zell_resume_fn)(void *)` |
| `int64_t zell_sched_workers(void)` | N |
| `bool zell_sched_is_worker(void)` | whether the current thread is a worker |
| `void zell_coro_resume(void *frame)` | resumes an LLVM switched-resume coroutine. It is exactly what `llvm.coro.resume` lowers to: it calls the function pointer stored in the first word of the frame. Pass `@zell_coro_resume` as `resume_fn` and the coroutine handle as `frame` |
| `zell_task *zell_task_new(int64_t result_size, const zell_value_ops *ops)` | a new **pending** task, +1, attached under the current task and made current (10.11). `result_size` is `sizeof(T)` (0 for `Task<void>`). `ops` describes ownership of the result. (C code wrapping a callback uses `zell_task_new_plain`; compiled coroutines use `zell_task_lazy`) |
| `zell_object *zell_coro_enter(int64_t *mark)` | the ramp's first call (10.3): returns the current context, **+1** (as `zell_task_get_ctx`), and stores in `*mark` the current task's cancel-hook link number (with no current task: the thread's orphan-hook number, and the thread counts a body that runs before its first suspension with no task, until its `zell_task_lazy`, `zell_coro_leave` or `zell_coro_defer`). Compiled ramps inline it on `zell_coro_tls` (below) |
| `zell_task *zell_task_lazy(zell_task *t, int64_t result_size, const zell_value_ops *ops, zell_object *ctx0, int64_t mark)` | the coroutine's own task at its first suspension (10.3): returns `t` if it is not NULL; otherwise a new pending task attached under the current task and made current, like `zell_task_new`, whose saved caller context is `ctx0` (**+1, consumed**; from `zell_coro_enter` at the start of the ramp). The cancel hooks linked on the current task (or, with none, on the thread's orphan list) after `mark` (from `zell_coro_enter`) move to the new task: the waits the body started before suspending are cancelled with it (a `task.Any` loser). A new task is returned with **two** references: the frame's and the one the ramp hands to its caller |
| `void zell_coro_leave(zell_object *ctx0)` | a coroutine that never suspended (so made no task) returns: `ctx0` (+1, consumed) is the current context again, so whatever the body set does not leak into the caller (10.8). Inlined by compiled code: `old = tls.ctx; tls.ctx = ctx0; zell_release(old)` |
| `zell_task *zell_task_completed(int64_t result_size, const zell_value_ops *ops, const void *result)` | an already **done** plain task, +1, holding `result` (`result_size` bytes copied; the slot takes over the reference, nothing is retained). Used by callers that keep the `Task<T>` of an async call that completed synchronously |
| `void zell_task_complete(zell_task *t, const void *result)` | stores the result (copies `result_size` bytes from `result`, which is borrowed: the task **retains** it with `ops`; `result` may be NULL for void tasks or a zero result), marks the task done, wakes blocking waiters, and posts the continuations. **Panics `task completed twice`** on a second call. After it returns, `t` may already have been freed by another thread unless the caller still owns a reference |
| `bool zell_task_is_done(const zell_task *t)` | acquire-load of the state (compiled code inlines it: "Task layout" below); panics on NULL |
| `bool zell_task_add_continuation(zell_task *t, zell_resume_fn fn, void *frame)` | atomically: if `t` is still pending, registers `(fn, frame)` and returns **true** (the caller must now suspend). If `t` is already done, registers nothing and returns **false** (the caller continues immediately). Each registration fires exactly once |
| `bool zell_task_add_continuation_linked(zell_task *t, zell_resume_fn fn, void *frame)` | the same for a resumed linked coroutine (10.3, no task of its own): the continuation carries the current task and context (retained), which stay current |
| `void zell_coro_defer(zell_task *t, void *frame, zell_object *ctx0)` / `void zell_coro_register(bool own)` | frame-linked await (10.3): a linked frame's first suspension on `t` is recorded (one per thread, with the current context; `ctx0`, +1 consumed, is current again) and published by the last frame of the synchronous chain to suspend, as `zell_task_add_continuation` would for that frame (`own`: its task stops being current); a done `t` posts the resumption instead |
| `extern _Thread_local void *zell_coro_next` | the awaiter a linked frame hands over when it ends; `run_job` resumes it (10.3) |
| `void zell_coro_unwound_linked(void *word, void *awaiter, zell_object *ctx0)` / `void zell_coro_link_panic(zell_error *e, void *value, int64_t size, int64_t err_off)` | a linked frame's destroy path (the `task.Panic` error into the awaiter's word) and the awaiter reading it (re-panic for `err_off < 0`, else `{zero, e}` into `value`), 10.3 |
| `void *zell_task_result(const zell_task *t)` | **[borrowed]** pointer to the result slot, valid while `t` is alive. Panics `task result read before completion` if pending. To keep a refcounted result, copy and **retain** it. For a done task that did not panic it is `t + 224` ("Task layout"), which compiled code computes inline |
| `void zell_task_run_main(zell_main_ramp_fn ramp, void *res)` | program entry of an `async func main` (11): posts `ramp(res)` as a job (the global queue: the main thread is no worker) and blocks the calling thread (10.5) until the ramp **returned**, i.e. until main's first suspension or its synchronous end. `typedef void (*zell_main_ramp_fn)(void *res)`; `res` is the ramp's result slot `{ ptr task; T value }` (10.3), read by the caller afterwards. The ramp runs with **no current task**, as it did on the main thread (its task, made at the first suspension, is a root; the debug agent keeps it as the main record, 22), but on a worker, so spawns, `Yield`s and wake-ups before its first suspension go to that worker's local queues. A panic before the first suspension is outside a task and ends the process (8.1) |
| `void *zell_task_await(zell_task *t)` | **blocking** await: waits until `t` is done and returns the **[borrowed]** result slot. See 10.5 for where it may be used |
| `zell_task *zell_task_spawn(zell_task_fn fn, void *ctx)` | creates a task (8-byte POD result slot) and posts `fn(task, ctx)` to the pool. `typedef void (*zell_task_fn)(zell_task *task, void *ctx)`. `fn` must call `zell_task_complete(task, ...)`, either before returning or later. The job holds its own reference only **until `fn` returns**, so to complete later `fn` must `zell_retain(task)`. Returns +1 |
| `zell_task_spawn_ex(fn, ctx, result_size, result_ops)` | the same, with an explicit result type |
| `zell_task *zell_task_delay(int64_t ms)` | `task.Delay(ms)`: a `Task<void>` completed by a timer thread after `ms` milliseconds (monotonic clock). `ms <= 0` returns `zell_task_done()`. Cancelling the delay task itself (`zell_task_cancel`, e.g. a loser of `zell_task_any`) completes it at once and drops its timer with the timer's reference, as cancelling the context it was started in does. +1 |
| `zell_task *zell_task_done(void)` / `extern struct zell_task zell_task_done_task` | the **immortal completed `Task<void>`** (the function returns the object's address; compiled code uses the symbol, 10.6): what an operation that did not have to wait returns (`Delay(0)`, `Mutex.Lock` of a free mutex). Awaiting it takes the `zell_task_is_done` fast path of 10.3; retain and release are no-ops; it is never cancelled (it is done) and must never be completed. +1 nominally |
| `zell_task *zell_task_all(zell_task *const *tasks, int64_t n)` | `Task<void>` that completes once every task in the array has completed. `n == 0` returns a completed task. Read the individual results from the input tasks. +1 |
| `zell_task *zell_task_all_cancel(zell_task *const *tasks, int64_t n, const uint8_t *fallible)` | `task.All` with cancellation (the codegen of both forms): completes once every input has settled; its 8-byte result is the `int64_t` **index** of the first input that completed with an error (a non-NULL error word at `ROUND8(result_size) - 8` when `fallible[i] != 0`, `fallible` may be NULL) or a panic, `-1` if none. That first failure cancels the inputs still pending with `zell_task_cancel` (before the result completes). Inputs are retained. `zell_task_all_cancel_uniform(tasks, n, bool fallible)` takes one flag for all inputs (slice forms). +1 |
| `zell_task *zell_task_any(zell_task *const *tasks, int64_t n)` | task whose 8-byte result is the `int64_t` **index** of the first input to complete. `n` must be at least 1. +1 |

**Task layout.** Compiled code reads four words of a task inline (the
offsets are the same on every supported target; `task.c` checks them with
`_Static_assert`, and `zell_debug_task_layout` publishes state and slot for
the debuggers, 20):

| offset | word | read by |
|---|---|---|
| +24 | `int32 state`: 0 pending, **1 `TASK_DONE`**; acquire load (stored with release once the result or panic is in place) | the `await` fast path (`zell_task_is_done` inline) and the result fast path |
| +144 | `int64 hook_seq`: the task's cancel-hook link number; relaxed load | the inlined `zell_coro_enter` (the ramp's mark) |
| +200 | `zell_string *panic_msg`: NULL unless the task panicked (written before the state) | the result fast path |
| +224 | the result slot (`result_size` bytes, 8-aligned) | the result fast path: `state == 1 && panic_msg == NULL` gives `t + 224`, exactly what `zell_task_result` / `zell_task_result_err` return then (borrowed: the reader copies and retains); every other case (a panicked task, `task.Yield`'s sentinel, which is never done) calls them |

**Thread state for inlined ramps.** `extern _Thread_local zell_coro_tls_t
zell_coro_tls` (same TLS model as `zell_coro_chain`: native TLS everywhere,
also on Windows, `docs/PLATFORMS.md`) is the thread's current context and
task as the runtime keeps them:

| offset | field | meaning |
|---|---|---|
| +0 | `zell_object *ctx` | the current context (owned, 10.8) |
| +8 | `zell_task *cur` | the current task (owned, 10.11), NULL = none |
| +16 | `int64_t orphan_seq` | the orphan cancel-hook link number (10.11) |
| +24 | `int32_t orphan_on` | orphan hooks are recorded while nonzero |

Compiled code reads `cur`, `orphan_seq` and `ctx`, writes `orphan_on = 1`
(the inlined `zell_coro_enter`: `mark = cur ? cur->hook_seq : orphan_seq`,
recording on when `cur` is NULL; `ctx`, retained) and swaps `ctx` (the
inlined `zell_coro_leave`). Nothing else touches it outside the runtime.

**Task ownership rules**

1. Whoever will call `zell_task_complete` must hold a reference to the task until
   that call returns. A pending task must never be freed: its continuations would
   be lost. All runtime producers (delay, all, any, mutex, channel, spawn) follow
   this rule.
2. The continuation list holds **no** references to anything. The awaiting
   coroutine holds its own reference to the awaited task (it is a local in its
   frame) and releases it after reading the result.
3. The task's result slot owns one reference to the result (via `ops`), which is
   released when the task is freed.

### 10.3 Lowering `async func` (the protocol)

For `async func f(params) T`, the compiler emits **one** LLVM function
`void @f(params, ptr %res) presplitcoroutine` (the ramp, which is also the
coroutine). `%res` points at a caller-owned slot `{ ptr task; T value }`
(`{ ptr task }` for `Task<void>`). The caller stores into `res.task`
before the call either NULL or, for a frame-linked await (below), its own
frame handle; `f` reads it at its first suspension. Calling `f` **starts it immediately**: the
body runs synchronously on the caller's thread until its first real
suspension, as in C#. The coroutine has **no task until then**: when the
body suspends for the first time it makes its `Task<T>` (`zell_task_lazy`)
and stores it, +1 for the caller, into `res.task`; when the body returns
without ever suspending it stores NULL into `res.task` and moves its result,
+1, into `res.value`. So an async call that completes synchronously costs
no task, and when the body has no suspend point at all LLVM also drops the
frame (`llvm.coro.alloc` folds to false).

```text
entry:
  task.slot = alloca ptr                           ; the coroutine's task, NULL until it suspends
  ctx.slot  = alloca ptr                           ; the caller's context (+1)
  mark.slot = alloca i64                           ; the cancel-hook mark
  id    = llvm.coro.id(0, null, null, null)
  need  = llvm.coro.alloc(id)                      ; false when LLVM proved there is no suspend point
  mem   = need ? zell_coro_frame_alloc_uninit(llvm.coro.size.i64()) : null   ; not zeroed (rules below)
  hdl   = llvm.coro.begin(id, mem)
  store null -> task.slot
  store zell_coro_enter(mark.slot) -> ctx.slot     ; +1 (inline on zell_coro_tls, 10.2)
  ... body ...

; each `x = await e` (e : Task<U>):
  t = <evaluate e>                                 ; +1 (owned local in the frame)
  if zell_task_is_done(t) goto ready              ; fast path, optional (inline: t->state == 1)
  cur = load task.slot
  own = zell_task_lazy(cur, sizeof(T), ops_T, load ctx.slot, load mark.slot)  ; makes the task on the first call
  store own -> task.slot
  if cur == null: store own -> res.task            ; first suspension: the caller gets the task (+1)
  save = llvm.coro.save(hdl)
  if !zell_task_add_continuation(t, @zell_coro_resume, hdl) goto ready
  s = llvm.coro.suspend(save, false)               ; must come IMMEDIATELY after
  switch s: 0 -> ready, 1 -> unwind, default -> done   ; unwind: 8.1
ready:
  x = load U from zell_task_result(t); retain x if U is a reference type
                                                   ; (inline: t + 224 when done and not panicked, 10.2)
  zell_release(t)

; `x = await g(args)` of a direct call of an async function g, outside a
; coroutine (with a NULL awaiter; inside one see "Frame-linked await"):
  r = alloca { ptr task; U value }
  store null -> r.task                             ; no awaiter
  call g(args, r)
  if r.task == null: x = load r.value             ; +1, moved: no retain, no task
  else: await r.task as above, then zell_release(r.task)
; `t = g(args)` kept as a Task<U> (not awaited here):
  store null -> r.task
  call g(args, r); t = r.task ?: zell_task_completed(sizeof(U), ops_U, &r.value)

; each `return v` (after the defers and the release of the locals):
  own = load task.slot
  if own == null:                                  ; never suspended: the synchronous path
    zell_coro_leave(load ctx.slot)                 ; the caller's context again (+1 consumed)
    store null -> res.task; store v -> res.value   ; v moved, +1 for the caller
  else:
    store v into a temporary; zell_task_complete(own, &tmp)  ; the task retains v
    release v (the temporary's own reference) if T is a reference type
    zell_release(own)                              ; drop the frame's reference
  br cleanup                                       ; NO final suspend

cleanup:
  mem = llvm.coro.free(id, hdl); if mem != null: zell_coro_frame_free_sized(mem, llvm.coro.size.i64())
  br done
done:
  llvm.coro.end(hdl, false, none)
  ret void
```

Rules the compiler must respect:

- **Do not touch the frame after `zell_task_add_continuation` (or
  `zell_task_add_continuation_linked`) returns true, nor after
  `zell_coro_register`.**
  Another worker may already be resuming the coroutine, because the task can
  complete at any moment and continuations are posted immediately. Between that
  call and `llvm.coro.suspend` there must be no frame accesses, and the code
  after the suspend point in the ramp must be just the `ret` (the
  `coro.end` block): that is why the task is handed to the caller through
  `res.task` *before* `coro.save` and the ramp returns `void`, not the task
  (a value defined before the suspend and returned after it would be
  reloaded from the frame; LLVM allows exactly one fall-through `coro.end`,
  so per-suspend return blocks are not an option either). LLVM's coroutine
  splitting supports this pattern; it is the one clang uses for C++
  `await_suspend` returning `bool`.
- **The task is made at the first suspension**, not at the call: until then
  the body runs as part of the caller's current task (10.11) and under the
  caller's context, which the ramp saved (`zell_coro_enter`) and gives back
  with `zell_coro_leave` or through `zell_task_lazy`'s `ctx0` (10.8). The
  waits the body starts meanwhile hook their cancellation on the caller's
  task (10.11); `zell_task_lazy` moves the hooks linked after the ramp's
  mark onto the new task, so cancelling the coroutine's task wakes them.
  With no current task at all (the ramp of an async `main`, an async call
  from synchronous code) such hooks go on a per-thread orphan list while a
  body started without a task runs before its first suspension, and move
  the same way; the list is emptied when no such body is left.
  `zell_task_lazy` must be called before `coro.save`, and `res.task` written
  before `zell_task_add_continuation`; both happen only once per coroutine
  (`cur == null`), so resume clones never write into the caller's slot, which
  is gone by then.
- **No final suspend point.** Once the result is published with
  `zell_task_complete`, nobody ever resumes the frame again, so the coroutine
  destroys its own frame (`coro.free` + `zell_coro_frame_free`). The only
  outside caller of the destroy function is the runtime unwinding a task
  panic (8.1), which also needs the frame chain pushes and pops of 8.1
  around the suspend points.
- Use `@zell_coro_resume` as the resume function (or your own trampoline that
  calls `llvm.coro.resume`). Coroutine resumption is always performed by a worker
  thread.
- Frame memory comes from `zell_coro_frame_alloc_uninit(size)` (from a
  per-thread cache of freed frames per size class, else the object
  heap's size classes, or `malloc` when the heap is off; **not zeroed**;
  `ZELL_FRAME_POISON=1` fills it with 0xA5 to catch a read of a slot nobody
  wrote) or `zell_coro_frame_alloc(size)` (the same, zeroed), and is released
  with `zell_coro_frame_free(p)` (NULL ok) or, by the frame's own
  destroy path, `zell_coro_frame_free_sized(p, size)` with the size it
  was allocated with (the size class without a heap lookup; NULL ok),
  possibly on another thread
  (the frame then goes into that thread's cache: LIFO, no atomics, at
  most 1024 frames and 256 KB per thread, the rest goes back to the
  heap; flushed at thread exit and before the leak report; bypassed when
  the heap is off, so under ASan). An
  uninitialised frame is correct only if the destroy path (8.1: a panic
  unwinds a frame on the chain, **also one that is running and never
  suspended**) reads nothing a suspension writes: the compiler stores zero
  into every local, drop flag, defer flag and counter, the awaited-task slot
  and the task slot right after `coro.begin` and makes them escape, so the
  destroy path reads them from memory; the values LLVM spills at a suspend
  point and its **suspend index** are written only by a suspension. After
  optimisation the compiler checks every destroy function: one that loads
  the suspend index (CoroSplit's `%index`, as in unoptimised code or when
  the destroy edges of the suspend points reload different spills), or a
  frame whose destroy function it cannot identify, gets the zeroing
  `zell_coro_frame_alloc` (index 0, NULL spills, as before). Never mix
  them with `zell_alloc` / `zell_free`. The frame
  is not a refcounted object. The task owns the result, and the frame owns its
  locals. Guard the allocation with `llvm.coro.alloc` and the free with the
  result of `llvm.coro.free`: a coroutine without suspend points then
  allocates nothing.
- **Frame cache** (inlined by the compiler at -O1 and above, after the
  frame fixup): the thread-local `zell_frame_cache` is `{ void *head[36];
  uint32_t n[36]; int64_t bytes; uint32_t lim; }` (offsets 0, 288, 432,
  440; 36 = the heap's small size classes: 16-byte steps to 128, then four
  per power of two up to 16384, `zell__hp_cls`). An inlined allocation of
  `size` <= 16384 pops `head[cls(size)]` (then `n[c] -= 1`, `bytes -=`
  the class's block size; the zeroing variant clears `size` bytes) and
  calls the runtime only when the list is empty; an inlined
  `zell_coro_frame_free_sized(p, size)` pushes `p` when `n[c] < lim`,
  `(n[c] + 1) * block size <= 256 KB` and `bytes + block size <= 256 KB`,
  else calls the runtime. `lim` is 0 (nothing is cached, so inlined code
  always calls the runtime) until the runtime has seen the heap on and
  `ZELL_FRAME_POISON` off on that thread; then it is 1024.
- `await` on a direct call of an async function (`await f()`) reads
  `res.value` when `res.task` is NULL (the value is +1, moved) and otherwise
  awaits `res.task`, which the frame owns and releases after the result is
  read. Any other use of the call's `Task<T>` (a variable, an argument,
  `task.All`, a `func() Task<T>` value whose body is async) goes through
  `zell_task_completed` when `res.task` is NULL, so a `Task<T>` seen by Zell
  code is always a real task. An async function used as a value is called
  through a thunk that does this. So is an `async` method through an interface
  (its vtable slot, `(data, args) -> Task<T>`, passes a NULL awaiter; DESIGN.md
  "Интерфейсы"): an `await` of an interface call awaits that task.
- `await f()?` (`T!` result): read `{value, err}` from the result slot, retain
  `err` (and the value, if needed), release the task, then apply the usual `?`
  lowering; on the synchronous path `{value, err}` comes moved from
  `res.value`.
- `defer` inside an async function: 0.1 recommendation is to run defers at the
  logical function exit, before `zell_task_complete` / the store into `res`.

**Frame-linked await.** An `await g(args)` of a direct call inside a
coroutine makes **no task for `g`**, however deep the chain: the caller
stores its frame handle `hdl` into the first word of the slot, which
becomes the hand-off between the two frames:

| word | meaning | written by |
|---|---|---|
| `hdl` | the awaiter, read by `g` at its first suspension | the caller, before the call |
| NULL | `g` returned synchronously, `res.value` holds the result (+1) | `g`'s synchronous return |
| 1 `ZELL_LINK_LINKED` | `g` suspended without a task; after `g` ended: it returned, the result (+1) was moved into `res.value` | `g`'s first suspension |
| other | `g` was unwound by a panic: its `task.Panic` error (+1) | `g`'s destroy path |

A linked frame keeps its awaiter in its task slot (`hdl + 1`: bit 0 tells
"linked" from a task of its own, which is never odd). Nothing in the
protocol is atomic: **the registration that makes a suspended chain
resumable is published once, by the last frame of the synchronous call
chain to suspend**. The innermost frame defers its registration on the
awaited task in a thread-local record (`zell_coro_defer`), every frame
above it that is linked as well just suspends, and the first frame that
is not linked now (one with a task of its own, made now or before, or a
frame resumed earlier, i.e. the bottom of the job's stack) publishes it
with `zell_coro_register`. Until then no other thread can see any frame of
the chain; after it, every awaiter is suspended, so the leaf's end hands
over to its awaiter unconditionally.

```text
; the first suspension of a coroutine (task.slot == null):
  aw = load res.task                               ; NULL or the awaiter
  if aw != null:                                   ; linked: no task
    store aw + 1 -> task.slot
    store 1 (LINKED) -> res.task
    ctxr = retain(load ctx.slot)                   ; the caller's context
  else: own = zell_task_lazy(null, ...) as above; store own -> res.task

; awaiting a pending task t:
  linked just now: zell_coro_defer(t, hdl, ctxr)   ; ctxr current again; always suspends
  else if task.slot holds a task: zell_task_add_continuation(t, ...) as above
  else (a resumed linked frame): zell_task_add_continuation_linked(t, @zell_coro_resume, hdl)
                                                   ; under the current task, which stays current

; `x = await g(args)` inside a coroutine:
  store hdl -> r.task
  call g(args, r)
  if load r.task == null: x = load r.value         ; synchronous: +1, moved
  else:                                            ; LINKED
    <first-suspension step above>
    llvm.coro.save(hdl)
    linked just now: zell_coro_leave(ctxr)         ; the registration stays pending
    else: zell_coro_register(task.slot holds a task)   ; publishes it; no frame access after
    suspend                                        ; resumed by g's end (zell_coro_next)
    w = load r.task
    if w != LINKED: zell_coro_link_panic(w, &r.value, sizeof(U), err_off)
    x = load r.value                               ; +1, moved

; `return v` of a linked frame (task.slot odd):
  store v -> res.value                             ; moved; a T! error is settled first
  zell_coro_leave(load ctx.slot)                   ; the awaiter's context is current
  zell_coro_next = task.slot - 1                   ; the awaiter, resumed by run_job
  br cleanup                                       ; frees the frame
```

- **Publishing.** `zell_coro_register(own)` is what
  `zell_task_add_continuation` does for the registering frame (with `own`
  its task stops being current, its reference moving into the
  continuation; without, the current task is retained), applied to the
  deferred frame and its context; if the task is done already (or is
  `task.Yield`'s sentinel) the deferred frame's resumption is posted to the
  ring instead, so the registering frame always suspends. The deferred
  record holds one registration per thread; `zell_coro_defer` is only
  called by a frame whose caller is a coroutine in the middle of the
  awaiter side above, which consumes or passes it on before any other code
  runs.
- **The resume-next slot** `zell_coro_next` (thread-local, exported like
  `zell_coro_chain`): `run_job` (task.c) resumes the frame it holds after
  the job's function returned, clearing it first, in a loop, so a chain of
  N linked frames that suspended at its leaf resumes level by level with
  no task, continuation, queue, atomic or recursion per level. A slot is
  set only as the last act of a frame that ends, so it holds at most one
  frame. The whole cascade runs on the thread that resumed the leaf,
  inside one job (one `setjmp`).
- **Ownership.** The result is moved once: `g` stores it +1 into
  `res.value`, the awaiter loads it without a retain (as on the
  synchronous path). `res` lives in the awaiter's frame, which is
  suspended until `g` hands it over. The `T!` error of a linked return is
  settled (`zell_error_caught`, keep) as for a task result.
- **Current task and cancellation.** A linked frame has no task. The
  chain is registered under the registering frame's task (its own, or
  for a resumed linked frame the current one), so it resumes and runs
  under the task of its root (the first frame up the chain that has a
  task). Waits created before the first suspension hook their
  cancellation on the task current then (the root's once the root has
  suspended before, else the root's caller's, from which the root's
  `zell_task_lazy` moves them onto the root's new task, as for the body of
  any coroutine before its first suspension); waits created after a
  resume hook on the root's task. Cancelling the root (a `task.Any` loser, a
  deadline) therefore wakes the waits of the whole linked chain under it.
- **Context.** The callee keeps the awaiter's context (`ctx.slot`, +1) and
  makes it current at its first linked suspension and at its return, so
  the awaiter resumes under its own context; the deferred registration
  carries the leaf's context.
- **Panics** (8.1). A linked frame's destroy path calls
  `zell_coro_unwound_linked(&res.task, awaiter, ctx0)`: the panic becomes a
  `task.Panic` error in the awaiter's word, `ctx0` is current again and
  the awaiter goes to `zell_coro_next` (the job loop resumes it after the
  unwinding longjmp). The awaiter's `zell_coro_link_panic` then re-panics
  (`Task<T>`, `err_off < 0`: `panic in awaited task: ...`, one prefix) or
  stores `{zero, error}` into `res.value` (`T!`), exactly as
  `zell_task_result` / `zell_task_result_err` do for a panicked task. A
  job that resumes coroutine frames (`zell__job_unwind.resumed`) contains
  a panic even when no task is current.
- **Diagnostics.** The debugger and `diag` list tasks, so a linked chain
  shows as its root task only (the frames have none); the frame chain of
  the running job is unchanged (`zell_coro_chain` pushes and pops).

**Never-suspending functions** (a codegen note: the runtime sees no
difference). The compiler emits an `async func` that provably never
suspends as a **plain function** with the same external ABI: `void
@f(params, ptr %res)`, no `presplitcoroutine`, no frame, no
`zell_coro_enter`, no task and no `zell_coro_chain` node. It ignores the
awaiter word the caller stored, and every return stores NULL into
`res.task` and the result (+1, moved) into `res.value`, exactly the
synchronous return of a coroutine, so every caller shape works unchanged
(`await f()`, a kept `Task<T>` through `zell_task_completed`, function
value thunks, `task.Go`/`Spawn`, interface methods returning the task,
`zell_task_run_main` for an `async main`). The caller's context is saved
at entry (`tls.ctx`, retained) and made current again at the return like
`zell_coro_leave`, unless the body is *context-neutral* (every call in it
is a direct call of an async function or of a synchronous function that
is context-neutral itself, and it has no extern, closure, interface or
`String()` call and no `Drop`), when nothing is saved. A direct `await
f()` of a plain `f` reads `res.value` without testing `res.task`.

The analysis (`compiler/src/hir/suspend.rs`) runs on the monomorphised
HIR: an async function **may suspend** if its body awaits anything other
than a direct call of an async function proven never to suspend (task
externs, a `Task` from a variable, field, `task.All`/`Any`, a closure,
interface or function value call, or a may-suspend async function);
starting from "never suspends" for all, it iterates to the greatest
fixpoint, so recursion that only awaits itself stays plain. Every
never-suspending function is lowered, with `defer`s and destructors too:
a panic unwinds it as a synchronous frame (8.1 item 4a: defers, `Drop`,
locals and the saved context). `zell check --dump-hir` prints them as
`async(plain) func`, and the LLVM function carries the string attribute
`"zell-plain-async"`. A panic inside one behaves as before (it runs in
its caller's task, as a coroutine does before its first suspension);
optimised builds may inline it, so its caller's line can be missing from
the backtrace, as for any inlined synchronous call (8.2).

`tests/coro_lowering.ll` is a complete, working example. `make test` compiles it
with `clang -O2` and runs it. It contains four async functions (one awaiting
`task.Delay`, one awaiting the first, one without suspend points, one
awaiting both) and a `main` that awaits with `zell_task_await` and reads
synchronous results, plus frame-linked awaits: one that suspends, one that
completes synchronously and one that panics. Start from that file.

### 10.4 Other ways to create tasks

- `task.Spawn(fn)`: `zell_task_spawn[_ex]`. It runs `fn` on a worker.
- Wrapping a callback-style operation: `t = zell_task_new(...)`, retain `t` for
  the producer, and the producer later calls `zell_task_complete(t, &res)` and
  then `zell_release(t)`.

### 10.5 Blocking await (`zell_task_await`)

- **Main thread** (or any non-worker thread): blocks on a condition variable until
  the task is done. This is how the C `main` waits for an async `main`
  (section 11: `zell_task_run_main` until its ramp returned on a worker,
  then `zell_task_await_main` on its task) and how `await` works in non-async code, if the language allows it.
- **Worker thread**: it also blocks, but before blocking, if no worker is idle, the
  scheduler starts a **spare worker thread** so N runnable workers remain. Spares
  retire once the blocked worker is running again and they find no work.
  **Spare limit**: at most max(16, 4N) spares exist at once (the
  `ZELL_SPARE_WORKERS` environment variable overrides the number); a worker
  that blocks when the spares are all in use blocks without a replacement,
  so blocked callers queue up on the existing threads instead of each
  getting one (10k producers blocked in `zell_chan_send` on a full channel
  stay within N + max(16, 4N) threads; `channel_fan_in_10k` in
  `tests/test_task.c` checks this). Past the limit the pool grows only on
  a **stall**: a worker blocked over the limit re-checks every 10 ms, and
  if every worker thread is blocked, none left `zell_task_await` meanwhile
  and work is queued (nothing can run it: jobs blocked on jobs behind
  them in the queue, as in `nested_await_on_worker`), it starts one more
  spare, so such programs still finish, one thread per 10 ms of stall
  instead of one per blocked call, never beyond 1024. Blocked threads that
  another thread keeps releasing (a consumer on the main thread) are
  progress, not a stall. Blocking on a worker remains expensive (one OS
  thread per blocked call). **Compiler-generated `async` code must never
  call `zell_task_await`.** It uses the continuation protocol in 10.3
  (`await` outside an `async func` is a compile error, and the `task`
  stdlib's `Channel`/`Mutex` are lowered to `zell_task_chan_send`,
  `zell_task_chan_recv`, `zell_task_mutex_lock`, which return tasks). The
  compiler emits `zell_task_await` only on the main thread (a synchronous
  `main` waiting for an async one, section 11), which is no worker.
  `zell_mutex_lock` and `zell_chan_send`/`zell_chan_recv` are blocking
  conveniences built on `zell_task_await`, so the same rule applies to them.

### 10.6 Mutex

`task.Mutex` lives in `src/task_sync.c`. The Zell struct is three `int`
words, all zero when unlocked, that the runtime owns:

```text
struct Mutex { state int; waitHead int; waitTail int }
  state    bit 0: locked; bit 1: the guard (a short spin lock) of the queue;
           bit 2: waiters queued (set together with the queue, under the guard);
           bit 3: woken (a dequeued waiter is on its way to retry; Unlock wakes nobody else);
           bit 4: starving (FIFO hand-off mode);
           bits 5-63 reserved (0)
  waitHead, waitTail: the queue of waiting tasks, oldest first, linked through their slots
```

The rules are Go's `sync.Mutex` (DESIGN.md "Асинхронность"). **Normal
mode:** `Unlock` clears the locked bit and, unless a woken waiter is
already on its way, dequeues the oldest waiter, sets the woken bit and
posts its retry as a job (run-next of the unlocking worker, 10.1); the
retry takes the mutex if it is still free, completing the waiter's task,
or puts the waiter back at the **head** of the queue (it stays the
oldest). Meanwhile any running task may take the free mutex (barging); a
contended `Lock` on a worker spins first (up to 100 CAS attempts with a
`pause` between them, only with more than one worker, no more threads
than cores and nothing else queued on its worker). **Starvation mode:** a
waiter that has waited more than 1 ms when it loses a retry sets the
starving bit; from then on `Unlock` hands ownership directly to the oldest
waiter (the mutex stays locked, its task completes and runs next on the
unlocking worker), and `Lock`/`TryLock` neither barge nor spin: new
waiters queue at the tail. The mode ends when the queue drains or when the
waiter receiving the mutex waited less than 1 ms.

The locked bit is set only by a CAS from a state with the guard and
starving bits clear, or by the guard's holder; the guarded word changes
only by its holder's stores. A free mutex is never starving (starving is
set only while locked with a waiter queued, and a hand-off keeps it
locked), but it may carry the waiters and woken bits, so a free mutex is
not always 0.

**Inline fast paths.** The compiler emits `Mutex.Lock` as a CAS of `state`
0 -> 1 (acquire) that yields `&zell_task_done_task` (the immortal done task,
10.2, whose `await` then takes the inline done path), and `Mutex.Unlock` as
a CAS 1 -> 0 (release): locked, nobody waiting or woken, not starving, no
guard. A failed CAS (or a nil pointer) calls `zell_task_mutex_lock` /
`zell_task_mutex_unlock`, which barge, spin, queue, wake and hand off as
above. `TryLock` stays a call. The word keeps `0` = free with nothing to do
on Lock and `1` = locked with nothing to do on Unlock, so the inline
sequences did not change with the barging rules; a free mutex with
queued or woken waiters (4, 8, 12) makes the inline Lock fail over to the
runtime, which barges.

The compiled code passes `&m.state` (so the three words must stay in this
order, 8 bytes each). It is **async-aware**: waiters are tasks, woken
oldest first (normal mode) or handed the lock in FIFO order (starvation
mode). `Lock` is not cancellable. It is not re-entrant. A `Mutex` must not
be copied while in use, and one dropped with waiters leaks them (they
never complete anyway); a woken waiter's retry reads the words, so a
`Mutex` must also outlive its last `Unlock`'s retry (it does whenever its
waiters hold a reference to it).

| Function | Semantics |
|---|---|
| `zell_task *zell_task_mutex_lock(int64_t *m)` | `Task<void>` that completes once the caller **owns** the lock. A free mutex (not starving) is taken with one compare-and-swap, after a short spin on a contended one, and `zell_task_done()` is returned (no allocation; the `await` takes the 10.3 fast path). Otherwise a waiter task is queued at the tail, +1 |
| `bool zell_task_mutex_try_lock(int64_t *m)` | takes the lock if it is free and not starving (a guarded word counts as taken) |
| `void zell_task_mutex_unlock(int64_t *m)` | releases the lock and wakes the oldest waiter to retry (normal mode), or completes the oldest waiter's task, which owns it from then on (starvation mode). Panics `unlock of unlocked mutex` |

**C API**: `zell_mutex *` is an opaque refcounted object around the same
three words (released waiters when dropped).

| Function | Semantics |
|---|---|
| `zell_mutex_new()` | unlocked mutex, +1 |
| `zell_task *zell_mutex_lock_async(m)` | `zell_task_mutex_lock` on the object's words |
| `zell_mutex_lock(m)` | blocking lock (`lock_async` + `zell_task_await`), for non-async code |
| `bool zell_mutex_try_lock(m)` | takes the lock if it is free |
| `zell_mutex_unlock(m)` | `zell_task_mutex_unlock` on the object's words. Panics `unlock of unlocked mutex` |

### 10.7 Channels

`task.Channel<T>` lives in `src/task_sync.c`: a refcounted `zell_chan`
object with a ring buffer of `capacity` elements (copied and retained with
the element type's ops on send, moved out on receive) and two FIFOs of
waiting tasks, senders and receivers, **each woken exactly once**: a send
that finds a waiting receiver completes that receiver's task with the value
directly (an unbuffered channel is a pure rendezvous: a sender waits in the
queue with its value until a receiver takes it); a receive that frees a
buffer slot refills it from the oldest waiting sender and completes that
sender. A waiting task's result slot holds its queue links (and a sender's
value) until it is dequeued, so a wait costs one task and no other
allocation; an operation that does not have to wait returns a completed
task (`zell_task_done()` for a send, a task completed before it is
returned for a receive).

The Zell side (`stdlib_src/task.zl`) is `struct Channel<T> { st *chanRef }`,
copies sharing the `chanRef { h int }` box, which releases the channel
(`zell_task_chan_release`) when the last copy is dropped. The handle `h` is
the channel object's address. `Channel<T>.New`, `Send`, `Recv` and `Next`
call the typed intrinsics `rtChanNew<T>`, `rtChanSend<T>`, `rtChanRecv<T>`
and `rtChanNext<T>` (`sema/arena.rs`, `codegen/ops.rs`), which pass the
element type's size and ops and the **compiled layouts of the two receive
results**: `T!` (`{T @0; ptr err @fal_err_off}`, `void!` being just the
error pointer) for `Recv` and `(T, bool)` (`{T @0; i1 ok @tup_ok_off}`) for
`Next`, each with its size, ops and the offset of the status word. The
runtime writes a receive's result in that layout straight into the task's
slot: the value (moved in) at 0 and the status at the given offset.

| Function | Semantics |
|---|---|
| `int64_t zell_task_chan_new(capacity, elem_size, elem_ops, fal_size, fal_ops, fal_err_off, tup_size, tup_ops, tup_ok_off)` | a channel, its handle (+1, owned by the box). `capacity == 0`: unbuffered |
| `void zell_task_chan_release(h)` | drops the box's reference (buffered values are released with the channel) |
| `zell_task *zell_task_chan_send(h, const void *elem)` | `Task<void>` that completes once the value (borrowed; copied and retained) has been handed to a receiver or buffered: `zell_task_done()` when that happened at once. Panics `send on closed channel`. In a **cancelled** task (10.11) a send that would have to wait returns done without sending |
| `zell_task *zell_task_chan_recv(h)` | `Task<T!>`: the value, or the immortal error `channel closed` once the channel is closed and drained, or `task.ErrCancelled` when the receiving task is cancelled (before waiting, or while queued). +1 |
| `zell_task *zell_task_chan_next(h)` | `Task<(T, bool)>`: `(value, true)`, or `(zero, false)` where `recv` would fail (the iteration protocol: `for v in ch`). +1 |
| `void zell_task_chan_close(h)` | wakes every pending receiver with the closed error. Panics `close of closed channel`, and `send on closed channel` if senders are blocked |
| `int64_t zell_task_chan_len(h)` / `zell_task_chan_cap(h)` | buffered elements / the capacity |
| `bool zell_task_chan_is_closed(h)` | whether `Close` was called |

A queued wait reacts to two cancellations (10.11), both idempotent: of the
task that started it (a `Recv` inside a cancelled scope fails with
`task.ErrCancelled`), through a cancel hook node embedded in the wait's task
slot (no allocation; the node holds a reference to the wait until that task
prunes it once the wait is done, is dropped, or is cancelled; completing the
wait never touches the starting task), and of its own task (`task.Any`
cancels its losers), through the task's `cancel_fn`, which
`zell_task_cancel(t)` runs when `t` itself is the target. The wait header
lies past the result in the slot, so a late cancel never reads a result. A
receive that completes at once gets a task with room for the result only; a
task is still returned (the compiled `Recv` is a first-class `Task<T!>`). The
wait holds a reference to the channel while queued, so a channel with
waiters is never freed.

**Select** (one wait on several channel operations; `task.RecvAny` and the
`select` statement):

| Function | Semantics |
|---|---|
| `zell_task *zell_task_select(const zell_select_case *cs, n, has_default)` | `zell_select_case = { int64_t chan; int64_t send; void *elem; uint8_t *ok; }`; `chan == 0`: the case is off. Receive: the value is **moved** to `*elem` and `*ok` (may be NULL) is 1, or 0 with the zero value for a closed, drained channel. Send: `*elem` is copied and retained at the call (the caller keeps and releases its own); panics `send on closed channel` when that case is chosen. `Task<int>`, +1: the index of the case that fired, `-1` with `has_default` when no case is ready at the call, and `-1` for a select of a cancelled task (at the call, or while waiting; then no case fires). `elem`/`ok` must stay valid until the task is done (the compiled statement points them into its frame) |
| `zell_task *zell_task_chan_recv_any(const int64_t *hs, n, res_size, val_off, ok_off)` | `task.RecvAny`: receive cases on `n` channel handles with one destination: the result is `(int index @0, T @val_off, bool @ok_off)` of `res_size` bytes (the slot owns the value; released with T's ops). `n == 0`: `-1` at once. +1 |

When several cases are ready at the call, the scan starts at a random case
(no case starves). The call locks the distinct channels in address order
(so two selects never deadlock), takes the first ready case, or else queues
one entry per case, under those locks. An entry is a `chan_wait` header with
`sel` set (the plain waits have NULL), all in the select task's slot after
the result. Whoever dequeues an entry first claims the select (an atomic
flag in the slot); entries of a claimed select are stale: a channel that
dequeues one unlinks it and releases what it held (the queue's reference to
the select task, the channel's, a send copy) without effect. The claiming
side finishes the case after dropping the channel lock, then takes the
other entries out of their queues one channel lock at a time. Cancellation
(the starter's cancel hook, or `zell_task_cancel` on the select task) claims
the select with `-1`. `zell_task_chan_close` panics only for live senders: a
stale send entry does not count.

**C API** (tests and C code; receives have the `{ int64_t ok @0; T value @8 }`
layout, `result_size = 8 + elem_size`; `ok = 1` means a value was received and
the slot owns it, `ok = 0` means closed and drained or cancelled, and the value
is all zero):

| Function | Semantics |
|---|---|
| `zell_chan_new(elem_size, ops, capacity)` | `capacity == 0` creates an unbuffered (rendezvous) channel; otherwise it has a buffer of `capacity` elements. +1 |
| `zell_task *zell_chan_send_async(ch, const void *elem)` | as `zell_task_chan_send` |
| `zell_task *zell_chan_recv_async(ch)` | a receive in the C layout, +1 |
| `zell_chan_send(ch, elem)` | blocking send |
| `bool zell_chan_recv(ch, void *out)` | blocking receive. Writes an **owned** copy to `*out` (the caller releases it) and returns `ok` |
| `zell_chan_close(ch)` | as `zell_task_chan_close` |
| `int64_t zell_chan_len(ch)` | number of buffered elements |

### 10.8 Task context

Every thread has a **current context**: one owned `zell_object *` (NULL =
empty). The runtime never looks inside it. `task` stores a one-entry
`map[string]any` there whose value is the head of an immutable linked list of
frames `ctxFrame{key int, val any, next *ctxFrame}`: `task.With(key, value, fn)`
pushes one frame for the duration of `fn`, `task.Get(key)` walks the list,
inheritance is copying the pointer. Keys are `task.Key<T>()` (ids from
`int64_t zell_task_next_key(void)`). Cancellation lives alongside, in the task
tree of 10.11.

| Function | Semantics |
|---|---|
| `zell_object *zell_task_get_ctx(void)` | the current context, **+1** (NULL if empty) |
| `void zell_task_set_ctx(zell_object *ctx)` | makes `ctx` (borrowed; retained) current and releases the previous one |

Propagation, with no help from compiled code:

- `zell_task_add_continuation` records the registering thread's context in the
  continuation (retained); the job that resumes it runs with that context. So an
  `async` function sees, after every `await`, the context it had when it
  suspended, on whatever worker resumes it.
- `zell_sched_post` (and so `zell_task_spawn[_ex]`) captures the poster's
  context for the job.
- A worker sets the job's context before calling `fn(arg)` and releases whatever
  is current when `fn` returns, so contexts never leak between jobs.
- Calling an `async` function starts it on the caller's thread, so it inherits
  the caller's context. The ramp saves the caller's context
  (`zell_coro_enter`, +1) when it starts; a body that returns without
  suspending gives it back with `zell_coro_leave`, and a body that suspends
  hands it to its task (`zell_task_lazy`, 10.3), which makes it current
  again when the coroutine first suspends (`zell_task_add_continuation`
  returning true) or completes while current (like the current task in
  10.11). Either way whatever the coroutine set (`task.With` inside it, a
  `diag` span, `log.Context.Push`) stays with the coroutine and never leaks
  into the caller. `task.RunWith(ctx, f)` sets `ctx`, calls `f`, and
  restores the previous context before returning `f`'s task.

### 10.9 I/O readiness waits (`src/task_io.c`)

**Sharded pollers.** The runtime runs S poller shards (`ZELL_IO_POLLERS`,
default half the scheduler workers, at least 1, at most 8), started on the
first wait and stopped by `zell_runtime_shutdown` after the timers. Each
shard owns one platform poller (an epoll instance on Linux, a kqueue on
macOS/BSD; section 15), the thread that waits on it (none with netpoll,
below), a mutex and a binary
min-heap of its waits' deadlines. A descriptor is assigned to the least
loaded shard (ties round-robin) when it is registered — `zell_io_register`
at socket creation, or implicitly by its first wait — and stays there until
`zell_io_forget`; so shards share no lock or poller on the hot path. A
wait is a `Task<int>` completed by its shard's thread, which posts the
continuations to the scheduler exactly like a timer.

| Function | Semantics |
|---|---|
| `zell_task *zell_io_wait_readable(int64_t fd, int64_t timeout_ms)` | +1 task whose `int64_t` result is **0** when `fd` is readable (or at end of stream / error), **1** when `timeout_ms` (>= 0) elapsed first, **2** when `zell_io_forget(fd)` was called (or the descriptor is closed), **3** when the waiting task was cancelled (10.11). `timeout_ms < 0`: no timeout |
| `zell_task *zell_io_wait_writable(int64_t fd, int64_t timeout_ms)` | the same for writability |
| `void zell_io_register(int64_t fd)` | optional: assigns `fd` to a shard now (net does it when a socket is created, so connections spread over the shards at accept/connect time) |
| `void zell_io_forget(int64_t fd)` | completes pending waits on `fd` with 2, removes it from its poller and unassigns it. **Call it before `close(fd)`** (a reused descriptor number must not inherit waits). net never calls `close` itself: its socket handles do both (section 16) |

Rules and guarantees:

- `fd` must be non-blocking. Any number of tasks may wait on one descriptor
  in each direction; a report wakes **every** waiter of that direction.
- **epoll: edge-triggered, armed once.** A descriptor is added to its
  shard's epoll at its first wait (`EPOLLIN | EPOLLOUT | EPOLLRDHUP |
  EPOLLET`, both directions) and never re-armed, so a wait costs no system
  call. A report that finds no waiter of a direction is kept in the fd's
  `ready` bits and consumed by the next wait of that direction, which
  completes at once; a peer shutdown (`EPOLLRDHUP`) or an error/hang-up
  makes the direction ready for good (`sticky`), since the read that
  returns the last data does not return the EOF behind it and no new edge
  follows. Contract for callers (all of the stdlib keeps it): **wait only
  after the operation failed with `EAGAIN`, or after a short read of a
  stream socket** (fewer bytes than asked: the socket is drained, any later
  data raises a new edge; `net.sock.readInto` waits right away then,
  saving the `recv` that would fail). A wait without that may park even
  though data is buffered. Measured on `benchmarks/websocket_echo`: per
  message 1 `epoll_ctl` and 1 `recv` fewer than the one-shot scheme.
- **Other backends: level-triggered, one-shot** (kqueue `EV_ONESHOT`,
  io_uring `POLL_ADD`, IOCP `IOCTL_AFD_POLL`, WSAPoll): each wait arms the
  fd once, so data that is already there is reported at once and nothing
  is lost between a read that saw `EAGAIN` and the arm; the contract above
  holds for them trivially. `zell__plat_poller_edge` tells `task_io.c`
  which scheme a poller uses.
- **Reports may be spurious** (a readiness that is gone, a direction nobody
  asked for); every caller retries its operation and waits again.
- **Stale reports** are dropped: poller events carry `(generation << 32) |
  fd`, and the generation changes with every `zell_io_forget`, so a report
  that raced with forget + close + reuse of the number never reaches the
  new owner's waits.
- **EINTR** from the platform wait counts as a timeout of 0 events and the
  loop recomputes its deadline; wake-ups between threads go through the
  poller's eventfd (EVFILT_USER on kqueue) and only when a new wait has an
  earlier deadline than the one the shard sleeps toward, or at shutdown.
- A descriptor that cannot be polled (a regular file) is always ready (the
  wait completes with 0 at once); a closed one completes with 2.
- **Polling from the workers (netpoll; epoll today).** When every shard's
  poller is shared (`zell__plat_poller_shared`) and `ZELL_IO_NETPOLL` is not
  `0`, the shards get no threads. Their pollers are linked into one hub
  poller (`zell__plat_poller_link`: the hub reports, level-triggered, the
  index of every shard that has events) and the scheduler polls (10.1):
  idle workers poll the hub and then the shards it names with timeout 0;
  the last worker to go idle blocks in the hub until readiness, the
  earliest deadline of all shards or a post. A thread inside a poller's
  wait holds that poller's claim (the others skip it), so the events of a
  shard are fetched and applied by one thread at a time, as with a shard
  thread. One watchdog thread remains: it blocks in the hub only when no
  worker can (none awake and none parked, e.g. all of them inside
  `zell_task_await`; when the I/O starts with every worker asleep it wakes
  one instead, and a worker that wants the hub kicks the watchdog out),
  and while workers are awake it polls for them when nobody polled the hub
  for 10 ms (every worker busy, like Go's sysmon) and completes due
  deadlines; idle with a worker in the hub, it sleeps until a worker wakes
  up. A wait with an earlier deadline than the hub blocker or the watchdog
  sleeps toward wakes it (both publish that they sleep before reading the
  deadlines; the wait stores its deadline before reading that flag).
  Polling from several threads makes spurious reports more frequent: a
  poll that runs between the data's arrival and its wake-up callback sees
  the data, and the callback then reports the fd once more. The io_uring,
  kqueue, IOCP and WSAPoll backends keep a dedicated thread per shard; a
  backend opts in by implementing `zell__plat_poller_shared` and
  `zell__plat_poller_link` (a kqueue can watch another kqueue).
- **io_uring backend (Linux 6.0+, `src/platform/poll_uring.c`, opt-in with
  `ZELL_IO_BACKEND=io_uring`)**: when `io_uring_setup` works (and the kernel has `IORING_FEAT_EXT_ARG`,
  `NODROP` and `IORING_REGISTER_SYNC_CANCEL`), each shard's poller is an io_uring instead of an epoll
  instance, with the same semantics: an arm submits one
  `IORING_OP_POLL_ADD` (one-shot, level: completes at once if already
  ready), a changed interest replaces the pending poll
  (`IORING_OP_POLL_REMOVE` + a new add; a per-fd sequence number in
  `user_data` drops completions of replaced polls), the wait is
  `io_uring_enter(GETEVENTS | EXT_ARG)` with a timeout, and the wake-up
  channel is an eventfd polled the same way. `zell_io_forget` cancels a
  pending poll synchronously (`IORING_REGISTER_SYNC_CANCEL`): a pending
  poll holds a reference to the file, so after an asynchronous remove a
  closed listening socket could still accept for a moment. The first arm of a descriptor
  `fstat`s it so that regular files and closed descriptors get the
  synchronous answers of epoll. Raw system calls; no liburing.
  `zell__plat_poller_backend` reports `io_uring` or `epoll`. A poll that
  has just completed may still hold its file reference for a moment (the
  submitting task frees it right after posting the CQE), so close(2) does
  not always release the socket at once: net shuts a listener down
  (`shutdown(SHUT_RD)`, which stops listening whatever holds the file)
  before closing it, so a closed listener never accepts again (connected
  sockets only see their FIN or RST postponed by microseconds);
  examples/tcp_echo passes 500 runs of 500. It is not the default: at
  parity with one-shot epoll on tcp_echo but about 20% fewer requests/s on
  http_hello (one submission per wait instead of one `epoll_ctl`, plus the
  completion reaped under the ring lock; benchmarks/README.md).
- **Shutdown with pending waits**: the shard threads are woken and joined;
  waits still pending are released without completing (the scheduler has
  stopped, nobody would run their continuations), like pending timers.

### 10.10 Primitives for the Zell-source `task`

| Function | Semantics |
|---|---|
| `bool zell_task_try_complete(zell_task *t, const void *result)` | `zell_task_complete` that returns false instead of panicking when `t` is already done |
| `zell_task *zell_task_rt_signal(void)` | a pending `Task<void>`, +1 (a "signal"), fired early when the current task is cancelled (10.11) |
| `zell_task *zell_task_rt_waker(void)` | the same, but never fired by cancellation (`Mutex`, `WaitGroup`) |
| `void zell_task_rt_fire(zell_task *t)` | completes `t` if it is still pending; if `t` is a pending `zell_task_delay` timer, the timer is cancelled first (used by `task.Timeout`) |
| `void zell_task_spin_lock(int64_t *slot)` / `zell_task_spin_unlock` | a test-and-set spin lock on an `int64_t` (0 = free, 1 = held; spins, then yields the CPU). Zell code passes `&this.guard` of an `int` field. Hold it only for a few statements, never across an `await` |
| `zell_task *zell_task_yield(void)` | `task.Yield()`: the immortal yield sentinel (retain/release no-ops, never done, no allocation). `zell_task_add_continuation` on it posts the continuation at once, with the caller's context and current task, to the **tail** of the worker's ring (global queue off a worker), never to run-next, so the coroutine resumes behind already queued work, on any worker; `zell_task_all`/`zell_task_any` treat it the same way, `zell_task_result` on it returns its empty slot, and cancel, deadlines and fire on it are no-ops. A blocking `zell_task_await` of it (non-task code) waits on a fresh task completed by a job |

`task.Delay`, `task.All`, `task.Any` and `task.Yield` are runtime functions; the
rest of `task` is Zell (`compiler/src/sema/stdlib_src/task.zl`):

- `Mutex` and `Channel<T>` are the runtime's (10.6, 10.7): the Zell structs
  only hold the three mutex words and the channel handle box.
- `WaitGroup`, `Spawn`/`Go` (`await task.Yield()` then call), `Timeout` (a delay
  that a watcher task cancels with `zell_task_rt_fire` when the watched task
  finishes), `Key`/`Get`/`With`/`CurrentContext`/`RunWith`/`SpawnWith` (10.8),
  `NewCancel`/`WithTimeout`/`CheckCancelled`/`IsCancelled`/`ErrCancelled` (10.11).

### 10.11 Cancellation and the current task

**The task tree.** Every thread has a *current task* (owned, NULL = none),
carried by jobs and continuations exactly like the context of 10.8.

- `zell_task_lazy` (the coroutine's call at its **first suspension**, 10.3;
  `zell_task_new` does the same at once) makes the new task a **child of
  the current task** and **current** itself. The current task becomes its parent
  again when the coroutine suspends (`zell_task_add_continuation` returning
  true) or completes (`zell_task_complete` of the current task); after that the
  code running on the thread is the caller's. So `async` calls form a tree with
  no help from the compiler. Until its first suspension a coroutine's body
  runs **as part of the caller's current task**: it inherits that task's
  cancellation, and scopes or tasks it starts before suspending (a
  `task.NewCancel()`, a nested call that suspends) attach to the caller's
  task, not to the task the coroutine gets later; a body that never suspends
  has no task at all, and neither has a coroutine awaited directly by
  another one (10.3 "Frame-linked await": it runs under the current task
  of the job that resumes it). Runtime-made tasks (delays, signals, waits, `All`,
  `Any`, spawns) use `zell_task_new_plain`, which does neither;
  **C code that makes a task for a callback must use `zell_task_new_plain`**.
- A child holds a reference to its parent until it is freed; the parent lists
  its children without owning them.

| Function | Semantics |
|---|---|
| `void zell_task_cancel(zell_task *t)` | sets the cancelled flag of `t` and of all its descendants (tasks created later under a cancelled task start cancelled) and runs their **cancel hooks**; `t` borrowed. Called by `CancelHandle.Cancel` (on the scope node) and by `t.Cancel()` on a `Task<T>` (on the task itself; idempotent, and a completed task's result is untouched) |
| `bool zell_task_is_cancelled(const zell_task *t)` / `bool zell_task_cur_cancelled(void)` | the flag of `t` / of the current task (`task.IsCancelled()`) |
| `void zell_task_set_deadline(zell_task *t, int64_t ms)` | the timer thread cancels `t` after `ms` milliseconds (`task.WithTimeout`) |
| `zell_task *zell_task_current(void)` | the current task, +1 (or NULL) |
| `zell_task *zell_task_new_scope(void)` | a plain task attached under the current task, used only as a cancellation node (`task.NewCancel`), +1 |
| `zell_task *zell_task_enter_scope(zell_task *scope)` / `void zell_task_leave_scope(zell_task *prev)` | make `scope` current (NULL keeps the current task) and return the previous one (+1); restore it (`task.RunWith`) |
| `zell_error *zell_task_err_cancelled(void)` | `task.ErrCancelled`, an immortal sentinel (`errors.Is`) with message `task cancelled` |

**Cancel hooks** wake the waits of a cancelled task (allocated nodes
holding a reference to the wait, or for channel waits a node embedded in the
wait, 10.7; done waits are pruned when the list reaches 8 and at the task's
drop): `zell_task_delay`
(completes at once), `zell_task_rt_signal` (fired, so `Channel.Send/Recv`
re-check and give up), `zell_io_wait_*` (complete with **3**, so `net`
operations fail with `task.ErrCancelled`). A wait started by an already
cancelled task completes immediately. `task.Any` cancels its losing inputs
before it completes (so the awaiter, once resumed, finds a losing channel
wait already out of the queue).

Limits (the compiler is not involved, so an `await` cannot be interrupted by
itself): awaiting another task's result waits until that task finishes (it
finishes early only if it reacts to the cancellation); `Delay`, `Channel.Send`
and other `Task<void>` waits wake early but cannot carry the error, so
cancellable loops call `task.CheckCancelled()?`; `Mutex.Lock` and
`WaitGroup.Wait` are not cancellable; `task.All` does not cancel the others on
the first error; there is no `task.Scope`, `WithDeadline` or `OnCancel` yet.

---

## 11. Program entry

The runtime does **not** define `main`. The compiler emits it:

```llvm
define i32 @main(i32 %argc, ptr %argv) {
  call void @zell_runtime_init(i32 %argc, ptr %argv)
  ; synchronous `func main()`:
  call void @app.main()
  ; or, for `async func main()`:
  ;   %res = alloca { ptr, T }            ; the ramp's result slot (10.3)
  ;   call void @zell_task_run_main(ptr @app.main, ptr %res)  ; the ramp runs on a worker until the first await
  ;   %t = load ptr, ptr %res             ; NULL: main never suspended, its result is in %res
  ;   call ptr @zell_task_await_main(ptr %t)  ; else the main thread blocks; a panic of main's task exits 2 (8.1)
  ;   call void @zell_release(ptr %t)
  call void @zell_runtime_shutdown()
  ret i32 0
}
```

For `func main() void!` the call returns the error (`ptr`; for `async` it
is read, and retained, from the awaited task's result slot). After the
globals are released and before `zell_runtime_shutdown`, a non-NULL error
is passed to `zell_error_report` and released (so `--leak-check` neither
counts it nor what it holds as leaked), and `main` returns 1.

| Function | Semantics |
|---|---|
| `void zell_runtime_init(int argc, char **argv)` | stores the arguments and starts the worker pool and the timer thread. Idempotent. Call it first, on the main thread |
| `void zell_runtime_shutdown(void)` | releases the process-wide singletons (diag's slot map: `http.DefaultTransport()`, the default logger, metrics; 18) while the scheduler still runs the tasks their destructors wake, then waits until the job queue is empty and no job is running, stops the workers and the timer thread (pending timers are dropped without firing), flushes stdio, frees the argument slice, releases singletons registered again meanwhile and merges the refcount queues (3.1) before the leak report. Call it from the main thread only (calling it from a worker deadlocks). After shutdown the scheduler restarts lazily if used again |
| `void zell_runtime_drain(void)` | the first step of `zell_runtime_shutdown` alone: waits until no job is queued or running and stops the workers and the timer thread. The compiler calls it after `main` returns, then releases the program's globals (their destructors may print or even start tasks again), then calls `zell_runtime_shutdown` |
| `void zell_os_args(zell_slice *out)` | `os.Args`: an owned `[]string` (`elem_size` 8, ops `zell_ops_ref`) that **shares** the runtime's backing array. Treat it as read-only |
| `void zell_exit(int32_t code)` | flushes stdout and stderr, then calls `_Exit(code)` immediately (no defers, no shutdown wait). `noreturn` |

Returning from `main` without calling `zell_runtime_shutdown` is allowed, but
output still sitting in buffers may be lost if workers are still printing.

---

## 12. Environment variables

| Variable | Effect |
|---|---|
| `ZELL_WORKERS=n` | number of worker threads (default: the CPUs the process may use, affinity and container CPU quota included, 10.1; max 256) |
| `ZELL_NO_BACKTRACE=1` | panic prints only the `panic: ...` line (useful for golden-output tests) |
| `ZELL_NO_LINES=1` | panic backtraces print symbol+offset frames instead of `file:line` from the line table (8.2) |
| `ZELL_CYCLES=auto\|manual\|off` | the cycle collector (3.8): automatic collections (default), only `runtime.CollectCycles()`, or none (plain reference counting). `ZELL_CYCLES_THRESHOLD=N` fixes the number of recorded roots that triggers an automatic collection; `ZELL_CYCLES_MAX_SCAN=N` bounds the objects one collection examines (default 2^22) |
| `ZELL_DIAG_HEAP=1` | per-type live object counters for `diag.HeapByType` / `/debug/heap` in any build (18) |
| `ZELL_NO_SLAB=1` | objects come straight from `malloc` instead of the object heap (3.2), e.g. so Valgrind memcheck sees each object |
| `ZELL_HEAP_RESERVE=n[K\|M\|G]` | free heap memory the scavenger leaves to the process (3.2; default 16M) |
| `ZELL_MEMLIMIT=n[K\|M\|G\|T][iB]` / `off` | memory limit (3.2): overrides the container's (cgroup v2 `memory.max`/`memory.high`, v1 `memory.limit_in_bytes`, a Windows job object's); the heap keeps no reserve past 15/16 of it |
| `ZELL_HUGEPAGES=1` | ask for transparent huge pages on heap segments after the first (3.2): fewer page faults for a big heap, possible compaction stalls (latency) |
| `ZELL_LIMITS_DEBUG=1` | print the worker count, the usable CPUs and the memory limit with their sources when the scheduler starts |
| `ZELL_NO_BIAS=1` | no thread owns objects: every refcount stays unowned (3.1), e.g. to compare |
| `ZELL_RC_STATS=1` | at shutdown, print to stderr how many objects were biased, queued for a merge and merged by another thread (3.1) |
| `ZELL_TEST_PANICS=process` | `testing.Panics` re-runs the test in a child process instead of catching the panic in-process (8) |
| `ZELL_IO_BACKEND=io_uring` | the io_uring readiness backend on Linux 6.0+ (10.9; opt-in, falls back to epoll) |
| `ZELL_DEBUG=[host]:port[,wait]` | programs built with `--debug-agent` (default for debug builds) start the in-process debug agent before `main` (§22); port 0 picks a free port, announced on stderr as `zell-debug: listening on HOST:PORT`; `,wait` (or `ZELL_DEBUG_WAIT=1`) holds `main` until the client sends `configurationDone` |

---

## 13. Known limits in v0.1

- Value alignment is limited to 8 bytes (slice data is 16-aligned; map values and
  task results are only 8-aligned). Do not store `long double` or 16-byte SIMD
  types in containers.
- There is no cycle collection (use weak references, 3.4; `--leak-check`
  reports what a cycle kept alive).
- Map iteration order is deterministic but unspecified. Mutating a map during
  iteration is only partly supported (6.3).
- `errors.Is` compares identity only (7.2).
- File I/O and `io` are blocking. Network I/O is asynchronous (10.9, 16),
  but there is no `select` over channels and no task cancellation.
- The I/O poller exists for Linux (epoll) only; the kqueue (macOS/BSD) and IOCP
  (Windows) backends are not written yet (15).
- The scheduler uses a single global queue (no work stealing). Blocking awaits on
  workers start spare threads.
- Panics do not unwind: no `defer`, no cleanup.
- Strings are byte strings: indexing, slicing and `len` work on bytes. Rune helpers
  are `zell_string_rune_count`, `zell_string_decode_rune` and
  `zell_string_from_rune`.

---

## 14. Standard library

The stdlib functions that `compiler/src/sema/stdlib.rs` lowers to externs.
Every symbol is `zell_<ns>_<snake_name>`. Implemented in `src/std_*.c`,
declared in the "Standard library" section of `zell_rt.h`.

### 14.1 Calling convention

| Zell type | C parameter | C result |
|---|---|---|
| `string` | `const zell_string *` (borrowed, NULL = `""`) | `zell_string *`, owned (+1), never NULL |
| `int` / `float` / `bool` | `int64_t` / `double` / `bool` | same |
| `[]T` | `const zell_slice *` (borrowed, may be NULL = empty) | **trailing** out-param `zell_slice *out`, function returns `void`. `*out` is overwritten without being released and receives an owned slice |
| `error` | `const zell_error *` (borrowed, NULL = nil) | `zell_error *`, owned (+1) or NULL |
| `T!` | | `T` is returned, plus a **trailing** `zell_error **err`: `*err = NULL` on success; on failure `*err` is an owned error and the returned value is the zero value (0 / NULL). `err` may be NULL to discard the error |
| `void!` | | `void` return plus the same trailing `zell_error **err` |
| `Task<void>` | | `zell_task *`, owned (+1) |

`[]string` slices produced or consumed here use `elem_size` 8 and ops
`&zell_ops_ref`; elements are `zell_string *` slots, NULL elements mean `""`.
A string result that equals an argument may be **the argument object itself**,
retained (strings are immutable); the caller just releases it as usual.

### 14.2 Signatures

| Zell | C signature | Semantics / notes |
|---|---|---|
| `strings.Split(s, sep)` | `void zell_strings_split(const zell_string *s, const zell_string *sep, zell_slice *out)` | Go `strings.Split`: n+1 pieces for n non-overlapping matches; `Split("", ",")` = `[""]`; empty `sep` splits after each UTF-8 sequence (an invalid byte is its own 1-byte piece) and `Split("", "")` = `[]` |
| `strings.Join(parts, sep)` | `zell_string *zell_strings_join(const zell_slice *parts, const zell_string *sep)` | empty/NULL `parts` gives `""` |
| `strings.Trim(s)` | `zell_string *zell_strings_trim(const zell_string *s)` | Go `strings.TrimSpace`: strips leading/trailing Unicode white space (`\t \n \v \f \r` space, U+0085, U+00A0, U+1680, U+2000..U+200A, U+2028, U+2029, U+202F, U+205F, U+3000). No cutset |
| `strings.Contains(s, sub)` | `bool zell_strings_contains(const zell_string *s, const zell_string *substr)` | `Contains(s, "")` is true |
| `strings.Replace(s, old, new)` | `zell_string *zell_strings_replace(const zell_string *s, const zell_string *old, const zell_string *new_)` | replaces **all** non-overlapping occurrences, left to right (Go `ReplaceAll`). Empty `old` inserts `new` at the start and after each UTF-8 sequence (`Replace("ab", "", "-")` = `-a-b-`) |
| `strings.ToUpper(s)` / `ToLower(s)` | `zell_string *zell_strings_to_upper(const zell_string *s)` / `zell_strings_to_lower` | simple 1:1 case mapping for ASCII, Latin-1, Latin Extended-A, Greek and Cyrillic (U+0400..U+04BF); other code points (e.g. `ß`) and invalid bytes are copied unchanged (Go would also map other scripts, and turns invalid bytes into U+FFFD) |
| `strings.HasPrefix(s, p)` / `HasSuffix(s, p)` | `bool zell_strings_has_prefix(const zell_string *s, const zell_string *prefix)` / `bool zell_strings_has_suffix(const zell_string *s, const zell_string *suffix)` | |
| `strings.Index(s, sub)` | `int64_t zell_strings_index(const zell_string *s, const zell_string *substr)` | **byte** offset of the first match, -1 if none, 0 for empty `sub` |
| `strings.Repeat(s, n)` | `zell_string *zell_strings_repeat(const zell_string *s, int64_t count)` | panics `strings: negative Repeat count` if `count < 0` and `strings: Repeat output length overflow` if the result would not fit |
| `strings.ToInt(s)` | `int64_t zell_strings_to_int(const zell_string *s, zell_error **err)` | same as `zell_string_to_int` (4.2): base 10, optional `+`/`-`, no whitespace; errors `invalid integer: "..."` / `integer out of range: "..."` |
| `strings.ToFloat(s)` | `double zell_strings_to_float(const zell_string *s, zell_error **err)` | same as `zell_string_to_float`; `1e400` is an error (`float out of range`) |
| `math.Sqrt/Abs/Floor/Ceil/Sin/Cos/Tan/Log/Exp(x)` | `double zell_math_sqrt(double x)` (and `_abs`, `_floor`, `_ceil`, `_sin`, `_cos`, `_tan`, `_log`, `_exp`) | C libm; IEEE results (`Sqrt(-1)` NaN, `Log(0)` -Inf) |
| `math.Pow(x, y)` | `double zell_math_pow(double x, double y)` | C `pow` |
| `math.Min(x, y)` / `Max(x, y)` | `double zell_math_min(double x, double y)` / `double zell_math_max(double x, double y)` | Go semantics, not C `fmin`: `Min(x, -Inf)` = -Inf and `Max(x, +Inf)` = +Inf even with NaN, otherwise any NaN gives NaN; `Min(-0, +0)` = -0, `Max(-0, +0)` = +0 |
| `fmt.Format(tmpl, args...)` | `zell_string *zell_fmt_format(const zell_string *tmpl, const zell_slice *args)` | `args` is a `[]string` of the arguments already formatted as `println` would (the compiler does the conversion). Each `{N}` (N = ASCII digits, leading zeros allowed) with `N < len(args)` is replaced by `args[N]`; `{N,W}` / `{N,-W}` (W = ASCII digits, at most 1000000) pads it with spaces on the left / right to W code points (UTF-8 lead bytes), never truncating; a placeholder whose index is out of range is **kept verbatim** (`{2}`); extra arguments are ignored; any other text, including `{`, `}`, `{}`, `{x}`, `$`, is copied verbatim (no `{{` escaping). `args` may be NULL |
| `io.ReadFile(path)` | `zell_string *zell_io_read_file(const zell_string *path, zell_error **err)` | = `zell_read_file` (9) |
| `io.WriteFile(path, data)` | `void zell_io_write_file(const zell_string *path, const zell_string *data, zell_error **err)` | `zell_write_file` with the error moved to `*err` (`void!` convention); with `err == NULL` the error is released |
| `io.ReadLine()` | `zell_string *zell_io_read_line(zell_error **err)` | = `zell_read_line`: at EOF returns NULL and `*err = zell_err_eof` (immortal; releasing it is harmless) |
| `errors.Wrap(err, msg)` | `zell_error *zell_errors_wrap(const zell_error *err, const zell_string *msg)` | = `zell_error_wrap`: new error, `Message()` = `msg + ": " + err.Message()`, cause `err` retained. **`err == NULL` returns NULL** (wrapping nil is nil) |
| `errors.Unwrap(err)` | `zell_error *zell_errors_unwrap(const zell_error *err)` | = `zell_error_unwrap`: the cause, owned (+1), or NULL if `err` is not a wrapper (or is NULL) |
| `errors.Is(err, target)` | `bool zell_errors_is(const zell_error *err, const zell_error *target)` | = `zell_error_is`: identity along the chain (7.2) |
| `task.Delay(ms)` | `zell_task *zell_task_delay(int64_t ms)` | the existing function (10.2); no alias needed |

Not externs (lowered by the compiler, see `stdlib.rs` specials): `errors.New`
-> `zell_error_new`, `errors.As` -> `zell_error_as`, `task.All` ->
`zell_task_all`, `println`/`print` -> `zell_print_*`. `math.Pi` and `math.E`
are inlined constants.

### 14.3 Second batch: time, os, fs, random, crypto, encoding, uuid

Same conventions as 14.1. Additional slice element layouts: `[]byte` is
`elem_size` 1 and `[]char` is `elem_size` 4 (`int32_t` code points), both with
NULL ops; `[]int` is `elem_size` 8 with NULL ops. Errors from the OS have the
message `"<op> <path>: <strerror(errno)>"` (e.g. `mkdir data: File exists`).
Paths and other strings with an embedded NUL byte are rejected with `EINVAL`
(or treated as non-existent by `Exists`/`IsDir`).

**strings** (`src/std_strings_ext.c`; byte offsets, invalid UTF-8 bytes count as one rune = U+FFFD)

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Fields(s) []string` | `void zell_strings_fields(const zell_string *s, zell_slice *out)` | Go `strings.Fields`: split around runs of Unicode white space (same set as `Trim`); `[]` for blank input |
| `Count(s, sub) int` | `int64_t zell_strings_count(const zell_string *s, const zell_string *substr)` | non-overlapping count; empty `sub` gives rune count + 1 |
| `LastIndex(s, sub) int` | `int64_t zell_strings_last_index(const zell_string *s, const zell_string *substr)` | byte offset of the last match, -1 if none, `len(s)` for empty `sub` |
| `TrimPrefix(s, p)` / `TrimSuffix(s, p)` | `zell_string *zell_strings_trim_prefix(const zell_string *s, const zell_string *prefix)` / `_trim_suffix` | removes one occurrence if present |
| `TrimLeft(s, cutset)` / `TrimRight(s, cutset)` | `zell_string *zell_strings_trim_left(const zell_string *s, const zell_string *cutset)` / `_trim_right` | Go semantics: strips leading/trailing runes contained in `cutset`; an invalid byte is stripped only if the same invalid byte is in `cutset` |
| `SplitN(s, sep, n) []string` | `void zell_strings_split_n(const zell_string *s, const zell_string *sep, int64_t n, zell_slice *out)` | Go: `n > 0` at most `n` pieces (last is the rest), `n == 0` gives `[]`, `n < 0` = `Split` |
| `EqualFold(a, b) bool` | `bool zell_strings_equal_fold(const zell_string *a, const zell_string *b)` | simple case folding over the `ToUpper`/`ToLower` ranges (`lower(upper(x))`, so `ς`/`σ`/`Σ` match) |
| `Reverse(s)` | `zell_string *zell_strings_reverse(const zell_string *s)` | reverses code points (invalid bytes kept as single units) |
| `PadLeft(s, width, pad)` / `PadRight` | `zell_string *zell_strings_pad_left(const zell_string *s, int64_t width, const zell_string *pad)` / `_pad_right` | pads to `width` **runes** by cycling the runes of `pad`; unchanged if already `>= width` or `pad == ""` |
| `Runes(s) []char` | `void zell_strings_runes(const zell_string *s, zell_slice *out)` | decoded code points, U+FFFD for invalid bytes |
| `FromRunes(r []char)` | `zell_string *zell_strings_from_runes(const zell_slice *runes)` | UTF-8 encoding; surrogates, negatives and > U+10FFFF become U+FFFD |
| `Bytes(s) []byte` / `FromBytes(b)` | `void zell_strings_bytes(const zell_string *s, zell_slice *out)` / `zell_string *zell_strings_from_bytes(const zell_slice *bytes)` | = `zell_string_to_bytes` / `zell_string_from_bytes` (copies) |
| `Compare(a, b) int` | `int64_t zell_strings_compare(const zell_string *a, const zell_string *b)` | -1 / 0 / 1, byte-wise |
| `rtCopy(dst, dstOff, src, srcOff, n)` | `void zell_strings_rt_copy(zell_slice *dst, int64_t dst_off, const zell_slice *src, int64_t src_off, int64_t n)` | internal (`strings.Builder`, `io` buffers): `memmove` of the bytes that fit in both spans; out-of-range offsets and counts are clamped, never a trap |
| `rtCopyString(dst, dstOff, s, from, n)` | `void zell_strings_rt_copy_string(zell_slice *dst, int64_t dst_off, const zell_string *s, int64_t src_off, int64_t n)` | internal: the same from the bytes of `s` |
| `rtIndexByte(b, from, to, c) int` | `int64_t zell_strings_rt_index_byte(const zell_slice *b, int64_t from, int64_t to, int64_t c)` | internal: `memchr` over `b[from..to]` (`to` clamped to the length); -1 when absent |
| `rtMakeBytes(n) []byte` | `void zell_strings_rt_make_bytes(int64_t n, zell_slice *out)` | internal: a zeroed `[]byte` of length and capacity `n` (`n < 0` gives an empty slice) |
| `rtSort(xs []string)` | `void zell_strings_rt_sort(zell_slice *xs)` | internal (`slices.Sort<string>`): byte-wise introsort (not stable, which is unobservable for equal strings) that permutes the `zell_string *` elements in place; the backing array keeps its references, nothing is retained or released |
| `Title(s)` | `zell_string *zell_strings_title(const zell_string *s)` | upper-cases the first rune of each white-space separated word (Go's deprecated `strings.Title`, white space as the only separator) |

**math** (`src/std_math_ext.c`). Zell has no overloading, so the int
versions of `Min`/`Max`/`Abs` (which stay `float`) are `IMin`/`IMax`/`IAbs`.
`MaxInt`/`MinInt` constants are not provided yet: the stdlib constant table
only holds untyped floats (`Pi`, `E`), and `2^63-1` is not representable.

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Round/Trunc/Log2/Log10(x float) float` | `double zell_math_round(double x)` (and `_trunc`, `_log2`, `_log10`) | libm; `Round` rounds half away from zero (Go) |
| `Mod(x, y)` / `Hypot(x, y)` / `Atan2(y, x)` | `double zell_math_mod(double x, double y)` / `zell_math_hypot` / `double zell_math_atan2(double y, double x)` | `fmod` (sign of `x`), `hypot`, `atan2` |
| `IsNaN(x) bool` | `bool zell_math_is_nan(double x)` | |
| `IsInf(x, sign int) bool` | `bool zell_math_is_inf(double x, int64_t sign)` | Go: `sign > 0` +Inf only, `< 0` -Inf only, `0` either |
| `Inf(sign int) float` / `NaN() float` | `double zell_math_inf(int64_t sign)` / `double zell_math_nan(void)` | `Inf(sign >= 0)` = +Inf |
| `IMin(a, b)` / `IMax(a, b)` / `IAbs(a)` (int) | `int64_t zell_math_imin(int64_t a, int64_t b)` / `_imax` / `int64_t zell_math_iabs(int64_t a)` | `IAbs(MinInt)` wraps to `MinInt` |

**io** (`src/std_io_ext.c`)

| Zell | C signature | Semantics / notes |
|---|---|---|
| `ReadAll() string!` | `zell_string *zell_io_read_all(zell_error **err)` | stdin (the same `FILE *` as `ReadLine`) to EOF; at EOF returns `""` (not an error) |
| `ReadLines(path) []string!` | `void zell_io_read_lines(const zell_string *path, zell_slice *out, zell_error **err)` | lines without `\n`, a trailing `\r` is dropped, a final newline adds no empty line; errors as `ReadFile` |
| `WriteLines(path, lines []string) void!` | `void zell_io_write_lines(const zell_string *path, const zell_slice *lines, zell_error **err)` | each line followed by `\n`; truncates/creates like `WriteFile` |
| `Eprint(s)` / `Eprintln(s)` | `void zell_io_eprint(const zell_string *s)` / `zell_io_eprintln` | to stderr (stdout is flushed first); one `string` argument, not variadic |

**time** (`src/std_time.c`): instants are `int` Unix **milliseconds**; calendar accessors are UTC.

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Now() int` / `NowNanos() int` | `int64_t zell_time_now(void)` / `zell_time_now_nanos` | `CLOCK_REALTIME` |
| `Sleep(ms int)` | `void zell_time_sleep(int64_t ms)` | blocks the calling thread (`nanosleep`, restarted on EINTR); `ms <= 0` returns at once. In async code prefer `await task.Delay(ms)` |
| `Since(startMs int) int` | `int64_t zell_time_since(int64_t start_ms)` | `Now() - startMs` |
| `Format(ms, layout) string` | `zell_string *zell_time_format(int64_t ms, const zell_string *layout)` | `strftime` in UTC, C locale, plus `%L` = 3-digit milliseconds; negative times floor to the previous second |
| `Parse(s, layout) int!` | `int64_t zell_time_parse(const zell_string *s, const zell_string *layout, zell_error **err)` | `strptime`, UTC unless the layout has `%z` (offset applied); the whole input must match; fields not in the layout default to 1970-01-01 00:00:00; error `time: cannot parse "<s>" as "<layout>"` |
| `Year/Month/Day/Hour/Minute/Second(ms) int` | `int64_t zell_time_year(int64_t ms)` (and `_month` 1..12, `_day` 1..31, `_hour`, `_minute`, `_second`) | UTC |
| `Weekday(ms) int` | `int64_t zell_time_weekday(int64_t ms)` | 0 = Sunday ... 6 = Saturday |

**os** (`src/std_os.c`)

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Args() []string` | `void zell_os_args(zell_slice *out)` | existing function (11): program name first |
| `Getenv(name) string` | `zell_string *zell_os_getenv(const zell_string *name)` | `""` if unset |
| `Setenv(name, value) void!` | `void zell_os_setenv(const zell_string *name, const zell_string *value, zell_error **err)` | EINVAL for an empty name or one containing `=` |
| `Environ() []string` | `void zell_os_environ(zell_slice *out)` | owned `[]string` of `"KEY=value"` in the process's current environment (after `Setenv`), from `zell__plat_environ` (POSIX `environ`, Windows `GetEnvironmentStringsW`) |
| `Exit(code int)` | `ZELL_NORETURN void zell_os_exit(int64_t code)` | = `zell_exit((int32_t)code)`: flushes stdout/stderr, no defers run |
| `Hostname() string!` / `Cwd() string!` | `zell_string *zell_os_hostname(zell_error **err)` / `zell_os_cwd` | `gethostname` / `getcwd` |
| `Pid() int` | `int64_t zell_os_pid(void)` | |
| `Chdir(path) void!` | `void zell_os_chdir(const zell_string *path, zell_error **err)` | process-wide |

**fs** (`src/std_fs.c`). Path functions are pure string operations on `/`-separated paths with Go `path/filepath` (Unix) semantics.

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Exists(path) bool` / `IsDir(path) bool` | `bool zell_fs_exists(const zell_string *path)` / `zell_fs_is_dir` | `stat` (follows symlinks); false on any error |
| `ReadDir(path) []string!` | `void zell_fs_read_dir(const zell_string *path, zell_slice *out, zell_error **err)` | entry names without `.`/`..`, sorted byte-wise |
| `Mkdir(path) void!` / `MkdirAll(path) void!` | `void zell_fs_mkdir(const zell_string *path, zell_error **err)` / `_mkdir_all` | mode 0777 & umask; `MkdirAll` creates parents, succeeds if the directory exists, fails with ENOTDIR through a file |
| `Remove(path) void!` | `void zell_fs_remove(const zell_string *path, zell_error **err)` | file or empty directory (`remove(3)`) |
| `RemoveAll(path) void!` | `void zell_fs_remove_all(const zell_string *path, zell_error **err)` | recursive, does not follow symlinks; a missing path (or `""`) is success |
| `Rename(from, to) void!` | `void zell_fs_rename(const zell_string *from, const zell_string *to, zell_error **err)` | `rename(2)` |
| `Size(path) int!` / `ModTime(path) int!` | `int64_t zell_fs_size(const zell_string *path, zell_error **err)` / `_mod_time` | bytes / mtime in Unix ms |
| `Copy(from, to) void!` | `void zell_fs_copy(const zell_string *from, const zell_string *to, zell_error **err)` | file contents only; `to` is created (0666 & umask) or truncated; a directory source is EISDIR |
| `AppendFile(path, data) void!` | `void zell_fs_append_file(const zell_string *path, const zell_string *data, zell_error **err)` | `O_APPEND`, creates the file |
| `Join(a, b)` | `zell_string *zell_fs_join(const zell_string *a, const zell_string *b)` | joins non-empty parts with `/` and cleans (`Join("a/b", "../c")` = `a/c`); `Join("", "")` = `""` |
| `Base(p)` / `Dir(p)` / `Ext(p)` | `zell_string *zell_fs_base(const zell_string *path)` / `_dir` / `_ext` | Go: `Base("")` = `.`, `Base("a/b/")` = `b`; `Dir("a")` = `.`, cleaned; `Ext` from the last `.` of the last element, `""` if none |
| `Abs(p) string!` | `zell_string *zell_fs_abs(const zell_string *path, zell_error **err)` | cleaned; relative paths are joined to the cwd |
| `TempDir() string` | `zell_string *zell_fs_temp_dir(void)` | `$TMPDIR` or `/tmp` |

**random** (`src/std_random.c`): xoshiro256** seeded through splitmix64.
Until `Seed` is called, each thread draws from its own state, seeded from
`getrandom(2)` on that thread's first use, without a lock. After `Seed(n)`,
all threads draw from one process-wide state behind a mutex, and the sequence
is fixed for `n`. Not for secrets.

| Zell | C signature | Semantics / notes |
|---|---|---|
| `Seed(n int)` | `void zell_random_seed(int64_t seed)` | |
| `Int() int` | `int64_t zell_random_int(void)` | uniform in `[0, 2^63)` |
| `IntN(n int) int` | `int64_t zell_random_int_n(int64_t n)` | uniform in `[0, n)` (rejection sampling); panics `random: invalid argument to IntN` if `n <= 0` |
| `Float() float` | `double zell_random_float(void)` | 53 random bits in `[0, 1)` |
| `Bytes(n int) []byte` | `void zell_random_bytes(int64_t n, zell_slice *out)` | panics if `n < 0` |
| `Shuffle(s []int)` | `void zell_random_shuffle(const zell_slice *s)` | in-place Fisher-Yates through the shared buffer; only `[]int` (no generics yet) |

**crypto.hash / crypto.hmac / crypto.rand** (`src/std_crypto.c`):
self-contained implementations of FIPS 180-4 (SHA-1/256/512), RFC 1321 (MD5),
RFC 2104 (HMAC) and IEEE CRC-32. Inputs are the bytes of the string; hex is lowercase.

| Zell | C signature | Semantics / notes |
|---|---|---|
| `hash.Sha256(data) []byte` / `Sha256Hex(data) string` | `void zell_crypto_hash_sha256(const zell_string *data, zell_slice *out)` / `zell_string *zell_crypto_hash_sha256_hex(const zell_string *data)` | 32 bytes / 64 hex chars |
| `hash.Sha512` / `Sha512Hex` | `zell_crypto_hash_sha512` / `_sha512_hex` (same shapes) | 64 bytes |
| `hash.Sha1` / `Sha1Hex`, `hash.Md5` / `Md5Hex` | `zell_crypto_hash_sha1` / `_sha1_hex`, `zell_crypto_hash_md5` / `_md5_hex` | legacy, for compatibility only |
| `hash.Crc32(data) int` | `int64_t zell_crypto_hash_crc32(const zell_string *data)` | IEEE (zlib, Go `crc32.ChecksumIEEE`), `0..2^32-1` |
| `hmac.Sha256(key, data) []byte` / `Sha256Hex` | `void zell_crypto_hmac_sha256(const zell_string *key, const zell_string *data, zell_slice *out)` / `zell_string *zell_crypto_hmac_sha256_hex(...)` | HMAC-SHA256 (RFC 4231 vectors in tests) |
| `hmac.Equal(a, b []byte) bool` | `bool zell_crypto_hmac_equal(const zell_slice *a, const zell_slice *b)` | time independent of the contents (lengths may leak) |
| `rand.Bytes(n) []byte!` | `void zell_crypto_rand_bytes(int64_t n, zell_slice *out, zell_error **err)` | `getrandom(2)`, falling back to `/dev/urandom`; `n < 0` is an error |
| `rand.Int() int!` | `int64_t zell_crypto_rand_int(zell_error **err)` | uniform in `[0, 2^63)` |
| `rand.Hex(n) string!` | `zell_string *zell_crypto_rand_hex(int64_t n, zell_error **err)` | `n` random bytes as `2n` hex chars |

**encoding.base64 / encoding.hex** (`src/std_encoding.c`, RFC 4648)

| Zell | C signature | Semantics / notes |
|---|---|---|
| `base64.Encode(b []byte)` / `EncodeString(s)` | `zell_string *zell_encoding_base64_encode(const zell_slice *data)` / `_encode_string(const zell_string *s)` | standard alphabet, `=` padding |
| `base64.Decode(s) []byte!` / `DecodeString(s) string!` | `void zell_encoding_base64_decode(const zell_string *s, zell_slice *out, zell_error **err)` / `zell_string *zell_encoding_base64_decode_string(const zell_string *s, zell_error **err)` | padding required; `\r`/`\n` skipped; unused trailing bits ignored (Go non-strict); error `illegal base64 data at input byte N` |
| `base64.EncodeURL(b)` / `DecodeURL(s) []byte!` | `zell_string *zell_encoding_base64_encode_url(const zell_slice *data)` / `void zell_encoding_base64_decode_url(const zell_string *s, zell_slice *out, zell_error **err)` | URL-safe alphabet (`-_`), **no padding** on output (Go `RawURLEncoding`); the decoder accepts padded or unpadded input |
| `hex.Encode(b)` / `EncodeString(s)` | `zell_string *zell_encoding_hex_encode(const zell_slice *data)` / `_encode_string` | lowercase |
| `hex.Decode(s) []byte!` / `DecodeString(s) string!` | `void zell_encoding_hex_decode(const zell_string *s, zell_slice *out, zell_error **err)` / `zell_string *zell_encoding_hex_decode_string(...)` | either case; errors `encoding/hex: invalid byte at offset N`, `encoding/hex: odd length hex string` |

**uuid** (`src/std_uuid.c`)

| Zell | C signature | Semantics / notes |
|---|---|---|
| `New() string` | `zell_string *zell_uuid_new(void)` | random version-4 UUID (RFC 9562) from `crypto.rand`, lowercase `8-4-4-4-12`; panics if no entropy source works |
| `IsValid(s) bool` | `bool zell_uuid_is_valid(const zell_string *s)` | canonical 36-char form, hex digits of either case, any version |


### 14.4 Tier 0/1 stdlib (docs/STDLIB.md)

C halves of the Zell-source modules in `compiler/src/sema/stdlib_src/`;
same conventions as 14.1 (borrowed arguments, owned results, trailing
`zell_error **`, `[]T` results through a trailing `zell_slice *out`).
`[]uint32` slices have `elem_size` 4, `[]int` 8, both with NULL ops. The
internal `rt*` externs are only called by the stdlib sources.

| File | Symbols | Notes |
|---|---|---|
| `std_strconv.c` | `zell_strconv_rt_format_float(double, int64 fmt, int64 prec)`, `zell_strconv_rt_parse_float(s, err)`, sentinels `zell_strconv_err_syntax/range()` | `fmt` is `'f' 'e' 'E' 'g' 'G'`; `prec < 0` = shortest round-trip digits (Go); overflow returns ±Inf with `ErrRange` |
| `std_unicode.c`, `std_unicode_tables.c` | `zell_unicode_is_{letter,digit,number,space,upper,lower,title,punct,symbol,mark,control,print,graphic}(int32)`, `zell_unicode_to_{upper,lower,title}`, `zell_unicode_simple_fold` | tables generated by `scripts/gen-unicode-tables.py` from Go's `unicode/tables.go` (Unicode 15) |
| `std_math_bits.c` | `zell_math_bits_*` (uint64/uint32), `zell_math_bits_rt_mul_hi`, `rt_div_quo/rem` (panic on y == 0 or overflow), `zell_math_float64bits/frombits`, `float32bits/frombits` | |
| `std_math_big.c` | `zell_math_big_rt_{add,sub,cmp,mul,divmod,shl,shr,bit_len,format,parse,from_bytes,to_bytes,from_uint}` | normalized little-endian `[]uint32` magnitudes (zero = empty); `divmod` writes `[len(q), q..., r...]`; Karatsuba above 48 limbs |
| `std_hash.c` | `zell_hash_maphash_rt_sip13(k0, k1, []byte)`, `_str(k0, k1, string)` | SipHash-1-3 |
| `std_time_zone.c` | `zell_time_rt_local_tzif(err)` | TZif bytes of `$TZ` / `/etc/localtime` (Windows: synthesized from the registry), read once; `""` = UTC |
| `std_time_zone.c` | `zell_time_rt_zone_tzif(name, err)` | TZif bytes of an IANA zone from a non-file zone database (Windows registry, `platform/tz_win32.c`); `""` where zones are tzdata files |
| `std_os_file.c` | `zell_os_rt_{open,read,pread,write,write_str,seek,close,sync,truncate,fstat,stat,read_dir,chmod}`, `zell_io_err_eof()` (= `zell_err_eof`) | blocking file descriptors; `rt_read` fills the caller's `[]byte` in place; `fstat`/`stat` give `[size, perm, kind, mtime_s, mtime_ns]`; writes to fd 1/2 flush stdio first |
| `std_os_signal.c` | `zell_os_signal_rt_{wait,catch,ignore,reset,raise,pending}` | `rt_wait` returns a `Task<void>` completed by a dedicated thread reading a self-pipe written by the `sigaction` handler; deliveries without waiters are counted |
| `std_process.c` | `zell_process_rt_{spawn,read,write,close,wait_pid,kill,executable,wait_readable,wait_writable}` | fork/exec, pipe ends non-blocking and `FD_CLOEXEC`; `rt_spawn` returns `[pid, pidfd, stdin, stdout, stderr]` (-1 = none); `rt_read`/`rt_write` return -1 for "would block", then the Zell side awaits `zell_io_wait_readable/writable` (10.9) |
| `std_os_user.c` | `zell_os_user_rt_{lookup,group,uid}` | `getpwuid_r` & co. |
| `std_sync.c` | `zell_sync_rt_{load,store,add,swap,cas}(int64_t *, ...)`, `zell_sync_rt_yield` | sequentially consistent `__atomic` builtins on a field passed as `&field` |

---

## 15. Platform layer

Everything OS-specific that the runtime needs is behind the internal interface
`src/platform/platform.h` (hidden `zell__plat_*` symbols, not part of the ABI);
see `docs/PLATFORMS.md`.

| File | Contents |
|---|---|
| `platform/posix.c` | threads (`zell__plat_thread_start[_detached]`, `_join`; `_start_background` for runtime threads that never touch objects and so leave `zell_threaded` alone: the heap scavenger), `cpu_count`, `yield`, monotonic clock `mono_ns`, entropy `random_bytes` (`getrandom`/`getentropy`, then `/dev/urandom`), `backtrace` (execinfo, or a frame-pointer walk without it, as on musl; frames named from the executable's ELF `.symtab` / Mach-O `LC_SYMTAB`), the `SIGPROF` CPU sampler and the symbolizer, `tz_tzif` (always false: tzdata files) |
| `platform/poll_epoll.c` | the readiness poller on Linux: level-triggered one-shot epoll (`EPOLLONESHOT`, re-armed per wait) plus an eventfd for wake-ups (`poller_new/free/arm/remove/wait/wake`; events carry a 64-bit token) |
| `platform/poll_kqueue.c` | the same poller on macOS and the BSDs: `EVFILT_READ`/`EVFILT_WRITE` with `EV_ONESHOT` (no `EV_CLEAR`), `EVFILT_USER` wake-ups (a self-pipe where it does not exist). Compiles only there; compile-checked on Linux against `runtime/stub-sdk/macos` (`make CROSS=aarch64-apple-darwin`, docs/PLATFORMS.md) |
| `platform/net_posix.c` | `process_init` (SIGPIPE ignored, open-files soft limit raised to the hard one), socket addresses (`addr_ip/unix/family/parts/format`, `parse_ip`, `format_ip`, interface name/index), non-blocking sockets (`sock_open/connect/error/bind/listen/accept/recv/send/sendv/recvfrom/sendto/shutdown/close/name`), options (`sock_setopt/getopt`: TCP_NODELAY, keep-alive idle/interval/count, SO_RCVBUF/SNDBUF, SO_LINGER, SO_REUSEADDR/PORT, SO_BROADCAST, TCP_CORK/TCP_NOPUSH, IPV6_V6ONLY, TCP_FASTOPEN, multicast loop/TTL/interface; `sock_mcast` join/leave), `sendfile` (sendfile(2) on Linux/FreeBSD/macOS, pread+send elsewhere), `file_open/size/close`, `unlink`, blocking `resolve` (getaddrinfo, RFC 6724 order or IPv4 first), `reverse` (getnameinfo), `lookup_port`, `interfaces` (getifaddrs), `err_class` (errno to the net sentinel classes), `strerror` (lower case). Results are `>= 0` or `-errno`; "would block" is `-ZELL_PLAT_EAGAIN` |
| `platform/certroots.c` | `cert_bundle_path`: the PEM bundle of trusted roots (`$SSL_CERT_FILE`, then the Linux/BSD/macOS bundle paths; Windows: the `ROOT` system store exported once per process to `%LOCALAPPDATA%\zell\roots.pem`), used by `crypto.x509.SystemRoots` (section 19) |
| `platform/platform.h` (inline, POSIX) / `platform/win32.c` | `zell__plat_mutex`/`_cond`/`_once`/`_tls_key` with `ZELL_PLAT_*_INIT` static initialisers (pthreads; SRWLOCK, CONDITION_VARIABLE, INIT_ONCE, FLS); `cond_wait_ns` (relative timeout, CLOCK_MONOTONIC for conds made by `cond_init` where the system allows); `flockfile` wrappers |
| `platform/posix.c` (more) / `win32.c` | `wall_ns`, `sleep_ns`, `strftime`/`strptime`, `localtime_path`/`zoneinfo_dir`, `aligned_alloc`/`aligned_free`/`alloc_size`, virtual memory for the object heap (`page_size`; `mem_map` an aligned anonymous mapping, `mem_unmap`; `mem_release` gives a range's memory back but keeps it mapped: `MADV_DONTNEED` on Linux, `MADV_FREE` on macOS, `DiscardVirtualMemory` or `MEM_RESET` on Windows; `mem_remap` = `mremap` on Linux, unsupported elsewhere; `malloc_trim`), `ZELL_PLAT_HAS_PANIC_CAPTURE` (testing.Panics' in-process capture, 8; 0 on Windows), `runtime_init` (Windows: UTF-8 argv from `CommandLineToArgvW`), `stream_write` (Windows console: `WriteConsoleW`), `pid`, `getenv`/`setenv`/`environ` (UTF-8; UTF-16 on Windows), `hostname`, `temp_dir`, `path_sep`, signals (`sig_start/catch/ignore/default/raise`: self-pipe thread / `SetConsoleCtrlHandler`), users (`user_lookup`, `group_lookup`, `uid`) |
| `platform/fs_posix.c` / `fs_win32.c` | files on descriptors (`open` with the os flag bits, `read`, `pread`, `write`, `write_all`, `seek`, `close`, `fsync`, `ftruncate`, `fstat`, `stat_path` into `zell__plat_stat`), `isatty`, `mkdir`, `rmdir`, `unlink_file`, `remove`, `rename`, `chmod`, `getcwd`, `chdir`, `is_executable`, directory iteration (`dir_open/next/close`). POSIX errno values on every platform (Win32 errors are mapped). On Windows: `CreateFileW` + `_open_osfhandle`, UTF-16 paths, `/` and `\` |
| `platform/fs_posix.c` / `proc_win32.c` | child processes: `spawn` (fork/exec, pidfd; `CreateProcessW` with a quoted command line; Windows pipes: overlapped named pipes waited on by the IOCP poller, descriptors `0x400000 + slot`, or with the WSAPoll fallback relayed to pollable loopback sockets), `pipe_read/write/close`, `wait_pid`, `kill` |
| `platform/poll_iocp.c` | the readiness poller on Windows: a completion port per poller with readiness from one `IOCTL_AFD_POLL` per armed socket (zero-byte overlapped `ReadFile` / write completion for child pipes), `PostQueuedCompletionStatus` wake-ups, `GetQueuedCompletionStatusEx` waits; falls back to `poll_wsapoll.c` (or `ZELL_POLLER=wsapoll`); `poller_backend` names the backend |
| `platform/poll_wsapoll.c` | the fallback poller on Windows: `WSAPoll` over the armed sockets plus a loopback UDP wake-up socket, one-shot by disarming on report |
| `platform/prof_win32.c` | Windows `backtrace` (CaptureStackBackTrace, names from the COFF symbol table, then DbgHelp `SymFromAddr`), the CPU sampler (sampler thread, `GetThreadTimes`, `SuspendThread` + `GetThreadContext` + `RtlVirtualUnwind`), `symbolize`, `exe_range`, `thread_stack_init` |
| `platform/tz_win32.c` | `tz_tzif` on Windows: TZif v2 synthesized from the registry's Time Zones (TZI, Dynamic DST), local zone from `GetDynamicTimeZoneInformation`, IANA names by a CLDR table |
| `platform/sqlite_dyn.h`, `sqlite_win32.c` | database.sqlite on Windows without `<sqlite3.h>`: the used SQLite API as function pointers bound from `sqlite3.dll` or `winsqlite3.dll` at run time |
| `platform/net_win32.c` | the `net_posix.c` functions on Winsock2 (`WSASocketW` overlapped + non-inheritable, `FIONBIO`), WSA error codes with POSIX-style messages, `GetAdaptersAddresses`, `sendfile` = overlapped `TransmitFile` (pread+send fallback); `process_init` = `WSAStartup` |
| `platform/timefmt.c` | calendar arithmetic for all platforms (`utc_tm`, `timegm`) and the "C"-locale `strftime`/`strptime` used on Windows and musl (`*_portable`, tested against glibc on Linux; `ZELL_PLAT_LIBC_TIMEFMT` marks the C libraries whose own are used) |

Users of the layer: every OS call of the runtime. `task.c`, `task_io.c`,
`mem.c`, `heap.c` (also virtual memory and the scavenger thread), `rc.c`, `weak.c`, `leak.c`, `debug.c` and the `std_*.c` modules use the
platform locks; `std_fs.c`, `std_os*.c`, `io.c`, `diag.c` the file
functions; `std_process.c` the process functions; `std_os_signal.c` the
signal functions; `std_time*.c` the clocks and time formatting;
`panic.c` the backtrace; `std_crypto.c`/`std_random.c` entropy;
`std_net.c` sockets and the resolver pool. No file outside `platform/`
has an OS conditional.

## 16. Networking: net and net.http

`net` and `net.http` are Zell (`compiler/src/sema/stdlib_src/net.zl`
and `net_udp.zl` (UDP, buffered I/O, interfaces), `http.zl`, see
`compiler/src/sema/stdsrc.rs`) over the internal externs below
(`src/std_net.c`, platform-independent; Zell names start with `rt`). Every
socket is non-blocking; an operation that would block reports it and the Zell
code awaits `rtWaitReadable/rtWaitWritable` (10.9), so no worker thread ever
blocks on the network.

### 16.1 Architecture decisions

- **Pollers: per-worker shards, not one I/O thread.** S shards (default
  `ceil(workers / 2)`, max 8, `ZELL_IO_POLLERS`) each with its own
  epoll/kqueue instance, thread, lock and deadline heap; a socket is pinned
  to the least loaded shard when it is created (accept/connect/listen) and
  deregistered when it closes. Chosen over a single poller with a lock-free
  registration ring because the shards remove the shared state entirely
  (no ring to size, no single thread that every readiness report and every
  deadline has to pass through, no O(n) deadline scan), and because the
  run queue is global: a worker-affine poller could not deliver locality
  anyway, so the shard threads complete waits and post the continuations
  to the shared scheduler queue (10.1). The shard threads (not the workers)
  wait on the pollers because workers park on the run queue's condition
  variable; integrating the poll into idle workers (Go's netpoller) needs a
  scheduler with per-worker queues and is the next step if the queue ever
  becomes the bottleneck (it is not on `benchmarks/tcp_echo`, see
  `benchmarks/README.md`). Level-triggered one-shot arming, stale-event
  generations, EINTR, wake-ups and shutdown: 10.9.
- **Sockets are handles that own the descriptor** (`net_sock` in
  std_net.c). Each operation acquires the handle (an atomic count of
  operations in flight with a closed bit), so a closed socket fails with
  `net.ErrClosed` without touching the descriptor, and `rtClose` sets the
  closed bit, wakes every waiter (`zell_io_forget`: they fail with
  `ErrClosed`) and closes the descriptor only when the last operation in
  flight has finished (then forgets it again, so a wait registered in that
  window is released too). A descriptor number can therefore never be
  reused while another thread may still pass it to a system call — close is
  race-free by construction, not by convention. The Zell `sock` object owns
  the handle; its `Drop` calls `rtFree` (close if needed, then free), so
  every `Conn`/`Listener`/`UDPConn` is closed by `Close()` or when its last
  copy goes away, and a double `Close` is a no-op.
- **DNS: a resolver pool.** `getaddrinfo`/`getnameinfo` run on up to
  `ZELL_RESOLVERS` (default 4) detached threads that start on demand and
  retire after 30 s idle; a lookup is a task completed by the pool, so a
  slow DNS server never blocks a worker. Address literals (with `%zone`)
  complete at once. Results keep the system's RFC 6724 order (net then
  interleaves families for Happy Eyeballs) or IPv4 first (`ResolveIP`). At
  shutdown queued lookups are dropped and running ones discard their
  result. Timeouts and retries are the system resolver's (`resolv.conf`
  `timeout:`/`attempts:`); a lookup cannot be cancelled. A native DNS client
  (UDP, `/etc/hosts`, TXT/MX) is not implemented, so there is no
  `LookupTXT/LookupMX`.
- **Errors**: the C side reports the *cause* — one of the immortal
  sentinels below for the errno classes net names, else an error with
  the lower-case system text — and net.zl wraps it in a `net.OpError`
  (`"dial tcp 10.0.0.1:80: connection refused"`). `rtOpError` builds that
  wrapper as an ordinary `zell_error` whose own text is the prefix and
  whose `inner` is the cause (so `errors.Is` sees the sentinel through it),
  carrying the boxed `OpError` as its typed payload (so `errors.As` finds
  it).
- **SIGPIPE** is ignored process-wide at scheduler start (like Rust's
  runtime) and every send uses `MSG_NOSIGNAL` / `SO_NOSIGPIPE`; a write to
  a peer that went away fails with `net.ErrReset`. The open-files soft
  limit is raised to the hard one at the same time (like Go).

### 16.2 Sentinels (`net` constants, `StdConst::Error`)

| Zell | C | Message | errno classes |
|---|---|---|---|
| `net.ErrClosed` | `zell_net_err_closed()` | `use of closed network connection` | operation on a closed socket, a wait woken by Close, `EBADF` |
| `net.ErrTimeout` | `zell_net_err_timeout()` | `i/o timeout` | deadline / timeout, `ETIMEDOUT` |
| `net.ErrRefused` | `zell_net_err_refused()` | `connection refused` | `ECONNREFUSED` |
| `net.ErrReset` | `zell_net_err_reset()` | `connection reset by peer` | `ECONNRESET`, `EPIPE`, `ECONNABORTED` |
| `net.ErrUnreachable` | `zell_net_err_unreachable()` | `network is unreachable` | `ENETUNREACH`, `EHOSTUNREACH`, `ENETDOWN`, `EHOSTDOWN` |
| `net.ErrAddrInUse` | `zell_net_err_addr_in_use()` | `address already in use` | `EADDRINUSE` |
| `net.ErrNotFound` | `zell_net_err_not_found()` | `no such host` | `EAI_NONAME`/`EAI_NODATA`, no addresses |
| `net.ErrEOF` | `zell_net_err_eof()` (= `zell_err_eof`) | `EOF` | `ReadExact`/`ReadLine`/`ReadByte` at the end of the stream |

### 16.3 Internal externs

Handles are `int64_t` (a `net_sock *`, never 0 or -1). "Would block" is
**-1** for the `int!` operations and an empty slice with `rtAgain()` for the
`[]byte!` reads. Error outputs are the causes of 16.1.

| Zell (internal) | C | Semantics |
|---|---|---|
| `rtDial(ip, port, lip, lport) int!` | `zell_net_rt_dial(const zell_string *ip, int64_t port, const zell_string *lip, int64_t lport, zell_error **err)` | TCP socket (TCP_NODELAY on), optional local bind (`lip == ""` and `lport == 0`: none), connect started; `ip` may carry `%zone` |
| `rtDialUnix(path) int!` | `zell_net_rt_dial_unix` | Unix stream connect (`@name`: Linux abstract namespace) |
| `rtConnectResult(h) void!` / `rtConnectCheck(h) int!` | `zell_net_rt_connect_result` / `_connect_check` | `SO_ERROR` of the connect / 1 connected, 0 still connecting, or the error |
| `rtWaitAnyWritable(hs []int, ms) Task<int>` | `zell_net_rt_wait_any_writable(const zell_slice *hs, int64_t ms)` | index of the first handle to become writable (or whose `ms` wait ended); Happy Eyeballs |
| `rtListen(ip, port, backlog, flags) int!` | `zell_net_rt_listen` | bound, listening TCP socket. `ip == ""`: `::` with `IPV6_V6ONLY` off (dual-stack), `0.0.0.0` without IPv6. `flags`: 1 no `SO_REUSEADDR` (on by default), 2 `SO_REUSEPORT`, 4 `IPV6_V6ONLY`, 8 `TCP_FASTOPEN`. `backlog <= 0`: 4096 |
| `rtListenUnix(path, backlog) int!` | `zell_net_rt_listen_unix` | Unix listener; the socket file is unlinked when the handle closes (not for `@name`) |
| `rtAccept(h) int!` | `zell_net_rt_accept` | new connection handle (non-blocking, close-on-exec, TCP_NODELAY), or -1 |
| `rtRead(h, n) []byte!` / `rtAgain() bool` | `zell_net_rt_read` / `_again` | up to `n` bytes (received into a per-thread 64 KB buffer and copied into an exact-size slice); empty at end of stream **or** when it would block: `rtAgain()` (read right after the call) tells which |
| `rtReadInto(h, buf, off) int!` | `zell_net_rt_read_into(int64_t h, const zell_slice *buf, int64_t off, zell_error **err)` | reads into `buf[off..]` (no allocation): bytes read, 0 at end of stream, -1 would block |
| `rtWrite(h, data, off)` / `rtWriteStr(h, s, off)` / `rtWriteV(h, bufs [][]byte, off) int!` | `zell_net_rt_write`, `_write_str`, `_write_v` | writes from byte `off` (of the concatenation for `WriteV`, one `sendmsg` of up to 64 buffers); bytes written or -1 |
| `rtShutdown(h, how) void!` | `zell_net_rt_shutdown` | `how`: 1 read, 2 write, 3 both |
| `rtClose(h)` / `rtFree(h)` / `rtIsClosed(h) bool` / `rtFd(h) int` | `zell_net_rt_close`, `_free`, `_is_closed`, `_fd` | 16.1; `rtFree` only from `Drop`; `rtFd` is -1 once closed. A listener is shut down for reading before its descriptor is closed (it stops accepting at once, even while the file is still referenced) |
| `rtWaitReadable(h, ms)` / `rtWaitWritable` `Task<int>` | `zell_net_rt_wait_readable(int64_t h, int64_t ms)` | `zell_io_wait_*` (codes of 10.9) with `ms <= 0` meaning no timeout; a closed handle gives a completed task with 2 |
| `rtLocalAddr(h)` / `rtRemoteAddr(h) string`, `rtPort(h) int` | `zell_net_rt_local_addr` ... | `"ip:port"`, `"[ip%zone]:port"` or the Unix path; IPv4-mapped peers of dual-stack sockets as plain IPv4; the remote address is kept from dial/accept (valid after Close) |
| `rtSetOpt(h, opt, v) void!` / `rtGetOpt(h, opt) int!` | `zell_net_rt_set_opt` / `_get_opt` | `opt` = `ZELL_PLAT_OPT_*` of platform.h: 1 NODELAY, 2 KEEPALIVE, 3 KEEPIDLE, 4 KEEPINTVL, 5 KEEPCNT, 6 RCVBUF, 7 SNDBUF, 8 LINGER (seconds, < 0 default), 9 REUSEADDR, 10 REUSEPORT, 11 BROADCAST, 12 CORK (TCP_NOPUSH on BSD), 13 V6ONLY, 14 FASTOPEN, 15 MCAST_LOOP, 16 MCAST_TTL, 17 MCAST_IF (interface index) |
| `rtMcast(h, join, group, ifname) void!` | `zell_net_rt_mcast` | join/leave an IPv4/IPv6 multicast group |
| `rtOpenFile(path) int!`, `rtFileSize(fd) int`, `rtCloseFile(fd)`, `rtSendFile(h, fd, off, n) int!` | `zell_net_rt_open_file` ... `_send_file` | `SendFile`: bytes sent, 0 at end of file, -1 would block |
| `rtParseIP(s) []byte`, `rtFormatIP(b) string`, `rtIsIP(s) bool`, `rtIfIndex(name) int`, `rtIfName(i) string` | `zell_net_rt_parse_ip` ... | 4/16 bytes (empty if invalid), `inet_ntop` text |
| `rtInterfaces() []string!` | `zell_net_rt_interfaces` | one line per interface: `name\tindex\t0\tflags\taddr/bits,...` (flags `u l m b p`) |
| `rtLookupPort(net, service) int` | `zell_net_rt_lookup_port` | port of a service name (or a number), -1 if unknown |
| `rtResolve(host, flags) Task<int>`, `rtReverse(ip) Task<int>`, `rtResolveTake(h) []string!`, `rtResolveSync(host, flags) []string!` | `zell_net_rt_resolve` ... | resolver pool (16.1): the task's result is a request handle that `rtResolveTake` turns into the addresses or `lookup host: cause` (cause `ErrNotFound` for unknown names), freeing it. `flags`: 1 IPv4 first, 2 passive. `rtResolveSync` resolves on the calling thread (`Listen` with a host name) |
| `rtUdp(ip, port, flags) int!`, `rtUdpDial(ip, port, lip, lport) int!` | `zell_net_rt_udp`, `_udp_dial` | bound UDP socket (`ip == ""`: dual-stack any) / connected UDP socket |
| `rtRecvFrom(h, n) []byte!`, `rtReadFrom(h, buf) int!`, `rtLastFrom() string`, `rtLastFromPort() int`, `rtLastTrunc() bool`, `rtSendTo(h, data, ip, port) int!` | `zell_net_rt_recv_from` ... | datagrams; the sender of the last datagram and whether it was truncated (`MSG_TRUNC`) are kept per thread like `rtAgain`; `rtSendTo` with `ip == ""` sends on a connected socket; IPv4 targets of a dual-stack socket are sent IPv4-mapped |
| `rtOpError(boxed error, prefix, inner error) error` | `zell_net_rt_op_error` | 16.1 |
| `rtBytes(n) []byte`, `rtCopyBytes(dst, doff, src, soff, n) int`, `rtIndexByte(b, from, c) int`, `rtJoin(a, b, bn) []byte` | `zell_net_rt_bytes` ... | byte helpers (zeroed buffer, memmove, memchr, `a ++ b[..bn]`) |

### 16.4 Public Zell API

The authoritative list with semantics is DESIGN.md, "Сеть и протоколы":
addresses (`IP`, `Addr`, `ParseIP`, `ParseAddr`, `SplitHostPort`,
`JoinHostPort`, `Interfaces`), resolution (`ResolveIP`, `Resolve`,
`LookupHost`, `LookupAddr`, `LookupPort`, `ResolveAddr`), TCP (`Dial`,
`DialTimeout`, `Dialer`, `Listen`, `ListenConfig`, `Conn`, `Listener`),
Unix (`ListenUnix`, `DialUnix`), UDP (`ListenUDP`, `DialUDP`, `UDPConn`),
buffered I/O (`BufferedReader`, `BufferedWriter`, `MakeBuffer`) and errors
(`OpError`, the sentinels of 16.2, `IsTimeout`). The pre-existing API
(`Conn.Read(n)`, `ReadString`, `Write`, `WriteString`, `SetTimeout`,
`Close`, `IsClosed`, `LocalAddr`, `RemoteAddr`, `Listener.Accept/Close/
Addr/Port`, `UDPConn.RecvFrom/SendTo/...`) is unchanged; error *messages*
follow Go now (`dial tcp 127.0.0.1:1: connection refused` instead of
`connect: Connection refused`).

**HTTP** (`http.zl`, `http_server.zl`, `http_client.zl`, `http2.zl`,
`http_extra.zl`, namespace `net.http`; `hpack.zl` = `net.http.hpack`,
`httptest.zl` = `net.http.test`, `ws.zl` = `net.ws`): the protocol
logic is Zell; the hot loops are the C helpers of 16.5. The authoritative
API list is DESIGN.md, "Сеть и протоколы". In short:

- **Server**: `http.Server{Addr, Handler, IdleTimeout, TLS, ReadTimeout,
  WriteTimeout, MaxHeaderBytes, MaxBodyBytes, H2C, DisableHTTP2,
  MaxConcurrentStreams, NoDate}`; one task per connection. HTTP/1.1
  connections read into one per-connection buffer (`rbuf`, 4 KB, grown on
  demand) that heads are parsed from in place (`rtHeadEnd`, `rtParseHead`);
  a request costs one read, one handler task and one write in the common
  case (`rtFormatHead` builds status line, headers, cached `Date` and the
  body as one string). HTTP/2 (RFC 9113) via TLS ALPN `h2` or prior
  knowledge (`H2C`): the connection task reads frames, each stream runs its
  handler in a task; HPACK with static/dynamic tables and Huffman;
  flow control on both sides; SETTINGS, PING, GOAWAY (also on Shutdown),
  RST_STREAM; priority ignored; no server push. `Response.Flush()` streams
  (chunked on HTTP/1.1, DATA frames on HTTP/2), `Response.SendFile` uses
  sendfile(2) on plain TCP, `Response.Hijack()` hands an HTTP/1.x
  connection over (`RawConn`, used by `ws.Upgrade`).
- **Client**: `http.Client{Timeout, TotalTimeout, MaxRedirects, Headers,
  TLS, Transport, Jar}` with `Get/Post/Do` (unchanged) and `Send(req)` /
  `Stream(req)` for `http.NewRequest(method, url)` builders. `Transport`
  pools connections per scheme+host+proxy (idle LIFO list, `MaxIdleConnsPerHost`
  8, `IdleConnTimeout` 90 s, `MaxConnsPerHost` with FIFO waiters), shares
  one HTTP/2 connection per host (no background reader: the waiting
  request reads frames while it holds the connection's reader token),
  resumes TLS sessions from its session cache, retries idempotent requests
  once when a reused connection turns out stale, speaks to proxies (absolute
  form for http, CONNECT for https; `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`),
  asks for and decodes gzip. The process-wide `DefaultTransport()` lives in
  the diag slot map, which `zell_runtime_shutdown` releases first,
  while the scheduler still runs: its pool closes the connections, and the
  HTTP/2 reader tasks that wakes finish before the leak report (11).

### 16.5 net.http C helpers (`src/std_http.c`)

Pure functions over their arguments (plus a per-thread Date cache, a
per-thread scratch buffer for heads and the per-thread result of
`rtHeadInfo`, valid until the next `rtParseHead` on the thread, like
`rtAgain`). Zell names are in `net.http` unless noted.

| Zell (internal) | C | Semantics |
|---|---|---|
| `rtCanonical(s) string` | `zell_net_http_rt_canonical` | `CanonicalHeader`: trims blanks, upper case at the start and after `-`; `s` itself (retained) when already canonical, an interned immortal string for ~90 common names, a name that is not a token unchanged |
| `rtHeadEnd(buf, from, to) int` | `zell_net_http_rt_head_end` | offset just past the blank line ending a head in `buf[from..to)` (`\r\n\r\n`, `\n\n`, `\n\r\n`), or -1 |
| `rtParseHead(buf, start, end, kind, h map[string]string) []string!` | `zell_net_http_rt_parse_head` | parses `buf[start..end)` into `h` (canonical names; repeats joined with `", "`, `Set-Cookie` with `"\n"`) and returns kind 0 `[method, path, query, proto]` (absolute-form targets keep their path) or kind 1 `[proto, status, reason]`; common methods, protocols and names are interned; errors `malformed request line "..."` and the like |
| `rtHeadInfo() int` | `zell_net_http_rt_head_info` | flags of the last parse: 1 Content-Length (value `>> 16`), 2 chunked, 4 other Transfer-Encoding, 8 `Connection: close`, 16 `keep-alive`, 32 `Expect: 100-continue`, 64 traceparent, 128 Upgrade, 256 invalid or conflicting Content-Length |
| `rtFormatHead(status, line, h, blen, flags, body) string` | `zell_net_http_rt_format_head` | a message head (+ `body` unless flag 4) as one string: status line (or `line` with flag 128, a request), `h` with canonical names (Content-Length/Connection/Transfer-Encoding ignored, `Set-Cookie` lines split at `"\n"`, CR/LF in values become spaces), default `Content-Type: text/plain; charset=utf-8` when `blen > 0` (not with 64), `Date` (not with 8), `Content-Length: blen` (16: `Transfer-Encoding: chunked`, 32: neither), 1 `Connection: keep-alive`, 2 `Connection: close` |
| `rtHasToken(v, token) bool` | `zell_net_http_rt_has_token` | comma-separated token membership, ASCII case-insensitive, `;params` ignored |
| `rtStatusText(code)`, `rtDate()`, `rtHttpTime(sec)`, `rtParseHttpTime(s) int` | `zell_net_http_rt_status_text`, `_date`, `_http_time`, `_parse_http_time` | reason phrases; the cached IMF-fixdate; Unix seconds to/from HTTP dates (IMF-fixdate, RFC 850, asctime; -1 if invalid) |
| `rtItoa(n)`, `rtAtoi(s) int` | `zell_net_http_rt_itoa`, `_atoi` | decimal; a non-negative decimal of at most 18 digits or -1 |
| `strconv.rtItoa(n int)`, `rtUtoa(n uint) string` | `zell_strconv_rt_itoa`, `_utoa` | base-10 digits written straight into the result (`zell_string_from_int/uint`); +1 |
| `rtMask(b, off, n, key, phase)` | `zell_net_http_rt_mask` | WebSocket masking in place with a big-endian 32-bit key starting at key byte `phase` (8 bytes per step) |
| `net.http.hpack.rtHuffLen(s) int`, `rtHuffEncode(s) []byte`, `rtHuffDecode(b, off, n) string!` | `zell_net_http_hpack_rt_huff_len`, `_encode`, `_decode` | the HPACK Huffman code of RFC 7541 appendix B (padding with EOS bits; decoding rejects EOS, padding over 7 bits or not all ones) |

Sentinels (`StdConst::Error`, immortal): `http.ErrBodyTooLarge`
(`zell_net_http_err_body_too_large`), `ErrHijacked`, `ErrAbortHandler`,
`ErrServerClosed`, `ErrHandlerTimeout`, `ErrBodyClosed`; `ws.ErrClosed`
(`zell_net_ws_err_closed`), `ws.ErrBadHandshake`. Integer constants
`ws.TextMessage` (1), `ws.BinaryMessage` (2) and the close codes
`ws.CloseNormal` (1000) ... `ws.CloseInternalError` (1011).

---

## 17. Reflection tables and encoding.json

Emitted by the compiler (`compiler/src/codegen/reflect.rs`) only for programs
that use `reflect` (directly, through `typeof`, `json`, or a builtin
attribute named as a type). C declarations and layout checks
(`_Static_assert`) are in the reflection section of `zell_rt.h` and in
`src/std_reflect.c`.

### 17.1 Type tables

With reflection on, **every `zell_type_info` the program emits is the head
of a full table**: the type id stored in box headers, at `vtable[0]`, in the
second word of an `any` and in `zell_error.user_type` is a
`zell_reflect_type *`. Tables are immortal objects
`{ i64 -1, ptr NULL, zell_reflect_type }`; the type id is the address of the
payload (object + 16), so a Zell `*reflect.TypeInfo` and the type id are the
same pointer and `==` on them compares types (one table per type: tables are
deduplicated by type in the compiler). Only types that some code or table
refers to get a table; unreferenced ones are removed by `globaldce`.

```c
struct zell_reflect_type {             /* reflect.TypeInfo, 240 bytes */
    zell_type_info rt;                 /* +0   name (C string), 16 + sizeof T, drop */
    int32_t kind;                       /* +24  zell_reflect_kind (reflect.Kind order) */
    zell_string *name;                 /* +32  short name: User, *User, []int, map[string]int */
    zell_string *full_name;            /* +40  namespace path + name */
    int64_t size, align;                /* +48, +56 */
    zell_slice fields;                 /* +64  []*FieldInfo of a struct */
    zell_slice methods;                /* +96  []*MethodInfo (full types only) */
    zell_slice variants;               /* +128 []*VariantInfo of an enum */
    const zell_reflect_type *elem;     /* +160 element of *T, weak *T, []T, [N]T, T!, Task<T>; map value */
    const zell_reflect_type *key;      /* +168 map key */
    int64_t len;                        /* +176 array length */
    zell_slice attrs;                  /* +184 []any, see 17.2 */
    const zell_reflect_type *ptr_to;   /* +216 table of *T (full types only, for TypeInfo.New) */
    const zell_reflect_type *canon;    /* +224 alias tables only */
    zell_string *package;              /* +232 namespace path of a struct/enum/interface (NULL = "") */
};
```

`zell_reflect_field` (FieldInfo, 72 bytes: `name`, `type`, `offset`,
`index`, `embedded`, `exported`, `attrs` at +40), `zell_reflect_method`
(MethodInfo, 104 bytes: `name`, `type`, `is_static` at +16, `invoke` at +24,
`fnval` at +32, `attrs` at +40, `params` at +72 — a `[]*ParamInfo` without
the receiver), `zell_reflect_param` (ParamInfo, 56 bytes: `name`, `type`,
`index` at +16, `attrs` at +24) and `zell_reflect_variant` (VariantInfo, 80
bytes: `name`, `index`, `fields` at +16, `attrs` at +48) are immortal objects
too; the slices holding them (and every other table slice) have a NULL
backing, so retaining or releasing them is a no-op. Strings are immortal
literals.

- Field offsets are the C layout of the struct (declaration order, natural
  alignment). Variant fields have offsets from the start of the enum value
  (`{i32 tag, payload}`, payload at +8).
- *Full* types — named in `reflect.TypeOf<T>()`, in a static `typeof(x)`, in
  `reflect.Fields<T>()`, declared `@reflect`, or listed by
  `reflect.TypesWith<A>()` — list their non-async methods and get `ptr_to`;
  other tables have no methods. Interface tables carry `attrs` (no methods).
- `reflect.TypesWith<A>()` returns a fresh +1 slice (`zell_slice_new_ops`,
  one allocation per call; nil when no type is marked) of the tables of the
  marked types, computed by the compiler over the whole program; the
  function is emitted once per attribute type `A`. The tables are immortal,
  so they are stored without a retain; the caller may sort or overwrite
  the slice.
- The layout grows only by appending (`package` at +232, `params` at +72 of
  a method were added on 2026-10-24); existing offsets never move.
- A struct field spelled with a builtin alias (`int64`, `uint`, `uint8`,
  `float64`) points at an *alias table*: a copy with that name whose `canon`
  is the real table (`Value.Get/Set` compare canonical tables).

### 17.2 Attribute values

Every attribute is a constant value of its attribute struct (`@json` is
`reflect.json {name string, omitEmpty bool, skip bool}`, `@deprecated`
is `{message string}`, user attributes are their `@attribute` structs, with
field defaults filled in). `attrs` is a `[]any` in source order whose
elements hold a `*A`: `{data, type}` with `type` = the table of `*A` and
`data` = an immortal box whose payload is the pointer to the immortal value
object. `@attribute` and `@reflect` themselves are not listed.

### 17.3 Methods

`invoke` is `{any, zell_error*} invoke(any recv, zell_slice args)` (Zell
ABI: aggregates by value): it accepts a receiver boxed as `*S` or `S`, checks
the argument count and that every argument's dynamic type is the parameter
type (`any` parameters take the value as is), unboxes them (retaining values
for moved parameters), calls the method and boxes a non-void result into a
new `any` (+1); a method error (or a check failure, `reflect: S.M: ...`) is
returned as the error. `fnval` is a closure-convention thunk
`fn(env, recv, args...)` (`MethodInfo.Call<F>()` returns `{fnval, NULL}`).

### 17.4 encoding.json

`src/std_json.c`. The runtime parses a JSON text (RFC 8259) in one pass
into a **document**: one array of 16-byte nodes in depth-first document
order, owned by the document together with a byte arena for the strings
that had escapes and the index tables of long arrays (built as the parser
closes them, or by `rt_index` for an array that has none). The
document retains the source text (string nodes without escapes point into
it) and is freed in one call. Handles are addresses passed as `int64_t`:
the document (`rt_parse` result) and nodes (`0` = no node, which every
query treats as null). Nodes have no refcounts; a node handle is valid
until its document is freed. `json.Value` (Zell,
`compiler/src/sema/reflect_src/json.zl`) is `{*document, node}` where
`document` is a refcounted Zell object whose `Drop` calls `rt_free`, so a
document lives as long as any Value made from it; `Parse<T>` walks the
nodes of one document and frees it when it returns. Documents are not
thread-safe (one task parses, Values may then be read from one thread at a
time).

```c
struct zell__json_node {           /* 16 bytes, private to std_json.c */
    uint8_t type;                  /* 0 null, 1 bool, 2 number, 3 string, 4 array, 5 object */
    uint8_t flags;                 /* 1: number is an int64 (u.i), 2: array has an index table */
    uint16_t pad;
    uint32_t len;                  /* string bytes; array elements; object pairs; bool value */
    union { int64_t i; double f; const char *s; struct { uint32_t span, table; } c; } u;
};
```

A container node is followed by the nodes of its elements (an object: key
node, value node, key node, ...); `u.c.span` is the number of nodes in its
subtree (itself included), so the next sibling of a node is `node + span`
(1 for scalars). Numbers: an integer literal that fits int64 (not `-0`) is
an int (flag 1); everything else is a double parsed with Clinger's fast
path (mantissa < 2^53, |exponent| <= 22) or `strtod`; `1e400` and the like
are rejected. Strings: `\uXXXX` and surrogate pairs decode to UTF-8, lone
surrogates to U+FFFD; other bytes pass through unchanged (no UTF-8
validation). Nesting deeper than 512 containers is rejected. The scanner
uses SSE2 where `__SSE2__` is defined (strings: the first `"`, `\` or
control byte in 16-byte blocks; whitespace runs), else portable loops.

| Zell (internal unless noted) | C | Notes |
|---|---|---|
| `json.Quote(s)` (public) | `zell_string *zell_encoding_json_quote(const zell_string *s)` | +1 JSON string literal: quotes, `\" \\ \n \r \t \b \f`, other control bytes as `\u00XX`; other bytes (UTF-8) unchanged |
| `rtParse(text)` | `int64_t zell_encoding_json_rt_parse(const zell_string *text, zell_error **err)` | the document (retains `text`), or 0 with `*err`. Lexical errors: `json: <what> at offset N` (`unterminated string`, `control character in string`, `invalid escape in string`, `invalid \u escape`, `invalid number`, `number out of range`, `invalid character 'c'`, `invalid byte 0xNN`); grammar: `json: unexpected end of input`, `json: expected , or ] in an array, found <token>`, `... , or } in an object, found ...`, `... a string key in an object, found ...`, `... : after an object key, found ...`, `json: unexpected <token>`, `json: unexpected <token> after the top-level value` (`<token>` is `number 2`, `string "x"` (as written), `true`, `'}'`), `json: nesting too deep` |
| `rtFree(d)` | `void zell_encoding_json_rt_free(int64_t d)` | frees the document (nodes, arena, tables) and releases the text |
| `rtRoot(d)` | `int64_t zell_encoding_json_rt_root(int64_t d)` | the root node |
| `rtKind(n)` | `int64_t zell_encoding_json_rt_kind(int64_t n)` | the type above (0 for node 0) |
| `rtIsInt(n)` | `bool zell_encoding_json_rt_is_int(int64_t n)` | number node written as an integer that fits int64 |
| `rtBool(n)`, `rtInt(n)` | `bool/int64_t zell_encoding_json_rt_{bool,int}(int64_t n)` | the value; 0/false for other kinds |
| `rtFloat(n)` | `double zell_encoding_json_rt_float(int64_t n)` | the number (an int node converted); **NaN** for every other kind, so one call reads and classifies (JSON has no NaN) |
| `rtMake<T>(n)` | compiler intrinsic → `zell_slice_new_ops(out, sizeof T, n, n, ops)` (5.2) | `n` zeroed elements, `len == cap == n` (nil for 0): the typed decoder sizes a slice once and decodes into its elements in place |
| `rtStr(n)` | `zell_string *zell_encoding_json_rt_str(int64_t n)` | +1 copy of a string node (NULL for `""` and other kinds) |
| `rtLen(n)` | `int64_t zell_encoding_json_rt_len(int64_t n)` | elements of an array / pairs of an object, 0 for scalars |
| `rtFirst(n)` | `int64_t zell_encoding_json_rt_first(int64_t n)` | first element / first key node, 0 for an empty container or a scalar |
| `rtNext(n)` | `int64_t zell_encoding_json_rt_next(int64_t n)` | the node after `n`'s subtree: the next sibling; after a key node, its value. Not bounds-checked: callers count with `rtLen` |
| `rtGet(obj, key)` | `int64_t zell_encoding_json_rt_get(int64_t obj, const zell_string *key)` | the value of the last pair named `key` (bytewise), 0 if absent or not an object |
| `rtGetFold(obj, key)` | `int64_t zell_encoding_json_rt_get_fold(int64_t obj, const zell_string *key)` | the same, comparing keys ASCII case-insensitively (`Parse<T>` falls back to it when there is no exact match) |
| `rtIndex(d, arr, i)` | `int64_t zell_encoding_json_rt_index(int64_t d, int64_t arr, int64_t i)` | element `i`, 0 if out of range or not an array. Arrays of 16+ elements get a `uint32_t` offset table in the document (built by the parser when the array closes, while its nodes are in cache, or on the first call otherwise; O(1) lookups); shorter ones are walked |
| `rtWNew()` | `int64_t zell_encoding_json_rt_w_new(void)` | a writer: one uniquely owned `zell_string` grown in place (`zell__object_resize`, doubling) |
| `rtWFinish(w)` | `zell_string *zell_encoding_json_rt_w_finish(int64_t w)` | +1 the text written so far; frees the writer (its capacity stays with the string) |
| `rtIndent(text, prefix, indent)` | `zell_string *zell_encoding_json_rt_indent(const zell_string *text, const zell_string *prefix, const zell_string *indent)` | +1 `text` re-laid out as Go `json.Indent` (`FormatIndent`): whitespace outside strings dropped, a newline + `prefix` + `indent` per depth after `{`/`[`/`,` and before `}`/`]`, `": "` after keys, `{}`/`[]` kept; strings copied as they are. The input is not validated |
| `rtWRaw(w, s)` | `void zell_encoding_json_rt_w_raw(int64_t w, const zell_string *s)` | appends the bytes of `s` as they are (`{`, `,`, `null`, ...) |
| `rtWKey(w, name, first)` | `void zell_encoding_json_rt_w_key(int64_t w, const zell_string *name, bool first)` | `,` unless `first`, then the quoted name and `:` |
| `rtWStr(w, s)` | `void zell_encoding_json_rt_w_str(int64_t w, const zell_string *s)` | the quoted string (as `Quote`) |
| `rtWInt(w, v)`, `rtWUint(w, v)` | `void zell_encoding_json_rt_w_{int,uint}(int64_t w, int64_t/uint64_t v)` | decimal digits |
| `rtWFloat(w, f)` | `void zell_encoding_json_rt_w_float(int64_t w, double f)` | the shortest round-trip text as `println` prints floats (fixed unless the exponent is < -4 or >= 6, `-0`); NaN and infinities as `null` |
| `rtWBool(w, b)` | `void zell_encoding_json_rt_w_bool(int64_t w, bool b)` | `true` / `false` |
| `rtWNode(w, n)` | `void zell_encoding_json_rt_w_node(int64_t w, int64_t n)` | the compact text of a node's subtree (node 0: `null`); int nodes as integers, other numbers as `rtWFloat` |
| `rtWKeyInt(w, name, first, v)`, `rtWKeyStr`, `rtWKeyFloat`, `rtWKeyBool` | `void zell_encoding_json_rt_w_key_{int,str,float,bool}(int64_t w, const zell_string *name, bool first, <value>)` | `rtWKey` followed by the value, in one call (the struct members of `Format<T>`) |
| `rtWStrs(w, strs)` | `void zell_encoding_json_rt_w_strs(int64_t w, const zell_slice *strs)` | a `[]string` (`elem_size` 8) as `["a","b"]` (the caller writes `null` for a nil slice) |

`json.Value`, `Parse<T>` (the typed decoder specialised per `T` at compile
time, calling the node queries), `Format<T>` (calling the writer with
constant field names) and the `New*` constructors (which write a text and
parse it) are Zell (`compiler/src/sema/reflect_src/json.zl`).

## 18. diag and log

Runtime halves of the Zell-source `diag` / `log`
(`compiler/src/sema/stdlib_src/{diag,log}.zl`), declared in the diag
section of `zell_rt.h`. Implemented in `src/diag.c` (slots, metrics,
files), `src/profile.c` (pprof profiles) and `src/platform/posix.c`
(SIGPROF sampler, stack bounds, ELF symbols).

| Zell (internal) | C | Notes |
|---|---|---|
| `diag.rtSlots()` | `zell_object *zell_diag_rt_slots(void)` | the process-wide `map[string]any` of the libraries' global state (+1, NULL until set). Callers hold `rtLock` while using it |
| `diag.rtSetSlots(m)` | `void zell_diag_rt_set_slots(zell_object *m)` | stores `m` (retained). The map is released by `zell_runtime_shutdown` before the leak report |
| `diag.rtLock()` / `rtUnlock()` | `void zell_diag_rt_lock(void)` / `zell_diag_rt_unlock(void)` | one global spin lock (yields while contended) |
| `diag.rtLiveObjects()` / `rtAllocations()` | `int64_t zell_diag_rt_live_objects(void)` / `zell_diag_rt_allocations(void)` | live objects; objects allocated since start (one relaxed atomic add per allocation) |
| `diag.rtLiveTasks()` / `rtQueueLen()` / `rtWorkers()` | `int64_t zell_diag_rt_live_tasks(void)` / `_queue_len` / `_workers` | live task objects; jobs in the run queue (walks it under the queue lock); `zell_sched_workers()` |
| `diag.rtMonoNanos()` | `int64_t zell_diag_rt_mono_nanos(void)` | monotonic clock |
| `diag.rtStartCPUProfile(path, hz)` | `void zell_diag_rt_start_cpu_profile(const zell_string *path, int64_t hz, zell_error **err)` | creates `path` (error if it cannot), allocates an 8 MB sample buffer and starts `ITIMER_PROF` at `hz` (default 100). Errors: already running, unsupported platform |
| `diag.rtStopCPUProfile()` | `int64_t zell_diag_rt_stop_cpu_profile(zell_error **err)` | stops, writes the profile, returns the number of samples. Samples that do not fit the buffer are dropped |
| `diag.rtHeapText()` | `zell_string *zell_diag_rt_heap_text(void)` | `"<count> <type> <bytes>\n"` per type of the live objects, most frequent first; `""` unless leak tracking with type names is on (`--leak-check` without `--release`) |
| `diag.rtHeapTypes()` | `zell_string *zell_diag_rt_heap_types(void)` (`src/diag_heap.c`) | `"<live>\t<allocated>\t<bytes>\t<type>\n"` per type, most live first: from the leak-check object list when it is on (`allocated` -1), else from the per-type counters of `ZELL_DIAG_HEAP=1` (read at startup: every allocation and free updates a per-thread table keyed by the type info, summed on demand; `bytes` = live x `type->size`), else `""` |
| `diag.rtHeapCounting()` | `bool zell_diag_rt_heap_counting(void)` | whether the counters are on |
| `runtime.rtCollectCycles()` / `runtime.rtCycleStat(i)` | `int64_t zell_runtime_rt_collect_cycles(void)` / `int64_t zell_runtime_rt_cycle_stat(int64_t i)` (`src/cycles.c`) | `runtime.CollectCycles` / `CycleStats` (3.8): stat 0 runs, 1 collected, 2 candidates, 3 examined, 4 pending, 5 threshold, 6 aborted, 7 mode |
| `diag.rtWriteHeapProfile(path)` | `void zell_diag_rt_write_heap_profile(const zell_string *path, zell_error **err)` | the same as a pprof profile (`inuse_objects`, `inuse_space`; one pseudo-function per type); error without tracking |
| `log.rtOpen(path)` | `int64_t zell_log_rt_open(const zell_string *path, zell_error **err)` | `open(O_WRONLY\|O_CREAT\|O_APPEND\|O_CLOEXEC, 0644)`; error `open <path>: <strerror>` |
| `log.rtWrite(fd, s)` | `void zell_log_rt_write(int64_t fd, const zell_string *s, zell_error **err)` | writes all of `s` (retries `EINTR`) |
| `log.rtClose(fd)` / `log.rtIsatty(fd)` | `void zell_log_rt_close(int64_t fd)` / `bool zell_log_rt_isatty(int64_t fd)` | |

**CPU profile format.** An uncompressed `perftools.profiles.Profile`
(`github.com/google/pprof/proto/profile.proto`): sample types
`samples/count` and `cpu/nanoseconds` (values `1` and the period), one
sample per SIGPROF tick with location ids leaf first, one location per
distinct address (non-leaf addresses are return address - 1), one function
per symbol name (`zell_app__main__run` is shown as `app.main.run`), one
mapping for the executable's text (`has_functions`), `time_nanos`,
`duration_nanos`, `period_type` `cpu/nanoseconds`, `period` = 1e9/hz.

**Sampler.** The SIGPROF handler (`SA_RESTART`) reads PC/FP/SP from the
`ucontext` (x86-64 and AArch64, Linux and macOS) and follows `{saved FP,
return address}` pairs while FP stays 8-aligned, increasing and inside the
thread's stack bounds; bounds are recorded by
`zell__plat_thread_stack_init` (every worker at start, and the thread that
starts the profiler); other threads report only the PC. Compiled Zell code
keeps frame pointers (`"frame-pointer"="all"` on every function, set in
`build.rs`) and the runtime is built with `-fno-omit-frame-pointer`.

---

## 19. Cryptography and TLS

The primitives behind `crypto.{aes, chacha, x25519, ecdh, ecdsa,
ed25519, rsa, hkdf}`, the additions to `crypto.{hash, hmac}`, and the
internal helpers of the Zell-source `crypto.x509`
(`compiler/src/sema/stdlib_src/x509.zl`) and `net.tls`
(`tls.zl`, `tls_client.zl`, `tls_server.zl`). C declarations:
`include/zell_crypto.h`; wrappers: `src/crypto/api.c`; primitives:
`src/crypto/*.c` behind the private header `src/crypto/crypto.h`
(`zell__` symbols, not ABI). Calling convention of 14.1: `[]byte`
arguments are borrowed slices (NULL = empty), `[]byte` results are
written to a trailing `zell_slice *out` (`elem_size` 1, NULL ops), `T!`
adds a trailing `zell_error **err`; on error `*out` is the empty slice.
Hash names are `"sha256"`, `"sha384"`, `"sha512"`, `"sha1"` (case and `-`
ignored); an unknown one is the error `crypto: unknown hash "<name>" ...`.

**Primitives** (all constant time in secret data unless noted):

| File | Contents |
|---|---|
| `sha2.c` | SHA-256/384/512, SHA-1; streaming `zell_hash_ctx` (plain bytes, copyable); HMAC. x86-64: SHA-256 compression with the SHA extensions (`sha256rnds2`/`msg1`/`msg2` intrinsics, `__attribute__((target))`) when cpuid reports SHA (leaf 7 EBX bit 29), SSSE3 and SSE4.1 (`zell__cpu_has_shani`); `ZELL_NO_SHANI=1` forces the portable path. Both paths give identical digests |
| `hkdf.c` | HKDF-Extract/Expand (RFC 5869) |
| `aes.c` | AES-128/192/256 encryption and GCM. Portable path: 64-bit bitsliced AES (4 blocks per call; Boyar-Peralta S-box circuit), GHASH by integer multiplication with holes (BearSSL `ctmul64` style). x86-64: AES-NI + PCLMULQDQ intrinsics (`__attribute__((target))`) when cpuid reports AES, PCLMULQDQ, SSSE3 and SSE4.1; `ZELL_NO_AESNI=1` forces the portable path. GCM there runs 8 counter blocks through the AES rounds at once with an aggregated 8-block GHASH (powers H^1..H^8 precomputed in `zell_gcm_ctx.hpow`, one reduction per batch) interleaved into those rounds. When cpuid also reports VAES, VPCLMULQDQ and AVX2 and XGETBV shows the OS saves YMM state (`zell__cpu_has_vaes`), those batches run two blocks per 256-bit instruction; `ZELL_NO_VAES=1` keeps the 128-bit AES-NI loop (not built by GCC for Windows, which misaligns 32-byte stack spills). All paths give identical output. The key schedule always runs on the bitsliced S-box |
| `chacha20poly1305.c` | ChaCha20, Poly1305 (radix 2^44, 4 blocks per reduction), the RFC 8439 AEAD. x86-64: ChaCha20 8 blocks per pass with AVX2 (`target("avx2")`, `zell__cpu_has_avx2`: cpuid 7.0 EBX bit 5 + OSXSAVE/XGETBV YMM state) and the Poly1305 of the ciphertext interleaved into its round loop in seal/open; `ZELL_NO_AVX2=1` forces the portable path. Both paths give identical output |
| `fe25519.h`, `x25519.c`, `ed25519.c` | GF(2^255-19) with 51-bit limbs (`unsigned __int128`), X25519 ladder (RFC 7748), Ed25519 (RFC 8032; extended coordinates, constant-time fixed-base comb over the precomputed `ed25519_table.h` with cmov table scans, variable-time sliding-window double multiplication for verification, constant-time mod-L reduction by three `lo - hi c` rounds) |
| `p256.c` | P-256: Montgomery arithmetic mod p and mod n on 4x64-bit limbs, complete projective addition and doubling (Renes-Costello-Batina 2016, alg. 4 and 6), 4-bit fixed windows with full-table scans for variable points and a constant-time signed radix-16 comb over the generated affine table `p256_table.h` (65 rows x 8 points, cmov scans) for G; ECDH, ECDSA with RFC 6979 nonces hedged with 32 random bytes; verification uses the same constant-time code |
| `rsa.c` | RSA up to 8192 bits, 64-bit limbs, Montgomery. Verify PKCS#1 v1.5 / PSS (public, variable time). Sign from a PKCS#1 `RSAPrivateKey` DER: CRT, exponent blinding `d_p + r(p-1)`, constant-time 4-bit window exponentiation, result checked with `e` |
| `util.c` | wiping, constant-time compare, CSPRNG (`zell__plat_random_bytes`), cpuid |

**Public externs** (`crypto.*`):

| Zell | C | Notes |
|---|---|---|
| `hash.Sha384(s)` / `Sha384Hex(s)` | `void zell_crypto_hash_sha384(const zell_string *, zell_slice *out)` / `zell_string *zell_crypto_hash_sha384_hex(const zell_string *)` | like `Sha256` (14.3) |
| `hash.Sum(alg, data) []byte!` | `void zell_crypto_hash_sum(const zell_string *alg, const zell_slice *data, zell_slice *out, zell_error **err)` | digest of bytes |
| `hmac.Sum(alg, key, data) []byte!` | `void zell_crypto_hmac_sum(alg, key, data, out, err)` | |
| `hkdf.Extract(alg, salt, ikm)` / `Expand(alg, prk, info, n)` | `zell_crypto_hkdf_extract(alg, salt, ikm, out, err)` / `zell_crypto_hkdf_expand(alg, prk, info, int64_t n, out, err)` | empty salt = HashLen zeros; `n > 255*HashLen` is an error |
| `aes.GcmSeal(key, nonce, pt, aad)` / `GcmOpen(key, nonce, ct, aad)` | `zell_crypto_aes_gcm_seal(key, nonce, pt, aad, out, err)` / `_open` | 16/24/32-byte key, 12-byte nonce; output = ciphertext \|\| 16-byte tag; a bad tag is `aes-gcm: message authentication failed` |
| `aes.EncryptBlock(key, block)` / `aes.HasHardware()` | `zell_crypto_aes_encrypt_block(key, block, out, err)` / `bool zell_crypto_aes_has_hardware(void)` | one raw block (16 bytes) |
| `chacha.Seal` / `chacha.Open` | `zell_crypto_chacha_seal(key, nonce, pt, aad, out, err)` / `_open` | 32-byte key |
| `x25519.GenerateKey()` / `PublicKey(priv)` / `SharedSecret(priv, peer)` | `zell_crypto_x25519_generate_key(out, err)` / `_public_key(priv, out, err)` / `_shared_secret(priv, peer, out, err)` | 32-byte values; an all-zero result (small-order peer) is an error |
| `ecdh.GenerateKey()` / `PublicKey(priv)` / `SharedSecret(priv, peer)` | `zell_crypto_ecdh_*` | P-256: 32-byte scalar, 65-byte uncompressed point (validated), 32-byte x coordinate |
| `ecdsa.GenerateKey` / `PublicKey` / `Sign(priv, digest)` / `Verify(pub, digest, sig) bool` | `zell_crypto_ecdsa_*`; `bool zell_crypto_ecdsa_verify(pub, digest, sig)` | P-256; signatures are ASN.1 DER; the leftmost 256 bits of longer digests are used; `Verify` requires strict DER |
| `ed25519.GenerateKey()` / `PublicKey(seed)` / `Sign(seed, msg)` / `Verify(pub, msg, sig) bool` | `zell_crypto_ed25519_*` | 32-byte seed, 32-byte key, 64-byte signature |
| `rsa.VerifyPKCS1v15(n, e, hash, digest, sig) bool` / `VerifyPSS(...)` | `bool zell_crypto_rsa_verify_pkcs1v15(n, e, hash, digest, sig)` / `_verify_pss` | big-endian modulus and exponent; PSS: MGF1 with the same hash, any salt length |
| `rsa.SignPKCS1v15(key, hash, digest)` / `SignPSS(key, hash, digest)` | `zell_crypto_rsa_sign_pkcs1v15(key, hash, digest, out, err)` / `_sign_pss` | `key` = PKCS#1 `RSAPrivateKey` DER; PSS salt = hash length |

**Internal externs** (Zell names start with `rt`):

| Zell (internal) | C | Semantics |
|---|---|---|
| `hash.rtNew(alg) []byte!` / `rtWrite(st, data)` / `rtSum(st) []byte` | `zell_crypto_hash_rt_new`, `_rt_write(const zell_slice *st, const zell_slice *data)`, `_rt_sum(st, out)` | streaming hash whose state is the bytes of a `zell_hash_ctx` kept in a `[]byte` (`rtWrite` updates it in place; `rtSum` finalizes a copy, so hashing can continue); a malformed state panics |
| `x509.rtSystemRootsPath() string` | `zell_string *zell_crypto_x509_rt_system_roots_path(void)` | `zell__plat_cert_bundle_path()` (15: `$SSL_CERT_FILE`, then the Linux/BSD bundle locations; NULL -> `""`) |
| `tls.rtConcat(a, b) []byte` | `void zell_net_tls_rt_concat(a, b, out)` | new slice `a \|\| b` |
| `tls.rtZeros(n) []byte` | `void zell_net_tls_rt_zeros(int64_t n, out)` | `n` zero bytes |
| `tls.rtCopy(dst, off, src)` | `void zell_net_tls_rt_copy(const zell_slice *dst, int64_t off, const zell_slice *src)` | `dst[off..off+len(src)] = src` in place; out of range panics |
| `tls.rtAeadNew(kind, key) []byte!` | `void zell_net_tls_rt_aead_new(int64_t kind, key, out, err)` | record-layer AEAD context: byte 0 = kind (1 AES-GCM: followed by the `zell_gcm_ctx` image with expanded keys, H and, on the AES-NI path, H^1..H^8; 2 ChaCha20-Poly1305: followed by the key) |
| `tls.rtAeadSeal(ctx, nonce, aad, pt) []byte` / `rtAeadOpen(ctx, nonce, aad, ct) []byte!` | `zell_net_tls_rt_aead_seal(ctx, nonce, aad, pt, out)` / `_open(..., out, err)` | 12-byte nonce; a bad tag is the error `tls: bad record MAC`; an invalid context panics |

The TLS protocol itself (record framing, key schedule, handshake state
machines, tickets) is Zell over these and over `net` (16); TLS
connections use the same non-blocking sockets and readiness waits, so a
handshake never blocks a worker thread. Platform layer (15):
`src/platform/certroots.c` provides `zell__plat_cert_bundle_path()`
(Linux/BSD paths; stubs elsewhere).

## 20. Debugger support

The DWARF the compiler emits (compiler/src/codegen/debug.rs) describes the
layouts of sections 3-7 directly; the lldb formatters and gdb printers in
`tools/` read them from memory (no calls into the debuggee, so they also
work on core dumps). The one private layout they need is exported by
`src/task.c`:

```c
const int64_t zell_debug_task_layout[4] = {
    1,                                   /* version of this table */
    offsetof(struct zell_task, state),  /* int32_t, atomic */
    offsetof(struct zell_task, slot),   /* result slot (result_size bytes) */
    TASK_DONE,                           /* value of `state` once completed */
};
```

It is read-only data and never used by compiled code; a `--release` link
(`--gc-sections`) may drop it, and the formatters then fall back to the
x86-64 glibc offsets (104, 216, 1).

## 21. Tier-2 stdlib helpers (regex, compress, xml, html, mime)

The tier-2 libraries (`docs/STDLIB.md`, level 2) are Zell
(`compiler/src/sema/stdlib_src/`); C does only the parts that need tight
loops over bytes or large generated tables. All functions follow the usual
conventions: `string` by `const zell_string *`, slices in/out by
`zell_slice *`, `T!` results with a trailing `zell_error **err`. They hold
no global mutable state except the mime table (under a mutex). The C tests
are `runtime/tests/test_std3.c`.

| Zell | C | Notes |
|---|---|---|
| `regex.rtUnicode(name) []int` | `void zell_regex_rt_unicode(name, out)` | sorted `lo, hi` rune ranges of a Unicode category or script (`std_regex_tables.h`, generated from Go 1.24's `unicode`; 199 names); empty for an unknown name |
| `regex.rtFold(c) int` | `int64_t zell_regex_rt_fold(c)` | next rune of the simple case-folding orbit of `c` (`c` itself if none), as Go's `unicode.SimpleFold` |
| `regex.rtInts(n) []int`, `rtBytes(n) []byte` | `zell_regex_rt_ints(n, out)`, `_bytes` | zeroed buffers (DFA tables) |
| `regex.rtIndex(s, sub, from) int` | `int64_t zell_regex_rt_index(s, sub, from)` | `memchr`/`memmem` search from byte offset `from`; -1 if absent (also used by textproto/multipart) |
| `regex.rtTryLock(*int) bool`, `rtUnlock(*int)` | `zell_regex_rt_try_lock(slot)`, `_unlock` | an atomic CAS flag guarding a compiled regex's lazy-DFA cache; a matcher that loses the race uses the Pike VM instead of waiting |
| `compress.rtCrc32(crc, b)`, `rtAdler32(a, b)` | `int64_t zell_compress_rt_crc32(crc, data)` / `_adler32` | running checksums (CRC-32 IEEE slicing-by-8); start with 0 / 1 |
| `compress.rtInflateNew() []byte` | `void zell_compress_rt_inflate_new(out)` | a resumable inflate state; it is POD (window, tables, bit buffer) stored in a `[]byte`, so it is freed with the slice |
| `compress.rtInflate(st, in) []byte!` | `void zell_compress_rt_inflate(st, in, out, err)` | feeds raw DEFLATE input, returns the bytes produced; errors `flate: corrupt input (...)` |
| `compress.rtInflateDone/Tail/Unused(st)` | `bool _done`, `_tail(st, out)`, `int64_t _unused` | final block seen / input bytes after the end of the stream / their count |
| `compress.rtDeflate(dict, data, level, final) []byte` | `void zell_compress_rt_deflate(dict, data, level, final, out)` | compresses `data` with up to 32 KiB of preceding `dict` as history (streaming); levels 0–9 (zlib-like lazy matching), stored/fixed/dynamic block choice; a non-final call ends with a sync flush (byte aligned) |
| `compress.rtConcat(a, b)`, `rtCopy(b, lo, hi)`, `rtZeros(n)` | `zell_compress_rt_concat/copy/zeros` | memcpy helpers |
| `compress.rtZstdDecode(b) []byte!` | `void zell_compress_rt_zstd_decode(data, out, err)` | RFC 8878 decoder (all frames, skippable frames, checksums, dictionaries are not supported); `rtZstdFrameLen(b)` is the length of the first complete frame or -1; `rtXxh64(b)` is XXH64 with seed 0 |
| `encoding.xml.rtScan(text, strict) []string!` | `void zell_encoding_xml_rt_scan(text, strict, out, err)` | tokenizes a document into lines: first char `S` start tag, `A`/`V` attribute name/value, `>`/`/` end of start tag (self-closing), `E` end tag, `T` text (entities decoded), `C` comment, `P` proc-inst, `D` directive; errors carry the line |
| `encoding.xml.rtEscape(s, attr) string` | `zell_string *zell_encoding_xml_rt_escape(s, attr)` | `&<>"'` (and `\t\n\r` in attributes) |
| `html.Escape`, `html.Unescape` | `zell_html_escape(s)`, `zell_html_unescape(s)` | Go's `html.EscapeString`/`UnescapeString` with all 2231 HTML5 entities (`std_html_entities.h`) |
| `mime.rtType(ext)`, `rtAdd(ext, type)`, `rtExts(type)` | `zell_mime_rt_type/add/exts` | the extension table (built-ins + additions) |

## 22. The in-process debug agent

Programs built with `--debug-agent` (the default for debug builds; release
builds only with the flag) carry statement hooks and static tables
(`compiler/src/codegen/debug_agent.rs`); the agent itself is Zell code
(`debug`, `compiler/src/sema/stdlib_src/debug*.zl`) that the compiler
instantiates and links in. The C side (`src/debug.c`) filters the hot path,
keeps the frame chain and per-task stop/step state, blocks stopped tasks,
and gives `debug` its primitives. docs/DEBUGGER.md (stage 2) describes
the design.

### 22.1 Tables

One `zell_dbg_tables` per program (`zell.dbg.tables`, passed to
`zell_dbg_boot`); every record is little-endian, 8-aligned:

```c
typedef struct { int32_t file, line, col, func, scope, kind; } zell_dbg_stmt;   /* 24 bytes */
typedef struct {
    zell_string *name;        /* +0  full name: "app.Point.Length", "app.Max<int>" (immortal literal) */
    zell_string *short_name;  /* +8  "Length" */
    int32_t file, line;        /* +16 +20  declaration */
    int32_t first_stmt;        /* +24 the entry statement (kind 1) */
    int32_t last_stmt;         /* +28 */
    int32_t nlocals, flags;    /* +32 +36  flags: 1 = async */
    const zell_dbg_local *locals;  /* +40 nlocals records, NULL in optimised builds */
    const void *addr;          /* +48 the function */
} zell_dbg_func;              /* 56 bytes */
typedef struct {
    zell_string *name;        /* +0 */
    const zell_type_info *type;  /* +8 a full reflect.TypeInfo (22.4) */
    int32_t slot;              /* +16 index into the frame's locals array */
    int32_t flags;             /* +20 1 = the slot holds a heap cell (value at cell + 16) */
    int32_t line, end_line;    /* +24 +28 visible after `line` (parameters: from `line`) up to `end_line` */
} zell_dbg_local;             /* 32 bytes */
typedef struct {
    int64_t version;           /* 1 */
    int64_t nstmts;  const zell_dbg_stmt *stmts;
    int64_t nfuncs;  const zell_dbg_func *funcs;
    int64_t nfiles;  zell_string *const *files;   /* absolute paths, by HIR file id */
    uint8_t *bp;               /* nstmts bytes: 1 = breakpoint (written by the agent) */
} zell_dbg_tables;            /* 64 bytes */
```

Statement kinds: 0 a statement (every HIR statement with a span, plus one
at each function's closing brace, run by every return), 1 a function
entry (function breakpoints; steps ignore it), 2 reserved. `func` indexes
`funcs`; statement ids are global and dense. Only user code gets records:
functions of the standard library (full name under a stdlib root; the agent included) have no
hooks.

### 22.2 Hooks and frames

```c
typedef struct zell_dbg_frame {
    struct zell_dbg_frame *prev;   /* +0  caller (the per-thread chain) */
    const zell_dbg_func *func;     /* +8  */
    void *const *locals;            /* +16 address of each local's slot, NULL in optimised builds */
    int32_t stmt;                   /* +24 current statement */
    int32_t active;                 /* +28 0 unlinked, 1 linked, 2 suspended at an await */
} zell_dbg_frame;                  /* 32 bytes, in the function's frame (coroutine frame for async) */
extern uint8_t zell_dbg_on;        /* hidden; 1 once the agent runs */
```

Every hooked function loads `zell_dbg_on` once at entry and calls, each
behind `if (on)`:

| Call | Where |
|---|---|
| `zell_dbg_enter(&F, &func, locals, entry_stmt)` | after the parameters are stored; initialises `F` (compiled code does not zero it), links it, runs the entry statement |
| `zell_dbg_stmt(&F, id)` | before every statement |
| `zell_dbg_stmt(&F, close)` then `zell_dbg_leave(&F)` | at the start of the exit block (`close` is the closing-brace statement); a frame-linked coroutine (10.3, its task slot odd) calls `zell_dbg_leave_linked(&F)` instead: its end is not the end of the task it ran under |
| `zell_dbg_suspend(&F)` | in `await`, before the continuation is registered (the frame leaves this thread's chain) |
| `zell_dbg_resume(&F)` | once the awaited task is ready (no-op unless suspended): the frame joins the chain of the thread that resumed it |

`zell_dbg_boot(&tables, entry)` runs in the C `main` right after
`zell_runtime_init`: with `ZELL_DEBUG` set and an agent linked (`entry`
is `debug.AgentMain` wrapped as `void(void)`, NULL otherwise) it sets
`zell_dbg_on` and calls `entry`. `zell_dbg_exit(code)` runs before the C
`main` returns and in `zell_exit` (`os.Exit`): it calls the agent's exit
hook once.

**Optimised builds** (`--release --debug-agent`) emit every hooked non-async
function twice: `sym.dbg` with the hooks, and `sym` without them, starting
with `if (zell_dbg_on) return sym.dbg(args...)`. Direct calls inside
`.dbg` copies go to `.dbg` copies; a plain function only called directly
from plain functions loses its check (plain code is entered with the agent
on only through a check: the C `main`, function values, vtables, code
without copies). With the agent off the plain code is that of a build
without the agent; locals are not described (`locals` NULL).

**Tasks.** A task gets a debug record (`zell_task.dbg_id`, never reused)
the first time it needs one; the C `main` thread is record 1. Async
functions run their ramp on the caller's thread, so a fresh coroutine's
frame links above its caller's until it first suspends.

### 22.3 The agent's hook

`debug.rtSetHook(func(stmt, frame, reason, task int) int)` registers
the callback `zell_dbg_stmt` (and the panic / error hooks) calls, with
hooks disabled on the calling thread, when a statement has a breakpoint
byte, a step of some task ends there, a freeze is on, or the next statement
was asked to stop (`stopOnEntry`). Reasons (bit set): 1 breakpoint, 2 step,
4 pause (freeze), 8 panic (`zell_panic*`, before the message is printed),
16 error creation (`zell_error_created`, when the agent asked for it),
32 entry. Before the call the task is marked paused at `frame` (so the
client can read its stack as soon as the `stopped` event is out). The
result: 0 go on, 1 stopped (reported to the client; blocks until
`rtResume` lets this task go), 2 frozen while another task is stopped
(silent; ends with the freeze).

Steps are per task: `rtResume(task, mode, single)` with mode 1 next, 2 step
in, 3 step out records the paused frame, its caller and depth; a step ends
at a statement of the same frame on another line (or an earlier statement:
a loop), in the caller once the frame returned (not when it only suspended
at an `await`: then the step goes on when the coroutine resumes, in its
own task), or (step in) in a callee of the frame. Without `single` the
freeze ends and every stopped task goes on (a task still deciding in its
hook is not released by another task's resume).

**Task records and ids** (the DAP `threadId`). A record is made when code
first stops (a hook that reports to the agent) or when a task suspends at
an `await`; statement hooks never make one. Code with no current task (the
main thread) uses the main record (id 1 at first). The id lives in
`zell_task.dbg_id` and stays with the task object across all its awaits.
Before its first suspension a coroutine runs in its caller's task (10.3),
so it is observed under the caller's record; at that first suspension
(`zell_dbg_suspend` finds the task without an id), if a step is under way
in that very frame the coroutine's new task **takes over** that record
(the caller, or the main thread, loses it and gets a fresh record if it
stops again), otherwise the task gets a fresh record. A taken-over record
goes on, when its task completes (`zell_dbg_leave` of the bottom async
frame), with the **awaiter** (the current task of the first continuation
registered on the task, `zell__task_awaiter`): the awaiter's own record,
made when it suspended, is retired unless it ever reported a stop. So one
logical await chain keeps one id; a frame-linked chain (10.3) runs under
its root's task and record throughout. `zell__task_parent`, `zell__task_awaiter`
(task.c, borrowed results) serve this.

### 22.4 What debug reads

Values are read with `unsafe` through the tables, frames (22.1, 22.2)
and the reflection tables of §17: a local's `type` is a full
`reflect.TypeInfo` (codegen loads reflection whenever the agent is
linked), read at the offsets of its LLVM layout: `Kind` (i32) +24, `Name`
+32, `Size` +48, `Fields` +64 (slice of `*FieldInfo`), `Variants` +128,
`Elem` +160, `Key` +168, `Len` +176; `FieldInfo`: `Name` +0, `Type` +8,
`Offset` +16; `VariantInfo`: `Name` +0, `Fields` +16. Interfaces use the
vtable header (`vtable[0]` type, `vtable[1]` boxed), `any` its type word;
`T!` keeps its error at `round8(sizeof T)`. Runtime helpers:

| Zell (`debug`) | C | Notes |
|---|---|---|
| `rtTables() int` | `zell_debug_rt_tables` | the tables (0 without agent) |
| `rtSetHook(f)`, `rtSetExitHook(f)` | `zell_debug_rt_set_hook/_exit_hook(fn, env)` | closures as two words, env retained |
| `rtSetBp(stmt, on)`, `rtOnError(on)`, `rtStopNext()` | | breakpoint byte, error-creation reports, stop at the next statement |
| `rtFreeze(on)`, `rtResume(task, mode, single) bool`, `rtReleaseAll()` | | 22.3; `rtReleaseAll` clears every breakpoint, step and freeze (disconnect) |
| `rtTasks() []int`, `rtTaskState(t)`, `rtTaskFrame(t)`, `rtTaskFunc(t)`, `rtTaskReason(t)` | | live task records; state 0 running, 1 paused, 2 at an await, -1 gone; the top frame of a paused/awaiting task |
| `rtLock(n)`, `rtUnlock(n)` | | two agent mutexes (0 state, 1 output; taken in that order) |
| `rtWaitReady()`, `rtSignalReady()` | | `,wait`: `AgentMain` blocks `main` until `configurationDone` |
| `rtIgnoreHooks()` | | no hooks on this thread (the agent thread) |
| `rtPanicText()`, `rtCurError() int`, `rtErrorText(e)`, `rtErrorType(e)` | | the panic / created error being reported; message (may run a user `Message()`), typed error's type name |
| `rtString(addr) string` | `zell_debug_rt_string` | the `string` stored at `addr`, retained |
| `rtMapLen(m)`, `rtMapEntries(m, start, n) []int` | | key/value slot addresses of entries `start..start+n` |
| `rtTaskDone(t)`, `rtTaskSlot(t)`, `rtSymName(fn)`, `rtTypeName(ti)` | | `Task<T>` state and result slot; symbol of a code address |
| `rtListen(host, port) int!`, `rtSockPort(fd)`, `rtAccept(fd)`, `rtRecv(fd, max) []byte`, `rtSend(fd, s) bool`, `rtClose(fd)` | | blocking TCP on a private poller: the agent thread is no scheduler task and must answer while every worker is blocked in a stopped task |
| `rtEprint(s)`, `rtExit(code)`, `rtNowMs()` | | unbuffered stderr, `_exit` after flushing, monotonic ms |
| `rtPathCanon(path) string` | `zell_debug_rt_path_canon` | a breakpoint path's canonical spelling (`zell__plat_path_canon`: `realpath`; Windows `GetFinalPathNameByHandleW` without `\\?\`), the path itself (retained) if it does not exist |
| `rtPathMatch(file, path) int` | `zell_debug_rt_path_match` | 3 same file, 2 `path` is a tail of `file` on a `/` boundary, 1 same base name, 0 none; with `zell__plat_path_fold()` (Windows) case-insensitive and `\` = `/` (`zell__dbg_path_match`, tested in `runtime/tests/test_debug.c`) |
| `rtWriteFloat(addr, size, f)`, `rtSetString(addr, s)` | | `setVariable`: a 4/8-byte float; a `string` slot takes `s` (borrowed, retained for the slot) and releases its old value. Integers, `bool` and `char` are written with `unsafe.WriteInt` |

### 22.5 unsafe

`ReadInt/ReadUint(addr, size)`, `ReadFloat(addr, size)`, `WriteInt(addr,
size, v)`, `ReadBytes(addr, n)`, `WriteBytes(addr, b)`, `CString(addr)`,
`AddrOf(x any)` / `TypeOf(x any)` (the two words of an `any`),
`CallFunc(addr, []int) int` (C ABI, up to 8 integer arguments). Nothing is
checked. Importing `unsafe` (and `plugin`) needs the capability in
the package manifest (`[capabilities] unsafe = true` / `plugin = true`);
plain (manifest-less) builds may not; the standard library may.

### 22.6 thread

`Spawn(f func()) int` starts an OS thread outside the scheduler (`await`
there blocks), `Join(h)`, `Sleep(ms)`.

### 22.7 plugin

`rtOpen(path) int!`, `rtSym(h, name) int!`, `rtClose(h)`: `dlopen(RTLD_NOW |
RTLD_LOCAL)` / `dlsym` through the platform layer (`zell__plat_dl_*`), for
expressions compiled outside the process (not used by the v1 evaluator).

## 23. Tier-3 stdlib helpers (sql, sqlite, crypto additions, zstd encoder)

The tier-3 libraries (`docs/STDLIB.md`, level 3) are Zell
(`compiler/src/sema/stdlib_src/{sql,sqlite,postgres,postgres_types,mail,kdf,password,rsa}.zl`);
the C side follows the usual conventions (14.1). C tests:
`runtime/tests/test_std4.c`.

### 23.1 Linking system libraries per module

`stdlib.rs` `LINK_LIBS` maps a runtime symbol prefix to a system library:
`build.rs` adds `-l<lib>` to the link only when the program's LLVM module
declares a function with that prefix (after optimisation for plain builds,
on the whole module for package builds). Today: `zell_database_sqlite_`
-> `-lsqlite3`. `std_sqlite.c` compiles without `<sqlite3.h>` too (every
function then fails with `sqlite: not available` and `rtAvailable()` is
false), but a program using the driver still needs the library to link.

### 23.2 database.sql / database.sqlite

| Zell | C | Notes |
|---|---|---|
| `sql.ErrNoRows`, `ErrTxDone`, `ErrConnDone` | `zell_database_sql_err_no_rows()` / `_tx_done` / `_conn_done` | immortal sentinels (`std_sql.c`) |
| `sqlite.rtOpen(path, flags) int!` | `zell_database_sqlite_rt_open` | `sqlite3_open_v2` (`flags` 0: read-write + create) with URI names and the serialized threading mode; extended result codes on; busy timeout 5 s. The handle is the `sqlite3 *` as an int, owned by the Zell `conn` (its `Drop` closes it) |
| `rtClose(h)`, `rtBusyTimeout(h, ms)`, `rtExec(h, sql) void!` | | `sqlite3_close_v2` (deferred until the statements are finalized), `sqlite3_exec` (scripts) |
| `rtPrepare(h, sql) int!` | | `sqlite3_prepare_v2`; a non-blank tail (second statement) is an error |
| `rtFinalize(st)`, `rtReset(st)`, `rtParamCount(st)`, `rtParamIndex(st, name)` | | `rtReset` also clears bindings |
| `rtBindNull/Int/Float/Text/Blob(st, i, v) void!` | | 1-based; text and blobs are copied (`SQLITE_TRANSIENT`) |
| `rtStep(st) int!` | | 1 row, 0 done |
| `rtColumnCount/Name/Decltype/Type/Int/Float/Text/Blob(st, i)` | | type codes 1 INTEGER, 2 FLOAT, 3 TEXT, 4 BLOB, 5 NULL |
| `rtChanges(h)`, `rtLastInsertId(h)`, `rtErrcode(h)`, `rtAutocommit(h)`, `rtAvailable()`, `rtVersion()` | | extended error code of the last failure |

### 23.3 Crypto additions (`crypto/{p384,cbc,kdf,api2}.c`, `std_argon2.c`)

| Zell | C | Notes |
|---|---|---|
| `kdf.Pbkdf2(pw, salt, iter, keyLen, hash) []byte!` | `zell_crypto_kdf_pbkdf2` | RFC 8018 over HMAC-SHA1/256/384/512 |
| `kdf.Scrypt(pw, salt, N, r, p, keyLen) []byte!` | `zell_crypto_kdf_scrypt` | RFC 7914; `N` a power of two, `r*p < 2^30`; `128 r N` bytes of memory |
| `kdf.rtArgon2(type, pw, salt, secret, ad, t, mKiB, p, keyLen) []byte!` | `zell_crypto_kdf_rt_argon2` | RFC 9106 v0x13; type 0 d, 1 i, 2 id; salt >= 8 bytes, secret <= 32 bytes; lanes are filled sequentially; memory is `malloc`ed and wiped |
| `hash.Blake2b(data, size, key) []byte!` | `zell_crypto_hash_blake2b` | RFC 7693, size 1..64, key <= 64 bytes |
| `ecdh.GenerateKeyP384()`, `PublicKeyP384(priv)`, `SharedSecretP384(priv, peer)` | `zell_crypto_ecdh_*_p384` | 48-byte scalars, 97-byte uncompressed points; constant-time 6x64-bit Montgomery arithmetic as P-256 |
| `ecdsa.GenerateKeyP384`, `PublicKeyP384`, `SignP384(priv, digest)` (DER), `VerifyP384(pub, digest, sig) bool`, `SignP384Raw` (r‖s, RFC 6979 without added randomness, for vectors) | `zell_crypto_ecdsa_*_p384` | nonces: RFC 6979 with HMAC-SHA384, hedged with 48 random bytes |
| `aes.DecryptBlock`, `CbcEncrypt/CbcDecrypt` (PKCS#7), `CbcEncryptRaw/CbcDecryptRaw`, `Pkcs7Pad/Pkcs7Unpad(data, block)` | `zell_crypto_aes_*` | decryption: AES-NI or a table-free byte-oriented inverse cipher; unpadding is constant time (`aes-cbc: invalid padding`) |
| `rsa.EncryptOAEP(n, e, hash, msg, label)`, `DecryptOAEP(keyDER, hash, ct, label)` | `zell_crypto_rsa_encrypt_oaep` / `_decrypt_oaep` | RFC 8017 7.1, MGF1 with the same hash; constant-time decoding, one error (`rsa: decryption error`) |
| `rsa.rtGenPrime(bits, e) []byte!`, `IsProbablePrime(p, rounds) bool` | | random candidates with the top two bits set, trial division by primes < 1000, `p mod e != 1`, Miller-Rabin (variable time: key generation data only); `rsa.GenerateKey` (Zell) builds the PKCS#1 key with `math.big` |

### 23.4 zstd encoder

`compress.rtZstdEncode(data, level, checksum) []byte!` ->
`zell_compress_rt_zstd_encode` (`std_zstd_enc.c`): one frame per call,
single segment up to 8 MiB (else an 8 MiB window), blocks of 128 KiB as
raw, RLE or compressed (Huffman literals with direct or FSE-coded weights;
sequences with predefined, RLE or per-block FSE tables; repeat offsets),
optional XXH64 checksum. Levels select the hash-chain depth (1..256) and
lazy matching (>= 5).

### 23.5 brotli decoder

`compress.rtBrotliDecode(data) []byte!` -> `zell_compress_rt_brotli_decode`
(`std_brotli.c`): RFC 7932 streams decoded whole (window bits 10..24;
large-window streams are an error). The static dictionary, the 121
transforms and the context lookup tables are in `std_brotli_tables.h`,
generated by `scripts/gen-brotli-tables.py` from the system
libbrotlicommon; the dictionary is stored zstd-compressed (54 KB) and
expanded once into a process-wide buffer on the first dictionary reference.

## 24. Tier-3b infrastructure clients (redis, kafka, s3, blob)

`data.redis`, `data.kafka`, `cloud.s3` and `cloud.blob` are
Zell (`compiler/src/sema/stdlib_src/{redis,redis_route,redis_client,kafka,
kafka_producer,kafka_consumer,kafka_admin,s3,s3_multi,blob}.zl`) over
net, net.tls, net.http and crypto. The C part is
`src/std_infra.c` (tests: `tests/test_infra.c`): sentinels and four byte
helpers, Zell namespace `data` (internal, names start with `rt`).

| Zell | C | Semantics |
|---|---|---|
| `data.rtAppend(a, b []byte) []byte` | `zell_data_rt_append(const zell_slice *a, const zell_slice *b, zell_slice *out)` | Go's `append(a, b...)`: `out` (+1) shares `a`'s backing when it has room, else grows it (amortized); used by the RESP writer and the Kafka encoder |
| `data.rtAppendStr(a []byte, s string) []byte` | `zell_data_rt_append_str` | the same with the bytes of a string |
| `data.rtCrc16(s string) int` | `zell_data_rt_crc16` | CRC16/XMODEM (poly 0x1021, init 0); `redis.HashSlot` takes it mod 16384 |
| `data.rtCrc32c(crc int, b []byte) int` | `zell_data_rt_crc32c` | CRC-32C (Castagnoli, reflected 0x82F63B78), slicing-by-8, continuing `crc` (0 to start); record batch v2 checksums. `uint32_t zell__crc32c(uint32_t, const uint8_t *, size_t)` is the same for C callers |
| `data.rtMurmur2(b []byte) int` | `zell_data_rt_murmur2` | the Kafka Java client's `Utils.murmur2` (seed 0x9747b28c) as a signed 32-bit value; the default partitioner is `(murmur2 & 0x7fffffff) % n` |

Sentinels (`StdConst::Error`, immortal, `errors.Is`):

| Zell | C | Message |
|---|---|---|
| `redis.Nil` | `zell_data_redis_err_nil()` | `redis: nil` |
| `redis.ErrClosed` | `zell_data_redis_err_closed()` | `redis: client is closed` |
| `redis.ErrTxFailed` | `zell_data_redis_err_tx_failed()` | `redis: transaction failed (a watched key changed)` |
| `redis.ErrPoolTimeout` | `zell_data_redis_err_pool_timeout()` | `redis: connection pool timeout` |
| `redis.ErrCrossSlot` | `zell_data_redis_err_cross_slot()` | `redis: keys of the command map to different cluster slots` |
| `kafka.ErrClosed` / `kafka.ErrNoBrokers` | `zell_data_kafka_err_closed()` / `_no_brokers()` | `kafka: client is closed` / `kafka: no broker is reachable` |
| `s3.ErrNoSuchKey`, `ErrNoSuchBucket`, `ErrAccessDenied`, `ErrPreconditionFailed` | `zell_cloud_s3_err_no_such_key()` ... `_precondition()` | `s3: NoSuchKey` ... ; an `s3.Error` with one of these codes is built with `net.rtOpError` (16.3): its text is `s3: <message> (<resource>)`, its inner error the sentinel and its payload the boxed `s3.Error` (`errors.As`) |
| `blob.ErrNotFound` | `zell_cloud_blob_err_not_found()` | `blob: not found` |

Design notes (the authoritative API is DESIGN.md, «Данные и форматы»):

- **Redis** connections are not shared between tasks: the pool hands one to
  a command (or a pipeline, a `Watch`) at a time; a failed or cancelled
  command marks its connection broken and the pool closes it on return.
  Pipeline command methods return tasks that wait on one waker fired by
  `Exec` (`zell_task_rt_waker`, 10.10). Subscriptions own a connection
  read by a background task; their callbacks and the Sentinel watcher hold
  the router through `weak` pointers so that `Close` frees everything.
- **Kafka** broker connections serialize requests (one in flight per
  connection, a `task.Mutex`); a client keeps separate connections per
  purpose (general, fetch, group) so a long-polling Fetch never delays a
  Heartbeat. The heartbeat task sleeps on a `task.Delay` that `Close`
  fires early (`zell_task_rt_fire`); it does not use a cancellation scope,
  which would keep the creating task — and the consumer it returns — alive.
- **S3** request bodies are in memory (`net.http` sends strings), so
  the payload is hashed; large objects go through multipart uploads.
