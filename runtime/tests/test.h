/* Minimal assert-based test helpers. */
#ifndef ZELL_TEST_H
#define ZELL_TEST_H
#undef NDEBUG
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#if !defined(_WIN32)
#include <sys/wait.h>
#else
#include <windows.h>
#include <direct.h>
#include <io.h>
/* POSIX helpers some tests use, on top of the C runtime. */
static inline int pipe(int fds[2]) { return _pipe(fds, 65536, _O_BINARY); }
static inline int setenv(const char *k, const char *v, int overwrite) {
    (void)overwrite;
    return _putenv_s(k, v);
}
static inline int unsetenv(const char *k) { return _putenv_s(k, ""); }
/* mingw-w64 15 declares mkdtemp in <stdlib.h>, older ones do not: a
 * helper of our own name works with both. */
static inline char *zell__test_mkdtemp(char *tmpl) {
    if (!_mktemp(tmpl) || _mkdir(tmpl) != 0) return NULL;
    return tmpl;
}
#define mkdtemp zell__test_mkdtemp
static inline int symlink(const char *target, const char *path) {
    return CreateSymbolicLinkA(path, target, 0x2 /* ALLOW_UNPRIVILEGED_CREATE */) ? 0 : -1;
}
#endif
#include "zell_rt.h"
#include <pthread.h>  /* Windows (test-wine): mingw's winpthreads */
/* Threads the tests start themselves touch refcounts too: switch the
 * runtime to atomic refcounting first (zell_threaded, ABI.md 3.2). */
#define pthread_create(...) (__atomic_store_n(&zell_threaded, 1, __ATOMIC_SEQ_CST), pthread_create(__VA_ARGS__))

#define S(lit) zell_string_new((lit), (int64_t)strlen(lit))
#define STR_IS(s, lit) (zell_string_len(s) == (int64_t)strlen(lit) && memcmp(zell_string_data(s), (lit), strlen(lit)) == 0)

#define TEST(name) static void name(void)
/* Flushed, so a test that crashes shows which steps passed (stdout is a pipe under make). */
#define RUN(name) do { name(); printf("  ok  %s\n", #name); fflush(stdout); } while (0)

#if !defined(_WIN32)
/* Run fn in a child process; capture its fd (1 or 2); return exit status. */
static inline int run_child_fd(void (*fn)(void), int fd, char *errbuf, size_t cap) {
    int fds[2];
    assert(pipe(fds) == 0);
    fflush(stdout);
    pid_t pid = fork();
    assert(pid >= 0);
    if (pid == 0) {
        close(fds[0]);
        dup2(fds[1], fd);
        if (fd != 2) { int nul = open("/dev/null", O_WRONLY); dup2(nul, 2); }
        fn();
        _exit(0);
    }
    close(fds[1]);
    size_t len = 0;
    ssize_t n;
    while ((n = read(fds[0], errbuf + len, cap - 1 - len)) > 0) len += (size_t)n;
    errbuf[len] = 0;
    close(fds[0]);
    int st;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}

#else
/* Windows (make test-wine): no fork, so the test executable is started
 * again with ZELL_TEST_CHILD = fn's offset in the image; a constructor
 * runs fn in that child before main and exits 0. The child starts fresh
 * (no state inherited from the parent beyond the environment). */
static inline int run_child_fd(void (*fn)(void), int fd, char *errbuf, size_t cap) {
    char exe[MAX_PATH], env[32];
    GetModuleFileNameA(NULL, exe, MAX_PATH);
    snprintf(env, sizeof env, "%llx", (unsigned long long)((uintptr_t)fn - (uintptr_t)GetModuleHandleA(NULL)));
    SetEnvironmentVariableA("ZELL_TEST_CHILD", env);
    SECURITY_ATTRIBUTES sa = { sizeof sa, NULL, TRUE };
    HANDLE rd, wr;
    assert(CreatePipe(&rd, &wr, &sa, 0));
    SetHandleInformation(rd, HANDLE_FLAG_INHERIT, 0);
    HANDLE nul = CreateFileA("NUL", GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, &sa, OPEN_EXISTING, 0, NULL);
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof si);
    si.cb = sizeof si;
    si.dwFlags = STARTF_USESTDHANDLES;
    HANDLE in = NULL;  /* the child reads the parent's fd 0, like after fork */
    DuplicateHandle(GetCurrentProcess(), (HANDLE)_get_osfhandle(0), GetCurrentProcess(), &in, 0, TRUE, DUPLICATE_SAME_ACCESS);
    si.hStdInput = in ? in : nul;
    si.hStdOutput = fd == 1 ? wr : nul;
    si.hStdError = fd == 2 ? wr : nul;
    fflush(stdout);
    assert(CreateProcessA(exe, NULL, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi));
    CloseHandle(wr);
    CloseHandle(nul);
    if (in) CloseHandle(in);
    size_t len = 0;
    DWORD n;
    while (len + 1 < cap && ReadFile(rd, errbuf + len, (DWORD)(cap - 1 - len), &n, NULL) && n > 0) len += n;
    errbuf[len] = 0;
    CloseHandle(rd);
    WaitForSingleObject(pi.hProcess, INFINITE);
    DWORD code = 0;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    SetEnvironmentVariableA("ZELL_TEST_CHILD", NULL);
    return (int)code;
}

__attribute__((constructor)) static void zell_test_child_hook(void) {
    char v[32];
    if (!GetEnvironmentVariableA("ZELL_TEST_CHILD", v, sizeof v)) return;
    uintptr_t fp = (uintptr_t)GetModuleHandleA(NULL) + (uintptr_t)strtoull(v, NULL, 16);
    void (*fn)(void);
    memcpy(&fn, &fp, sizeof fn);
    fn();
    fflush(stdout);
    fflush(stderr);
    _exit(0);
}
#endif

static inline int run_child(void (*fn)(void), char *errbuf, size_t cap) {
    return run_child_fd(fn, 2, errbuf, cap);
}

#endif
