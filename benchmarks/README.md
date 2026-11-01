# Benchmarks: Zell vs Go

Equivalent programs in `*/main.zl` and `*/main.go`. Run with
`./scripts/bench-perf.sh [name...]` (needs `target/release/zell` and `go`).
The script builds both with release settings and reports wall time, CPU time
(user+sys) and peak RSS.

| Benchmark | What it exercises |
|---|---|
| `fib` | recursion, function calls (fib(38)) |
| `sieve` | slice append, indexing with bounds checks, inner loops (5 x 10M) |
| `map` | map[int]int insert/update/lookup (2M + 2M ops) |
| `strings` | string concat, Join/Split/ToUpper/Contains on a 1.2 MB string |
| `nbody` | structs in a slice, float math, method calls (5M steps) |
| `json_like` | string building: `fmt.Format` of 100k JSON-like records (ints, floats, bools) + `Join`, 5 rounds |
| `struct_map` | map[string]Point insert/update/lookup (100k keys, 2M updates, 100k lookups) |
| `tcp_echo` | net: 1000 concurrent loopback TCP connections, 1 MB each written in 64 KB chunks and echoed back (`ReadInto` into per-connection buffers) |
| `http_hello` | net.http server answering a keep-alive load generator (64 connections, 200k `GET /` in all); `run.sh` cross-measures both servers with both load generators in separate processes, then HTTP/2 over TLS (`--server-tls` / `-server-tls`) with Go's h2 client (`-load2`) |
| `regex` | regex: `FindAll` of an e-mail pattern over a 10 MB generated text (lazy DFA with prefix acceleration vs Go's `regexp`) |
| `gzip` | compress.gzip: level-6 compression and decompression of 20 MB of mixed text, round trip checked |
| `alloc_heavy` | binary trees of small heap nodes (depth 4..16) and 4M rounds of non-escaping temporaries (`new Stats{}`, `&acc` passed to a method) |
| `heap_churn` | 20 rounds of 40 MB of small strings (two size classes) plus 32 x 256 KB buffers, all dropped each round; prints the RSS after the last round (memory returned to the OS) |
| `shared_objects` | reference counts across threads: one task builds 200k objects that 8 tasks read 10 times, 8 tasks build objects one task reads, 8 tasks churn task-local objects; prints the ms of each phase |
| `spawn_tasks` ... `select_many` | the async scheduler suite (`./scripts/bench-perf.sh async`): tasks, channels, mutex, timers, await chains, cancellation, loopback latency, first-of-many; see "Async benchmarks" at the end |

## Results, performance pass 2 (2026-10-21, 4 vCPU x86-64, clang/LLVM 18, go 1.24, `--release`)

Wall time, best of 5 runs with the three binaries of a benchmark run
alternately (`before` = the compiler and runtime of commit ec01e80, `after`
= this tree). The machine is shared, so differences under about 8% are
noise (map, struct_map: repeated runs give either one ahead).

| bench | before s | after s | go s | after / go |
|---|---|---|---|---|
| fib | 0.115 | 0.110 | 0.223 | 0.49 |
| sieve | 0.318 | 0.304 | 0.605 | 0.50 |
| map | 0.208 | 0.224 | 0.331 | 0.68 |
| strings | 0.225 | **0.072** | 0.199 | **0.36** |
| nbody | 0.473 | 0.458 | 0.481 | 0.95 |
| json_like | 0.225 | 0.213 | 0.306 | 0.70 |
| struct_map | 0.206 | 0.212 | 0.226 | 0.94 |
| alloc_heavy (new) | 1.119 | **0.398** | 0.598 | **0.67** |
| regex | 0.266 | 0.230 | 0.610 | 0.38 |
| gzip | 1.931 | 1.881 | 2.093 | 0.90 |

Networking (same session): `tcp_echo` 0.463 s before, 0.466 s after (epoll),
0.456 s with the io_uring backend, Go 0.540 s; `http_hello` with Go's load
generator, best of 3 rounds: 101 000 req/s before, 101 300 after (epoll),
104 500 (io_uring), Go 110 800 (rounds vary by ±8%).

Startup of `hello` (`--release`, 300 runs): 1.45 ms -> 1.08 ms (min), 1.81 ms
-> 1.30 ms (median). Sizes (`--release`, stripped): `hello` 35 584 -> 14 840
bytes (10 320 with `ZELL_LINKER=lld`), `http_server` 1 203 680 -> 1 232 968
(the stdlib grew in between), `web_app` 1 756 672 (the old compiler cannot
build today's `web_app`; the example left with `web`, tag `web-in-tree`).

Compile time (`zell build`, best of 3, wall / CPU):

| program | before | after | after, 1 codegen thread |
|---|---|---|---|
| hello (debug) | 0.82 s | 0.65 s | 0.63 s |
| hello `--release` | 1.35 s | **0.31 s** | 0.30 s |
| http_server (debug) | 3.63 s | **1.94 s** (CPU 4.3 s) | 3.23 s |
| http_server `--release` | 11.6 s | 7.9 s (CPU 11.2 s) | 11.7 s |
| web_app (debug) | ~3.0 s (start of the session) | 2.62 s (CPU 5.5 s) | 4.08 s |
| web_app `--release` | ~9.8 s (start of the session) | 9.6 s (CPU 13.0 s) | 12.5 s |

A small debug build (a spec test) takes 0.72 -> 0.67 s. Building the whole
spec corpus 4 at a time took 133 s before and 159 s after on the shared
machine (load 4-6 from other jobs; the old compiler builds only 500 of the
820 files with today's stdlib, the new one 509), so that comparison is
inconclusive. `ZELL_TIME=1` prints the duration of each build phase.

What changed, by work item:

1. **Strings: views instead of copies** (runtime/ABI.md 4.1). `zell_string`
   is now `{hdr, len, char *data}` (32 bytes, owned bytes inline at +32), so
   a string can point into another string's bytes. `Split`/`Fields` put all
   their pieces into one *view block* (one allocation; each piece's refcount
   word forwards to the block), `s[a:b]`, `Trim*`, `SplitN` (and everything
   built on `s[a..b]` in Zell: `Cut`, regex match texts) return standalone
   views for pieces of 64 bytes or more and copies below. Views are not
   NUL-terminated: C APIs go through `zell_string_cstr`, and the runtime's
   test build `-DZELL_STRESS_NONUL` (no terminator on any string) passes the
   C tests, the examples and the spec corpus. `Split` with a one-byte
   separator counts and cuts 8 bytes at a time. `strings` 0.225 -> 0.072 s
   (Go 0.199 s). The other big part of that gain is general: object counts
   are per-thread counters instead of two shared `lock xadd` per allocation
   and one per free, and refcounts are updated without atomic instructions
   while the process has one thread (next item). JSON one-byte tokens are
   immortal statics. HTTP heads are parsed from a reused `[]byte` buffer, so
   they cannot be views (they were already copy-free C helpers).
2. **Reference counting fast paths.** The scheduler (workers and timer
   thread) now starts with the first task, so a program without tasks stays
   single-threaded; until a second thread starts (`zell_threaded`, set by
   the platform's thread start), `zell_retain`/`zell_release` use plain
   stores instead of atomic RMW. The compiler can emit that fast path inline
   (`codegen/rc.rs`, with the owner's half of a biased count too); this is
   on by default since the biased fast path landed (`ZELL_NO_INLINE_RC=1`
   at build time emits the calls): it gave json_like 7% and struct_map 2%,
   iface_call/closure_call/error_sentinel in `defer_errors_iface` 5-25%,
   and grew `http_server` by 30% and its `--release` compile
   time by 70%.
3. **Escape analysis** (`hir::ownership::stack_locals`, DESIGN.md
   "Память"): an address-taken local whose address is only read through or
   passed to non-escaping parameters (escape facts that also count runtime
   calls as escaping), and `var p = new T{...}` whose `p` never escapes, live
   in an entry alloca with an immortal header; leaving the scope releases
   the fields (running `Drop`). alloc_heavy: 0.395 -> 0.301 s from escape
   analysis alone (`ZELL_NO_ESCAPE=1` turns it off), 1.12 -> 0.40 s in all
   (the binary trees part is allocator/refcount bound). nbody's only boxing
   (`for b in bodies { b.Energy() }`) runs 5 times, so it stays at parity: it
   is bound by `sqrt` and division latency.
4. **Interfaces and `any`.** Inside `case T x:` of a type switch on an
   interface local, calls through that local use `T`'s constant vtable and
   become direct (inlinable) calls. `any` holds bool/integers/floats/char
   inline in its data word (ABI.md 3.7): a loop boxing ints into `any` and
   switching on them went from 0.9 s to nothing (LLVM folds it).
5. **io_uring** (`runtime/src/platform/poll_uring.c`, Linux 6.0+, raw
   system calls): readiness polls (`IORING_OP_POLL_ADD`) with the epoll
   backend's one-shot level-triggered semantics, synchronous cancel on
   forget. Opt-in (`ZELL_IO_BACKEND=io_uring`): it is not faster than
   one-shot epoll here (one submission per wait, like one `epoll_ctl`), and
   the kernel drops a completed poll's file reference lazily, so a listening
   socket closed right after its poll completed may still accept briefly
   (seen in 2% of `examples/tcp_echo` runs). Multishot accept/recv was not
   attempted.
6. **Startup and size.** Lazy scheduler start, lazy `os.Args`, and
   shutdown/panic/leak-report/debug-agent hooks reached through function
   pointers set by the subsystems when they start, so `hello` links none of
   the I/O poller, resolver, debug agent or leak report code (text 24.6 KB
   -> 6.5 KB). `--gc-sections` already dropped unused stdlib code.
   `ZELL_LINKER=lld` saves another 4.5 KB but is not the default: its
   layout made fib 15% slower.
7. **Compile time.** The biggest hotspot was LLVM working on the whole
   stdlib: code generation emits every stdlib function a program could reach
   (1012 functions, 5.8 MB of IR for `hello`), and `-O2` spent a second on
   them before dropping them; a `globaldce` pass now runs first (`hello
   --release` 1.35 -> 0.31 s, debug object emission 465 -> 265 ms). Large
   modules (over 60k instructions after optimisation) are compiled to
   machine code on up to 8 threads (`compiler/src/parcodegen.rs`: the
   optimised module is split by function, each thread re-parses the bitcode
   and keeps its part, the objects are merged with `clang -r`;
   `ZELL_CODEGEN_THREADS=1` turns it off; it costs about 30% more CPU). The
   stdlib sources are parsed on several threads (168 -> 37 ms per build).
   Generic instantiation was not a hotspot. The 2x target is reached for
   debug builds of large programs (http_server 1.87x) and small release
   builds, not for large release builds (1.46x), where the serial `-O2`
   pipeline on the whole module dominates.

## Results (2026-10-20, x86-64, clang/LLVM 18, go 1.24, `--release`)

Wall time, best of 5 runs (`before` = commit 9b8d6b1 compiler + runtime,
`after` = this tree); Go measured in the same session. Peak RSS from
`./scripts/bench-perf.sh`.

| bench | zell before s | zell after s | go s | after / go | zell RSS MB | go RSS MB |
|---|---|---|---|---|---|---|
| fib | 0.11 | 0.13 | 0.23 | 0.57 | 10.9 | 10.8 |
| map | 0.64 | 0.29 | 0.52 | 0.56 | 53.0 | 45.3 |
| nbody | 0.47 | 0.47 | 0.48 | 0.97 | 10.8 | 10.9 |
| sieve | 0.90 | 0.36 | 0.59 | 0.61 | 13.1 | 42.4 |
| strings | 0.69 | 0.22 | 0.27 | 0.80 | 20.4 | 20.4 |
| json_like | 1.24 | 0.23 | 0.37 | 0.62 | 23.4 | 25.9 |
| struct_map | 0.38 | 0.21 | 0.25 | 0.86 | 12.3 | 14.2 |

What changed (runtime, plus codegen fast paths):

- **Small-object allocator** (`runtime/src/mem.c`): objects up to 128 bytes
  come from per-thread free lists (16-byte classes, 4 MB regions, a bitmap
  identifies region memory on free) instead of `calloc`/`free`. This alone
  took `strings` from ~0.6 s to ~0.2 s: `Split` makes 200k small strings per
  round and glibc's malloc spent most of its time consolidating. Strings are
  allocated uninitialised (only `len` and the NUL are written).
- **`zell_release`** skips the atomic RMW when the count is exactly 1 (the
  caller holds the only reference); retain stays a relaxed increment and the
  shared-count decrement `acq_rel`. Slice backings of `zell_ops_ref`
  elements release their elements without an indirect call per element.
- **strings**: `Split`/`Count`/`Replace`/`Index`/`Contains` search with
  `memchr` + `memcmp` instead of a `memcmp` at every byte; `Split` counts
  first and fills the result slice once; `ToUpper`/`ToLower` write straight
  into the result string with an 8-bytes-at-a-time ASCII path (no temporary
  buffer).
- **map** (`runtime/src/map.c`): the chained table with one `malloc` per
  entry and function-pointer hash/eq became a SwissTable-style open-addressing
  table (control bytes, SWAR group matching, 7/8 load) with entries inline,
  specialised int and string key paths (inlined mixer / word-at-a-time string
  hash, no indirect calls) and new `zell_map_get_or_insert_int/_str`, which
  the compiler now uses for `m[k] op= v`.
- **append** (`codegen/ops.rs`): `xs = append(xs, x)` stores the element and
  bumps `len` inline when `len < cap`; the runtime is called only to grow.
  `sieve` went 0.90 → 0.36 s (its fill loop was 50M runtime calls). The
  strided inner loop (`j += i`) cannot be vectorised by LLVM, but it is a
  plain load/compare/store with `len`/`data` reloaded (the slice variable's
  address escapes to the grow call, so LLVM cannot keep them in registers).
- **float/int formatting** (`runtime/src/string.c`): `zell__format_float`
  tries an exact fast path for fixed-notation values (1e-4 <= |v| < 1e6, at
  most 15 digits) before the `%.*e` + `strtod` search, and ints are formatted
  without `snprintf`. `json_like` went 1.24 → 0.23 s (79% of its time was
  float formatting). Verified identical output against the old algorithm on
  6M random values.
- **nbody**: the hot loop was already a direct GEP + load/store with no
  refcount traffic; `math.Sqrt/Abs/Floor/Ceil` now lower to LLVM intrinsics
  (inline `sqrtsd`) instead of runtime calls. It stays at parity, bound by
  the `sqrt` + divide latency.
- `fib` emits identical IR before and after; its ±15 ms difference is
  code layout (it flips sign between the stripped and unstripped builds).

Notes on the first measurement (2026-10-20, naive ARC, before the changes
above): fib 0.15 / nbody 0.59 / sieve 2.23 / map 0.94 / strings 0.70 s
against Go 0.26 / 0.51 / 0.77 / 0.53 / 0.24 s; `sieve` and `map` paid a
runtime call per element operation, `strings` per-element `malloc` traffic.

## Networking (2026-10-20, 4 vCPU x86-64 gVisor sandbox, loopback, go 1.24)

`tcp_echo` (`./scripts/bench-perf.sh tcp_echo` measures one run; the table is the best of 5, wall time for
1000 connections x 1 MB echoed = 2 GB through the kernel):

| | wall s | CPU s (user+sys) | peak RSS MB | throughput |
|---|---|---|---|---|
| Zell | 0.52 | 1.64 | 57 | ~1.9 GB/s echoed |
| Go | 0.62 | 1.91 | 110 | ~1.6 GB/s echoed |

The kernel copies dominate both (sys time); only about 1 in 15 reads or
writes has to wait for readiness, so the poller round trip (epoll_ctl
one-shot arm, shard thread, run-queue post) is not the limit here. The
first version of the benchmark built its buffers with `bytes.Repeat`
(a Zell append loop: 196M appends, 5 s of user time); `net.MakeBuffer(n)`
/ `strings.Bytes(strings.Repeat(..))` allocate in one C call.

`http_hello` (`benchmarks/http_hello/run.sh`, requests/s, 64 keep-alive
connections, one request in flight each, server and load generator in
separate processes sharing the 4 vCPUs; the machine was shared with other
jobs, so the numbers move by ±15% between runs; two runs, the better one per cell):

| server \ load generator | Zell load | Go load | Go HTTP/2 load (TLS, 64 streams, 1 connection) |
|---|---|---|---|
| Zell `net.http` | 67 100 | 103 300 | 31 600 |
| Go `net/http` | 64 400 | 117 300 | 32 600 |

Six alternating rounds of the Go load generator against both servers
(`go -load`, same process pair): Zell 71–86k req/s, Go 61–85k req/s, ratio
98–117% (median about 110%), with the Zell server using 13–16 µs of CPU per
request (user + system) against Go's 15–17 µs. Before the rework of the HTTP
layer the Zell server did 51 700 req/s against Go's 104 300 (about half)
and used 35 µs of CPU per request. The HTTP/2 column is limited by the Go
client (its single h2 connection serializes the 64 streams' writes); both
servers stay below 100% CPU there.

What changed (a callgrind profile of the old server put ~90% of its
instructions in `http.zl`: `CanonicalHeader` alone 29%, `writeResponse` 22%,
the router 14%, and in task allocation of `async` helpers):

- Heads are parsed in place from the connection's read buffer by C helpers
  (`rtHeadEnd`, `rtParseHead`, ABI.md 16.5): no per-line strings, common
  methods/protocols/header names interned (no allocation), the facts the
  server needs (Content-Length, chunked, Connection, Expect, traceparent)
  returned as flags instead of map lookups.
- The response head (status line, headers, cached `Date`, Content-Length)
  and the body are formatted by one C call into one string and sent with one
  write (sendfile with TCP_CORK for files); `CanonicalHeader` returns its
  argument when it is already canonical.
- The connection loop does the reads itself (`conn.readInto` straight into
  the buffer): about three tasks per request (the read, the handler, the
  write) instead of ten; the router compares static routes as strings
  without splitting the path or allocating a parameter map.

HTTP/2 uses the same handler path with a task per stream; HPACK Huffman is
C, the rest of the framing Zell.

## Tier-2 stdlib (2026-10-20, `--release`, go 1.24)

The time inside the measured section, as each program prints it (best of 3);
the wall time from `bench-perf.sh` also includes generating the input.

| bench | zell ms | go ms | zell / go | note |
|---|---|---|---|---|
| regex `FindAll` (10 MB, 30323 matches) | 55 | 505 | 0.11 | Go's `regexp` has no DFA (NFA or backtracker); Zell runs a lazy DFA forward, then a reverse DFA for the start |
| gzip compress, level 6 (20 MB) | 1444 | 1705 | 0.85 | output 3 044 375 vs 3 022 734 bytes (+0.7%) |
| gzip decompress | 83 | 203 | 0.41 | table-driven inflate in C |

Peak RSS is higher for Zell (46 / 104 MB vs 27 / 76 MB): the inputs are
built by appending, and the gzip program keeps the input, output and
round-trip copy alive.


## Final results (2026-10-21, commit after the last wave, x86-64 Linux, clang/LLVM 18, go 1.24)

Best of several runs on an otherwise idle machine; `--release` for Zell, default build for Go.

| bench | Zell s | Go s | Zell/Go | Zell RSS | Go RSS |
|---|---|---|---|---|---|
| strings (Split/ToUpper/Join, 1.2 MB) | 0.07 | 0.19 | **0.37x** | 23 MB | 21 MB |
| regex (FindAll e-mails, 10 MB) | 0.21 | 0.58 | **0.36x** | 47 MB | 27 MB |
| fib(38) | 0.10 | 0.22 | **0.45x** | 11 MB | 11 MB |
| json_like (format 100k records) | 0.19 | 0.36 | **0.53x** | 23 MB | 27 MB |
| alloc_heavy (tree of small nodes) | 0.35 | 0.60 | **0.58x** | 11 MB | 14 MB |
| sieve (5 x 10M) | 0.28 | 0.46 | **0.61x** | 13 MB | 45 MB |
| map (4M int ops) | 0.19 | 0.30 | **0.63x** | 53 MB | 47 MB |
| tcp_echo (1000 conns x 1 MB) | 0.51 | 0.69 | **0.74x** | 55 MB | 109 MB |
| gzip (20 MB, level 6) | 1.68 | 2.03 | **0.83x** | 105 MB | 75 MB |
| nbody (5M steps) | 0.43 | 0.43 | 1.0x | 11 MB | 11 MB |
| struct_map | 0.23 | 0.22 | 1.0x | 12 MB | 13 MB |

HTTP (`benchmarks/http_hello/run.sh`, Go load generator, 64 keep-alive connections):

| server | HTTP/1.1 req/s | HTTP/2 over TLS req/s (64 streams) |
|---|---|---|
| Zell `net.http` | **116,507** | 26,290 |
| Go `net/http` | 101,979 | 29,389 |

The Zell load generator itself is slower than Go's (76k vs 117k req/s against the same server): client-side request building is the next thing to optimise.

Hello-world binary sizes (`scripts/bench-size.sh`, stripped release):

| target | size |
|---|---|
| Zell linux-amd64 (dynamic libc) | 14 KB |
| Zell linux-arm64 | 13 KB |
| Zell linux-musl (fully static) | 58 KB |
| Zell windows-amd64 | 68 KB |
| C (dynamic libc) | 15 KB |
| Go (stripped) | 1404 KB |

The panic line table (`zell_lines`, runtime/ABI.md 8.2) is linked in as
an extra object of read-only data (since 2026-10-24 on every platform, not
a post-link ELF section): `hello` 46,160 bytes (-176 against the ELF
section), `examples/kafka.zl` 2,303,968 bytes (+3.0 KB, +0.13%; table
9.3 KB, about 0.4% of the executable); on windows-amd64, which had no table
before, `hello` is unchanged (102,912) and `kafka` grows by 44 KB (+1.9%):
7 KB of table and 37 KB of code for task-panic unwinding (the coroutine
frame chain of ABI.md 8.1, now emitted for Windows as everywhere else). The code is unchanged (DWARF
line tables do not affect code generation; functions get hidden external
linkage only after optimisation). `zell build --release examples/kafka.zl`
takes 8.6 s, as before.

Summary: Zell is faster than Go on 9 of 11 CPU/memory benchmarks and on the HTTP/1.1 server, at parity on nbody and struct_map, and slightly behind on HTTP/2 over TLS. Memory use is lower or equal everywhere except regex (lazy DFA cache), gzip and map.

## Cycle collector (2026-10-21, 4 vCPU x86-64, `--release`, 10 alternating runs, wall seconds min / median)

`base` is the previous commit; `off`/`auto` are the same binary with
`ZELL_CYCLES=off` / the default `auto` (DESIGN.md "Циклы"). `base` vs
`off` also contains the concurrent allocator/biased-refcount work, so the
collector's own cost is `auto` vs `off`.

| Benchmark | base | off | auto | auto vs off |
|---|---:|---:|---:|---:|
| `json_like` | 0.180 / 0.199 | 0.172 / 0.185 | 0.172 / 0.186 | noise (no cyclic user types) |
| `http_hello` | 3.006 / 3.123 | 3.051 / 3.207 | 2.975 / 3.168 | noise (net.http has a few cyclic types: pools, h2 connections) |
| `alloc_heavy` | 0.362 / 0.402 | 0.444 / 0.499 | 0.415 / 0.446 | noise; callgrind (depth-12 run) 233M vs 224M instructions, +4 % |

`alloc_heavy`'s `Node {Left, Right *Node}` is a cyclic type (W0001 warns,
and it is deliberately not `@acyclic`), so all 14.7M nodes are flagged.
The first version recorded each of them as a possible root (the pin taken
around `check(n.Left)` is released to a non-zero count) and kept a hidden
buffer-index word per flagged object: +35..40 % (327M vs 209M
instructions). Pins of `*T` of cyclic types now remember the collection
epoch and are released with `zell_ptr_release_noroot`, which records
nothing unless a collection overlapped the pin (0 candidates in this
benchmark), the hidden word is gone, and the death of a flagged object
that was never recorded takes the plain drop + free path. RSS is equal
(11 MB). Binary size: `hello` 33 KB (15 KB at `base`; none of it is the
collector, which is linked through weak symbols only when a cyclic type
is allocated), `http_hello` 1.14 MB vs 1.11 MB.

## Object heap, biased refcounts, io_uring, testing.Panics (2026-10-21, 4 vCPU x86-64 Firecracker VM, Linux 6.18, `--release`)

`old` = the compiler and runtime of commit 5cbb7c9, `new` = this tree.
The machine was shared with another agent's test runs (load average 2 to
24 during these measurements), so wall times move by ±10-15% between
sessions; where they disagree, the callgrind instruction counts
(deterministic) and CPU time per request decide. `./scripts/bench-perf.sh`
now takes `RUNS=N` (best of N, Zell and Go alternating) and `NOGO=1`.

**1. Returning memory to the OS** (`runtime/src/heap.c`, ABI.md 3.2).
RSS (MB, `/proc/self/statm`) of a program that allocates 250 MB in 1 MB
`[]byte` buffers and 250 MB in 2.2M 80-byte strings (112-byte blocks),
then drops them and sleeps (the spec test `memory2/heap_release_rss`
checks the same with a 1 s limit):

| | start | allocated | freed +0 ms | +200 ms | +400 ms | +1000 ms |
|---|---:|---:|---:|---:|---:|---:|
| old | 1 | 540 | 271 | 271 | 271 | 271 |
| new | 1 | 540 | 352 | 352 | 18 | 18 |

(old: the 1 MB buffers came from glibc's mmap and went back; the
128-byte slab regions never did. new: the freed pages stay as reserve for
0.3 s, then the scavenger gives everything above 16 MB back;
`runtime.FreeOSMemory()` does it at once. Allocating the 540 MB took
1.94 s new vs 2.31 s old.)

Best of 5, alternating, `new` built from this tree (`alloc_heavy` with
`ZELL_CYCLES=off`, so the cycle collector's own cost is excluded; see the
previous section), wall s and peak RSS MB:

| bench | old s | new s | old MB | new MB | instructions old → new |
|---|---:|---:|---:|---:|---|
| fib | 0.097 | 0.112 | 11.2 | 11.2 | 1.343G → 1.343G (identical code: the wall time is noise) |
| sieve | 0.286 | 0.303 | 13.1 | 12.9 | 1.829G → 1.822G |
| map | 0.223 | 0.183 | 52.8 | 52.9 | 615M → 615M |
| strings | 0.065 | 0.065 | 23.3 | 23.7 | 719M → 711M |
| json_like | 0.180 | 0.168 | 23.3 | 24.8 | 1.651G → 1.701G (+3%; page faults 17.3k → 6.7k) |
| struct_map | 0.203 | 0.198 | 12.1 | 12.4 | 914M → 913M |
| alloc_heavy | 0.387 | 0.435 | 11.5 | 11.5 | 4.158G → 4.751G, of which 0.37G is `zell_cycles_new*` and the rest the collector's codegen; the heap's own alloc + free paths are 2.6G in both |
| regex | 0.207 | 0.209 | 46.6 | 51.0 | |
| gzip | 1.694 | 1.752 | 104.7 | 113.3 | |
| heap_churn (new) | 2.290 | 2.113 | 42.1 | 46.0 | RSS after the last round: 19 MB old (glibc trimmed the 132-byte strings) vs 43 MB new (one round's worth kept until the scavenger runs) |

Design notes and tuning: a first version kept a reserve of max(16 MB,
in use / 4) and released the rest synchronously; heap_churn then did 6.5k
`madvise` calls and re-faulted 40 MB per round (2.80 vs 2.34 s). Keeping
everything up to the recent peak synchronously made gzip's RSS 173 MB (vs
105): the reserve is now bounded by *peak − in use*, so in use + reserve
stays within the recent peak, and a background scavenger (a thread that
never touches objects, so `zell_threaded` stays 0) releases what stayed
unused for 0.3 s beyond 16 MB. The allocation fast path is inlined into
`zell_object_new` from `heap.h` (out of line it cost 18% more
instructions on alloc_heavy). Medium objects (16-128 KB) stay with malloc
(a mapping per 20 KB string would hit the 65k VMA limit); larger ones get a
mapping each, grown by `mremap`, with freed mappings cached within the
reserve.

**2. Biased reference counting** (`runtime/src/rc.c`, ABI.md 3.1).
`http_hello`, Go's load generator against the Zell server in another
process, best req/s of 4-5 rounds and the server's CPU time per request
(user + sys, minimum over the rounds):

| server | load ~5 | | load ~20 | |
|---|---:|---:|---:|---:|
| old | 101 536 req/s | 16.4 µs | 74 585 req/s | 15.7 µs |
| new (biased) | 98 440 req/s | 16.2 µs | 81 996 req/s | 16.2 µs |
| new, `ZELL_NO_BIAS=1` | 100 272 req/s | 16.4 µs | 84 032 req/s | 15.5 µs |

Before tasks were excluded from biasing (`ZELL_RC_NOBIAS`), the server
used 19.9 µs per request (vs 18.6 without biasing): with `ZELL_RC_STATS=1`
the in-process benchmark queued 1.8M objects for a merge (9 per request,
91% of them `Task` objects, which every await hands to another thread);
with tasks never biased it is 183k (5.6M -> 2.4M objects biased).

`shared_objects` (new), ms per phase (one builder / 8 readers, 8 builders
/ one reader, 8 tasks with local churn), minimum of 10 alternating runs:

| | phase 1 | phase 2 | phase 3 |
|---|---:|---:|---:|
| old (load ~5) | 426 | 125 | 124 |
| new (load ~5) | 350 | 104 | 107 |
| new, `ZELL_NO_BIAS=1` (load ~5) | 451 | 147 | 131 |
| old (load ~20) | 437 | 137 | 140 |
| new (load ~20) | 387 | 146 | 132 |
| new, `ZELL_NO_BIAS=1` (load ~20) | 314 | 144 | 129 |
| Go | 60-69 | 23-39 | 124-196 |

The quieter session shows biasing 15-30% ahead in every phase, the loaded
one does not separate them: a win on the churn phase (owner-local counts
are plain stores) is plausible but not established beyond the noise here.
Phase 1 is bound by the shared counts of the 200k objects every reader
touches (cache-line ping-pong; Go has no counts). Single-threaded
programs never bias (their instruction counts above are unchanged).
`tcp_echo` (`ZELL_RC_STATS`: 93k biased, 24k queued) is within noise.
`hello` grew from 15 KB to 33 KB (the heap, the scavenger and rc.c are
reachable from `zell_release`).

**3. io_uring** (`ZELL_IO_BACKEND=io_uring`): `examples/tcp_echo` gave 0
wrong outputs in 500 runs before and after the fix (the 2% flake was seen
under gVisor; the race — a completed poll's file reference dropped just
after its CQE is posted — remains possible, so a listener is now shut down
before close, which stops it accepting whatever still holds the file).
Performance: `tcp_echo` best 0.431 s vs 0.456 s for epoll (median 0.536 vs
0.558, 12 alternating runs each): parity; `http_hello` 66-73k vs 82-91k
req/s and 20.6 vs 16.2 µs of server CPU per request: about 20% slower (a
POLL_ADD submission per wait plus the completion reaped under the ring
lock; submitting outside the lock did not help). So it stays opt-in.

**4. testing.Panics** runs the function on a thread of its own and catches
its panic (setjmp/longjmp), so the test's code runs once instead of once
more per `Panics` call in a child process; `compiler/tests/zell_test_cmd.rs`
covers both paths (`ZELL_TEST_PANICS=process` forces the old one).

## Memory wave results (2026-10-21, after cycle collector, OS memory return, biased RC)

Best of 3 on an idle machine, `--release`, cycle collector in default `auto` mode.

| bench | Zell s | Go s | Zell/Go | Zell RSS | Go RSS |
|---|---|---|---|---|---|
| strings | 0.069 | 0.167 | **0.41x** | 24 MB | 18 MB |
| regex | 0.234 | 0.588 | **0.40x** | 51 MB | 24 MB |
| sieve | 0.324 | 0.608 | **0.53x** | 13 MB | 43 MB |
| fib | 0.115 | 0.213 | **0.54x** | 11 MB | 11 MB |
| json_like | 0.180 | 0.293 | **0.61x** | 25 MB | 34 MB |
| map | 0.242 | 0.386 | **0.63x** | 53 MB | 44 MB |
| alloc_heavy | 0.448 | 0.561 | **0.80x** | 11 MB | 14 MB |
| struct_map | 0.196 | 0.227 | **0.86x** | 12 MB | 13 MB |
| tcp_echo | 0.482 | 0.553 | **0.87x** | 56 MB | 117 MB |
| gzip | 1.910 | 2.101 | **0.91x** | 113 MB | 75 MB |
| nbody | 0.430 | 0.446 | 0.96x | 11 MB | 11 MB |
| heap_churn (new) | 2.244 | 0.976 | 2.3x slower | 46 MB | 67 MB |
| shared_objects (new) | 0.636 | 0.217 | 2.9x slower | 47 MB | 20 MB |

Regressions and open items from this wave:

- `heap_churn` (allocate/free 1 MB buffers and small strings in a loop) and
  `shared_objects` (objects created on one task, read from 8 workers) are
  the two benchmarks where Go's allocator and GC beat ARC: large-buffer
  churn pays mmap/munmap and page-fault costs, and cross-thread reads pay
  atomic shared counts even with biased RC. Both are new benchmarks added
  to make this visible; they are the next optimisation targets (large-object
  cache sizing, read-only borrows without retain across tasks).
- Release `hello` grew from 14 KB to 32 KB: the segmented heap, the
  scavenger and `rc.c` are reachable from `zell_release`.
- `alloc_heavy` with the collector on is within noise of `ZELL_CYCLES=off`
  after the pin-release optimisation; the W0001 warning it prints is
  expected (`Node` is a tree, but the type can form a cycle).

## Async benchmarks (2026-10-22, 4 vCPU x86-64 VM, clang/LLVM 18, go 1.24, `--release`)

Nine pairs of programs that compare the task scheduler (`async`/`await`,
`task.Channel`, `task.Mutex`, `task.Delay`, cancellation, the I/O poller)
with goroutines. Each one has a fixed workload and prints a checksum line
first (identical for Zell and Go; `bench-perf.sh` reports a `checksum
mismatch` otherwise), then its phase timings. `./scripts/bench-perf.sh
async` runs the suite, `SCALE=small` gives every program a smoke workload
that finishes in well under a second, and `DETAIL=1` prints the phase lines.

| Benchmark | Workload |
|---|---|
| `spawn_tasks` | 1M tasks that yield once, compute a value and finish, all awaited; then 1M tasks parked at once on a `WaitGroup` gate (peak RSS = memory per suspended task) and released |
| `channel_pingpong` | two tasks exchange 5M messages over unbuffered channels; 1M values through a 4-stage pipeline of buffered (128) channels |
| `fanout_fanin` | 10k producers x 100 values into one channel (1024) and one consumer; a pool of 8 workers over 1M jobs (jobs and results channels of 1024) |
| `mutex_contention` | 8 tasks, 10M increments in all under one `task.Mutex` (Go: `sync.Mutex`); one task locking 10M times uncontended |
| `timers` | 100k tasks each `task.Delay` 1-10 ms (LCG durations); max and mean wake-up lateness |
| `await_chain` | an async function awaiting itself 1000 deep, 10k times (no suspension), and 1k times with a `task.Yield` at the bottom (the chain suspends and 1000 frames resume); Go: plain recursion and `runtime.Gosched` |
| `cancel_tree` | 10k tasks blocked (half `Recv`, half a 100 s `Delay`) in a `task.NewCancel` scope, cancelled at once, 10 rounds; the same with `task.WithTimeout(20)`; time from the cancel to the last task done (Go: `context.WithCancel` / `WithTimeout`) |
| `async_io` | 1000 loopback TCP connections x 1000 round trips of 64 bytes (latency-bound, unlike `tcp_echo`); round trips/s and p50/p99/max latency |
| `select_many` | one consumer takes 2M events from whichever of 8 unbuffered channels is ready; Zell has no `select`, so it keeps a `Recv` per channel and waits with `task.Any` (which cancels the losers, so every event re-arms the receives); for reference the same events through a merge channel fed by 8 forwarders. Since 2026-11-02 both versions stop polling a closed channel (Zell drops it from the set, Go sets it to nil): before, the tail of the loop was a busy poll, which hung Zell with one worker and inflated both sides; numbers before that date are not comparable |

### Results (2026-10-22, quiet machine, best of 5)

`RUNS=5 DETAIL=1 ./scripts/bench-perf.sh async` at commit 450e35f (which
contains the scheduler fixes described under "Analysis"), Zell and Go
alternating, full size, nothing else running (load < 0.5 at the start).
Wall, CPU (user+sys) and peak RSS are for the best run (by wall time) of
the whole process; the phase times are that run's own timings. Zell / Go
with Go = 1.00: below 1 is better, except for round trips/s, where above
1 is better. Every checksum matched.

The `before fixes` column is the preliminary single run at commit e51cf3d
on a shared machine (load 2-4, so up to 2x off); the fixes themselves were
never measured separately, so it is the only "before".

| bench | phase | before fixes (Zell) | Zell | Go | Zell / Go |
|---|---|---|---|---|---|
| spawn_tasks | wall s (CPU s) | 10.1 (12.2) | 4.78 (9.60) | 3.43 (8.89) | 1.39 |
| | peak RSS MB | 806 | 606 | 2 579 | **0.24** |
| | run 1M yielding tasks | 4 971 ms | 3 195 ms | 629 ms | 5.08 |
| | park 1M tasks | 4 360 ms | 596 ms | 2 237 ms | **0.27** |
| | wake 1M parked | 500 ms | 779 ms | 364 ms | 2.14 |
| channel_pingpong | wall s (CPU s) | 167.7 (261) | 42.1 (76.9) | 1.39 (2.03) | 30.4 |
| | peak RSS MB | - | 10.8 | 10.7 | 1.01 |
| | ping-pong 5M | 162 459 ms | 39 035 ms | 1 110 ms | 35.2 |
| | 4-stage pipeline 1M | 5 280 ms | 3 081 ms | 271 ms | 11.4 |
| fanout_fanin | wall s (CPU s) | 9.68 (35.8) | 3.73 (14.0) | 0.85 (1.90) | 4.38 |
| | peak RSS MB | - | 24.3 | 27.5 | 0.88 |
| | fan-in 10k producers | 7 126 ms | 1 759 ms | 692 ms | 2.54 |
| | worker pool 1M jobs | 2 545 ms | 1 962 ms | 154 ms | 12.7 |
| mutex_contention | wall s (CPU s) | - | 22.8 (37.6) | 0.84 (2.02) | 27.3 |
| | peak RSS MB | - | 10.7 | 10.8 | 0.99 |
| | contended, 8 tasks | 34 006 ms | 22 025 ms | 662 ms | 33.3 |
| | uncontended | 1 002 ms | 776 ms | 165 ms | 4.70 |
| timers | wall s (CPU s) | 194.3 | 0.166 (0.40) | 0.119 (0.33) | 1.39 |
| | peak RSS MB | - | 40.1 | 35.2 | 1.14 |
| | max / mean lateness | 187 s / 127 s | 10.8 / 2.81 ms | 19.7 / 0.65 ms | 0.55 / 4.3 |
| | timers late by > 1 ms | - | 60 618 | 17 256 | 3.5 |
| await_chain | wall s (CPU s) | - | 3.86 (5.24) | 0.105 (0.11) | 36.8 |
| | 10M async calls, no suspension | 1 600 ms | 1 364 ms (136 ns/call) | 81 ms (8 ns/call) | 16.8 |
| | 1k chains of 1000 resuming | 782 ms | 2 494 ms | 20 ms | 125 |
| cancel_tree | wall s (CPU s) | - | 0.785 (1.03) | 0.589 (1.38) | 1.33 |
| | peak RSS MB | 28 | 46.9 | 38.0 | 1.23 |
| | drain after Cancel(), avg / max | 158 / 300 ms | 18.1 / 35.8 ms | 8.9 / 10.3 ms | 2.04 / 3.48 |
| | drain after WithTimeout, avg | 113 ms | 17.1 ms | 9.3 ms | 1.84 |
| async_io | wall s (CPU s) | 34.5 (84) | 18.5 (61.0) | 5.12 (19.4) | 3.62 |
| | peak RSS MB | - | 28.1 | 28.9 | 0.97 |
| | round trips/s (> 1 better) | 29 100 | 54 365 | 202 388 | 0.27 |
| | p50 / p99 / max | 33.7 / 61.3 / 103 ms | 18.7 / 24.8 / 39.5 ms | 4.7 / 7.7 / 80.6 ms | 3.94 / 3.23 / 0.49 |
| select_many | wall s (CPU s) | - | 53.1 (120) | 3.80 (7.93) | 14.0 |
| | peak RSS MB | - | 10.8 | 10.8 | 1.00 |
| | `task.Any` emulation / Go `select` | 4 161 ms (1 worker) | 29 059 ms | 2 376 ms | 12.2 |
| | merge channel | 15 043 ms (1 worker) | 24 054 ms | 1 421 ms | 16.9 |

What the quiet run shows:

- The timer heap fixed `timers` outright (194 s -> 0.17 s, max lateness
  now below Go's; the mean is 4x Go's because the timer thread wakes the
  tasks through the global queue) and `cancel_tree` (drain 158 -> 18 ms,
  about 2x Go).
- The spin-before-park change cut ping-pong 4x (162 -> 39 s) and the
  pipeline, fan-in and contended mutex by 1.5-4x, but hand-off patterns
  are still 11-35x Go: every hand-off is a trip through the global queue.
- Memory per task stays Zell's strength: 1M parked tasks need 0.24x
  Go's RSS and park in a quarter of Go's time.
- `await_chain`'s resuming phase (1000 frames resumed per chain) got
  slower than the preliminary figure (782 -> 2 494 ms, 2.5 us per resumed
  frame); the preliminary number was a single run on a loaded machine, so
  this is not established as a regression, but it is now the largest
  ratio in the suite (125x). `select_many` at 4 workers is 12-17x Go
  (the preliminary Zell figures were 1-worker runs and are not
  comparable; the suspect 70 s Go `select` figure was indeed noise: 2.4 s
  now).
- The open items below (per-worker queues and a LIFO slot, a
  non-allocating async call path, `task.Channel` over `zell_chan`, mutex
  barging, `select`) are what is left; nothing in this run contradicts
  their order.

### Analysis

What the scheduler does well:

- **Memory per task.** A suspended task is its coroutine frame (sized
  exactly by LLVM) plus a `zell_task` header: 1M parked tasks peak at
  806 MB including the 1M-element task slice, the gate's waiter slice and
  phase 1's leftovers, about 0.8 KB per task against about 2.6 KB per
  parked goroutine (2 KB minimum stack plus `g`). Parking and waking are
  cheaper than in Go for the same reason (no stacks to allocate or fault
  in). `cancel_tree` uses less memory than Go as well.
- **Cancellation is cheap in itself**: `zell_task_cancel` collects the
  hooks of the whole subtree in one walk and fires them. The drain time
  above was dominated by the timer list (next point), not by the tree.

What it does badly, and what was fixed in `runtime/src/task.c`:

1. **Timers: O(n) list (fixed).** Timers were a sorted singly linked list:
   every `task.Delay` insert walked it, and `zell_task_rt_fire`, which every
   channel, mutex and WaitGroup wake-up goes through, scanned the *whole*
   list under the global timer lock to see whether the signal was a
   pending delay. 100k sleepers took 194 s to start and woke up to 187 s
   late; cancelling 5k delays cost 5k list scans. Now: a binary min-heap
   on (deadline, insertion sequence), each delay task points at its timer
   so `rt_fire` removes it in O(log n) and skips the timer lock entirely
   for tasks without a timer, the timer thread takes all due timers in one
   lock round trip, and inserts wake the timer thread only for a new
   earliest deadline.
2. **A futex wake per job (fixed in part).** One global FIFO queue under one
   mutex and condvar; every post signalled the condvar, and an idle worker
   parked as soon as the queue was empty. Hand-off patterns (ping-pong, a
   mutex passed to the next waiter, an await chain resuming) therefore
   paid a sleep/wake round trip per message and bounced the queue lock
   between threads: ping-pong is 25x *slower* with 4 workers than with 1.
   Now: a worker that runs out of work polls the queue without the lock
   for up to 50 us (one poller at a time, `sched_yield` every 64 polls)
   before it parks; a post wakes a parked worker only when nobody polls;
   and a poller that takes a job while more are queued wakes one parked
   worker, so bursts still spread over the pool. Tested by
   `relay_handoffs` and `timer_heap_order_and_fire` in
   `runtime/tests/test_task.c` (also under `make tsan`).

Open items (larger than a safe fix in this pass):

- **No per-worker run queues, no work stealing, no LIFO slot.** Every job,
  including the continuation of the task that just completed on this
  worker, goes through the global queue (a `malloc`ed job and a lock round
  trip); Go runs a just-readied goroutine next on the same P (`runnext`)
  and steals from other Ps when idle. Per-worker deques with stealing, and
  running the awaiting continuation inline when a task completes on a
  worker, are the next scheduler steps; they would address `spawn_tasks`
  phase 1 (5x), `await_chain` resumption (780 ns per resumed frame) and
  `async_io` latency (two queue trips per round trip).
- **Async call cost** (done, see "Synchronous async calls" below): a call
  that never suspends used to allocate a frame and a task (160 ns vs a 9 ns
  Go call); the task is now made only at the first suspension and the frame
  only when the body has a suspend point.
- **`task.Channel` is written in Zell over signals and spin locks**
  (std/task/task.zl), not over the C `zell_chan` in task.c: every wait
  allocates a signal task and a cancellation hook, `send`/`recv` are
  coroutines, an unbuffered `Send` waits for an acknowledgement (two
  hand-offs per message), and `recv` wakes *every* blocked sender
  (`wakeSenders`): with 10k producers on a full channel each receive wakes
  10k tasks that re-queue (fan-in is 6x Go even with one worker). Waking
  one sender needs a cancelled sender to remove its stale signal from
  `sendq` (otherwise a wake-up can be lost), so it was left for a channel
  rewrite on top of `zell_chan` with cancellation.
- **`task.Mutex` convoys.** `Unlock` hands ownership to the oldest waiter
  (FIFO, as specified), so under contention every lock is a scheduler
  round trip (58x Go); Go's `sync.Mutex` lets a running goroutine barge
  in and spins before queueing. An uncontended `Lock` still allocates a
  completed task (`Delay(0)`): 100 ns vs 16 ns. A barging/spinning mode
  (or handing ownership only after a starvation threshold, as Go does) and
  a non-allocating fast path are open. (Both done: C4 and R5, 2026-10-25.)
- **No `select`.** `task.Any` cancels the losers, so waiting on whichever
  of N channels is ready costs N new receives per event, and a loser that
  already took a value must still be awaited. A `select` statement or
  `task.Select` over channel operations needs a spec decision (DESIGN.md
  "Асинхронность") first.

## Performance pass 3 (2026-10-22, 4 vCPU x86-64, `--release`)

The machine was shared by five agents during this pass, so wall times are
**preliminary (shared machine)**; the decisions below rest on callgrind
instruction counts (`ZELL_TARGET_CPU=x86-64-v3`, since Valgrind lacks
AVX-512) and syscall counts. `old` = commit e51cf3d, `new` = this pass.

The quiet re-run of the full table (best of 5 alternating, 2026-10-22,
commit 450e35f) is "Final results, quiet machine" at the top of this
file; the wall times below are from the shared machine.

Instructions (callgrind, whole program; `shared_objects` with
`ZELL_WORKERS=4`, serialised by Valgrind):

| bench | old | new | new / old |
|---|---:|---:|---:|
| heap_churn | 15.80G | 2.29G | 0.14 |
| shared_objects | 12.36G | 4.40G | 0.36 |
| alloc_heavy | 5.291G | 5.291G | 1.00 |
| json_like | 1.701G | 1.701G | 1.00 |
| strings | 711M | 711M | 1.00 |
| map | 615M | 615M | 1.00 |
| struct_map | 913M | 913M | 1.00 |
| Zell HTTP/1.1 load generator, per request (Go server) | 35.3k | 26.5k | 0.75 |
| Zell HTTP/2 + TLS server, per request (Go h2 client) | 100.2k* | 87.6k | 0.87 |

(* already with the task and frame changes below; before them the server
was not measured.)

Preliminary wall times (shared machine, best of 3, ms or s; the quiet
best-of-5 numbers are in the final table at the top: heap_churn 0.375 s vs
Go 1.170 s, shared_objects 0.327 s vs 0.226 s):

| bench | old | new | Go |
|---|---:|---:|---:|
| heap_churn | 2.175 s | 0.354 s | 1.185 s |
| shared_objects phase 1 / 2 / 3 (ms) | 493-724 / 172-233 / 179-232 | 71-97 / 44-50 / 137-252 | 66-116 / 43-52 / 258-335 |

Release `hello` (`--release`, stripped): 33 040 -> 14 696 bytes (text of
the runtime part 11.4 KB -> 0.9 KB).

What changed:

1. **`heap_churn`: `strings.Repeat`** copied its input `count` times, one
   `memcpy` per copy: `Repeat("c", 256 << 10)` made 262 144 one-byte
   copies, and the benchmark made 480M of them in all — that, not mmap or
   page faults, was the gap (`strace -c`: 74 mmap, 28 munmap, 160 mremap
   for the whole run; the reserve already keeps the freed pages). Repeat
   now fills one-byte inputs with `memset` and doubles the copied prefix
   otherwise (as Go does), into an uninitialised string; `strings.Bytes`
   (`zell_string_to_bytes`) no longer zeroes a backing it overwrites
   completely. The allocator itself was left alone: its alloc + free fast
   paths are ~60 + ~85 instructions per small string here.
2. **`shared_objects`: borrowed locals** (`compiler/src/hir/borrow.rs`,
   DESIGN.md "Память", spec test `memory2/borrowed_locals`). In a
   heap-pure function (no stores outside plain locals, no unknown calls,
   no `append`/`await`/`defer`, no values whose `Drop` touches the heap;
   a fixpoint over the call graph), a `for x in slice` element and a
   `var x = path` from a never-reassigned root are not retained or
   released. `sumItems` lost all six refcount operations per item (three
   atomic retains and releases on objects shared by 8 readers): phase 1
   (8 readers) 5-7x faster, phase 2 (one reader of objects biased to 8
   other threads) ~3.5x. `ZELL_NO_BORROW=1` at build time turns it off.
3. **Release `hello` size.** The shutdown path reached `zell_release`
   (through `zell__diag_shutdown`), the scheduler drain and the refcount
   queues; all three are now function pointers set by the subsystem when it
   starts (`zell_diag_rt_set_slots`, `zell__sched_start`,
   `zell__rc_thread_enable`), so `hello` links none of the heap free path,
   scavenger, biased-RC or scheduler-stop code.
4. **HTTP.** Every async call made a task with a pthread mutex and
   condition variable (init, lock, broadcast, destroy) and a coroutine
   frame from `calloc`. Tasks now use a spin lock (critical sections are a
   few stores) and threads blocking in `zell_task_await` wait on one
   process-wide condition, woken only for tasks with waiters; frames come
   from the object heap (`zell_coro_frame_alloc` / `zell_coro_frame_free`,
   ABI.md 10.3); retain/release of never-biased (`NOBIAS`) objects such as
   tasks is one atomic add instead of a CAS loop. On the TLS server,
   `zell__crypto_wipe` (a volatile byte loop) wiped a 6 KB AES-GCM context
   copy per record — 12% of the server's instructions; it is now `memset`
   plus a compiler barrier. Preliminary req/s on the shared machine were
   within noise of `old` (load generator 26.5k/23.3k -> 26.9k/27.3k against
   the Go server, h2 server 19.3k/20.4k -> 19.5k/21.8k, Go h2 server 16.7k
   in the same session); the quiet re-run (`http_hello` in the final table
   at the top) has the Zell h2 server at 28.6k req/s vs Go's 24.5k (1.17x)
   and the Zell load generator at 52.1k vs Go's 86.5k against the Go
   server (0.60x).

What remains: the load generator still spends ~30% of its instructions in
refcount operations on task, context and buffer objects and ~4% in the
benchmark's own `ToLower` (an async call that completed without suspending
allocated a task at the time of this pass; "Synchronous async calls" below
removed that). The TLS server copies the GCM context per record (could be
used in place with an aligned copy kept per connection), and does one
`zell__aead` call per response rather than batching streams into records.

## Synchronous async calls (2026-10-23, 16-core x86-64 Fedora 42, clang/LLVM 18, go 1.24, `--release`)

An async call that completes without suspending (a cached lookup, a
buffered read, a free mutex: most awaits of an HTTP service) no longer
makes a task. The ramp of `async func f(params) T` is now
`void @f(params, ptr %res)` with `%res -> { ptr task; T value }`
(runtime/ABI.md 10.3): the coroutine makes its `Task<T>` only at its first
real suspension (`zell_task_lazy`) and hands it to the caller through
`res.task`; a body that returns before that stores NULL there and moves its
result into `res.value`, and `await f()` reads it straight from the slot. The
frame allocation is guarded by `llvm.coro.alloc`, so a body without suspend
points has no frame either: LLVM turns it into a plain function. Callers
that keep the `Task<T>` (a variable, `task.All`, a `func() Task<T>` value)
get an already completed task from `zell_task_completed`, so nothing
observable changes; the slow path (a body that does suspend) does the same
work as before, only later.

Micro-benchmark: a loop of 10M `s += await f(i)`:

| callee | before, ns/call | after, ns/call |
|---|---:|---:|
| `async func f(i int) int { return i + 1 }` (no suspend point) | 42 | 3 |
| `async func f(t Task<int>, i int) int { return await t + i }`, `t` already done (a suspend point that never suspends: frame allocated and freed) | 42 | 15 |

(Go: 8 ns for a plain call on the 4 vCPU VM of the "Async benchmarks"
section, where the same loop took 136 ns per call before this change.) What
remains on the fast path is `zell_task_get_ctx` + `zell_coro_leave` (the
caller's context is saved and restored so that what the body sets never
leaks out) and, with a suspend point, one frame allocation and free from the
object heap's size classes plus `zell_task_is_done` / `zell_task_result`.

`RUNS=2 DETAIL=1 ./scripts/bench-perf.sh await_chain async_io` on the same
machine (load average 1.7 before the run), "before" being the master
compiler built from the same checkout:

| bench | metric | Zell before | Zell after | Go | after / Go (Go = 1.00) |
|---|---|---:|---:|---:|---:|
| await_chain | 10M async calls, no suspension | 500 ms (50 ns/call) | 118 ms (11.8 ns/call) | 10 ms | 11.8 |
| | 1k chains of 1000 resuming | 381 ms | 375 ms | 7 ms | 54 |
| | wall s (CPU s) | - | 0.495 (1.38) | 0.019 (0.02) | 26 |
| async_io | round trips/s (> 1 better) | - | 353 982 | 766 283 | 0.46 |
| | p50 / p99 / max | - | 2.8 / 3.1 / 10.3 ms | 1.1 / 5.9 / 52.6 ms | 2.5 / 0.52 / 0.20 |
| | wall s (CPU s) | - | 2.90 (29.8) | 1.40 (12.2) | 2.07 |

The chain's synchronous phase is 4.2x faster (each level still allocates
its frame, since its `await` of the recursive call is a real suspend point
the optimiser cannot fold); the resuming phase, where every level really
suspends, costs the same as before, as intended. `async_io` is bound by the
scheduler's queue trips, not by the call protocol (the figures above are
from a 16-core desktop, not the 4 vCPU VM of the earlier table).

Latency, cores scaling and language-mechanism benchmarks: see [RESULTS-latency.md](RESULTS-latency.md).
Stdlib benchmarks (`json_codec`, `encodings`, `crypto`, `file_io`, `sort_collect`): see [RESULTS-stdlib.md](RESULTS-stdlib.md).
Framework benchmarks (orm_sqlite, logging, websocket_echo, startup): [RESULTS-framework.md](RESULTS-framework.md).

## Performance wave 2 (2026-10-23, 16-core x86-64 Fedora 42, AMD Ryzen 7 8845HS, clang/LLVM 18, go 1.24.7, `--release`)

Five changes landed on master the same day (`e64d51d..ff07086`): SHA-NI and
pipelined/VAES AES-GCM in the crypto runtime, the synchronous fast path for
async calls (section above), `encoding.json` parsing and writing in the C
runtime (arena document, `Parse<T>` walking it directly), the scheduler
rewrite (per-worker run queues, run-next slot, work stealing,
`task.Channel`/`task.Mutex` in C), and the inline refcount fast path plus
borrowed struct temporaries in the compiler. The numbers below are **one
run per side** (`RUNS=1`, every benchmark under `timeout 120`; the whole
re-measure took 64 s), so treat them as ±5%. "before" is the same machine
on the morning's master (`e64d51d`), from the same scripts; the 4 vCPU
tables above are not comparable in absolute time. Go = 1.00.

| bench | phase | before | after | Zell / Go after |
|---|---|---:|---:|---:|
| crypto | SHA-256 1 GB | 373 MB/s | **2 507 MB/s** | 1.00 (was 6.7x) |
| crypto | AES-256-GCM seal 1 GB | 2 013 MB/s | **7 483 MB/s** | 0.93 (was 3.5x) |
| crypto | HKDF-SHA256 200k | 661k ops/s | **2 949k ops/s** | 0.43 (was 2.0x) |
| crypto | TLS 1.3 handshakes | 967 ops/s | 1 080 ops/s | 2.2 (P-256/Ed25519-bound) |
| crypto | SHA-512, ChaCha20-Poly1305, Ed25519, P-256, RSA, Argon2id | unchanged | unchanged | 1.8 / 7.8 / 8.2 (sign) / 6.5 (sign) / 1.6-2.5 / 3.9 |
| crypto | whole program (s) | 20.3 | 17.1 | 2.76 |
| json_codec (100 MB, `JSON_DOC_FILE`) | typed parse | not finishing (8 MB: 96.9 s) | **360 ms** (277 MB/s) | **0.34** |
| json_codec | typed format | 4.8x (8 MB) | 169 ms | 0.64 |
| json_codec | Value parse | 463x (8 MB) | 161 ms (619 MB/s) | **0.18** |
| json_codec | Value lookups | 5.7x (8 MB) | 143 ms | 3.3 (extern call per `Get`/`Index`, see below) |
| json_codec | Value format | 2.3x (8 MB) | 149 ms | 0.17 |
| json_codec | 1M small messages | 0.37x Go throughput | 1 650k msg/s | **3.6x Go throughput** |
| json_codec | whole program: wall, peak RSS | - | 2.20 s, 985 MB | 0.35 (Go 6.35 s, 1 347 MB) |
| defer_errors_iface | iface_call / closure_call / iface_sum | 1.94 / 2.60 / 2.45 | - | **1.39 / 1.27 / 1.24** |
| defer_errors_iface | defer_call | 8.45 | - | 6.79 |
| defer_errors_iface | error_sentinel (with traces) | 5.40 | - | 5.84 (trace copy, untouched) |
| defer_errors_iface | direct_call / generic_sum | 0.18 / 0.24 | - | 0.17 / 0.24 |
| async suite | spawn_tasks (wall) | 1.39 (4 vCPU) | 1.86 s vs 1.49 s | 1.25 |
| async suite | channel_pingpong (wall) | 30.4 (4 vCPU) | 0.86 s vs 0.49 s | **1.77** |
| async suite | fanout_fanin (wall) | 4.38 (4 vCPU) | 0.89 s vs 0.39 s | 2.32 |
| async suite | mutex_contention (wall) | 27.3 (4 vCPU) | 0.61 s vs 0.24 s | **2.60** |
| async suite | timers (wall) | 1.39 (4 vCPU) | 89 ms vs 58 ms | 1.53 |
| async suite | await_chain (wall) | 36.8 (4 vCPU) | 208 ms vs 19 ms | 10.9 (resume phase ~75 ns per frame) |
| async suite | cancel_tree (wall) | 1.33 (4 vCPU) | 465 ms vs 498 ms | **0.93** |
| async suite | async_io (wall) | 3.62 (4 vCPU) | 2.01 s vs 1.33 s | 1.51 |
| async suite | select_many (wall) | 14.0 (4 vCPU) | 2.33 s vs 1.34 s | **1.73** |

All checksums matched Go's. What remains, in order of size: `await_chain`'s
resume phase (run-next dispatch and `task_complete` atomics), `defer` (two
heap objects per call), sentinel errors with traces, Ed25519/P-256
(fixed-base tables) and ChaCha20 (AVX2), `Value` lookups (one runtime call
per `Get`/`Index`; LTO between program and runtime, or a cheaper key
compare in `rtGet`, is the next step), `fanout_fanin`'s pool phase and
`timers` lateness (timer wake-ups go through the global queue).

## Netpoll in the scheduler (2026-10-24, docs/ASYNC_PLAN.md R4, same machine as wave 2)

Idle workers poll epoll themselves and the last idle worker blocks in it
(ABI.md 10.1/10.9) instead of one poller thread per shard waking a worker
through the global queue. Three runs each, `perf stat`:

| bench | before (7a4dc64) | after | Go |
|---|---|---|---|
| async_io round trips/s | 637k | 742k | 802k |
| async_io p99 | 6.2 ms | 2.0 ms | 2.4 ms |
| async_io CPU (task-clock) | 24.2 s | 21.0 s | 18.8 s |
| async_io context switches | 794k | 5.9k | 3.2k |
| websocket_echo, zell client -> zell server | 378k msg/s | 442k msg/s | - |
| websocket_echo, go client -> zell server | 491k msg/s | 514k msg/s | - |
| http_hello h2 over TLS, go load -> zell | 84.1k req/s, 31.5 us/req | 86.8k req/s, 28.5 us/req | 77.6k, 23.2 us |

channel_pingpong and select_many (no I/O) are unchanged within noise.
What is left of the async_io CPU gap is outside the scheduler: `sendto`
is 59% of samples (as in Go), nftables 11% (host), the runtime's own code
16% with `worker_main` under 1%.

## Barging mutex (2026-10-25, docs/ASYNC_PLAN.md R5, same machine as wave 2)

`task.Mutex` moved to Go's `sync.Mutex` rules (DESIGN.md "Асинхронность",
ABI.md 10.6): `Unlock` wakes the oldest waiter without handing over, a
running task barges, a contended `Lock` spins up to 100 CAS attempts, and a
waiter older than 1 ms switches the mutex to FIFO hand-off. Medians of 3,
`ZELL_WORKERS` / `GOMAXPROCS` = 1 / 4 / 16; wall ms, CPU (user+sys) s,
context switches (voluntary+involuntary):

| mutex_contention | before (0d93b0e) | after | Go |
|---|---|---|---|
| contended wall, W=1 / 4 / 16 | 37 / 527 / 515 | 37 / 103 / 105 | 35 / 193 / 201 |
| uncontended wall, W=1 / 4 / 16 | 36 / 37 / 36 | 36 / 37 / 36 | 35 / 34 / 35 |
| CPU, W=1 / 4 / 16 | 0.07 / 1.07 / 1.05 | 0.07 / 0.24 / 0.24 | 0.07 / 0.64 / 0.64 |
| context switches, W=1 / 4 / 16 | 8 / 13.5k / 10.2k | 11 / 1.8k / 4.7k | 72 / 4.4k / 12.3k |

Contended is 0.52x Go at 16 workers (was 2.6x). channel_pingpong,
fanout_fanin, select_many and spawn_tasks are unchanged within noise
(wall, CPU and context switches). The scheduler gained a sentry: every
park now has a timeout (1 ms for the one worker watching, 10 ms for the
others, backing off while the pool is idle), because a run-next post made
while a spinner held the token, which then took other work, could wait
forever behind a job that never yields (found by the new 16-worker
starvation test in `runtime/tests/test_task.c`).

## Async wave result (2026-10-25)

Final measurement of the async wave (`docs/ASYNC_PLAN.md`), master `a1cd2e6`,
`--release` (as `scripts/bench-perf.sh`), go 1.24.7, .NET SDK 10.0.108
(Release, workstation non-concurrent GC, see `RESULTS-dotnet.md`), 16 threads
(AMD Ryzen 7 8845HS), Fedora 42, loopback. One session, one process at a time,
runtimes interleaved (Zell, Go, .NET, again) for 5 rounds; medians of the
phase values each program prints. "Start of wave" is the first table of
`docs/ASYNC_PLAN.md` (`b933053`, one run); timers and cancel_tree had no
16-thread number then, so their start column is the 4 vCPU table of this file.
Times in ms unless noted, lower is better; ratios below 1 mean Zell is faster.

| bench | phase | Zell start of wave | Zell now | Go | .NET | Zell/Go | Zell/.NET |
|---|---|---:|---:|---:|---:|---:|---:|
| spawn_tasks | run1M ms | 937 | **246** | 148 | 233 | 1.66 | 1.06 |
| spawn_tasks | park1M ms | 285 | **235** | 1 101 | 456 | 0.21 | 0.52 |
| spawn_tasks | wake1M ms | 469 | **89** | 121 | 204 | 0.74 | 0.44 |
| channel_pingpong | pingpong 5M ms | 619 | **430** | 418 | 2 365 | 1.03 | 0.18 |
| channel_pingpong | pipeline 1M ms | 149 | **125** | 76 | 269 | 1.64 | 0.46 |
| fanout_fanin | fan-in ms | 606 | **460** | 312 | 547 | 1.47 | 0.84 |
| fanout_fanin | pool ms | 251 | **136** | 78 | 430 | 1.74 | 0.32 |
| mutex_contention | contended ms | 624 | **107** | 203 | 3 452 | 0.53 | 0.03 |
| mutex_contention | uncontended ms | 71 | **39** | 35 | 257 | 1.11 | 0.15 |
| timers | total ms (start: 4 vCPU) | 89 | **66** | 51 | 66 | 1.29 | 1.00 |
| timers | mean late us | - | **41** | 126 | 793 | 0.33 | 0.05 |
| await_chain | sync 10M ms | 130 | **11** | 10 | 299 | 1.10 | 0.04 |
| await_chain | yield 1k chains ms | 96 | **14** | 7 | 129 | 2.00 | 0.11 |
| cancel_tree | cancel drain avg us (start: 4 vCPU) | 18 100 | **2 323** | 4 336 | 8 491 | 0.54 | 0.27 |
| cancel_tree | timeout drain avg us (start: 4 vCPU) | 17 100 | **2 777** | 4 756 | 7 974 | 0.58 | 0.35 |
| async_io | total ms | 1 555 | **1 254** | 1 256 | 1 527 | 1.00 | 0.82 |
| async_io | p99 us | 6 200 | **1 591** | 2 420 | 2 882 | 0.66 | 0.55 |
| select_many | select ms | 1 115 | **1 237** | 967 | 1 001 | 1.28 | 1.24 |
| select_many | merge ms | 1 006 | **827** | 396 | 956 | 2.09 | 0.87 |
| websocket_echo | msg/s, Go client | - | 619 086 | 663 983 | n/a | 0.93 (Go/Zell rate) | n/a |
| http_hello | req/s, Go load | - | 608 886 | 396 296 | n/a | 1.54 (rate; server CPU 8.3 vs 17.1 us/req) | n/a |

websocket_echo and http_hello are Zell vs Go only (5 rounds of their `run.sh`,
rates are medians; ratio column for these two is the Zell/Go rate).

CPU (user+sys of the whole process, `perf stat`) and context switches,
median of 5:

| bench | Zell CPU s | Zell switches | Go CPU s | Go switches | .NET CPU s | .NET switches |
|---|---:|---:|---:|---:|---:|---:|
| spawn_tasks | 1.43 | 94 005 | 5.32 | 168 756 | 7.94 | 142 969 |
| channel_pingpong | 1.27 | 18 377 | 0.59 | 15 849 | 24.14 | 605 |
| fanout_fanin | 2.66 | 126 305 | 1.46 | 193 237 | 13.07 | 158 040 |
| mutex_contention | 0.26 | 3 486 | 0.67 | 12 168 | 37.25 | 2 368 |
| timers | 0.2 | 13 189 | 0.21 | 5 587 | 0.57 | 9 291 |
| await_chain | 0.04 | 309 | 0.02 | 1 058 | 0.75 | 2 778 |
| cancel_tree | 0.28 | 5 892 | 0.97 | 80 820 | 2.86 | 27 842 |
| async_io | 19.7 | 4 226 | 19.01 | 2 647 | 21.14 | 158 158 |
| select_many | 6.19 | 507 057 | 2.23 | 76 022 | 22.36 | 895 174 |

Worker-count sweep (`ZELL_WORKERS` / `GOMAXPROCS` / `DOTNET_PROCESSOR_COUNT`, median of 5, ms):

| bench phase | Zell 1 / 4 / 16 | Go 1 / 4 / 16 | .NET 1 / 4 / 16 |
|---|---|---|---|
| mutex contended | 38 / 96 / 107 | 35 / 175 / 203 | 573 / 2838 / 3452 |
| ping-pong | 409 / 439 / 430 | 459 / 425 / 418 | 1921 / 2090 / 2365 |
| pipeline | 129 / 122 / 125 | 86 / 75 / 76 | 522 / 160 / 269 |

Findings:

- Zell beats Go on mutex_contention (0.53x, barging mutex, 2.6x less CPU), cancel_tree (0.54x, 14x fewer switches), timers lateness (mean 41 vs 126 us, nothing over 1 ms), park1M (0.21x) and wake1M (0.74x), and has the best async_io p99 (1.6 vs 2.4 ms).
- At parity: ping-pong (1.03x), await_chain sync (11 vs 10 ms, was 13x), async_io throughput (1254 vs 1256), uncontended mutex (39 vs 35), timers total.
- Remaining gaps: select_many (select 1.28x, merge 2.1x, 507k switches and 6.2 vs 2.2 CPU s: the idle protocol and cancel hooks per wait, ASYNC_PLAN R3 leftovers), fanout_fanin (1.5x / 1.7x, parking churn), pipeline 1.6x and await_chain yield 2x (a task object per resumed frame, remaining C2 cost), spawn run1M 1.7x (`async main` still posts through the global queue).
- Against the start of the wave every phase improved except select (1115 -> 1237 in the interleaved run); checked separately: select_many in isolation, 5 interleaved runs per commit, select/merge medians 0d93b0e 1131/840, b4ad300 (R5) 1108/842, ef15437 1111/849, 14ab1da (frame cache) 1143/848, HEAD 1124/835. No regression: the 1237 is interleaving noise (it follows a .NET run), the true value is ~1120, equal to the start of wave.
- .NET: ahead of Go on nothing but park1M; 5-15x behind on hand-off chains and burns 13-37 CPU s on pingpong, fanout, mutex and select_many (spinning pool workers).
- Caveats: one 16-thread machine with a desktop session running (load average 3-4 at start), same-session interleaving but not isolated cores, cancel_tree/timers start values are 4 vCPU, the start column is single-run, ws/http client dominates its own rate (Go client against Zell server 619k vs Go server 664k msg/s; Zell load generator is slower than Go's), the .NET ports are not rendezvous channels (see `RESULTS-dotnet.md`).

## json_codec on .NET

`benchmarks/json_codec/dotnet/` is the same program on System.Text.Json (typed path through a `[JsonSerializable]` source-generated context, schema-less path through `JsonDocument`/`JsonElement`, net10.0, workstation GC, invariant globalization). `dotnet/run.sh` publishes once (`dotnet publish -c Release -r linux-x64 --self-contained false -o bin`) and runs it with the same `SCALE`, `JSON_DOC_BYTES` and `JSON_DOC_FILE` variables as the others. `benchmarks/json_codec/compare.sh` (`ZELL=... JSON_DOC_FILE=... ./compare.sh`) builds and runs Zell, Go and .NET once each on one document and prints the checksum and phase lines side by side, then states whether the checksum lines are identical. The .NET value-format phase writes sorted keys and shortest-round-trip numbers to match Go's `json.Marshal(any)` byte for byte.

## Soak and fuzz

Long-running stability checks (not part of `cargo test`). The HTTP soak
`scripts/soak-http.sh` drove the `web_api` server and left with `web`
(tag `web-in-tree`); a `net.http` replacement is not written yet.

- `make -C runtime fuzz-json` builds the runtime and the libFuzzer target
  `runtime/tests/fuzz/fuzz_json.c` (JSON parse, node walk, writer round
  trip) with `-fsanitize=fuzzer,address,undefined` in `runtime/build/fuzz`,
  seeds it from `runtime/tests/fuzz/corpus/json/` and runs `FUZZ_SECONDS`
  (default 300) with `-max_len=65536`. A crash input lands in
  `runtime/build/fuzz/`; keep reproducers in `runtime/tests/fuzz/crashes/`.
  `FUZZ_STANDALONE=1` replays the corpus without libFuzzer.
