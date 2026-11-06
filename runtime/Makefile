# Zell runtime library (v0.1)
#   make        -> build/libzell_rt.a (also copied to ./libzell_rt.a)
#   make test   -> build and run tests/*.c
#   make clean
#   make LTO=1 CC=clang -> build/lto/libzell_rt.a of ThinLTO bitcode objects, used by
#               `ZELL_LTO=1 zell build --release` (docs/PLATFORMS.md, «LTO»)
#   make asan   -> clean rebuild + tests under AddressSanitizer
#   make tsan   -> build/tsan/: the poller/network/scheduler tests under ThreadSanitizer
#   CFLAGS_EXTRA=... is appended to CFLAGS (and to the coro test link, done with $(CC)),
#   e.g. make clean && make CFLAGS_EXTRA=-fsanitize=address test
#   make CROSS=x86_64-w64-mingw32 -> build/win64/libzell_rt.a (Windows, mingw ABI;
#               also copied to build/x86_64-pc-windows-gnu/ where `zell build
#               --target windows-amd64` looks), compiled with clang -target
#               $(CROSS) (or $(CROSS)-gcc without clang) against the mingw-w64 headers
#   make CROSS=x86_64-w64-mingw32 test-wine -> the C unit tests built for Windows
#               and run under wine (WINE=wine); prints which pass
#   make CROSS=aarch64-apple-darwin (or x86_64-apple-darwin) -> macOS: with an SDK
#               (SDKROOT, or xcrun on a Mac) build/<triple>/libzell_rt.a; without
#               one, a compile check of every file against stub-sdk/macos/
#               (objects in build/<triple>-stubcheck/, never linked)
#   make CROSS=aarch64-linux-gnu / x86_64-linux-musl / aarch64-linux-musl -> Linux
#               cross runtimes in build/<triple>/ (gcc-aarch64-linux-gnu sysroot,
#               musl-gcc's headers); test-qemu runs the C tests under qemu-<arch>
#   SYSROOT=dir -> cross builds compile against that sysroot (--sysroot; for musl
#               set MUSL_INC instead): scripts/bundle-targets.sh builds the bundled
#               runtimes against glibc 2.31 / mingw-w64 trees it unpacked
#   make CROSS=aarch64-apple-darwin STUBSDK=yes -> a macOS runtime archive compiled
#               against stub-sdk/macos/ (build/<triple>-stubsdk/libzell_rt.a, what the
#               toolchain bundle ships for cross-linking macOS without an SDK)

CC      ?= cc
AR      ?= ar
CFLAGS  ?= -O2
CROSS   ?=
# clang of the compiler's LLVM first (apt.llvm.org installs only clang-23), then clang.
CLANG   ?= $(shell command -v clang-23 >/dev/null 2>&1 && echo clang-23 || echo clang)
SYSROOT ?=
STUBSDK ?=
HAVE_CLANG := $(shell command -v $(CLANG) >/dev/null 2>&1 && echo yes)
# Native Windows host (MSYS2, Git Bash, Cygwin): recipes see MSYS' POSIX-style temp
# paths (/tmp) or none, which the native compilers cannot use ("unable to make
# temporary file"), so export a Windows-style one (cygpath -m: forward slashes, a
# path every native tool accepts). Non-Windows hosts are untouched.
ifeq ($(findstring NT,$(shell uname -s)),NT)
WIN_TEMP := $(shell cygpath -m "$${TEMP:-/tmp}" 2>/dev/null)
ifeq ($(WIN_TEMP),)
WIN_TEMP := $(TEMP)
endif
ifeq ($(WIN_TEMP),)
WIN_TEMP := C:/Windows/Temp
endif
export TEMP := $(WIN_TEMP)
export TMP := $(WIN_TEMP)
export TMPDIR := $(WIN_TEMP)
endif

ifneq ($(CROSS),)
# Cross build. clang is preferred: one compiler for every target, and the
# same TLS model (native, not emutls) as the code LLVM generates for Zell.
ifeq ($(HAVE_CLANG),yes)
CC      := $(CLANG) -target $(CROSS)
# The archiver next to the clang in use first (Homebrew's llvm@18 is keg-only:
# its llvm-ar is not on PATH), then llvm-ar-18 / llvm-ar on PATH, then the
# host's own ar on macOS (Apple ar handles Mach-O), else a binutils cross ar.
AR      := $(shell for a in "$(dir $(CLANG))llvm-ar" llvm-ar-18 llvm-ar; do command -v "$$a" >/dev/null 2>&1 && { echo "$$a"; exit 0; }; done; \
             if [ "$$(uname -s)" = Darwin ] && [ -n "$(findstring apple-darwin,$(CROSS))" ]; then echo ar; else echo $(CROSS)-ar; fi)
else
CC      := $(CROSS)-gcc
AR      := $(CROSS)-ar
endif
ifneq ($(findstring mingw,$(CROSS)),)
TARGET_OS := windows
BUILD   := build/win64
TRIPLE  := x86_64-pc-windows-gnu
EXE     := .exe
PICFLAG :=
OSFLAGS := -D_WIN32_WINNT=0x0A00 $(if $(SYSROOT),--sysroot=$(SYSROOT))
LDLIBS  = -lws2_32 -liphlpapi -lbcrypt -lshell32 -ladvapi32 -lm
RDYNAMIC :=
else ifneq ($(findstring apple-darwin,$(CROSS)),)
TARGET_OS := macos
TRIPLE  := $(CROSS)
DARWIN_ARCH := $(if $(findstring aarch64,$(CROSS)),arm64,x86_64)
# deployment target; 12.3 is the first with sqlite3_changes64 in the SDK
MACOS_MIN ?= 12.3
SDKROOT ?= $(shell xcrun --sdk macosx --show-sdk-path 2>/dev/null)
ifeq ($(HAVE_CLANG),yes)
CC      := $(CLANG) -target $(DARWIN_ARCH)-apple-macos$(MACOS_MIN)
endif
ifeq ($(STUBSDK),yes)
# A runtime archive from the stub headers (the toolchain bundle's macOS runtime).
BUILD   := build/$(CROSS)-stubsdk
OSFLAGS := -nostdlibinc -isystem stub-sdk/macos/include
else ifneq ($(SDKROOT),)
BUILD   := build/$(CROSS)
# -mmacos-version-min repeats the -target version: the SDK's availability
# checks (sqlite3_changes64 is macOS 12.3+) must see the deployment target
# whichever clang and SDK pair the build machine has.
OSFLAGS := -isysroot $(SDKROOT) -mmacos-version-min=$(MACOS_MIN)
else
# No SDK: compile-check against the stub headers; the objects are not a runtime.
BUILD   := build/$(CROSS)-stubcheck
OSFLAGS := -nostdlibinc -isystem stub-sdk/macos/include
STUBCHECK := yes
endif
EXE     :=
PICFLAG := -fPIC
LDLIBS  = -lpthread -lm
RDYNAMIC :=
else
TARGET_OS := other
BUILD   := build/$(CROSS)
# the LLVM triple the compiler looks for (build/<triple>/): aarch64-linux-gnu
# -> aarch64-unknown-linux-gnu, x86_64-linux-musl -> x86_64-unknown-linux-musl
TRIPLE  := $(if $(findstring -unknown-,$(CROSS)),$(CROSS),$(subst -linux-,-unknown-linux-,$(CROSS)))
EXE     :=
PICFLAG := -fPIC
# SYSROOT (a glibc tree): only its headers, never the host's or a cross gcc's
OSFLAGS := $(if $(SYSROOT),--sysroot=$(SYSROOT) -nostdinc -isystem $(shell $(CLANG) -print-resource-dir)/include -isystem $(SYSROOT)/usr/include/$(CROSS) -isystem $(SYSROOT)/usr/include)
LDLIBS  = -lpthread -lm
RDYNAMIC := -rdynamic
# musl: clang -target x86_64-linux-musl with musl-gcc's headers (musl-tools);
# aarch64-linux-gnu: gcc-aarch64-linux-gnu's sysroot (clang finds it).
ifneq ($(findstring musl,$(CROSS)),)
MUSL_INC ?= $(firstword $(wildcard /usr/include/$(subst -linux-musl,,$(CROSS))-linux-musl /usr/lib/musl/include /usr/local/musl/include))
ifneq ($(MUSL_INC),)
OSFLAGS := -nostdinc -isystem $(MUSL_INC) -isystem $(shell $(CLANG) -print-resource-dir)/include
endif
endif
endif
else
TARGET_OS := host
BUILD   := build
EXE     :=
PICFLAG := -fPIC
OSFLAGS :=
LDLIBS  = -lpthread -lm
RDYNAMIC := -rdynamic
# make LTO=1 -> build/lto/libzell_rt.a of ThinLTO bitcode objects (clang only),
# for `zell build --release` with ZELL_LTO=1 (docs/PLATFORMS.md): the linker
# then inlines runtime helpers into the Zell code. Not copied to ./libzell_rt.a.
ifeq ($(LTO),1)
BUILD   := build/lto
CFLAGS  += -flto=thin
AR      := $(shell command -v llvm-ar >/dev/null 2>&1 && echo llvm-ar || (command -v llvm-ar-18 >/dev/null 2>&1 && echo llvm-ar-18 || echo ar))
endif
endif

CFLAGS  += -std=c11 -Wall -Wextra -Werror $(PICFLAG) -fno-omit-frame-pointer -ffunction-sections -fdata-sections -D_GNU_SOURCE $(OSFLAGS) -Iinclude -g $(CFLAGS_EXTRA)
SRCS    := $(wildcard src/*.c) $(wildcard src/platform/*.c) $(wildcard src/crypto/*.c)
OBJS    := $(patsubst src/%.c,$(BUILD)/%.o,$(SRCS))
LIB     := $(BUILD)/libzell_rt.a
TESTS   := $(patsubst tests/%.c,$(BUILD)/tests/%,$(wildcard tests/test_*.c))
# Extra environment for test_task only. `make asan` sets ZELL_NO_BACKTRACE=1:
# under ASan, backtrace() in a panicking fork()ed child can deadlock on the
# sanitizer allocator lock inherited from a worker thread (intermittent hang).
TASK_TEST_ENV ?=

.PHONY: all test clean asan tsan tsan-lib test-wine test-cross fuzz-json

ifeq ($(CROSS)$(LTO),1)
all: $(LIB)
else ifeq ($(CROSS),)
all: libzell_rt.a

libzell_rt.a: $(LIB)
	cp $(LIB) $@
else ifeq ($(STUBCHECK),yes)
all: $(OBJS)
	@for t in tests/test_*.c; do $(CC) $(CFLAGS) -fsyntax-only $$t || exit 1; done
	@echo "macOS stub-SDK compile check passed: $(words $(OBJS)) runtime files + tests ($(BUILD)/; not a usable runtime: set SDKROOT or build on a Mac)"
else
all: build/$(TRIPLE)/libzell_rt.a

build/$(TRIPLE)/libzell_rt.a: $(LIB)
	@mkdir -p build/$(TRIPLE)
	cp $(LIB) $@
endif

$(LIB): $(OBJS)
	$(AR) rcs $@ $^

$(BUILD)/%.o: src/%.c include/zell_rt.h src/rt_internal.h src/heap.h src/platform/platform.h $(wildcard src/crypto/*.h) | $(BUILD) $(BUILD)/platform $(BUILD)/crypto
	$(CC) $(CFLAGS) -c $< -o $@

$(BUILD)/tests/%: tests/%.c tests/test.h $(wildcard tests/vectors/*.h) $(LIB) | $(BUILD)/tests
	$(CC) $(CFLAGS) $(RDYNAMIC) $< -o $@$(EXE) $(LIB) $(LDLIBS)

$(BUILD) $(BUILD)/tests $(BUILD)/platform $(BUILD)/crypto:
	mkdir -p $@

# Real LLVM-coroutine lowering example (tests/coro_lowering.ll), see ABI.md.
$(BUILD)/tests/coro_lowering: tests/coro_lowering.ll $(LIB) | $(BUILD)/tests
	$(CLANG) -O2 -Wno-override-module -c $< -o $@.o
	$(CC) $(CFLAGS_EXTRA) $@.o -o $@ $(LIB) $(LDLIBS)

ifeq ($(HAVE_CLANG),yes)
CORO_TEST := $(BUILD)/tests/coro_lowering
endif

test: $(TESTS) $(CORO_TEST)
	@set -e; pass=0; for t in $(TESTS); do \
	  echo "== $$t"; \
	  case "$$t" in *test_task) env $(TASK_TEST_ENV) ./$$t;; *) ./$$t;; esac; \
	  pass=$$((pass+1)); \
	done; \
	if [ -n "$(CORO_TEST)" ]; then \
	  echo "== $(CORO_TEST)"; \
	  out=$$(./$(CORO_TEST) | tr '\n' ' '); \
	  if [ "$$out" != "21 41 4 3 16 8 121 " ]; then echo "coro_lowering: unexpected output '$$out'"; exit 1; fi; \
	  echo "  ok  llvm.coro lowering prints 21 41 4 3 16 8 121"; pass=$$((pass+1)); \
	else echo "(clang not found: skipping coro_lowering)"; fi; \
	echo "ALL TESTS PASSED ($$pass binaries)"

clean:
	rm -rf $(BUILD) libzell_rt.a

# The C unit tests cross-built for Windows and run under wine (the test code
# itself may use mingw's winpthreads and POSIX shims from tests/test.h; the
# runtime does not). A test that
# does not build for Windows (POSIX-only test code) or fails is reported, not
# fatal; the summary line lists both. WINEDEBUG=-all keeps wine quiet.
WINE ?= wine
WINE_TESTS := $(patsubst tests/%.c,%,$(wildcard tests/test_*.c))
test-wine: $(LIB) | $(BUILD)/tests
	@if [ -z "$(CROSS)" ]; then echo "test-wine needs CROSS=x86_64-w64-mingw32"; exit 2; fi
ifneq ($(WINE),)
	@WINEDEBUG=-all $(WINE) cmd /c "if not exist %TEMP% mkdir %TEMP%" >/dev/null 2>&1 || true  # fresh wine prefixes lack it; with WINE= on a native Windows host this would run cmd.exe interactively (MSYS turns /c into C:/) and hang
endif
	@pass=""; fail=""; nobuild=""; for t in $(WINE_TESTS); do \
	  if ! $(CC) $(CFLAGS) -static tests/$$t.c -o $(BUILD)/tests/$$t.exe $(LIB) $(LDLIBS) -lpthread > $(BUILD)/tests/$$t.build.log 2>&1; then \
	    nobuild="$$nobuild $$t"; echo "== $$t: does not build for Windows (see $(BUILD)/tests/$$t.build.log)"; continue; fi; \
	  echo "== $$t (wine)"; \
	  if LC_ALL=C.UTF-8 WINEDEBUG=-all timeout 300 $(WINE) $(BUILD)/tests/$$t.exe > $(BUILD)/tests/$$t.log 2>&1; then \
	    pass="$$pass $$t"; tail -1 $(BUILD)/tests/$$t.log; \
	  else fail="$$fail $$t"; tail -5 $(BUILD)/tests/$$t.log; fi; \
	done; \
	echo "WINE PASS:$$pass"; echo "WINE FAIL:$$fail"; echo "WINE NOT BUILT:$$nobuild"; \
	[ -z "$$fail" ]

# The C unit tests built for a Linux cross target and run here: under
# user-mode QEMU for another CPU (QEMU=qemu-aarch64-static, with the target's
# loader from QEMU_LD_PREFIX=/usr/<cpu>-linux-gnu), directly for musl on the
# same CPU (static, linked by musl-gcc). Same report as test-wine.
CROSS_CPU := $(firstword $(subst -, ,$(CROSS)))
HOST_CPU := $(shell uname -m)
ifneq ($(findstring musl,$(CROSS)),)
TEST_LINK ?= $(if $(filter $(CROSS_CPU),$(HOST_CPU)),musl-gcc,$(CC) -fuse-ld=lld) -static
else
TEST_LINK ?= $(CC) -fuse-ld=lld
endif
QEMU ?= $(if $(filter $(CROSS_CPU),$(HOST_CPU)),,qemu-$(CROSS_CPU)-static)
QEMU_LD_PREFIX ?= /usr/$(CROSS_CPU)-linux-gnu
test-cross: all | $(BUILD)/tests
	@if [ -z "$(CROSS)" ]; then echo "test-cross needs CROSS=aarch64-linux-gnu or x86_64-linux-musl"; exit 2; fi
	@pass=""; fail=""; nobuild=""; for t in $(WINE_TESTS); do \
	  if ! $(CC) $(CFLAGS) -c tests/$$t.c -o $(BUILD)/tests/$$t.o > $(BUILD)/tests/$$t.build.log 2>&1 || \
	     ! $(TEST_LINK) $(BUILD)/tests/$$t.o -o $(BUILD)/tests/$$t $(LIB) $(LDLIBS) >> $(BUILD)/tests/$$t.build.log 2>&1; then \
	    nobuild="$$nobuild $$t"; echo "== $$t: does not build (see $(BUILD)/tests/$$t.build.log)"; continue; fi; \
	  echo "== $$t ($(if $(QEMU),$(QEMU),native))"; \
	  if QEMU_LD_PREFIX=$(QEMU_LD_PREFIX) timeout 600 $(QEMU) $(BUILD)/tests/$$t > $(BUILD)/tests/$$t.log 2>&1; then \
	    pass="$$pass $$t"; tail -1 $(BUILD)/tests/$$t.log; \
	  else fail="$$fail $$t"; tail -5 $(BUILD)/tests/$$t.log; fi; \
	done; \
	echo "CROSS PASS:$$pass"; echo "CROSS FAIL:$$fail"; echo "CROSS NOT BUILT:$$nobuild"; \
	[ -z "$$fail" ] && [ -z "$$nobuild" ]

asan:
	$(MAKE) clean
	LSAN_OPTIONS=suppressions=$(CURDIR)/tests/lsan.supp:print_suppressions=0 \
	  $(MAKE) CFLAGS_EXTRA="-fsanitize=address -fno-omit-frame-pointer" \
	  TASK_TEST_ENV=ZELL_NO_BACKTRACE=1 test

# ThreadSanitizer run of the concurrency-heavy tests (sharded pollers, socket
# close races, resolver pool, scheduler, cycle collector) in its own build directory, so it
# does not disturb the normal build.
TSAN_TESTS ?= test_net test_platform test_task test_cycles test_mem
tsan:
	$(MAKE) BUILD=build/tsan CFLAGS_EXTRA="-fsanitize=thread" $(addprefix build/tsan/tests/,$(TSAN_TESTS))
	@set -e; for t in $(TSAN_TESTS); do \
	  echo "== tsan $$t"; \
	  TSAN_OPTIONS="halt_on_error=1 second_deadlock_stack=1 suppressions=$(CURDIR)/tests/tsan.supp" \
	    ZELL_NO_BACKTRACE=1 ./build/tsan/tests/$$t; \
	done; echo "TSAN TESTS PASSED"

# build/tsan/libzell_rt.a: the runtime under ThreadSanitizer, for Zell
# programs built with ZELL_SANITIZE=thread (scripts/tsan-run.sh).
tsan-lib:
	$(MAKE) BUILD=build/tsan CFLAGS_EXTRA="-fsanitize=thread" build/tsan/libzell_rt.a

# libFuzzer target for encoding.json (tests/fuzz/fuzz_json.c) under ASan+UBSan,
# the runtime rebuilt with the same sanitizers in build/fuzz. The corpus
# (tests/fuzz/corpus/json) is read; new inputs go to build/fuzz/corpus-new,
# crashes to build/fuzz/ (move reproducers to tests/fuzz/crashes/).
# FUZZ_SECONDS (default 300). Needs clang with -fsanitize=fuzzer; otherwise
# FUZZ_STANDALONE=1 builds a driver that only replays the corpus files.
FUZZ_SECONDS ?= 300
FUZZ_SAN := -fsanitize=address,undefined -fno-sanitize-recover=undefined
fuzz-json:
	$(MAKE) BUILD=build/fuzz CC=clang CFLAGS_EXTRA="$(FUZZ_SAN) -fno-omit-frame-pointer" build/fuzz/libzell_rt.a
	mkdir -p build/fuzz/corpus-new
ifeq ($(FUZZ_STANDALONE),1)
	clang $(FUZZ_SAN) -g -O1 -D_GNU_SOURCE -DFUZZ_STANDALONE -Iinclude tests/fuzz/fuzz_json.c build/fuzz/libzell_rt.a -lpthread -lm -rdynamic -o build/fuzz/fuzz_json
	ASAN_OPTIONS=detect_leaks=0 ./build/fuzz/fuzz_json tests/fuzz/corpus/json/*
else
	clang -fsanitize=fuzzer,address,undefined -fno-sanitize-recover=undefined -g -O1 -D_GNU_SOURCE -Iinclude tests/fuzz/fuzz_json.c build/fuzz/libzell_rt.a -lpthread -lm -rdynamic -o build/fuzz/fuzz_json
	ASAN_OPTIONS=detect_leaks=0 ZELL_NO_BACKTRACE=1 ./build/fuzz/fuzz_json -max_len=65536 -max_total_time=$(FUZZ_SECONDS) -artifact_prefix=build/fuzz/ build/fuzz/corpus-new tests/fuzz/corpus/json
endif
