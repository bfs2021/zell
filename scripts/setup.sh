#!/bin/sh
# Development dependencies for building the Zell compiler (Ubuntu 24.04).
#
# The compiler links LLVM 23 (llvm-sys 231: LLVM_SYS_231_PREFIX=/usr/lib/llvm-23),
# installed from apt.llvm.org; the runtime Makefile and the compiler use
# clang-23 when it is there. Ubuntu's own LLVM 18 stays for the bundle
# scripts: scripts/bundle-targets.sh builds the compiler-rt 18.1.8 builtins
# with clang-18 and scripts/build-lld.sh links bin/lld from liblld-18-dev.
set -e
apt-get update
apt-get install -y llvm-18-dev libpolly-18-dev libzstd-dev clang-18 lld-18 \
    wget gnupg lsb-release software-properties-common
if ! command -v llvm-config-23 >/dev/null 2>&1; then
    wget -qO /tmp/llvm.sh https://apt.llvm.org/llvm.sh
    bash /tmp/llvm.sh 23
fi
apt-get install -y llvm-23-dev libpolly-23-dev clang-23 lld-23
llvm-config-23 --version
