/* llvm-config-shim.c - an llvm-config.exe for llvm-sys on Windows.
 *
 * The official clang+llvm-23 tarball for MSVC prints, in
 * `llvm-config --system-libs`, the absolute path of the build machine's
 * zstd_static.lib (S:/llvm/utils/release/.../zstd_static.lib), which
 * llvm-sys turns into an unlinkable library name. This program runs the
 * real llvm-config (REAL_LLVM_CONFIG, set when compiling) and drops the
 * directory part of every absolute *.lib path in its output; the libraries
 * themselves are put into the LLVM lib directory by
 * scripts/windows/llvm.ps1. Build:
 *   clang-cl /O2 -DREAL_LLVM_CONFIG="\"C:\\\\llvm\\\\bin\\\\llvm-config.exe\"" llvm-config-shim.c */
#include <fcntl.h>
#include <io.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef REAL_LLVM_CONFIG
#error "define REAL_LLVM_CONFIG as the path of the real llvm-config.exe"
#endif

int main(int argc, char **argv) {
    static char cmd[32768];
    snprintf(cmd, sizeof cmd, "\"\"%s\"", REAL_LLVM_CONFIG);
    for (int i = 1; i < argc; i++) {
        strncat(cmd, " \"", sizeof cmd - strlen(cmd) - 1);
        strncat(cmd, argv[i], sizeof cmd - strlen(cmd) - 1);
        strncat(cmd, "\"", sizeof cmd - strlen(cmd) - 1);
    }
    strncat(cmd, "\"", sizeof cmd - strlen(cmd) - 1);
    _setmode(_fileno(stdout), _O_BINARY);
    FILE *p = _popen(cmd, "r");
    if (!p) return 1;
    static char line[1 << 16], out[1 << 16];
    while (fgets(line, sizeof line, p)) {
        char *tok = line;
        out[0] = 0;
        for (;;) {
            size_t n = strcspn(tok, " \r\n");
            char word[4096];
            if (n >= sizeof word) n = sizeof word - 1;
            memcpy(word, tok, n);
            word[n] = 0;
            size_t wl = strlen(word);
            char *base = word;
            if (wl > 4 && _stricmp(word + wl - 4, ".lib") == 0) {
                char *s = strrchr(word, '/'), *b = strrchr(word, '\\');
                if (b > s) s = b;
                if (s) base = s + 1;
            }
            strncat(out, base, sizeof out - strlen(out) - 1);
            tok += n;
            if (!*tok) break;
            size_t sp = strspn(tok, " \r\n");
            strncat(out, tok, sp < sizeof out - strlen(out) - 1 ? sp : sizeof out - strlen(out) - 1);
            tok += sp;
            if (!*tok) break;
        }
        fputs(out, stdout);
    }
    return _pclose(p);
}
