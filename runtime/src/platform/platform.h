/* platform.h - the runtime's internal platform layer (docs/PLATFORMS.md).
 *
 * Everything OS-specific that the rest of the runtime needs goes through
 * these functions; the implementations live in runtime/src/platform/:
 *   posix.c        threads, CPU count, monotonic clock, entropy, backtraces
 *   poll_epoll.c   readiness poller on Linux (epoll + eventfd)
 *   poll_kqueue.c  readiness poller on macOS/BSD (kqueue + EVFILT_USER)
 *   net_posix.c    BSD sockets (TCP/UDP/Unix, options, sendfile), names
 *   fs_posix.c     files, directories, environment, child processes,
 *                  signals, users (POSIX)
 *   win32.c        Windows: threads, locks, clocks, entropy,
 *                  dynamic loading, console, environment, signals
 *   prof_win32.c   Windows backtraces, CPU sampler, symbols (COFF, DbgHelp)
 *   tz_win32.c     Windows time zones (registry -> TZif)
 *   fs_win32.c     Windows files and directories (UTF-16 at the boundary)
 *   proc_win32.c   Windows child processes (CreateProcessW)
 *   poll_iocp.c    Windows readiness poller (IOCP + IOCTL_AFD_POLL)
 *   poll_wsapoll.c Windows fallback poller (WSAPoll)
 *   net_win32.c    Winsock2
 *   timefmt.c      strftime/strptime where the C library lacks them
 * Not part of the ABI: every symbol is hidden and prefixed zell__plat_
 * (types: zell__plat_mutex, zell__plat_cond, ...). This header and the
 * files above are the only places with OS conditionals (#ifdef _WIN32,
 * __linux__, __APPLE__); the rest of the runtime is OS-independent. */
#ifndef ZELL_PLATFORM_H
#define ZELL_PLATFORM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <setjmp.h>
#include <time.h>

#if defined(_WIN32)
/* PE/COFF has no ELF visibility; symbols of a static archive are not
 * exported from the executable anyway. */
#define ZELL_PLAT_HIDDEN
#else
#define ZELL_PLAT_HIDDEN __attribute__((visibility("hidden")))
#endif

/* ---- threads ---------------------------------------------------------- */

typedef void *(*zell__plat_thread_fn)(void *arg);
typedef struct zell__plat_thread { uintptr_t handle; } zell__plat_thread;

/* Starts a joinable thread; returns false on failure. */
ZELL_PLAT_HIDDEN bool zell__plat_thread_start(zell__plat_thread *t, zell__plat_thread_fn fn, void *arg);
/* Starts a detached thread; returns false on failure. */
ZELL_PLAT_HIDDEN bool zell__plat_thread_start_detached(zell__plat_thread_fn fn, void *arg);
/* A detached runtime-internal thread that never touches Zell objects (the
 * heap's scavenger): unlike the two above it leaves zell_threaded alone,
 * so a program whose only other thread is this one keeps the
 * single-threaded refcount paths. */
ZELL_PLAT_HIDDEN bool zell__plat_thread_start_background(zell__plat_thread_fn fn, void *arg);
ZELL_PLAT_HIDDEN void zell__plat_thread_join(zell__plat_thread t);
/* Number of online CPUs (at least 1). */
ZELL_PLAT_HIDDEN int64_t zell__plat_cpu_count(void);
/* The CPUs this process may use (at least 1, at most the online ones): the
 * affinity mask (taskset, a cpuset) and a CPU quota rounded up (Linux
 * cgroup v2 cpu.max on the way to the root, else v1 cfs_quota/period;
 * Windows: a hard-capped job object's CPU rate). The default worker count. */
ZELL_PLAT_HIDDEN int64_t zell__plat_cpu_limit(void);
/* Gives up the CPU (spin-lock back-off). */
ZELL_PLAT_HIDDEN void zell__plat_yield(void);
/* A spin-wait hint to the CPU (x86 `pause`, arm64 `yield`): no system
 * call, the sibling hyperthread gets the core for a few dozen cycles. */
static inline void zell__plat_cpu_relax(void) {
#if defined(__x86_64__) || defined(__i386__)
    __builtin_ia32_pause();
#elif defined(__aarch64__) || defined(__arm__)
    __asm__ __volatile__("yield" ::: "memory");
#else
    __asm__ __volatile__("" ::: "memory");
#endif
}

/* ---- mutexes, condition variables, once, thread-local keys ------------
 * pthreads on POSIX (inline), SRWLOCK / CONDITION_VARIABLE / INIT_ONCE /
 * FLS on Windows (win32.c). Mutexes are not recursive. Static
 * initialisers: ZELL_PLAT_MUTEX_INIT, ZELL_PLAT_COND_INIT,
 * ZELL_PLAT_ONCE_INIT. */

#if defined(_WIN32)
typedef struct zell__plat_mutex { void *p; } zell__plat_mutex;  /* SRWLOCK */
typedef struct zell__plat_cond { void *p; } zell__plat_cond;    /* CONDITION_VARIABLE */
typedef struct zell__plat_once { void *p; } zell__plat_once;    /* INIT_ONCE */
typedef struct zell__plat_tls_key { unsigned long idx; } zell__plat_tls_key;  /* FLS index */
#define ZELL_PLAT_MUTEX_INIT {0}
#define ZELL_PLAT_COND_INIT {0}
#define ZELL_PLAT_ONCE_INIT {0}
ZELL_PLAT_HIDDEN void zell__plat_mutex_init(zell__plat_mutex *m);
ZELL_PLAT_HIDDEN void zell__plat_mutex_destroy(zell__plat_mutex *m);
ZELL_PLAT_HIDDEN void zell__plat_mutex_lock(zell__plat_mutex *m);
ZELL_PLAT_HIDDEN void zell__plat_mutex_unlock(zell__plat_mutex *m);
ZELL_PLAT_HIDDEN void zell__plat_cond_init(zell__plat_cond *c);
ZELL_PLAT_HIDDEN void zell__plat_cond_destroy(zell__plat_cond *c);
ZELL_PLAT_HIDDEN void zell__plat_cond_wait(zell__plat_cond *c, zell__plat_mutex *m);
ZELL_PLAT_HIDDEN bool zell__plat_cond_wait_ns(zell__plat_cond *c, zell__plat_mutex *m, int64_t ns);
ZELL_PLAT_HIDDEN void zell__plat_cond_signal(zell__plat_cond *c);
ZELL_PLAT_HIDDEN void zell__plat_cond_broadcast(zell__plat_cond *c);
ZELL_PLAT_HIDDEN void zell__plat_once_run(zell__plat_once *o, void (*fn)(void));
ZELL_PLAT_HIDDEN bool zell__plat_tls_new(zell__plat_tls_key *k, void (*dtor)(void *));
ZELL_PLAT_HIDDEN void zell__plat_tls_set(zell__plat_tls_key k, void *v);
#else
#include <errno.h>
#include <pthread.h>
typedef pthread_mutex_t zell__plat_mutex;
/* `mono`: created by zell__plat_cond_init, whose timed waits use
 * CLOCK_MONOTONIC where the system allows it (not macOS). */
typedef struct zell__plat_cond { pthread_cond_t c; int mono; } zell__plat_cond;
typedef pthread_once_t zell__plat_once;
typedef struct zell__plat_tls_key { pthread_key_t k; } zell__plat_tls_key;
#define ZELL_PLAT_MUTEX_INIT PTHREAD_MUTEX_INITIALIZER
#define ZELL_PLAT_COND_INIT {PTHREAD_COND_INITIALIZER, 0}
#define ZELL_PLAT_ONCE_INIT PTHREAD_ONCE_INIT
static inline void zell__plat_mutex_init(zell__plat_mutex *m) { pthread_mutex_init(m, NULL); }
static inline void zell__plat_mutex_destroy(zell__plat_mutex *m) { pthread_mutex_destroy(m); }
static inline void zell__plat_mutex_lock(zell__plat_mutex *m) { pthread_mutex_lock(m); }
static inline void zell__plat_mutex_unlock(zell__plat_mutex *m) { pthread_mutex_unlock(m); }
static inline void zell__plat_cond_init(zell__plat_cond *c) {
#if defined(__APPLE__)
    pthread_cond_init(&c->c, NULL);
    c->mono = 0;
#else
    pthread_condattr_t a;
    pthread_condattr_init(&a);
    c->mono = pthread_condattr_setclock(&a, CLOCK_MONOTONIC) == 0;
    pthread_cond_init(&c->c, &a);
    pthread_condattr_destroy(&a);
#endif
}
static inline void zell__plat_cond_destroy(zell__plat_cond *c) { pthread_cond_destroy(&c->c); }
static inline void zell__plat_cond_wait(zell__plat_cond *c, zell__plat_mutex *m) { pthread_cond_wait(&c->c, m); }
/* Waits at most ns nanoseconds; false on timeout. */
static inline bool zell__plat_cond_wait_ns(zell__plat_cond *c, zell__plat_mutex *m, int64_t ns) {
    if (ns < 0) ns = 0;
    struct timespec ts;
    clock_gettime(c->mono ? CLOCK_MONOTONIC : CLOCK_REALTIME, &ts);
    int64_t t = (int64_t)ts.tv_nsec + ns % 1000000000;
    ts.tv_sec += (time_t)(ns / 1000000000 + t / 1000000000);
    ts.tv_nsec = (long)(t % 1000000000);
    return pthread_cond_timedwait(&c->c, m, &ts) != ETIMEDOUT;
}
static inline void zell__plat_cond_signal(zell__plat_cond *c) { pthread_cond_signal(&c->c); }
static inline void zell__plat_cond_broadcast(zell__plat_cond *c) { pthread_cond_broadcast(&c->c); }
static inline void zell__plat_once_run(zell__plat_once *o, void (*fn)(void)) { pthread_once(o, fn); }
static inline bool zell__plat_tls_new(zell__plat_tls_key *k, void (*dtor)(void *)) { return pthread_key_create(&k->k, dtor) == 0; }
static inline void zell__plat_tls_set(zell__plat_tls_key k, void *v) { pthread_setspecific(k.k, v); }
#endif

/* stdio locking around a line of output (flockfile & co. on POSIX,
 * _lock_file on Windows). */
#if defined(_WIN32)
#define zell__plat_flockfile(f) _lock_file(f)
#define zell__plat_funlockfile(f) _unlock_file(f)
#else
#define zell__plat_flockfile(f) flockfile(f)
#define zell__plat_funlockfile(f) funlockfile(f)
#endif

/* Whether a panic can be caught on its own thread with setjmp / longjmp
 * (testing.Panics, std_testing.c). Windows: no (mingw's longjmp unwinds
 * with SEH through frames whose unwind data the runtime does not control). */
#if defined(_WIN32)
#define ZELL_PLAT_HAS_PANIC_CAPTURE 0
#else
#define ZELL_PLAT_HAS_PANIC_CAPTURE 1
#endif

/* The non-local jump of a task panic (ABI.md 8.1, task.c run_job): it only
 * restores registers, with no SEH or DWARF unwinding of the frames in
 * between, and leaves the signal mask alone. x86-64 POSIX:
 * __builtin_setjmp / __builtin_longjmp (three words, no library call).
 * Windows x64: _setjmp with a NULL frame, for which longjmp restores the
 * saved registers without an SEH unwind; __builtin_setjmp is unusable there
 * (LLVM saves the frame address, not the frame pointer, which Win64 frames
 * offset from it, so code after the landing addresses the wrong slots).
 * Elsewhere (arm64: no __builtin_longjmp): POSIX _setjmp / _longjmp. */
#if defined(_WIN32)
typedef jmp_buf zell__plat_jmpbuf;
#define zell__plat_setjmp(b) _setjmp((b), NULL)
#define zell__plat_longjmp(b) longjmp((b), 1)
#elif defined(__x86_64__)
typedef void *zell__plat_jmpbuf[5];
#define zell__plat_setjmp(b) __builtin_setjmp(b)
#define zell__plat_longjmp(b) __builtin_longjmp((b), 1)
#else
typedef jmp_buf zell__plat_jmpbuf;
#define zell__plat_setjmp(b) _setjmp(b)
#define zell__plat_longjmp(b) _longjmp((b), 1)
#endif

/* Whether functions using 256-bit AVX registers may be built (crypto's
 * VAES path). GCC targeting Windows: no, it does not align the stack for
 * 32-byte AVX spills there (GCC bug 54412); clang/mingw is fine. */
#if defined(_WIN32) && !defined(__clang__)
#define ZELL_PLAT_HAS_AVX_CODEGEN 0
#else
#define ZELL_PLAT_HAS_AVX_CODEGEN 1
#endif

/* ---- memory ------------------------------------------------------------- */

/* Aligned allocation (align a power of two, size a multiple of it); must
 * be released with zell__plat_aligned_free. */
ZELL_PLAT_HIDDEN void *zell__plat_aligned_alloc(size_t align, size_t size);
ZELL_PLAT_HIDDEN void zell__plat_aligned_free(void *p);
/* Usable size of a malloc'd block (malloc_usable_size / malloc_size / _msize). */
ZELL_PLAT_HIDDEN size_t zell__plat_alloc_size(void *p);

/* Virtual memory for the object heap (heap.c). Lengths are multiples of
 * zell__plat_page_size(). zell__plat_mem_map: a fresh zeroed read-write
 * mapping of len bytes aligned to `align` (a power of two, >= the page
 * size), NULL on failure; zell__plat_mem_unmap frees a whole mapping.
 * zell__plat_mem_release returns the physical memory of [p, p + len)
 * (page-aligned, inside a mapping) to the OS while keeping the address
 * range usable: the next touch gets a zero page (Linux MADV_DONTNEED) or
 * the old contents or zeros (macOS MADV_FREE, Windows DiscardVirtualMemory
 * / MEM_RESET); callers must not rely on the contents. zell__plat_mem_remap
 * grows or shrinks a mapping, possibly moving it (Linux mremap); NULL means
 * unsupported or failed, and the old mapping is untouched.
 * zell__plat_mem_hint_huge asks for transparent huge pages on a mapping
 * (Linux MADV_HUGEPAGE: one fault per 2 MB where THP is "madvise"; a no-op
 * elsewhere).
 * zell__plat_malloc_trim gives free malloc memory back (glibc malloc_trim). */
ZELL_PLAT_HIDDEN size_t zell__plat_page_size(void);
ZELL_PLAT_HIDDEN void *zell__plat_mem_map(size_t len, size_t align);
ZELL_PLAT_HIDDEN void zell__plat_mem_unmap(void *p, size_t len);
ZELL_PLAT_HIDDEN void zell__plat_mem_release(void *p, size_t len);
ZELL_PLAT_HIDDEN void zell__plat_mem_hint_huge(void *p, size_t len);
ZELL_PLAT_HIDDEN void *zell__plat_mem_remap(void *p, size_t old_len, size_t new_len);
ZELL_PLAT_HIDDEN void zell__plat_malloc_trim(void);

/* ---- time and entropy ------------------------------------------------- */

/* Monotonic clock in nanoseconds (arbitrary origin). */
ZELL_PLAT_HIDDEN int64_t zell__plat_mono_ns(void);
/* Wall clock: nanoseconds since the Unix epoch. */
ZELL_PLAT_HIDDEN int64_t zell__plat_wall_ns(void);
/* Sleeps the calling thread (at least ns nanoseconds). */
ZELL_PLAT_HIDDEN void zell__plat_sleep_ns(int64_t ns);
/* Calendar fields of Unix second t in UTC (gmtime_r, proleptic Gregorian,
 * any int64 year range the struct holds) and the inverse (timegm, which
 * normalises out-of-range fields like mktime). Pure C, timefmt.c. */
ZELL_PLAT_HIDDEN void zell__plat_utc_tm(int64_t t, struct tm *tm);
ZELL_PLAT_HIDDEN int64_t zell__plat_timegm(const struct tm *tm);
/* strftime with the C99/POSIX conversions on every platform (the C library's
 * own where it has them all, timefmt.c otherwise); 0 if cap is too small. */
ZELL_PLAT_HIDDEN size_t zell__plat_strftime(char *buf, size_t cap, const char *fmt, const struct tm *tm);
/* strptime; *gmtoff gets the offset parsed by %z (seconds east), else 0.
 * Returns the end of the parsed input or NULL. */
ZELL_PLAT_HIDDEN const char *zell__plat_strptime(const char *s, const char *fmt, struct tm *tm, long *gmtoff);
/* The C library's strftime/strptime have every conversion time uses
 * (%k %l %z ...) on glibc, macOS and the BSDs; elsewhere (Windows, musl)
 * zell__plat_strftime/strptime are the portable ones below. */
#if !defined(_WIN32) && (defined(__GLIBC__) || defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__) || \
                         defined(__NetBSD__) || defined(__DragonFly__))
#define ZELL_PLAT_LIBC_TIMEFMT 1
#endif
/* The "C"-locale implementations behind them on Windows (timefmt.c,
 * compiled everywhere so POSIX builds test them against the C library). */
ZELL_PLAT_HIDDEN size_t zell__plat_strftime_portable(char *buf, size_t cap, const char *fmt, const struct tm *tm);
ZELL_PLAT_HIDDEN const char *zell__plat_strptime_portable(const char *s, const char *fmt, struct tm *tm, long *gmtoff);
/* The local zone's TZif file when $TZ is unset ("/etc/localtime"), and the
 * default zoneinfo directory; NULL where the system has none (Windows:
 * UTC unless $TZ and $ZONEINFO name a TZif file). */
ZELL_PLAT_HIDDEN const char *zell__plat_localtime_path(void);
ZELL_PLAT_HIDDEN const char *zell__plat_zoneinfo_dir(void);
/* TZif bytes (RFC 8536 v2, malloc'd) of a zone from a system database that
 * is not a tzdata directory: on Windows the registry's Time Zones with an
 * IANA name table (tz_win32.c). name NULL = the local zone. false if the
 * zone is unknown or the system has no such database (POSIX). */
ZELL_PLAT_HIDDEN bool zell__plat_tz_tzif(const char *name, char **out, size_t *len);
/* Fills buf with n cryptographically secure random bytes; false on failure. */
ZELL_PLAT_HIDDEN bool zell__plat_random_bytes(void *buf, size_t n);

/* ---- diagnostics ------------------------------------------------------ */

/* Writes a best-effort backtrace of the calling thread to fd, skipping the
 * innermost `skip` frames. */
ZELL_PLAT_HIDDEN void zell__plat_backtrace(int fd, int skip);
/* The same frames as text into buf (NUL-terminated, truncated to cap): a
 * contained task panic keeps its trace (ABI.md 8.1). */
ZELL_PLAT_HIDDEN void zell__plat_backtrace_buf(char *buf, size_t cap, int skip);
/* Writes the frames `pcs` (return addresses; a faulting PC is passed + 1)
 * to fd in the same format. Runtime-internal panic entry points
 * (`zell_panic*`, `zell__panic*`) are left out of every backtrace. */
ZELL_PLAT_HIDDEN void zell__plat_backtrace_pcs(int fd, const uintptr_t *pcs, int n);
/* The line table (ABI.md 8.2; lines.c): source file, line and Zell
 * function (NULL if unknown) of code address `pc` (a return address minus
 * one); false if the address has no line, the program registered no table
 * or ZELL_NO_LINES is set. */
ZELL_PLAT_HIDDEN bool zell__lines_find(uintptr_t pc, const char **file, uint32_t *line, const char **func);
/* `file` as printed: relative to the working directory when under it. */
ZELL_PLAT_HIDDEN const char *zell__lines_shown(const char *file);

/* Reports a fatal signal / exception and exits with code 2 (ABI.md 8.3):
 * flushes stdout, prints "fatal error: unexpected signal <name>", the
 * fault (`what`, address) and the frames `pcs` (faulting PC first, others
 * return addresses). Called from the platform's fault handler. */
ZELL_PLAT_HIDDEN _Noreturn void zell__fatal_signal(const char *name, const char *what, uintptr_t addr, const uintptr_t *pcs, int n);

/* ---- CPU profiling (posix.c; diag, runtime/src/profile.c) ---------
 * A process-wide CPU-time timer (SIGPROF on POSIX) calls `fn` from the
 * signal handler of whichever thread was running, with the interrupted PC
 * first and then the return addresses found by walking frame pointers
 * (only within the thread's stack bounds recorded by
 * zell__plat_thread_stack_init; other threads report the PC alone). `fn`
 * must be async-signal-safe. */
typedef void (*zell__plat_prof_fn)(const uintptr_t *pcs, int n);
/* Records the calling thread's stack bounds for frame walks (cheap; called
 * by worker threads at start and by the profiler for the caller). */
ZELL_PLAT_HIDDEN void zell__plat_thread_stack_init(void);
/* Starts sampling `hz` times per CPU second; false if unsupported. */
ZELL_PLAT_HIDDEN bool zell__plat_prof_start(int hz, zell__plat_prof_fn fn);
ZELL_PLAT_HIDDEN void zell__plat_prof_stop(void);
/* Symbolizes a code address of the process: writes the function name to
 * buf (demangling nothing) and its start address to *start; false if not
 * found. Uses the executable's own symbol table (local symbols included),
 * then the dynamic linker. Not async-signal-safe. */
ZELL_PLAT_HIDDEN bool zell__plat_symbolize(uintptr_t pc, char *buf, size_t cap, uintptr_t *start);
/* The source name of a symbol: strips the "__slp<hex>." prefix that the
 * compiler's parallel code generation (compiler/src/parcodegen.rs) gives
 * internal symbols it makes visible across partitions. */
static inline const char *zell__plat_sym_clean(const char *name) {
    if (name[0] == '_' && name[1] == '_' && name[2] == 's' && name[3] == 'l' && name[4] == 'p') {
        const char *p = name + 5;
        while ((*p >= '0' && *p <= '9') || (*p >= 'a' && *p <= 'f')) p++;
        if (*p == '.' && p > name + 5) return p + 1;
    }
    return name;
}
/* Address range and path of the main executable's text (for pprof
 * mappings); false if unknown. */
ZELL_PLAT_HIDDEN bool zell__plat_exe_range(uintptr_t *lo, uintptr_t *hi, char *path, size_t cap);
/* Drops the symbol table cache. */
ZELL_PLAT_HIDDEN void zell__plat_symbolize_done(void);

/* ---- dynamic loading (plugin, posix.c) ---------------------------- */

/* Opens a shared object (symbols bound now, kept local); NULL on failure. */
ZELL_PLAT_HIDDEN void *zell__plat_dl_open(const char *path);
/* Address of `name` in an opened object, NULL if absent. */
ZELL_PLAT_HIDDEN void *zell__plat_dl_sym(void *h, const char *name);
ZELL_PLAT_HIDDEN void zell__plat_dl_close(void *h);
/* The last loader error (static text). */
ZELL_PLAT_HIDDEN const char *zell__plat_dl_error(void);

/* ---- readiness poller (async I/O backend) -----------------------------
 * A readiness poller is level-triggered and one-shot (except epoll, see
 * zell__plat_poller_edge below): poller_arm(fd,
 * interest, token) asks for ONE report when fd is ready for any direction
 * in `interest`; after that report (or a spurious one) the fd stays
 * registered but disarmed until it is armed again. One-shot level
 * triggering gives the correctness of level triggering (data that is
 * already there is reported at once, nothing is ever lost between a read
 * that saw EAGAIN and the arm) with one system call per wait. Reports may
 * be spurious (a direction nobody asked for, readiness that is gone by the
 * time the caller looks): callers retry the operation and wait again.
 * Every function is thread-safe; arm/remove may run while another thread
 * is inside poller_wait. Backends: poll_epoll.c (Linux), poll_kqueue.c
 * (macOS/BSD), poll_iocp.c (Windows: a completion port per poller with
 * readiness from IOCTL_AFD_POLL, falling back to poll_wsapoll.c), see
 * docs/PLATFORMS.md. */

/* ZELL_PLAT_HUP (edge-triggered pollers only): the peer shut down its side,
 * so the fd stays readable (end of stream) from now on. */
enum { ZELL_PLAT_READ = 1, ZELL_PLAT_WRITE = 2, ZELL_PLAT_ERROR = 4, ZELL_PLAT_HUP = 8 };

/* Token of the poller's own wake-up channel in events. */
#define ZELL_PLAT_WAKE_TOKEN UINT64_MAX

typedef struct zell__plat_event {
    uint64_t token;  /* the token given to poller_arm, or ZELL_PLAT_WAKE_TOKEN */
    int32_t events;  /* ZELL_PLAT_READ | ZELL_PLAT_WRITE | ZELL_PLAT_ERROR */
} zell__plat_event;

typedef struct zell__plat_poller zell__plat_poller;

/* A new poller with a wake-up channel; NULL on failure. */
ZELL_PLAT_HIDDEN zell__plat_poller *zell__plat_poller_new(void);
ZELL_PLAT_HIDDEN void zell__plat_poller_free(zell__plat_poller *p);
/* Arms fd for one report of `interest` (READ/WRITE mask) carrying `token`.
 * `registered` says whether fd was already added to this poller (the
 * function copes with a wrong guess). interest 0 disarms. Returns 0 or an
 * errno: ZELL_PLAT_ENOTPOLLABLE for a descriptor that cannot be polled
 * (a regular file: always ready), EBADF for a closed one. */
#define ZELL_PLAT_ENOTPOLLABLE 100001
ZELL_PLAT_HIDDEN int zell__plat_poller_arm(zell__plat_poller *p, int64_t fd, int32_t interest, uint64_t token, bool registered);
/* Removes fd (before it is closed). */
ZELL_PLAT_HIDDEN void zell__plat_poller_remove(zell__plat_poller *p, int64_t fd);
/* Waits up to timeout_ms (-1 = forever) and stores up to max events.
 * Returns the number of events (0 on timeout or EINTR). The wake-up
 * channel shows up as ZELL_PLAT_WAKE_TOKEN and is drained internally. */
ZELL_PLAT_HIDDEN int zell__plat_poller_wait(zell__plat_poller *p, zell__plat_event *evs, int max, int64_t timeout_ms);
/* Makes a concurrent or the next poller_wait return (thread-safe). */
ZELL_PLAT_HIDDEN void zell__plat_poller_wake(zell__plat_poller *p);
/* Name of the backend of p: "epoll", "kqueue", "iocp" or "wsapoll". */
ZELL_PLAT_HIDDEN const char *zell__plat_poller_backend(zell__plat_poller *p);
/* Whether p is edge-triggered (epoll on Linux; the io_uring, kqueue, IOCP
 * and WSAPoll backends are one-shot as described above). An edge-triggered
 * poller is armed once per fd (registered = false; interest is ignored:
 * both directions are watched) and then reports every new readiness of
 * either direction without being armed again; the fd's current readiness
 * is reported once when it is added. The caller (task_io.c) remembers a
 * report that found no waiter, and a wait must only follow an operation
 * that failed with EAGAIN (or a short stream read). Events carry
 * ZELL_PLAT_HUP for a peer shutdown and ZELL_PLAT_ERROR for an error or
 * hang-up: both conditions last, but a read that returns the data before
 * them does not report them, so the caller treats them as permanent. */
/* Scheduler polling (ABI.md 10.9 "Polling from the workers"): whether p
 * may be polled by any thread of the scheduler, one at a time and mostly
 * with timeout 0 (an idle worker looking for work), and linked into a hub
 * poller with zell__plat_poller_link. A backend that answers true opts
 * its pollers out of the dedicated poller threads: task_io.c then blocks
 * in one hub (the last worker to go idle does, like Go's netpoller) and
 * the workers pick readiness up between jobs. Today epoll answers true
 * (io_uring, kqueue, IOCP and WSAPoll keep the dedicated threads); kqueue
 * can opt in by implementing both functions (a kqueue is pollable by
 * another kqueue). */
#if defined(__linux__)
ZELL_PLAT_HIDDEN bool zell__plat_poller_edge(zell__plat_poller *p);
ZELL_PLAT_HIDDEN bool zell__plat_poller_shared(zell__plat_poller *p);
/* Makes `hub` report `token` (level-triggered, no direction bits) while p
 * has events to report, so one blocking wait on the hub covers every
 * linked poller; the caller then polls p with timeout 0. Both must be
 * shared pollers. Returns 0 or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_poller_link(zell__plat_poller *hub, zell__plat_poller *p, uint64_t token);
#else
static inline bool zell__plat_poller_edge(zell__plat_poller *p) { (void)p; return false; }
static inline bool zell__plat_poller_shared(zell__plat_poller *p) { (void)p; return false; }
static inline int zell__plat_poller_link(zell__plat_poller *hub, zell__plat_poller *p, uint64_t token) { (void)hub; (void)p; (void)token; return 95; /* EOPNOTSUPP */ }
#endif

/* ---- process, console, environment (posix.c / win32.c) ---------------- */

/* Once, from zell_runtime_init before anything else: installs the fatal
 * fault handler (SIGSEGV/SIGBUS/SIGILL/SIGFPE on an alternate signal stack
 * on POSIX, a vectored exception handler plus an unhandled-exception filter
 * on Windows; both report through zell__fatal_signal, ABI.md 8.3). On
 * Windows it also replaces *argv by the UTF-8 command line
 * (CommandLineToArgvW); the constructor in win32.c puts stdout/stderr in
 * binary mode (println writes \n) and switches an attached console to
 * UTF-8 with ANSI escape processing. */
ZELL_PLAT_HIDDEN void zell__plat_runtime_init(int *argc, char ***argv);
/* Writes n bytes to a stdio stream: fwrite, except that on a Windows
 * console the UTF-8 text goes through WriteConsoleW (a sequence split
 * across calls is carried over). Caller holds the stream lock or not. */
ZELL_PLAT_HIDDEN void zell__plat_stream_write(FILE *f, const char *p, size_t n);
ZELL_PLAT_HIDDEN int64_t zell__plat_pid(void);
/* malloc'd UTF-8 value of an environment variable, NULL if unset. */
ZELL_PLAT_HIDDEN char *zell__plat_getenv(const char *name);
/* setenv / unsetenv (value NULL); 0 or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_setenv(const char *name, const char *value);
/* A malloc'd NULL-terminated copy of the environment ("K=V" UTF-8
 * strings, each malloc'd); zell__plat_free_strv releases it. */
ZELL_PLAT_HIDDEN char **zell__plat_environ(void);
ZELL_PLAT_HIDDEN void zell__plat_free_strv(char **v);
/* Host name into buf; 0 or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_hostname(char *buf, size_t cap);
/* malloc'd directory for temporary files ($TMPDIR or /tmp; GetTempPathW). */
ZELL_PLAT_HIDDEN char *zell__plat_temp_dir(void);
/* Path separator of the platform ('/' or '\\'). */
ZELL_PLAT_HIDDEN char zell__plat_path_sep(void);
/* Whether file names compare case-insensitively and with '\\' and '/'
 * equal (Windows: 1; POSIX: 0). */
ZELL_PLAT_HIDDEN int zell__plat_path_fold(void);
/* malloc'd canonical absolute form of an existing path (symlinks resolved;
 * Windows: no `\\?\` prefix), or NULL if it cannot be resolved. */
ZELL_PLAT_HIDDEN char *zell__plat_path_canon(const char *path);

/* ---- files and directories (fs_posix.c / fs_win32.c) --------------------
 * Descriptors are int64_t (POSIX fds; C runtime fds over Win32 handles on
 * Windows, so 0/1/2 are the standard streams everywhere). Paths are UTF-8
 * (UTF-16 at the Win32 boundary, where '/' and '\\' both separate).
 * Functions returning int64_t give a value >= 0 or -errno; int ones 0 or an
 * errno (POSIX errno values on every platform, so strerror and
 * zell_error_errno work unchanged). EINTR is retried internally. */

/* Open flags: the portable bits of os (os.zl). */
enum {
    ZELL_PLAT_O_RDONLY = 0, ZELL_PLAT_O_WRONLY = 1, ZELL_PLAT_O_RDWR = 2, ZELL_PLAT_O_APPEND = 8,
    ZELL_PLAT_O_CREATE = 64, ZELL_PLAT_O_EXCL = 128, ZELL_PLAT_O_SYNC = 256, ZELL_PLAT_O_TRUNC = 512,
};
enum { ZELL_PLAT_KIND_FILE = 0, ZELL_PLAT_KIND_DIR = 1, ZELL_PLAT_KIND_LINK = 2, ZELL_PLAT_KIND_OTHER = 3 };
typedef struct zell__plat_stat {
    int64_t size;
    int64_t mtime_sec;
    int32_t mtime_nsec;
    uint32_t perm;   /* permission bits (07777) */
    int32_t kind;    /* ZELL_PLAT_KIND_* */
} zell__plat_stat;

/* Opens (close-on-exec / not inherited); perm applies when creating. */
ZELL_PLAT_HIDDEN int64_t zell__plat_open(const char *path, int flags, int perm);
ZELL_PLAT_HIDDEN int64_t zell__plat_read(int64_t fd, void *buf, int64_t n);
ZELL_PLAT_HIDDEN int64_t zell__plat_pread(int64_t fd, void *buf, int64_t n, int64_t off);
/* One write (may be short). */
ZELL_PLAT_HIDDEN int64_t zell__plat_write(int64_t fd, const void *buf, int64_t n);
/* Writes everything; 0 or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_write_all(int64_t fd, const void *buf, int64_t n);
/* whence: 0 set, 1 current, 2 end. */
ZELL_PLAT_HIDDEN int64_t zell__plat_seek(int64_t fd, int64_t off, int whence);
ZELL_PLAT_HIDDEN int zell__plat_close(int64_t fd);
ZELL_PLAT_HIDDEN int zell__plat_fsync(int64_t fd);
ZELL_PLAT_HIDDEN int zell__plat_ftruncate(int64_t fd, int64_t size);
ZELL_PLAT_HIDDEN int zell__plat_fstat(int64_t fd, zell__plat_stat *st);
/* stat (follow) or lstat. */
ZELL_PLAT_HIDDEN int zell__plat_stat_path(const char *path, bool follow, zell__plat_stat *st);
ZELL_PLAT_HIDDEN bool zell__plat_isatty(int64_t fd);
ZELL_PLAT_HIDDEN int zell__plat_mkdir(const char *path);
ZELL_PLAT_HIDDEN int zell__plat_rmdir(const char *path);
ZELL_PLAT_HIDDEN int zell__plat_unlink_file(const char *path);
/* remove(3): a file or an empty directory. */
ZELL_PLAT_HIDDEN int zell__plat_remove(const char *path);
/* Replaces an existing destination (like POSIX rename). */
ZELL_PLAT_HIDDEN int zell__plat_rename(const char *from, const char *to);
ZELL_PLAT_HIDDEN int zell__plat_chmod(const char *path, int mode);
/* malloc'd current directory, NULL with errno-style *err. */
ZELL_PLAT_HIDDEN char *zell__plat_getcwd(int *err);
ZELL_PLAT_HIDDEN int zell__plat_chdir(const char *path);
/* A regular file the current user may execute (on Windows: any regular
 * file, executability is by extension). */
ZELL_PLAT_HIDDEN bool zell__plat_is_executable(const char *path);
/* Directory listing without "." and "..". dir_next returns the next name
 * (valid until the next call) and its kind, or -1 if unknown (callers
 * stat the entry); NULL at the end or on error (*err set, 0 at the end). */
typedef struct zell__plat_dir zell__plat_dir;
ZELL_PLAT_HIDDEN zell__plat_dir *zell__plat_dir_open(const char *path, int *err);
ZELL_PLAT_HIDDEN const char *zell__plat_dir_next(zell__plat_dir *d, int *kind, int *err);
ZELL_PLAT_HIDDEN void zell__plat_dir_close(zell__plat_dir *d);

/* ---- child processes (fs_posix.c / proc_win32.c) ------------------------ */

enum { ZELL_PLAT_STDIO_INHERIT = 0, ZELL_PLAT_STDIO_PIPE = 1, ZELL_PLAT_STDIO_NULL = 2 };
typedef struct zell__plat_child {
    int64_t pid;
    int64_t pidfd;   /* pollable exit notification (Linux pidfd) or -1 */
    int64_t fds[3];  /* parent ends of stdin/stdout/stderr pipes or -1: non-blocking, pollable */
} zell__plat_child;
/* Starts argv[0] (searched in PATH) with argv, envp (NULL = inherit) in
 * dir (NULL = current) with the stdio modes. 0 or an errno; *op names the
 * failing step ("pipe", "fork", "exec"). */
ZELL_PLAT_HIDDEN int zell__plat_spawn(char *const *argv, char *const *envp, const char *dir, const int modes[3],
                                        zell__plat_child *out, const char **op);
/* Non-blocking pipe I/O on the parent ends: bytes, 0 at EOF (read),
 * -ZELL_PLAT_EAGAIN, or -errno. */
ZELL_PLAT_HIDDEN int64_t zell__plat_pipe_read(int64_t fd, void *buf, int64_t n);
ZELL_PLAT_HIDDEN int64_t zell__plat_pipe_write(int64_t fd, const void *buf, int64_t n);
ZELL_PLAT_HIDDEN void zell__plat_pipe_close(int64_t fd);
/* Reaps without blocking: 1 with *status = exit code (or 1000 + signal),
 * 0 while running, or -errno. */
ZELL_PLAT_HIDDEN int zell__plat_wait_pid(int64_t pid, int64_t *status);
/* Sends sig (TerminateProcess on Windows); 0 (also for a gone process) or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_kill(int64_t pid, int sig);

/* ---- signals (fs_posix.c / win32.c) -------------------------------------
 * Signal numbers are the POSIX ones (SIGINT 2, SIGTERM 15, ...). On
 * Windows SetConsoleCtrlHandler maps Ctrl+C / Ctrl+Break to SIGINT and
 * close/logoff/shutdown to SIGTERM; other signals cannot be caught. */

/* Starts the delivery thread once: `deliver(sig)` runs on an ordinary
 * thread (not in a signal handler) for every caught signal. */
ZELL_PLAT_HIDDEN void zell__plat_sig_start(void (*deliver)(int sig));
/* True if sig can be caught (not KILL/STOP, in range). */
ZELL_PLAT_HIDDEN bool zell__plat_sig_catchable(int64_t sig);
/* Installs the catching handler / SIG_IGN / SIG_DFL for sig. */
ZELL_PLAT_HIDDEN void zell__plat_sig_catch(int sig);
ZELL_PLAT_HIDDEN void zell__plat_sig_ignore(int sig);
ZELL_PLAT_HIDDEN void zell__plat_sig_default(int sig);
/* Sends sig to the process itself. */
ZELL_PLAT_HIDDEN void zell__plat_sig_raise(int sig);

/* ---- users (fs_posix.c; Windows: the current user only) ----------------- */

typedef struct zell__plat_user {
    int64_t uid, gid;
    char name[256], full[512], home[1024];
} zell__plat_user;
/* 0 found, -1 not found, else an errno. */
ZELL_PLAT_HIDDEN int zell__plat_user_lookup(const char *name, int64_t uid, bool by_id, zell__plat_user *u);
/* group name of gid / gid of name: 0 found, -1 not found, else an errno. */
ZELL_PLAT_HIDDEN int zell__plat_group_lookup(const char *name, int64_t gid, bool by_id, int64_t *gid_out, char *gname, size_t cap);
/* Real user id (-1 where there is none: Windows). */
ZELL_PLAT_HIDDEN int64_t zell__plat_uid(void);

/* ---- process setup ------------------------------------------------------ */

/* Once per process (scheduler start): ignores SIGPIPE (a write to a peer
 * that went away reports EPIPE instead of killing the process; sockets use
 * MSG_NOSIGNAL/SO_NOSIGPIPE as well) and raises the soft open-files limit
 * to the hard one, like Go and Rust do. */
ZELL_PLAT_HIDDEN void zell__plat_process_init(void);

/* ---- sockets (net_posix.c) --------------------------------------------
 * Descriptors are int64_t. Functions returning int64_t give a value >= 0 or
 * -errno; ZELL_PLAT_EAGAIN is the (negated) "would block" code (also
 * "connect in progress"). All sockets are non-blocking and close-on-exec,
 * and never raise SIGPIPE. Errno-style codes are the host's; the
 * ZELL_PLAT_E* classes below name the ones net maps to sentinels. */

#define ZELL_PLAT_EAGAIN 100000  /* returned as -ZELL_PLAT_EAGAIN (not an errno value) */

/* Error classes of an errno-style code (zell__plat_err_class). */
enum {
    ZELL_PLAT_E_OTHER = 0, ZELL_PLAT_E_REFUSED, ZELL_PLAT_E_RESET, ZELL_PLAT_E_UNREACHABLE,
    ZELL_PLAT_E_ADDRINUSE, ZELL_PLAT_E_TIMEOUT, ZELL_PLAT_E_CLOSED, ZELL_PLAT_E_INVAL,
};
ZELL_PLAT_HIDDEN int zell__plat_err_class(int code);
/* Lower-case message for an errno-style code ("connection refused"). */
ZELL_PLAT_HIDDEN const char *zell__plat_strerror(int code, char *buf, size_t cap);

/* A socket address (IPv4, IPv6 with scope id, or Unix domain path). */
enum { ZELL_PLAT_AF_INET = 4, ZELL_PLAT_AF_INET6 = 6, ZELL_PLAT_AF_UNIX = 1 };
typedef struct zell__plat_addr {
    union { uint64_t align; unsigned char bytes[128]; } u;  /* struct sockaddr_storage */
    uint32_t len;
} zell__plat_addr;

/* An IPv4 (4 bytes) or IPv6 (16 bytes) address with port and IPv6 scope. */
ZELL_PLAT_HIDDEN bool zell__plat_addr_ip(zell__plat_addr *a, const unsigned char *ip, int iplen, int64_t port, uint32_t scope);
/* A Unix domain socket path; "@name" is the Linux abstract namespace.
 * False if the path is too long (or abstract names are unsupported). */
ZELL_PLAT_HIDDEN bool zell__plat_addr_unix(zell__plat_addr *a, const char *path);
/* ZELL_PLAT_AF_* of a, or 0. */
ZELL_PLAT_HIDDEN int zell__plat_addr_family(const zell__plat_addr *a);
/* Parts of an IP address: bytes (4 or 16; *iplen), port, scope. */
ZELL_PLAT_HIDDEN void zell__plat_addr_parts(const zell__plat_addr *a, unsigned char ip[16], int *iplen, int64_t *port, uint32_t *scope);
/* "ip:port", "[ip%zone]:port" or the Unix path ("@name" for abstract,
 * "" for unnamed). */
ZELL_PLAT_HIDDEN void zell__plat_addr_format(const zell__plat_addr *a, char *buf, size_t cap);
/* Numeric IP text (no brackets; IPv4-mapped IPv6 keeps the ::ffff: form)
 * to 4 or 16 bytes; returns the length or 0. */
ZELL_PLAT_HIDDEN int zell__plat_parse_ip(const char *s, unsigned char out[16]);
/* Formats 4 or 16 bytes; false for another length. */
ZELL_PLAT_HIDDEN bool zell__plat_format_ip(const unsigned char *ip, int len, char *buf, size_t cap);
/* Interface name <-> index (zones of link-local IPv6 addresses); 0 / false if unknown. */
ZELL_PLAT_HIDDEN uint32_t zell__plat_if_index(const char *name);
ZELL_PLAT_HIDDEN bool zell__plat_if_name(uint32_t index, char *buf, size_t cap);
/* True if s is a numeric IPv4 or IPv6 address. */
ZELL_PLAT_HIDDEN bool zell__plat_is_ip(const char *s);

enum { ZELL_PLAT_STREAM = 1, ZELL_PLAT_DGRAM = 2 };
/* A new non-blocking socket of family ZELL_PLAT_AF_* and type. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_open(int family, int type);
/* Starts a connect: 0 when connected, -ZELL_PLAT_EAGAIN when in progress
 * (wait for writability, then sock_error), -errno on failure. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_connect(int64_t fd, const zell__plat_addr *a);
/* SO_ERROR (0 or errno). */
ZELL_PLAT_HIDDEN int zell__plat_sock_error(int64_t fd);
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_bind(int64_t fd, const zell__plat_addr *a);
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_listen(int64_t fd, int backlog);
/* A non-blocking connection and its peer address (from may be NULL). */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_accept(int64_t fd, zell__plat_addr *from);
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_recv(int64_t fd, void *buf, int64_t n);  /* 0 = EOF */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_send(int64_t fd, const void *buf, int64_t n);
typedef struct zell__plat_iovec { const void *base; size_t len; } zell__plat_iovec;
/* Gathered send of up to 64 buffers. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_sendv(int64_t fd, const zell__plat_iovec *iov, int n);
/* Datagrams. *truncated is set when the datagram was longer than n (its
 * rest is discarded); from may be NULL. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_recvfrom(int64_t fd, void *buf, int64_t n, zell__plat_addr *from, bool *truncated);
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_sendto(int64_t fd, const void *buf, int64_t n, const zell__plat_addr *to);
/* shutdown(2): how = ZELL_PLAT_READ, ZELL_PLAT_WRITE or both. */
ZELL_PLAT_HIDDEN int zell__plat_sock_shutdown(int64_t fd, int how);
ZELL_PLAT_HIDDEN void zell__plat_sock_close(int64_t fd);
/* Local (peer = false) or remote address; false on error. */
ZELL_PLAT_HIDDEN bool zell__plat_sock_name(int64_t fd, bool peer, zell__plat_addr *a);

/* Socket options (sock_setopt): value is a flag (0/1) or a number. */
enum {
    ZELL_PLAT_OPT_NODELAY = 1, ZELL_PLAT_OPT_KEEPALIVE, ZELL_PLAT_OPT_KEEPIDLE, ZELL_PLAT_OPT_KEEPINTVL,
    ZELL_PLAT_OPT_KEEPCNT, ZELL_PLAT_OPT_RCVBUF, ZELL_PLAT_OPT_SNDBUF, ZELL_PLAT_OPT_LINGER,
    ZELL_PLAT_OPT_REUSEADDR, ZELL_PLAT_OPT_REUSEPORT, ZELL_PLAT_OPT_BROADCAST, ZELL_PLAT_OPT_CORK,
    ZELL_PLAT_OPT_V6ONLY, ZELL_PLAT_OPT_FASTOPEN, ZELL_PLAT_OPT_MCAST_LOOP, ZELL_PLAT_OPT_MCAST_TTL,
    ZELL_PLAT_OPT_MCAST_IF,  /* value: interface index (0 = default) */
};
/* Returns 0 or an errno (ENOPROTOOPT where the platform lacks the option).
 * LINGER: value < 0 restores the default, >= 0 lingers that many seconds
 * (0 = reset on close). */
ZELL_PLAT_HIDDEN int zell__plat_sock_setopt(int64_t fd, int opt, int64_t value);
/* Reads back RCVBUF/SNDBUF/NODELAY/KEEPALIVE...; -errno on error. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sock_getopt(int64_t fd, int opt);
/* Joins (join = true) or leaves a multicast group on interface ifindex (0 = default). */
ZELL_PLAT_HIDDEN int zell__plat_sock_mcast(int64_t fd, bool join, const unsigned char *group, int len, uint32_t ifindex);

/* Files for sendfile: a read-only descriptor, its size; close. */
ZELL_PLAT_HIDDEN int64_t zell__plat_file_open(const char *path);
ZELL_PLAT_HIDDEN int64_t zell__plat_file_size(int64_t fd);
ZELL_PLAT_HIDDEN void zell__plat_file_close(int64_t fd);
/* Sends up to n bytes of file fd from offset off to socket sock:
 * sendfile(2) on Linux and the BSDs, a pread+send loop elsewhere. Bytes
 * sent (0 at end of file) or -errno / -ZELL_PLAT_EAGAIN. */
ZELL_PLAT_HIDDEN int64_t zell__plat_sendfile(int64_t sock, int64_t fd, int64_t off, int64_t n);
/* unlink(2) of a Unix socket path (not for abstract names). */
ZELL_PLAT_HIDDEN void zell__plat_unlink(const char *path);

/* ---- name resolution (blocking; std_net.c runs these on its resolver pool) */

enum { ZELL_PLAT_RESOLVE_V4FIRST = 1, ZELL_PLAT_RESOLVE_PASSIVE = 2 };
/* getaddrinfo: on success *out is a malloc'd array of n malloc'd numeric
 * address strings (with "%zone" for scoped IPv6), in the system's
 * preference order (RFC 6724) or IPv4 first with RESOLVE_V4FIRST, without
 * duplicates; returns NULL, else an error message (static storage) and
 * *notfound tells whether the name does not exist. */
ZELL_PLAT_HIDDEN const char *zell__plat_resolve(const char *host, int flags, char ***out, int *n, bool *notfound);
/* Reverse lookup (getnameinfo) of a numeric address: one name. */
ZELL_PLAT_HIDDEN const char *zell__plat_reverse(const char *ip, char ***out, int *n, bool *notfound);
/* Port of a service name ("http") for network "tcp"/"udp"; -1 if unknown. */
ZELL_PLAT_HIDDEN int64_t zell__plat_lookup_port(const char *network, const char *service);
/* Network interfaces: malloc'd lines "name\tindex\tmtu\tflags\taddr/prefix,addr/prefix"
 * with flags a string of u (up), l (loopback), m (multicast), b (broadcast),
 * p (point-to-point); returns 0 or an errno. */
ZELL_PLAT_HIDDEN int zell__plat_interfaces(char ***out, int *n);

/* ---- trust store (certroots.c; crypto.x509) ------------------------ */

/* Path of the system's PEM bundle of trusted root certificates
 * ($SSL_CERT_FILE first, then the distribution locations), or NULL if
 * none exists (and on systems whose roots are not files: stub). */
ZELL_PLAT_HIDDEN const char *zell__plat_cert_bundle_path(void);

#endif
