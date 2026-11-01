/* posix.c - POSIX implementation of the platform layer (Linux, macOS, BSD):
 * threads, CPU count, clocks, sleeping, time formatting, entropy, aligned
 * memory, console and environment, backtraces and the CPU profiler's
 * SIGPROF sampler and symbolizer. Files, processes, signals and users are
 * in fs_posix.c. The
 * readiness poller is in poll_epoll.c / poll_kqueue.c. */
#include "platform.h"

#if defined(__unix__) || defined(__APPLE__)

#include <errno.h>
#if defined(__has_include)
#if __has_include(<execinfo.h>)
#include <execinfo.h>
#define ZELL_HAVE_EXECINFO 1
#endif
#endif
#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <sched.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/mman.h>
#if defined(__APPLE__)
#include <malloc/malloc.h>
#include <sys/random.h> /* getentropy */
#elif defined(__FreeBSD__) || defined(__DragonFly__)
#include <malloc_np.h>
#else
#include <malloc.h>
#endif
#if defined(__linux__)
#include <sys/random.h>
#endif

_Static_assert(sizeof(pthread_t) <= sizeof(uintptr_t), "pthread_t must fit in uintptr_t");

/* mem.c: refcounts switch to atomic RMW for good before a second thread runs. */
extern uint8_t zell_threaded;

bool zell__plat_thread_start(zell__plat_thread *t, zell__plat_thread_fn fn, void *arg) {
    pthread_t th;
    __atomic_store_n(&zell_threaded, 1, __ATOMIC_SEQ_CST);
    if (pthread_create(&th, NULL, fn, arg) != 0) return false;
    t->handle = (uintptr_t)th;
    return true;
}

static bool start_detached(zell__plat_thread_fn fn, void *arg, int threaded) {
    pthread_t th;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    if (threaded) __atomic_store_n(&zell_threaded, 1, __ATOMIC_SEQ_CST);
    int rc = pthread_create(&th, &attr, fn, arg);
    pthread_attr_destroy(&attr);
    return rc == 0;
}

bool zell__plat_thread_start_detached(zell__plat_thread_fn fn, void *arg) { return start_detached(fn, arg, 1); }

bool zell__plat_thread_start_background(zell__plat_thread_fn fn, void *arg) { return start_detached(fn, arg, 0); }

void zell__plat_thread_join(zell__plat_thread t) {
    pthread_join((pthread_t)t.handle, NULL);
}

int64_t zell__plat_cpu_count(void) {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    return n > 0 ? (int64_t)n : 1;
}

void zell__plat_yield(void) { sched_yield(); }

int64_t zell__plat_mono_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

int64_t zell__plat_wall_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

void zell__plat_sleep_ns(int64_t ns) {
    if (ns <= 0) return;
    struct timespec ts = { (time_t)(ns / 1000000000), (long)(ns % 1000000000) };
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {}
}

#if defined(ZELL_PLAT_LIBC_TIMEFMT)
size_t zell__plat_strftime(char *buf, size_t cap, const char *fmt, const struct tm *tm) {
    return strftime(buf, cap, fmt, tm);
}

const char *zell__plat_strptime(const char *s, const char *fmt, struct tm *tm, long *gmtoff) {
    const char *end = strptime(s, fmt, tm);
    *gmtoff = end ? (long)tm->tm_gmtoff : 0;  /* set by %z (glibc, BSD), 0 otherwise */
    return end;
}
#endif /* else timefmt.c (musl: no %k/%l in strftime, no %z in strptime) */

const char *zell__plat_localtime_path(void) { return "/etc/localtime"; }
const char *zell__plat_zoneinfo_dir(void) { return "/usr/share/zoneinfo"; }
bool zell__plat_tz_tzif(const char *name, char **out, size_t *len) {
    (void)name;
    (void)out;
    (void)len;
    return false;  /* tzdata files: std_time_zone.c and time.zl read them */
}

/* ---- memory ------------------------------------------------------------ */

void *zell__plat_aligned_alloc(size_t align, size_t size) { return aligned_alloc(align, size); }
void zell__plat_aligned_free(void *p) { free(p); }

size_t zell__plat_alloc_size(void *p) {
#if defined(__APPLE__)
    return malloc_size(p);
#else
    return malloc_usable_size(p);
#endif
}

size_t zell__plat_page_size(void) {
    static size_t ps;
    size_t v = __atomic_load_n(&ps, __ATOMIC_RELAXED);
    if (!v) {
        long n = sysconf(_SC_PAGESIZE);
        v = n > 0 ? (size_t)n : 4096;
        __atomic_store_n(&ps, v, __ATOMIC_RELAXED);
    }
    return v;
}

void *zell__plat_mem_map(size_t len, size_t align) {
    size_t ps = zell__plat_page_size();
    if (align < ps) align = ps;
    size_t over = align > ps ? len + align : len;
    if (over < len) return NULL;
    char *p = mmap(NULL, over, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return NULL;
    if (over == len) return p;
    /* Trim the over-allocation to an aligned range. */
    char *a = (char *)(((uintptr_t)p + align - 1) & ~(uintptr_t)(align - 1));
    if (a > p) munmap(p, (size_t)(a - p));
    size_t tail = (size_t)((p + over) - (a + len));
    if (tail) munmap(a + len, tail);
    return a;
}

void zell__plat_mem_hint_huge(void *p, size_t len) {
#if defined(__linux__) && defined(MADV_HUGEPAGE)
    madvise(p, len, MADV_HUGEPAGE);
#else
    (void)p;
    (void)len;
#endif
}

void zell__plat_mem_unmap(void *p, size_t len) {
    if (p) munmap(p, len);
}

void zell__plat_mem_release(void *p, size_t len) {
#if defined(__APPLE__) && defined(MADV_FREE)
    madvise(p, len, MADV_FREE);
#else
    madvise(p, len, MADV_DONTNEED);
#endif
}

void *zell__plat_mem_remap(void *p, size_t old_len, size_t new_len) {
#if defined(__linux__) && defined(MREMAP_MAYMOVE)
    void *q = mremap(p, old_len, new_len, MREMAP_MAYMOVE);
    return q == MAP_FAILED ? NULL : q;
#else
    (void)p;
    (void)old_len;
    (void)new_len;
    return NULL;
#endif
}

void zell__plat_malloc_trim(void) {
#if defined(__GLIBC__)
    malloc_trim(0);
#endif
}

/* ---- process, console, environment -------------------------------------- */

static void fatal_init(void);

void zell__plat_runtime_init(int *argc, char ***argv) {
    (void)argc;
    (void)argv;
    fatal_init();
}

void zell__plat_stream_write(FILE *f, const char *p, size_t n) {
    if (n) fwrite(p, 1, n, f);
}

int64_t zell__plat_pid(void) { return (int64_t)getpid(); }

char *zell__plat_getenv(const char *name) {
    const char *v = getenv(name);
    return v ? strdup(v) : NULL;
}

int zell__plat_setenv(const char *name, const char *value) {
    int rc = value ? setenv(name, value, 1) : unsetenv(name);
    return rc == 0 ? 0 : errno;
}

extern char **environ;

char **zell__plat_environ(void) {
    size_t n = 0;
    for (char **e = environ; *e; e++) n++;
    char **v = calloc(n + 1, sizeof *v);
    if (!v) return NULL;
    for (size_t i = 0; i < n; i++) v[i] = strdup(environ[i]);
    return v;
}

void zell__plat_free_strv(char **v) {
    if (!v) return;
    for (char **p = v; *p; p++) free(*p);
    free(v);
}

int zell__plat_hostname(char *buf, size_t cap) {
    if (gethostname(buf, cap) != 0) return errno;
    buf[cap - 1] = 0;
    return 0;
}

char *zell__plat_temp_dir(void) {
    const char *t = getenv("TMPDIR");
    return strdup(t && *t ? t : "/tmp");
}

char zell__plat_path_sep(void) { return '/'; }

int zell__plat_path_fold(void) { return 0; }

char *zell__plat_path_canon(const char *path) {
    return path && *path ? realpath(path, NULL) : NULL;
}

bool zell__plat_random_bytes(void *buf, size_t n) {
    unsigned char *p = buf;
    size_t got = 0;
#if defined(__linux__)
    while (got < n) {
        ssize_t r = getrandom(p + got, n - got, 0);
        if (r < 0) {
            if (errno == EINTR) continue;
            break;
        }
        got += (size_t)r;
    }
    if (got == n) return true;
#elif defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__)
    while (got < n) {
        size_t k = n - got > 256 ? 256 : n - got;
        if (getentropy(p + got, k) != 0) break;
        got += k;
    }
    if (got == n) return true;
#endif
    /* Fallback: /dev/urandom. */
    int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return false;
    while (got < n) {
        ssize_t r = read(fd, p + got, n - got);
        if (r <= 0) {
            if (r < 0 && errno == EINTR) continue;
            close(fd);
            return false;
        }
        got += (size_t)r;
    }
    close(fd);
    return true;
}

/* One frame in the format of glibc's backtrace_symbols_fd,
 * "module(function+0xoff)[0xaddr]", named from the executable's own symbol
 * table (local symbols too, unlike dladdr) where possible. */
static bool bt_symbolize(uintptr_t pc, char *buf, size_t cap, uintptr_t *start);
static int bt_format(uintptr_t pc, char *line, size_t cap);
static void bt_frame(int fd, uintptr_t pc) {
    char line[700];
    int k = bt_format(pc, line, sizeof line);
    if (k > 0) zell__plat_write_all(fd, line, k < (int)sizeof line ? k : (int)sizeof line - 1);
}

/* Appends one frame line to buf (len/cap), truncating silently. */
static void bt_frame_buf(char *buf, size_t cap, size_t *len, uintptr_t pc) {
    char line[700];
    int k = bt_format(pc, line, sizeof line);
    if (k <= 0) return;
    if (k >= (int)sizeof line) k = (int)sizeof line - 1;
    if (*len + (size_t)k >= cap) return;
    memcpy(buf + *len, line, (size_t)k);
    *len += (size_t)k;
    buf[*len] = 0;
}

static bool bt_lines(uintptr_t pc, const char **file, uint32_t *ln, const char **func);
static int bt_format(uintptr_t pc, char *line, size_t cap) {
    char sym[256];
    uintptr_t start = 0;
    const char *file, *func;
    uint32_t ln;
    /* Zell code: "  file:line function" from the line table (ABI.md 8.2) */
    if (bt_lines(pc - 1, &file, &ln, &func)) {
        if (!func) func = bt_symbolize(pc - 1, sym, sizeof sym, &start) ? sym : "?";
        return snprintf(line, cap, "  %s:%u %s\n", zell__lines_shown(file), (unsigned)ln, func);
    }
    const char *mod = "";
    Dl_info di;
    if (dladdr((void *)pc, &di) && di.dli_fname) mod = di.dli_fname;
    int k;
    /* a return address points after the call: look up pc - 1 */
    bool named = !getenv("ZELL_NO_SYMBOLS") && bt_symbolize(pc - 1, sym, sizeof sym, &start) && start <= pc;
    /* The runtime's panic entry points are not the program's frames. */
    if (named && (strncmp(sym, "zell_panic", 10) == 0 || strncmp(sym, "zell__panic", 11) == 0)) return 0;
    if (named)
        k = snprintf(line, cap, "%s(%s+0x%lx)[0x%lx]\n", mod, sym, (unsigned long)(pc - start), (unsigned long)pc);
    else if (di.dli_fbase && *mod)
        k = snprintf(line, cap, "%s(+0x%lx)[0x%lx]\n", mod, (unsigned long)(pc - (uintptr_t)di.dli_fbase), (unsigned long)pc);
    else
        k = snprintf(line, cap, "[0x%lx]\n", (unsigned long)pc);
    return k;
}

#if defined(ZELL_HAVE_EXECINFO)
void zell__plat_backtrace(int fd, int skip) {
    void *frames[64];
    int n = backtrace(frames, 64);
    for (int i = skip; i < n; i++) bt_frame(fd, (uintptr_t)frames[i]);
}

void zell__plat_backtrace_buf(char *buf, size_t cap, int skip) {
    void *frames[64];
    size_t len = 0;
    if (cap) buf[0] = 0;
    int n = backtrace(frames, 64);
    for (int i = skip; i < n; i++) bt_frame_buf(buf, cap, &len, (uintptr_t)frames[i]);
}
#else
/* No <execinfo.h> (musl): walk the frame-pointer chain (the runtime and
 * Zell code keep frame pointers). */
void zell__plat_backtrace(int fd, int skip) {
    uintptr_t fp = (uintptr_t)__builtin_frame_address(0);
    for (int i = 0; i < 64 && fp && !(fp & (sizeof(void *) - 1)); i++) {
        uintptr_t next = ((uintptr_t *)fp)[0], ret = ((uintptr_t *)fp)[1];
        if (!ret) break;
        if (i >= skip) bt_frame(fd, ret);
        if (next <= fp) break;  /* the chain must go up the stack */
        fp = next;
    }
}

void zell__plat_backtrace_buf(char *buf, size_t cap, int skip) {
    size_t len = 0;
    if (cap) buf[0] = 0;
    uintptr_t fp = (uintptr_t)__builtin_frame_address(0);
    for (int i = 0; i < 64 && fp && !(fp & (sizeof(void *) - 1)); i++) {
        uintptr_t next = ((uintptr_t *)fp)[0], ret = ((uintptr_t *)fp)[1];
        if (!ret) break;
        if (i >= skip) bt_frame_buf(buf, cap, &len, ret);
        if (next <= fp) break;
        fp = next;
    }
}
#endif

void zell__plat_backtrace_pcs(int fd, const uintptr_t *pcs, int n) {
    for (int i = 0; i < n; i++) bt_frame(fd, pcs[i]);
}

/* ---- CPU profiling: SIGPROF sampler, frame-pointer walk, symbols ------- */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <dlfcn.h>
#if defined(__linux__)
#include <elf.h>
#include <link.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <ucontext.h>
#elif defined(__APPLE__)
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach-o/nlist.h>
#include <sys/ucontext.h>
#endif

#define PROF_MAX_FRAMES 64

static _Thread_local uintptr_t t_stack_lo, t_stack_hi;
static zell__plat_prof_fn g_prof_fn;
static struct sigaction g_prof_old;
static int g_prof_on;

static void fatal_altstack(void);

void zell__plat_thread_stack_init(void) {
    fatal_altstack();
    if (t_stack_hi) return;
#if defined(__linux__)
    pthread_attr_t a;
    if (pthread_getattr_np(pthread_self(), &a) != 0) return;
    void *addr = NULL;
    size_t size = 0;
    if (pthread_attr_getstack(&a, &addr, &size) == 0 && addr) {
        t_stack_lo = (uintptr_t)addr;
        t_stack_hi = (uintptr_t)addr + size;
    }
    pthread_attr_destroy(&a);
#elif defined(__APPLE__)
    pthread_t self = pthread_self();
    uintptr_t hi = (uintptr_t)pthread_get_stackaddr_np(self);
    t_stack_hi = hi;
    t_stack_lo = hi - pthread_get_stacksize_np(self);
#endif
}

/* PC, frame pointer and stack pointer of an interrupted context. */
static bool ctx_regs(void *uc_, uintptr_t *pc, uintptr_t *fp, uintptr_t *sp) {
#if defined(__linux__) && defined(__x86_64__)
    ucontext_t *uc = uc_;
    *pc = (uintptr_t)uc->uc_mcontext.gregs[REG_RIP];
    *fp = (uintptr_t)uc->uc_mcontext.gregs[REG_RBP];
    *sp = (uintptr_t)uc->uc_mcontext.gregs[REG_RSP];
    return true;
#elif defined(__linux__) && defined(__aarch64__)
    ucontext_t *uc = uc_;
    *pc = (uintptr_t)uc->uc_mcontext.pc;
    *fp = (uintptr_t)uc->uc_mcontext.regs[29];
    *sp = (uintptr_t)uc->uc_mcontext.sp;
    return true;
#elif defined(__APPLE__) && defined(__x86_64__)
    ucontext_t *uc = uc_;
    *pc = (uintptr_t)uc->uc_mcontext->__ss.__rip;
    *fp = (uintptr_t)uc->uc_mcontext->__ss.__rbp;
    *sp = (uintptr_t)uc->uc_mcontext->__ss.__rsp;
    return true;
#elif defined(__APPLE__) && defined(__aarch64__)
    ucontext_t *uc = uc_;
    *pc = (uintptr_t)uc->uc_mcontext->__ss.__pc;
    *fp = (uintptr_t)uc->uc_mcontext->__ss.__fp;
    *sp = (uintptr_t)uc->uc_mcontext->__ss.__sp;
    return true;
#else
    (void)uc_; (void)pc; (void)fp; (void)sp;
    return false;
#endif
}

static void prof_handler(int sig, siginfo_t *info, void *uc) {
    (void)sig;
    (void)info;
    int saved = errno;
    uintptr_t pcs[PROF_MAX_FRAMES];
    int n = 0;
    uintptr_t pc, fp, sp;
    if (ctx_regs(uc, &pc, &fp, &sp)) {
        pcs[n++] = pc;
        uintptr_t lo = t_stack_lo, hi = t_stack_hi;
        /* Walk {saved fp, return address} pairs while they stay inside this
         * thread's stack, above the interrupted SP and strictly increasing. */
        if (hi && sp >= lo && sp < hi) {
            uintptr_t prev = sp;
            while (n < PROF_MAX_FRAMES && fp >= prev && fp + 2 * sizeof(uintptr_t) <= hi &&
                   (fp & (sizeof(uintptr_t) - 1)) == 0) {
                const uintptr_t *f = (const uintptr_t *)fp;
                uintptr_t ret = f[1];
                if (ret < 4096) break;
                pcs[n++] = ret;
                prev = fp + 1;
                fp = f[0];
            }
        }
    }
    zell__plat_prof_fn fn = __atomic_load_n(&g_prof_fn, __ATOMIC_ACQUIRE);
    if (fn && n > 0) fn(pcs, n);
    errno = saved;
}

bool zell__plat_prof_start(int hz, zell__plat_prof_fn fn) {
    if (g_prof_on || hz <= 0) return false;
    zell__plat_thread_stack_init();
    __atomic_store_n(&g_prof_fn, fn, __ATOMIC_RELEASE);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = prof_handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGPROF, &sa, &g_prof_old) != 0) return false;
    struct itimerval it;
    memset(&it, 0, sizeof it);
    long us = 1000000L / hz;
    if (us < 1) us = 1;
    it.it_interval.tv_sec = us / 1000000L;
    it.it_interval.tv_usec = us % 1000000L;
    it.it_value = it.it_interval;
    if (setitimer(ITIMER_PROF, &it, NULL) != 0) {
        sigaction(SIGPROF, &g_prof_old, NULL);
        return false;
    }
    g_prof_on = 1;
    return true;
}

void zell__plat_prof_stop(void) {
    if (!g_prof_on) return;
    struct itimerval it;
    memset(&it, 0, sizeof it);
    setitimer(ITIMER_PROF, &it, NULL);
    /* A signal already in flight still finds a handler; it sees fn == NULL
     * once the store below is visible. Keep our handler installed (ignoring
     * SIGPROF afterwards would be equivalent). */
    __atomic_store_n(&g_prof_fn, (zell__plat_prof_fn)NULL, __ATOMIC_RELEASE);
    g_prof_on = 0;
}

/* -- symbols of the main executable (ELF .symtab on Linux, LC_SYMTAB on macOS) -- */

typedef struct { uintptr_t addr, size; const char *name; } sym_ent;
static sym_ent *g_syms;
static size_t g_nsyms;
static char *g_symstr;        /* copy of the string table */
static int g_syms_loaded;
static uintptr_t g_exe_lo, g_exe_hi, g_exe_bias;
static char g_exe_path[512];

#if defined(__linux__)
static int phdr_cb(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    (void)data;
    /* The first object is the main program. */
    g_exe_bias = (uintptr_t)info->dlpi_addr;
    for (int i = 0; i < info->dlpi_phnum; i++) {
        const ElfW(Phdr) *ph = &info->dlpi_phdr[i];
        if (ph->p_type == PT_LOAD && (ph->p_flags & PF_X)) {
            uintptr_t lo = g_exe_bias + ph->p_vaddr, hi = lo + ph->p_memsz;
            if (!g_exe_lo || lo < g_exe_lo) g_exe_lo = lo;
            if (hi > g_exe_hi) g_exe_hi = hi;
        }
    }
    return 1;
}

static int sym_cmp(const void *a, const void *b) {
    const sym_ent *x = a, *y = b;
    return x->addr < y->addr ? -1 : x->addr > y->addr;
}

static void load_syms(void) {
    g_syms_loaded = 1;
    dl_iterate_phdr(phdr_cb, NULL);
    ssize_t pl = readlink("/proc/self/exe", g_exe_path, sizeof g_exe_path - 1);
    if (pl <= 0) return;
    g_exe_path[pl] = 0;
    int fd = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return;
    struct stat st;
    if (fstat(fd, &st) != 0 || st.st_size < (off_t)sizeof(ElfW(Ehdr))) {
        close(fd);
        return;
    }
    size_t len = (size_t)st.st_size;
    unsigned char *m = mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (m == MAP_FAILED) return;
    const ElfW(Ehdr) *eh = (const ElfW(Ehdr) *)m;
    if (memcmp(eh->e_ident, ELFMAG, SELFMAG) != 0 || eh->e_shoff == 0 ||
        eh->e_shoff + (size_t)eh->e_shnum * sizeof(ElfW(Shdr)) > len) {
        munmap(m, len);
        return;
    }
    const ElfW(Shdr) *sh = (const ElfW(Shdr) *)(m + eh->e_shoff);
    int pick = -1;
    for (int pass = 0; pass < 2 && pick < 0; pass++)
        for (int i = 0; i < eh->e_shnum; i++)
            if (sh[i].sh_type == (pass == 0 ? SHT_SYMTAB : SHT_DYNSYM)) { pick = i; break; }
    if (pick < 0 || sh[pick].sh_link >= eh->e_shnum) {
        munmap(m, len);
        return;
    }
    const ElfW(Shdr) *ss = &sh[pick], *sstr = &sh[ss->sh_link];
    if (ss->sh_offset + ss->sh_size > len || sstr->sh_offset + sstr->sh_size > len || ss->sh_entsize == 0) {
        munmap(m, len);
        return;
    }
    size_t n = ss->sh_size / ss->sh_entsize;
    g_symstr = malloc(sstr->sh_size + 1);
    g_syms = malloc((n ? n : 1) * sizeof *g_syms);
    if (!g_symstr || !g_syms) {
        free(g_symstr);
        free(g_syms);
        g_symstr = NULL;
        g_syms = NULL;
        munmap(m, len);
        return;
    }
    memcpy(g_symstr, m + sstr->sh_offset, sstr->sh_size);
    g_symstr[sstr->sh_size] = 0;
    for (size_t i = 0; i < n; i++) {
        const ElfW(Sym) *s = (const ElfW(Sym) *)(m + ss->sh_offset + i * ss->sh_entsize);
        if (ELF64_ST_TYPE(s->st_info) != STT_FUNC || s->st_value == 0 || s->st_name >= sstr->sh_size) continue;
        g_syms[g_nsyms].addr = g_exe_bias + s->st_value;
        g_syms[g_nsyms].size = s->st_size;
        g_syms[g_nsyms].name = g_symstr + s->st_name;
        g_nsyms++;
    }
    munmap(m, len);
    qsort(g_syms, g_nsyms, sizeof *g_syms, sym_cmp);
}
#elif defined(__APPLE__) && defined(__LP64__)
/* Mach-O: the main image's LC_SYMTAB, read from its __LINKEDIT segment,
 * which dyld keeps mapped (no file access). */
static int sym_cmp(const void *a, const void *b) {
    const sym_ent *x = a, *y = b;
    return x->addr < y->addr ? -1 : x->addr > y->addr;
}

static void load_syms(void) {
    g_syms_loaded = 1;
    const struct mach_header_64 *mh = (const struct mach_header_64 *)_dyld_get_image_header(0);
    if (!mh || mh->magic != MH_MAGIC_64) return;
    g_exe_bias = (uintptr_t)_dyld_get_image_vmaddr_slide(0);
    uint32_t pcap = (uint32_t)sizeof g_exe_path;
    if (_NSGetExecutablePath(g_exe_path, &pcap) != 0) g_exe_path[0] = 0;
    const struct segment_command_64 *linkedit = NULL;
    const struct symtab_command *symtab = NULL;
    const unsigned char *lc = (const unsigned char *)(mh + 1);
    for (uint32_t i = 0; i < mh->ncmds; i++) {
        const struct load_command *c = (const struct load_command *)lc;
        if (c->cmd == LC_SEGMENT_64) {
            const struct segment_command_64 *seg = (const struct segment_command_64 *)lc;
            if (strncmp(seg->segname, "__TEXT", 16) == 0) {
                g_exe_lo = g_exe_bias + (uintptr_t)seg->vmaddr;
                g_exe_hi = g_exe_lo + (uintptr_t)seg->vmsize;
            } else if (strncmp(seg->segname, "__LINKEDIT", 16) == 0) {
                linkedit = seg;
            }
        } else if (c->cmd == LC_SYMTAB) {
            symtab = (const struct symtab_command *)lc;
        }
        lc += c->cmdsize;
    }
    if (!linkedit || !symtab || !symtab->nsyms) return;
    uintptr_t base = g_exe_bias + (uintptr_t)linkedit->vmaddr - (uintptr_t)linkedit->fileoff;
    const struct nlist_64 *nl = (const struct nlist_64 *)(base + symtab->symoff);
    const char *str = (const char *)(base + symtab->stroff);
    g_syms = malloc(symtab->nsyms * sizeof *g_syms);
    if (!g_syms) return;
    for (uint32_t i = 0; i < symtab->nsyms; i++) {
        if ((nl[i].n_type & N_STAB) || (nl[i].n_type & N_TYPE) != N_SECT || !nl[i].n_value) continue;
        if (nl[i].n_un.n_strx >= symtab->strsize) continue;
        const char *name = str + nl[i].n_un.n_strx;
        if (name[0] == '_') name++;  /* C names carry a leading underscore */
        g_syms[g_nsyms].addr = g_exe_bias + (uintptr_t)nl[i].n_value;
        g_syms[g_nsyms].size = 0;
        g_syms[g_nsyms].name = name;  /* points into the mapped __LINKEDIT */
        g_nsyms++;
    }
    qsort(g_syms, g_nsyms, sizeof *g_syms, sym_cmp);
    /* Mach-O symbols have no size: up to the next symbol (or end of text) */
    for (size_t i = 0; i < g_nsyms; i++) {
        uintptr_t end = i + 1 < g_nsyms ? g_syms[i + 1].addr : g_exe_hi;
        g_syms[i].size = end > g_syms[i].addr ? end - g_syms[i].addr : 1;
    }
}
#else
static void load_syms(void) {
    g_syms_loaded = 1;
    (void)g_exe_bias;
}
#endif

bool zell__plat_exe_range(uintptr_t *lo, uintptr_t *hi, char *path, size_t cap) {
    if (!g_syms_loaded) load_syms();
    if (!g_exe_hi) return false;
    *lo = g_exe_lo;
    *hi = g_exe_hi;
    if (cap) snprintf(path, cap, "%s", g_exe_path);
    return true;
}

bool zell__plat_symbolize(uintptr_t pc, char *buf, size_t cap, uintptr_t *start) {
    if (!g_syms_loaded) load_syms();
    if (g_nsyms && pc >= g_syms[0].addr) {
        size_t lo = 0, hi = g_nsyms;
        while (hi - lo > 1) {
            size_t mid = (lo + hi) / 2;
            if (g_syms[mid].addr <= pc) lo = mid;
            else hi = mid;
        }
        const sym_ent *e = &g_syms[lo];
        if (pc < e->addr + (e->size ? e->size : 1 << 20)) {
            snprintf(buf, cap, "%s", zell__plat_sym_clean(e->name));
            *start = e->addr;
            return true;
        }
    }
    Dl_info di;
    if (dladdr((void *)pc, &di) && di.dli_sname) {
        snprintf(buf, cap, "%s", zell__plat_sym_clean(di.dli_sname));
        *start = (uintptr_t)di.dli_saddr;
        return true;
    }
    return false;
}

static bool bt_symbolize(uintptr_t pc, char *buf, size_t cap, uintptr_t *start) { return zell__plat_symbolize(pc, buf, cap, start); }

static bool bt_lines(uintptr_t pc, const char **file, uint32_t *ln, const char **func) { return zell__lines_find(pc, file, ln, func); }

void zell__plat_symbolize_done(void) {
    free(g_syms);
    free(g_symstr);
    g_syms = NULL;
    g_symstr = NULL;
    g_nsyms = 0;
    g_syms_loaded = 0;
    g_exe_lo = g_exe_hi = 0;
}

/* ---- fatal signals (ABI.md 8.3) ------------------------------------------
 * SIGSEGV/SIGBUS/SIGILL/SIGFPE run fatal_handler on an alternate signal
 * stack (a stack overflow leaves none), which walks the interrupted
 * context's frame pointers and reports through zell__fatal_signal. Each
 * thread that runs Zell code gets its own alternate stack: the main thread
 * here, workers from zell__plat_thread_stack_init; it is unmapped when the
 * thread exits. */

#define FATAL_ALTSTACK_SIZE (64 * 1024)
static const int g_fatal_sigs[] = {SIGSEGV, SIGBUS, SIGILL, SIGFPE};
static pthread_key_t g_altstack_key;
static pthread_once_t g_altstack_once = PTHREAD_ONCE_INIT;
static _Thread_local bool t_altstack;

static void altstack_free(void *p) {
    stack_t ss;
    memset(&ss, 0, sizeof ss);
    ss.ss_flags = SS_DISABLE;
    sigaltstack(&ss, NULL);
    munmap(p, FATAL_ALTSTACK_SIZE);
}

static void altstack_key_init(void) {
    if (pthread_key_create(&g_altstack_key, altstack_free) != 0) g_altstack_key = (pthread_key_t)-1;
}

static void fatal_altstack(void) {
    if (t_altstack) return;
    t_altstack = true;
    stack_t old;
    /* Keep an alternate stack somebody else installed (sanitizers). */
    if (sigaltstack(NULL, &old) != 0 || !(old.ss_flags & SS_DISABLE)) return;
    pthread_once(&g_altstack_once, altstack_key_init);
    if (g_altstack_key == (pthread_key_t)-1) return;
    void *p = mmap(NULL, FATAL_ALTSTACK_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return;
    stack_t ss;
    memset(&ss, 0, sizeof ss);
    ss.ss_sp = p;
    ss.ss_size = FATAL_ALTSTACK_SIZE;
    if (sigaltstack(&ss, NULL) != 0) {
        munmap(p, FATAL_ALTSTACK_SIZE);
        return;
    }
    pthread_setspecific(g_altstack_key, p);
}

static const char *fatal_what(int sig, int code) {
    switch (sig) {
    case SIGSEGV: return code == SEGV_ACCERR ? "invalid permissions for mapped object" : "segmentation violation";
    case SIGBUS: return "bus error";
    case SIGILL: return "illegal instruction";
    case SIGFPE: return code == FPE_INTDIV ? "integer divide by zero" : "floating-point exception";
    default: return "fault";
    }
}

static void fatal_handler(int sig, siginfo_t *info, void *uc) {
    const char *name = sig == SIGSEGV ? "SIGSEGV" : sig == SIGBUS ? "SIGBUS" : sig == SIGILL ? "SIGILL" : "SIGFPE";
    uintptr_t pcs[PROF_MAX_FRAMES];
    int n = 0;
    uintptr_t pc, fp, sp, addr = info ? (uintptr_t)info->si_addr : 0;
    const char *what = fatal_what(sig, info ? info->si_code : 0);
    if (ctx_regs(uc, &pc, &fp, &sp)) {
        /* the faulting instruction itself: + 1 so the lookup of pc - 1 hits it */
        pcs[n++] = pc + 1;
        zell__plat_thread_stack_init();
        uintptr_t lo = t_stack_lo, hi = t_stack_hi;
        /* A fault just below the stack (the guard pages) is an overflow;
         * the interrupted SP may then be below the recorded bounds too. */
        if (sig == SIGSEGV && hi && addr < lo + 4096 && addr + (1u << 20) >= lo) what = "stack overflow";
        if (hi && sp < hi && sp + (1u << 20) >= lo) {
            uintptr_t prev = sp;
            while (n < PROF_MAX_FRAMES && fp >= prev && fp >= lo && fp + 2 * sizeof(uintptr_t) <= hi &&
                   (fp & (sizeof(uintptr_t) - 1)) == 0) {
                const uintptr_t *f = (const uintptr_t *)fp;
                uintptr_t ret = f[1];
                if (ret < 4096) break;
                pcs[n++] = ret;
                prev = fp + 1;
                fp = f[0];
            }
        }
    }
    zell__fatal_signal(name, what, addr, pcs, n);
}

static void fatal_init(void) {
    if (getenv("ZELL_NO_FATAL_HANDLER")) return;
    fatal_altstack();
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = fatal_handler;
    /* SA_RESETHAND: a fault inside the handler gets the default action */
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK | SA_RESETHAND;
    sigemptyset(&sa.sa_mask);
    for (size_t i = 0; i < sizeof g_fatal_sigs / sizeof g_fatal_sigs[0]; i++) sigaction(g_fatal_sigs[i], &sa, NULL);
}

/* ---- dynamic loading (plugin) ---- */

void *zell__plat_dl_open(const char *path) { return dlopen(path, RTLD_NOW | RTLD_LOCAL); }

void *zell__plat_dl_sym(void *h, const char *name) { return h ? dlsym(h, name) : NULL; }

void zell__plat_dl_close(void *h) {
    if (h) dlclose(h);
}

const char *zell__plat_dl_error(void) {
    const char *e = dlerror();
    return e ? e : "unknown error";
}

#endif /* POSIX; Windows: win32.c */
