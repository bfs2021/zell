#!/bin/bash
# Verifies the macOS port on a real Mac (docs/PLATFORMS.md, "macOS: что
# проверить на настоящем Mac"). Everything here was only compiled on Linux
# (runtime against stub headers, Zell code as Mach-O objects); this script
# builds, links and runs it for real and prints one line per check.
#
# Needs: Xcode command line tools (clang, the SDK), Rust (cargo), LLVM 23
# (brew install llvm; LLVM_SYS_231_PREFIX=$(brew --prefix llvm)).
# Usage: scripts/macos-verify.sh [--quick]   (--quick: skip cargo test)
# Exit status: the number of failed checks (0 = the port works).
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
quick=no
[ "${1:-}" = "--quick" ] && quick=yes
fails=0
log=$(mktemp -d)
check() { # name command...
    local name=$1; shift
    if "$@" > "$log/$name.log" 2>&1; then
        echo "PASS $name"
    else
        echo "FAIL $name (log: $log/$name.log)"; tail -n 15 "$log/$name.log" | sed 's/^/    /'
        fails=$((fails + 1))
    fi
}
[ "$(uname -s)" = Darwin ] || { echo "macos-verify.sh: run this on macOS" >&2; exit 100; }
export LLVM_SYS_231_PREFIX=${LLVM_SYS_231_PREFIX:-$(brew --prefix llvm@23 2>/dev/null || brew --prefix llvm 2>/dev/null || true)}
export SDKROOT=${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}
echo "macOS $(sw_vers -productVersion) $(uname -m), SDK $SDKROOT"

# 1. the C runtime with the real SDK, -Werror, and its unit tests (kqueue
#    poller, sharded waits, sockets, files, processes, signals, sampler)
check runtime_build make -C runtime -j8
check runtime_tests make -C runtime test
check runtime_tsan make -C runtime tsan
# the stub-SDK compile check must agree with the real SDK
check stub_sdk_check env -u SDKROOT make -C runtime CROSS="$(uname -m | sed s/arm64/aarch64/)-apple-darwin" SDKROOT=
# 2. the compiler; every example byte-exact (run_examples), spec corpus
check compiler_build cargo build --release
if [ $quick = no ]; then
    check cargo_test cargo test --workspace
fi
S=$root/target/release/zell
# 3. platform specifics
tmp=$log/work; mkdir -p "$tmp"
check hello_link_run sh -c "'$S' build examples/hello.zl -o '$tmp/hello' && '$tmp/hello' | grep -q 'Hello, Zell!'"
check release_dead_strip sh -c "'$S' build --release examples/hello.zl -o '$tmp/hello_r' && ls -l '$tmp/hello_r'"
check net_examples sh -c "for e in tcp_echo http_server tls_echo redis http_realtime; do '$S' run examples/\$e.zl > '$tmp/'\$e.got && cmp '$tmp/'\$e.got examples/\$e.out || exit 1; done"
check cpuprofile sh -c "'$S' run examples/cpuprofile.zl | cmp - examples/cpuprofile.out"
cat > "$tmp/panic.zl" <<'SL'
namespace verify.panic

import io

func inner(xs []int) int {
    return xs[5]
}

func main() {
    println(inner([1, 2]))
}
SL
check backtrace_symbols sh -c "'$S' build '$tmp/panic.zl' -o '$tmp/panic'; '$tmp/panic' 2>&1 | grep -q 'inner'"
cat > "$tmp/plat.zl" <<'SL'
namespace verify.plat

import io
import crypto

async func main() void! {
    println(os.Name, os.Arch)
    var t = time.Unix(1720000000, 0).In(time.LoadLocation("Europe/Berlin")?)
    println(t.Format("2006-01-02 15:04 MST"))
    println("roots:", x509.SystemRootsPath() != "", x509.SystemRoots()?.Len() > 10)
    var r = await process.Run("/bin/echo", ["spawned"])?
    print(r.Stdout)
}
SL
check platform_features sh -c "'$S' run '$tmp/plat.zl' > '$tmp/plat.got' && printf 'macos %s\n2024-07-03 11:46 CEST\nroots: true true\nspawned\n' \"\$(uname -m | sed 's/x86_64/amd64/')\" | cmp - '$tmp/plat.got'"
# 4. cross targets from the Mac (optional toolchains)
if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || [ -d /opt/homebrew/opt/mingw-w64 ]; then
    check windows_cross sh -c "make -C runtime CROSS=x86_64-w64-mingw32 -j8 && '$S' build --target windows-amd64 examples/hello.zl -o '$tmp/hello.exe'"
fi
check linux_obj "$S" build --target linux-arm64 --emit=obj examples/structs.zl -o "$tmp/structs.o"
# 5. packaging
check package_toolchain sh -c "scripts/package-toolchain.sh --no-build --out '$tmp/dist' && tar tzf '$tmp/dist/zell-'*'-macos-'*'.tar.gz' | grep -q targets/"
echo "macos-verify: $fails failed check(s); logs in $log"
exit $fails
