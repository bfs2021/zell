/* win32.c - Windows implementation of the platform layer (platform.h):
 * threads (CreateThread), SRWLOCK / CONDITION_VARIABLE / INIT_ONCE / FLS,
 * clocks (QueryPerformanceCounter, GetSystemTimePreciseAsFileTime),
 * entropy (BCryptGenRandom), aligned memory, dynamic loading
 * (LoadLibraryW), the UTF-8
 * command line and console (WriteConsoleW), environment, signals
 * (SetConsoleCtrlHandler) and the current user. Files are in fs_win32.c,
 * processes in proc_win32.c, sockets in net_win32.c, the poller in
 * poll_iocp.c, backtraces / CPU profiling / symbols in prof_win32.c, the
 * time zone in tz_win32.c. */
#include "platform.h"

#if defined(_WIN32)

#include "win32_util.h"

#include <bcrypt.h>
#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <malloc.h>
#include <shellapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- UTF-8 <-> UTF-16, errors ---------------------------------------------- */

wchar_t *zell__w32_wide(const char *s) {
    if (!s) s = "";
    int n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, s, -1, NULL, 0);
    if (n <= 0) return NULL;
    wchar_t *w = malloc((size_t)n * sizeof *w);
    if (!w) return NULL;
    MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, s, -1, w, n);
    return w;
}

wchar_t *zell__w32_wpath(const char *s) {
    wchar_t *w = zell__w32_wide(s);
    if (w)
        for (wchar_t *p = w; *p; p++)
            if (*p == L'/') *p = L'\\';
    return w;
}

char *zell__w32_utf8(const wchar_t *w, int len) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, len, NULL, 0, NULL, NULL);
    if (n < 0) return NULL;
    char *s = malloc((size_t)n + 1);
    if (!s) return NULL;
    WideCharToMultiByte(CP_UTF8, 0, w, len, s, n, NULL, NULL);
    s[n] = 0;
    return s;
}

int zell__w32_errno(DWORD e) {
    switch (e) {
    case ERROR_SUCCESS: return 0;
    case ERROR_FILE_NOT_FOUND: case ERROR_PATH_NOT_FOUND: case ERROR_INVALID_DRIVE: case ERROR_BAD_NETPATH:
    case ERROR_BAD_NET_NAME: case ERROR_INVALID_NAME: case ERROR_MOD_NOT_FOUND:
        return ENOENT;
    case ERROR_ACCESS_DENIED: case ERROR_PRIVILEGE_NOT_HELD: case ERROR_WRITE_PROTECT: return EACCES;
    case ERROR_SHARING_VIOLATION: case ERROR_LOCK_VIOLATION: case ERROR_BUSY: return EBUSY;
    case ERROR_FILE_EXISTS: case ERROR_ALREADY_EXISTS: return EEXIST;
    case ERROR_DIR_NOT_EMPTY: return ENOTEMPTY;
    case ERROR_DIRECTORY: return ENOTDIR;
    case ERROR_NOT_ENOUGH_MEMORY: case ERROR_OUTOFMEMORY: return ENOMEM;
    case ERROR_INVALID_HANDLE: return EBADF;
    case ERROR_BROKEN_PIPE: case ERROR_NO_DATA: case ERROR_PIPE_NOT_CONNECTED: return EPIPE;
    case ERROR_DISK_FULL: case ERROR_HANDLE_DISK_FULL: return ENOSPC;
    case ERROR_INVALID_PARAMETER: case ERROR_NEGATIVE_SEEK: case ERROR_INVALID_FUNCTION: return EINVAL;
    case ERROR_NOT_SAME_DEVICE: return EXDEV;
    case ERROR_TOO_MANY_OPEN_FILES: return EMFILE;
    case ERROR_FILENAME_EXCED_RANGE: return ENAMETOOLONG;
    case ERROR_NOT_SUPPORTED: case ERROR_CALL_NOT_IMPLEMENTED: return ENOSYS;
    case ERROR_BAD_EXE_FORMAT: case ERROR_EXE_MACHINE_TYPE_MISMATCH: return ENOEXEC;
    case ERROR_WAIT_NO_CHILDREN: return ECHILD;
    default: return EIO;
    }
}

/* ---- threads ----------------------------------------------------------------- */

typedef struct trampoline { zell__plat_thread_fn fn; void *arg; } trampoline;

static DWORD WINAPI thread_entry(LPVOID p) {
    trampoline t = *(trampoline *)p;
    free(p);
    t.fn(t.arg);
    return 0;
}

/* mem.c: refcounts switch to atomic RMW for good before a second thread runs. */
extern uint8_t zell_threaded;

static HANDLE start_ex(zell__plat_thread_fn fn, void *arg, int threaded) {
    trampoline *t = malloc(sizeof *t);
    if (!t) return NULL;
    if (threaded) __atomic_store_n(&zell_threaded, 1, __ATOMIC_SEQ_CST);
    t->fn = fn;
    t->arg = arg;
    HANDLE h = CreateThread(NULL, 0, thread_entry, t, 0, NULL);
    if (!h) free(t);
    return h;
}

static HANDLE start(zell__plat_thread_fn fn, void *arg) { return start_ex(fn, arg, 1); }

bool zell__plat_thread_start(zell__plat_thread *t, zell__plat_thread_fn fn, void *arg) {
    HANDLE h = start(fn, arg);
    if (!h) return false;
    t->handle = (uintptr_t)h;
    return true;
}

bool zell__plat_thread_start_detached(zell__plat_thread_fn fn, void *arg) {
    HANDLE h = start(fn, arg);
    if (!h) return false;
    CloseHandle(h);
    return true;
}

bool zell__plat_thread_start_background(zell__plat_thread_fn fn, void *arg) {
    HANDLE h = start_ex(fn, arg, 0);
    if (!h) return false;
    CloseHandle(h);
    return true;
}

void zell__plat_thread_join(zell__plat_thread t) {
    HANDLE h = (HANDLE)t.handle;
    WaitForSingleObject(h, INFINITE);
    CloseHandle(h);
}

int64_t zell__plat_cpu_count(void) {
    DWORD n = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS);
    if (n == 0) {
        SYSTEM_INFO si;
        GetSystemInfo(&si);
        n = si.dwNumberOfProcessors;
    }
    return n > 0 ? (int64_t)n : 1;
}

void zell__plat_yield(void) { SwitchToThread(); }

/* ---- locks ------------------------------------------------------------------- */

_Static_assert(sizeof(SRWLOCK) == sizeof(zell__plat_mutex), "SRWLOCK layout");
_Static_assert(sizeof(CONDITION_VARIABLE) == sizeof(zell__plat_cond), "CONDITION_VARIABLE layout");
_Static_assert(sizeof(INIT_ONCE) == sizeof(zell__plat_once), "INIT_ONCE layout");

void zell__plat_mutex_init(zell__plat_mutex *m) { InitializeSRWLock((PSRWLOCK)m); }
void zell__plat_mutex_destroy(zell__plat_mutex *m) { (void)m; }
void zell__plat_mutex_lock(zell__plat_mutex *m) { AcquireSRWLockExclusive((PSRWLOCK)m); }
void zell__plat_mutex_unlock(zell__plat_mutex *m) { ReleaseSRWLockExclusive((PSRWLOCK)m); }
void zell__plat_cond_init(zell__plat_cond *c) { InitializeConditionVariable((PCONDITION_VARIABLE)c); }
void zell__plat_cond_destroy(zell__plat_cond *c) { (void)c; }
void zell__plat_cond_wait(zell__plat_cond *c, zell__plat_mutex *m) {
    SleepConditionVariableSRW((PCONDITION_VARIABLE)c, (PSRWLOCK)m, INFINITE, 0);
}
bool zell__plat_cond_wait_ns(zell__plat_cond *c, zell__plat_mutex *m, int64_t ns) {
    int64_t ms = ns <= 0 ? 0 : (ns + 999999) / 1000000;
    if (ms >= (int64_t)INFINITE) ms = (int64_t)INFINITE - 1;
    if (SleepConditionVariableSRW((PCONDITION_VARIABLE)c, (PSRWLOCK)m, (DWORD)ms, 0)) return true;
    return GetLastError() != ERROR_TIMEOUT;
}
void zell__plat_cond_signal(zell__plat_cond *c) { WakeConditionVariable((PCONDITION_VARIABLE)c); }
void zell__plat_cond_broadcast(zell__plat_cond *c) { WakeAllConditionVariable((PCONDITION_VARIABLE)c); }

typedef void (*once_fn)(void);
static BOOL CALLBACK once_cb(PINIT_ONCE o, PVOID param, PVOID *ctx) {
    (void)o;
    (void)ctx;
    once_fn fn;
    memcpy(&fn, &param, sizeof fn);
    fn();
    return TRUE;
}
void zell__plat_once_run(zell__plat_once *o, void (*fn)(void)) {
    PVOID param;
    memcpy(&param, &fn, sizeof param);
    InitOnceExecuteOnce((PINIT_ONCE)o, once_cb, param, NULL);
}

/* Thread-local keys with destructors: FLS callbacks. Like pthreads, the
 * destructors do not run for the main thread when the process exits (it
 * may hold locks the destructor needs while other threads are gone), so
 * each key gets its own callback that checks the thread. */
#define TLS_MAX 8
static void (*g_tls_dtor[TLS_MAX])(void *);
static DWORD g_main_thread;
static volatile LONG g_tls_n;
#define TLS_THUNK(i) \
    static VOID WINAPI tls_thunk##i(PVOID v) { \
        if (v && g_tls_dtor[i] && GetCurrentThreadId() != g_main_thread) g_tls_dtor[i](v); \
    }
TLS_THUNK(0) TLS_THUNK(1) TLS_THUNK(2) TLS_THUNK(3) TLS_THUNK(4) TLS_THUNK(5) TLS_THUNK(6) TLS_THUNK(7)
static PFLS_CALLBACK_FUNCTION const g_tls_thunks[TLS_MAX] = {
    tls_thunk0, tls_thunk1, tls_thunk2, tls_thunk3, tls_thunk4, tls_thunk5, tls_thunk6, tls_thunk7,
};

bool zell__plat_tls_new(zell__plat_tls_key *k, void (*dtor)(void *)) {
    LONG i = InterlockedIncrement(&g_tls_n) - 1;
    if (i >= TLS_MAX) return false;
    g_tls_dtor[i] = dtor;
    DWORD idx = FlsAlloc(dtor ? g_tls_thunks[i] : NULL);
    if (idx == FLS_OUT_OF_INDEXES) return false;
    k->idx = idx;
    return true;
}

void zell__plat_tls_set(zell__plat_tls_key k, void *v) { FlsSetValue(k.idx, v); }

/* ---- memory ------------------------------------------------------------------ */

void *zell__plat_aligned_alloc(size_t align, size_t size) { return _aligned_malloc(size, align); }
void zell__plat_aligned_free(void *p) { _aligned_free(p); }
size_t zell__plat_alloc_size(void *p) { return _msize(p); }

size_t zell__plat_page_size(void) { return 4096; }

void *zell__plat_mem_map(size_t len, size_t align) {
    if (align <= 65536) return VirtualAlloc(NULL, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    /* Reserve an over-sized range to find an aligned address, release it
     * and map exactly there (another thread may take it: retry). */
    for (int i = 0; i < 16; i++) {
        char *r = VirtualAlloc(NULL, len + align, MEM_RESERVE, PAGE_NOACCESS);
        if (!r) return NULL;
        char *a = (char *)(((uintptr_t)r + align - 1) & ~(uintptr_t)(align - 1));
        VirtualFree(r, 0, MEM_RELEASE);
        void *p = VirtualAlloc(a, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if (p) return p;
    }
    return NULL;
}

void zell__plat_mem_unmap(void *p, size_t len) {
    (void)len;
    if (p) VirtualFree(p, 0, MEM_RELEASE);
}

/* Large pages need SeLockMemoryPrivilege and are never transparent. */
void zell__plat_mem_hint_huge(void *p, size_t len) {
    (void)p;
    (void)len;
}

/* DiscardVirtualMemory (Windows 8.1+) drops the pages' contents and their
 * physical memory but keeps them committed, so the range needs no
 * re-commit before reuse (MEM_DECOMMIT would). Older systems: MEM_RESET. */
typedef DWORD(WINAPI *discard_fn)(PVOID, SIZE_T);
void zell__plat_mem_release(void *p, size_t len) {
    static discard_fn fn;
    static int looked;
    if (!__atomic_load_n(&looked, __ATOMIC_ACQUIRE)) {
        HMODULE k = GetModuleHandleW(L"kernel32.dll");
        discard_fn f = k ? (discard_fn)(void (*)(void))GetProcAddress(k, "DiscardVirtualMemory") : NULL;
        __atomic_store_n(&fn, f, __ATOMIC_RELAXED);
        __atomic_store_n(&looked, 1, __ATOMIC_RELEASE);
    }
    discard_fn f = __atomic_load_n(&fn, __ATOMIC_RELAXED);
    if (f && f(p, len) == 0) return;
    VirtualAlloc(p, len, MEM_RESET, PAGE_READWRITE);
}

void *zell__plat_mem_remap(void *p, size_t old_len, size_t new_len) {
    (void)p;
    (void)old_len;
    (void)new_len;
    return NULL;
}

void zell__plat_malloc_trim(void) { _heapmin(); }

/* ---- time and entropy ----------------------------------------------------------- */

int64_t zell__plat_mono_ns(void) {
    static LARGE_INTEGER freq;
    if (!freq.QuadPart) QueryPerformanceFrequency(&freq);
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    int64_t f = freq.QuadPart;
    return c.QuadPart / f * 1000000000 + c.QuadPart % f * 1000000000 / f;
}

int64_t zell__plat_wall_ns(void) {
    FILETIME ft;
    GetSystemTimePreciseAsFileTime(&ft);
    int64_t t = (int64_t)(((uint64_t)ft.dwHighDateTime << 32) | ft.dwLowDateTime);
    return (t - 116444736000000000LL) * 100;  /* 100 ns ticks since 1601 */
}

void zell__plat_sleep_ns(int64_t ns) {
    if (ns <= 0) return;
    int64_t ms = (ns + 999999) / 1000000;
    while (ms > 0) {
        DWORD d = ms > 0x7fffffff ? 0x7fffffff : (DWORD)ms;
        Sleep(d);
        ms -= d;
    }
}

const char *zell__plat_localtime_path(void) { return NULL; }
const char *zell__plat_zoneinfo_dir(void) { return NULL; }

bool zell__plat_random_bytes(void *buf, size_t n) {
    unsigned char *p = buf;
    while (n > 0) {
        ULONG k = n > 0x10000000 ? 0x10000000 : (ULONG)n;
        if (BCryptGenRandom(NULL, p, k, BCRYPT_USE_SYSTEM_PREFERRED_RNG) != 0) return false;
        p += k;
        n -= k;
    }
    return true;
}

/* Diagnostics (backtraces, CPU sampler, symbols) are in prof_win32.c. */

/* ---- dynamic loading ------------------------------------------------------------ */

static _Thread_local char t_dl_err[256];

static void dl_set_error(void) {
    DWORD e = GetLastError();
    DWORD n = FormatMessageA(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, NULL, e, 0, t_dl_err,
                             sizeof t_dl_err, NULL);
    while (n > 0 && (t_dl_err[n - 1] == '\n' || t_dl_err[n - 1] == '\r' || t_dl_err[n - 1] == '.')) t_dl_err[--n] = 0;
    if (n == 0) snprintf(t_dl_err, sizeof t_dl_err, "error %lu", (unsigned long)e);
}

void *zell__plat_dl_open(const char *path) {
    wchar_t *w = zell__w32_wpath(path);
    if (!w) {
        snprintf(t_dl_err, sizeof t_dl_err, "invalid path");
        return NULL;
    }
    HMODULE h = LoadLibraryW(w);
    free(w);
    if (!h) dl_set_error();
    return h;
}

void *zell__plat_dl_sym(void *h, const char *name) {
    if (!h) return NULL;
    FARPROC p = GetProcAddress((HMODULE)h, name);
    if (!p) {
        dl_set_error();
        return NULL;
    }
    void *r;
    memcpy(&r, &p, sizeof r);
    return r;
}

void zell__plat_dl_close(void *h) {
    if (h) FreeLibrary((HMODULE)h);
}

const char *zell__plat_dl_error(void) { return t_dl_err[0] ? t_dl_err : "unknown error"; }

/* ---- command line and console --------------------------------------------------- */

static bool g_console[3];
static unsigned char g_carry[3][5];

/* Before main (and before anything can panic): binary standard streams
 * and console setup; the UTF-8 argv comes with zell_runtime_init. */
__attribute__((constructor)) static void console_init(void) {
    g_main_thread = GetCurrentThreadId();
    /* println writes \n: no CRLF translation in the C runtime. */
    _setmode(0, _O_BINARY);
    _setmode(1, _O_BINARY);
    _setmode(2, _O_BINARY);
    static const DWORD ids[3] = {STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE};
    for (int i = 1; i < 3; i++) {
        HANDLE h = GetStdHandle(ids[i]);
        DWORD mode;
        if (h && h != INVALID_HANDLE_VALUE && GetConsoleMode(h, &mode)) {
            g_console[i] = true;
            SetConsoleMode(h, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
    if (g_console[1] || g_console[2]) SetConsoleOutputCP(CP_UTF8);
}

/* ---- fatal exceptions (ABI.md 8.3) ----------------------------------------
 * A vectored handler reports faults raised in the executable's own code
 * (Zell code and the static runtime) through zell__fatal_signal with the
 * POSIX signal name; a fault elsewhere (a system DLL may probe memory under
 * its own __try) is left to SEH and reaches the unhandled-exception filter
 * only if nobody handles it. */

static uintptr_t g_exe_lo, g_exe_hi;

static bool fatal_kind(const EXCEPTION_RECORD *er, const char **name, const char **what, uintptr_t *addr) {
    *addr = (uintptr_t)er->ExceptionAddress;
    switch (er->ExceptionCode) {
    case EXCEPTION_ACCESS_VIOLATION:
        *name = "SIGSEGV";
        *what = "access violation";
        if (er->NumberParameters >= 2) *addr = (uintptr_t)er->ExceptionInformation[1];
        return true;
    case EXCEPTION_STACK_OVERFLOW: *name = "SIGSEGV"; *what = "stack overflow"; return true;
    case EXCEPTION_IN_PAGE_ERROR:
        *name = "SIGBUS";
        *what = "in-page error";
        if (er->NumberParameters >= 2) *addr = (uintptr_t)er->ExceptionInformation[1];
        return true;
    case EXCEPTION_DATATYPE_MISALIGNMENT: *name = "SIGBUS"; *what = "misaligned access"; return true;
    case EXCEPTION_ILLEGAL_INSTRUCTION:
    case EXCEPTION_PRIV_INSTRUCTION: *name = "SIGILL"; *what = "illegal instruction"; return true;
    case EXCEPTION_INT_DIVIDE_BY_ZERO: *name = "SIGFPE"; *what = "integer divide by zero"; return true;
    default: return false;
    }
}

static LONG fatal_report(EXCEPTION_POINTERS *ep) {
    const char *name, *what;
    uintptr_t addr;
    if (!fatal_kind(ep->ExceptionRecord, &name, &what, &addr)) return EXCEPTION_CONTINUE_SEARCH;
    uintptr_t pcs[64];
    CONTEXT ctx = *ep->ContextRecord;
    ULONG_PTR lo = 0, hi = 0;
    GetCurrentThreadStackLimits(&lo, &hi);
    int n = zell__w32_walk(&ctx, lo, hi, pcs, 64);
    /* the faulting instruction itself: + 1 so the lookup of pc - 1 hits it */
    if (n > 0) pcs[0] += 1;
    zell__fatal_signal(name, what, addr, pcs, n);
}

static LONG WINAPI fatal_veh(EXCEPTION_POINTERS *ep) {
    uintptr_t pc = (uintptr_t)ep->ExceptionRecord->ExceptionAddress;
    if (ep->ExceptionRecord->ExceptionCode != EXCEPTION_STACK_OVERFLOW && (pc < g_exe_lo || pc >= g_exe_hi))
        return EXCEPTION_CONTINUE_SEARCH;
    return fatal_report(ep);
}

static LONG WINAPI fatal_unhandled(EXCEPTION_POINTERS *ep) { return fatal_report(ep); }

void zell__w32_fatal_thread_init(void) {
    static _Thread_local bool done;
    if (done) return;
    done = true;
    /* room for the handler (backtrace, symbols) after an overflow */
    ULONG size = 64 * 1024;
    SetThreadStackGuarantee(&size);
}

static void fatal_init(void) {
    if (getenv("ZELL_NO_FATAL_HANDLER")) return;
    HMODULE m = GetModuleHandleW(NULL);
    const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)m;
    const IMAGE_NT_HEADERS *nt = (const IMAGE_NT_HEADERS *)((const char *)m + dos->e_lfanew);
    g_exe_lo = (uintptr_t)m;
    g_exe_hi = g_exe_lo + nt->OptionalHeader.SizeOfImage;
    zell__w32_fatal_thread_init();
    AddVectoredExceptionHandler(1, fatal_veh);
    SetUnhandledExceptionFilter(fatal_unhandled);
}

void zell__plat_runtime_init(int *argc, char ***argv) {
    fatal_init();
    /* The UTF-8 command line (the C runtime's argv is in the ANSI code page). */
    int n = 0;
    LPWSTR *w = CommandLineToArgvW(GetCommandLineW(), &n);
    if (!w || !argc || !argv) return;
    char **v = calloc((size_t)n + 1, sizeof *v);
    if (!v) { LocalFree(w); return; }
    for (int i = 0; i < n; i++) {
        v[i] = zell__w32_utf8(w[i], -1);
        if (!v[i]) v[i] = "";
    }
    LocalFree(w);
    *argc = n;
    *argv = v;  /* lives for the whole process */
}

void zell__w32_console_write(HANDLE h, unsigned char carry[5], const char *p, size_t n) {
    /* Prepend an incomplete sequence left by the previous call. */
    while (n > 0) {
        char buf[4096 + 4];
        size_t k = carry[4];
        memcpy(buf, carry, k);
        size_t take = n > 4096 ? 4096 : n;
        memcpy(buf + k, p, take);
        p += take;
        n -= take;
        size_t len = k + take;
        /* Keep back a trailing incomplete UTF-8 sequence. */
        size_t cut = len, back = 0;
        while (back < 3 && back < len && ((unsigned char)buf[len - 1 - back] & 0xC0) == 0x80) back++;
        if (back < len) {
            unsigned char lead = (unsigned char)buf[len - 1 - back];
            size_t need = lead >= 0xF0 ? 4 : lead >= 0xE0 ? 3 : lead >= 0xC0 ? 2 : 1;
            if (need > back + 1) cut = len - 1 - back;
        }
        carry[4] = (unsigned char)(len - cut);
        memcpy(carry, buf + cut, len - cut);
        if (cut == 0) continue;
        wchar_t wbuf[4096 + 4];
        int wn = MultiByteToWideChar(CP_UTF8, 0, buf, (int)cut, wbuf, (int)(sizeof wbuf / sizeof wbuf[0]));
        DWORD done = 0;
        int off = 0;
        while (off < wn) {
            if (!WriteConsoleW(h, wbuf + off, (DWORD)(wn - off), &done, NULL) || done == 0) break;
            off += (int)done;
        }
    }
}

bool zell__w32_is_console_fd(int64_t fd) { return (fd == 1 || fd == 2) && g_console[fd]; }

void zell__w32_console_fd_write(int64_t fd, const char *p, size_t n) {
    zell__w32_console_write(GetStdHandle(fd == 1 ? STD_OUTPUT_HANDLE : STD_ERROR_HANDLE), g_carry[fd], p, n);
}

void zell__plat_stream_write(FILE *f, const char *p, size_t n) {
    if (!n) return;
    int fd = _fileno(f);
    if (fd == 1 || fd == 2) {
        if (g_console[fd]) {
            fflush(f);
            zell__w32_console_fd_write(fd, p, n);
            return;
        }
    }
    fwrite(p, 1, n, f);
}

/* ---- environment and identity ---------------------------------------------------- */

int64_t zell__plat_pid(void) { return (int64_t)GetCurrentProcessId(); }

char *zell__plat_getenv(const char *name) {
    wchar_t *wn = zell__w32_wide(name);
    if (!wn) return NULL;
    DWORD n = GetEnvironmentVariableW(wn, NULL, 0);
    if (n == 0) {
        free(wn);
        return NULL;
    }
    wchar_t *v = malloc((size_t)n * sizeof *v);
    char *r = NULL;
    if (v) {
        DWORD k = GetEnvironmentVariableW(wn, v, n);
        if (k < n) r = zell__w32_utf8(v, (int)k);
        free(v);
    }
    free(wn);
    return r;
}

int zell__plat_setenv(const char *name, const char *value) {
    size_t nl = strlen(name), vl = value ? strlen(value) : 0;
    char *kv = malloc(nl + vl + 2);
    if (!kv) return ENOMEM;
    memcpy(kv, name, nl);
    kv[nl] = '=';
    memcpy(kv + nl + 1, value ? value : "", vl + 1);
    wchar_t *w = zell__w32_wide(kv);
    free(kv);
    if (!w) return EINVAL;
    /* _wputenv updates the C runtime's copy and the process environment;
     * "NAME=" removes the variable. An empty value needs the Win32 call. */
    int rc = _wputenv(w);
    free(w);
    if (rc != 0) return errno ? errno : EINVAL;
    if (value && !*value) {
        wchar_t *wn = zell__w32_wide(name);
        if (wn) {
            SetEnvironmentVariableW(wn, L"");
            free(wn);
        }
    }
    return 0;
}

char **zell__plat_environ(void) {
    LPWCH block = GetEnvironmentStringsW();
    if (!block) return calloc(1, sizeof(char *));
    size_t n = 0;
    for (LPWCH p = block; *p; p += wcslen(p) + 1) n++;
    char **v = calloc(n + 1, sizeof *v);
    size_t k = 0;
    for (LPWCH p = block; *p && v; p += wcslen(p) + 1) {
        if (*p == L'=') continue;  /* hidden per-drive directories ("=C:=C:\\x") */
        v[k] = zell__w32_utf8(p, -1);
        if (v[k]) k++;
    }
    FreeEnvironmentStringsW(block);
    return v;
}

void zell__plat_free_strv(char **v) {
    if (!v) return;
    for (char **p = v; *p; p++) free(*p);
    free(v);
}

int zell__plat_hostname(char *buf, size_t cap) {
    wchar_t w[256];
    DWORD n = 256;
    if (!GetComputerNameExW(ComputerNameDnsHostname, w, &n)) return zell__w32_errno(GetLastError());
    char *u = zell__w32_utf8(w, (int)n);
    if (!u) return ENOMEM;
    snprintf(buf, cap, "%s", u);
    free(u);
    return 0;
}

char *zell__plat_temp_dir(void) {
    wchar_t w[MAX_PATH + 2];
    DWORD n = GetTempPathW(MAX_PATH + 1, w);
    if (n == 0 || n > MAX_PATH + 1) return _strdup("C:\\Windows\\Temp");
    if (n > 3 && w[n - 1] == L'\\') w[--n] = 0;  /* like Go: no trailing separator */
    return zell__w32_utf8(w, (int)n);
}

char zell__plat_path_sep(void) { return '\\'; }

int zell__plat_path_fold(void) { return 1; }

/* GetFinalPathNameByHandleW resolves symlinks and junctions; its `\\?\`
 * (or `\\?\UNC\`) prefix is dropped, as compiler/src/paths.rs does. */
char *zell__plat_path_canon(const char *path) {
    if (!path || !*path) return NULL;
    wchar_t *w = zell__w32_wpath(path);
    if (!w) return NULL;
    HANDLE h = CreateFileW(w, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING,
                           FILE_FLAG_BACKUP_SEMANTICS, NULL);
    free(w);
    if (h == INVALID_HANDLE_VALUE) return NULL;
    wchar_t buf[4096];
    DWORD n = GetFinalPathNameByHandleW(h, buf, (DWORD)(sizeof buf / sizeof buf[0]), FILE_NAME_NORMALIZED);
    CloseHandle(h);
    if (n == 0 || n >= sizeof buf / sizeof buf[0]) return NULL;
    const wchar_t *p = buf;
    wchar_t unc[4096];
    if (wcsncmp(p, L"\\\\?\\UNC\\", 8) == 0) {
        unc[0] = L'\\';
        unc[1] = L'\\';
        wcscpy(unc + 2, p + 8);
        p = unc;
    } else if (wcsncmp(p, L"\\\\?\\", 4) == 0) {
        p += 4;
    }
    return zell__w32_utf8(p, -1);
}

/* ---- signals: console control events ----------------------------------------------- */

#define NSIG_MAX 65
static void (*g_deliver)(int);
static volatile LONG g_sig_state[NSIG_MAX];  /* 0 default, 1 caught, 2 ignored */
static zell__plat_once g_sig_once = ZELL_PLAT_ONCE_INIT;

static BOOL WINAPI ctrl_handler(DWORD ev) {
    int sig = (ev == CTRL_C_EVENT || ev == CTRL_BREAK_EVENT) ? 2 : 15;
    LONG st = g_sig_state[sig];
    if (st == 1 && g_deliver) {
        g_deliver(sig);  /* the handler runs on its own thread */
        return TRUE;
    }
    return st == 2;
}

static void sig_init(void) { SetConsoleCtrlHandler(ctrl_handler, TRUE); }

void zell__plat_sig_start(void (*deliver)(int sig)) {
    g_deliver = deliver;
    zell__plat_once_run(&g_sig_once, sig_init);
}

bool zell__plat_sig_catchable(int64_t sig) { return sig > 0 && sig < NSIG_MAX && sig != 9 && sig != 19; }
void zell__plat_sig_catch(int sig) { InterlockedExchange(&g_sig_state[sig], 1); }
void zell__plat_sig_ignore(int sig) { InterlockedExchange(&g_sig_state[sig], 2); }
void zell__plat_sig_default(int sig) { InterlockedExchange(&g_sig_state[sig], 0); }

void zell__plat_sig_raise(int sig) {
    if (sig <= 0 || sig >= NSIG_MAX) return;
    LONG st = g_sig_state[sig];
    if (st == 1 && g_deliver) { g_deliver(sig); return; }
    if (st == 2) return;
    fflush(stdout);
    fflush(stderr);
    TerminateProcess(GetCurrentProcess(), 128u + (unsigned)sig);
}

/* ---- users: the current one ----------------------------------------------------------- */

int64_t zell__plat_uid(void) { return -1; }

int zell__plat_user_lookup(const char *name, int64_t uid, bool by_id, zell__plat_user *u) {
    wchar_t w[257];
    DWORD n = 257;
    if (!GetUserNameW(w, &n)) return zell__w32_errno(GetLastError());
    char *me = zell__w32_utf8(w, -1);
    if (!me) return ENOMEM;
    bool match = by_id ? uid == -1 : _stricmp(name, me) == 0;
    if (!match) {
        free(me);
        return -1;
    }
    u->uid = -1;
    u->gid = -1;
    snprintf(u->name, sizeof u->name, "%s", me);
    snprintf(u->full, sizeof u->full, "%s", me);
    free(me);
    char *home = zell__plat_getenv("USERPROFILE");
    snprintf(u->home, sizeof u->home, "%s", home ? home : "");
    free(home);
    return 0;
}

int zell__plat_group_lookup(const char *name, int64_t gid, bool by_id, int64_t *gid_out, char *gname, size_t cap) {
    (void)name;
    (void)gid;
    (void)by_id;
    (void)gid_out;
    (void)gname;
    (void)cap;
    return -1;  /* no POSIX groups on Windows */
}

#else
/* POSIX: posix.c, fs_posix.c. */
typedef int zell__win32_unused;
#endif
