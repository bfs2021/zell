#!/bin/sh
# scripts/macos-llvm.sh - LLVM 23 for building the compiler on macOS arm64
# (ci.yml, release.yml). Homebrew has no LLVM 23 yet, so this uses the
# official LLVM-<version>-macOS-ARM64 build (there is none for x86-64 macOS,
# which is why macos-amd64 is not built until Homebrew catches up).
#
# Self-contained binaries: the Rust build must see only LLVM's static
# archives. llvm-sys gets a shim prefix: libLLVM*.a by symlink in a directory
# of its own (no libc++/libunwind, those come from the OS), Homebrew's static
# libzstd, and an llvm-config wrapper that answers --libdir/--ldflags with
# that directory and turns absolute library paths in --system-libs (from the
# release build machine) into -l<name>.
#
# Writes LLVM_SYS_231_PREFIX and SDKROOT to $GITHUB_ENV when it is set, and
# prints them. The LLVM bin directory is not put on PATH: its clang has no
# default SDK, and the runtime builds with Apple's clang.
#   scripts/macos-llvm.sh [version]
set -e
v=${1:-23.1.4}
dest=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
name=LLVM-$v-macOS-ARM64
llvm=$dest/$name
if [ ! -x "$llvm/bin/llvm-config" ]; then
    curl -fsSL --retry 3 -o "$dest/$name.tar.xz" \
        "https://github.com/llvm/llvm-project/releases/download/llvmorg-$v/$name.tar.xz"
    tar xf "$dest/$name.tar.xz" -C "$dest"
    rm -f "$dest/$name.tar.xz"
fi
brew list zstd >/dev/null 2>&1 || brew install zstd
shim=$dest/llvm-$v-static
rm -rf "$shim"
mkdir -p "$shim/bin" "$shim/lib"
ln -s "$llvm/include" "$shim/include"
for a in "$llvm"/lib/lib*.a; do
    case "$a" in *libunwind*|*libc++*) continue ;; esac
    ln -s "$a" "$shim/lib/"
done
cp "$(brew --prefix zstd)/lib/libzstd.a" "$shim/lib/"
cat > "$shim/bin/llvm-config" <<EOF
#!/bin/sh
for a in "\$@"; do
    case "\$a" in
        --libdir) echo "$shim/lib"; exit 0 ;;
        --ldflags) echo "-L$shim/lib"; exit 0 ;;
        --system-libs)
            "$llvm/bin/llvm-config" "\$@" | tr ' ' '\n' |
                sed -E 's#^/.*/lib([^/]+)\.(a|dylib|tbd)\$#-l\1#' | grep -v '^\$' | tr '\n' ' '
            echo
            exit 0 ;;
    esac
done
exec "$llvm/bin/llvm-config" "\$@"
EOF
chmod +x "$shim/bin/llvm-config"
"$shim/bin/llvm-config" --version --libdir --system-libs
sdk=$(xcrun --show-sdk-path)
if [ -n "${GITHUB_ENV:-}" ]; then
    echo "LLVM_SYS_231_PREFIX=$shim" >> "$GITHUB_ENV"
    echo "SDKROOT=$sdk" >> "$GITHUB_ENV"
fi
echo "LLVM_SYS_231_PREFIX=$shim"
echo "SDKROOT=$sdk"
