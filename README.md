# Zell

Zell is a compiled, statically typed language for services, tools and
command-line programs. It keeps Go's small and readable core and adds
what Go makes you write by hand:

- **C#-style namespaces and `var`** inference, **methods inside structs**;
- **errors as values without the boilerplate**: `T!` results, `?` to
  propagate, `try ... catch` / `try ... else` to handle, and an error
  trace of where each error was created and passed through;
- **`async`/`await`** on a multi-threaded task scheduler, with channels,
  structured cancellation and task-local values;
- **generics** (monomorphized), enums with data, exhaustive `switch`,
  attributes and compile-time reflection (JSON, SQL mapping and DI cost
  nothing at run time);
- **no garbage collector**: reference counting with compile-time
  ownership, destructors (`Drop`) and `weak` pointers, so there are no GC
  pauses and resources close deterministically;
- a large standard library written mostly in Zell (HTTP/1.1 and 2, TLS
  1.3, SQL with PostgreSQL and SQLite drivers, JSON/YAML/TOML/XML, regex,
  crypto, compression, structured logging, metrics and tracing, an ORM),
  compiled through LLVM 23 to native code.

```zell
namespace hello

import io

struct User {
    Name string
    Age  int
}

func parse(line string) User! {
    var parts = strings.Split(line, ":")
    if len(parts) != 2 {
        return error("expected name:age, got " + strconv.Quote(line))
    }
    return User{Name: parts[0], Age: strings.ToInt(parts[1])?("age of " + parts[0])}
}

async func main() {
    for line in ["ann:31", "bob:x", "cid"] {
        var u = try parse(line) catch err {
            println("skipped:", err)
            continue
        }
        await task.Delay(1)
        println(u.Name, "is", u.Age)
    }
}
// Output:
// ann is 31
// skipped: age of bob: invalid integer: "x"
// skipped: expected name:age, got "cid"
```

## Install

### Prebuilt toolchain

Download an archive from the [releases page](https://github.com/zell-lang/zell/releases).
There is one per host, `zell-<version>-<host>.tar.xz` (Windows also
`.tar.gz`), next to `SHA256SUMS`:

| Host | Archive |
|---|---|
| Linux x86-64 | `zell-<version>-linux-amd64.tar.xz` |
| Linux arm64 | `zell-<version>-linux-arm64.tar.xz` |
| macOS Apple Silicon | `zell-<version>-macos-arm64.tar.xz` |
| macOS Intel | `zell-<version>-macos-amd64.tar.xz` |
| Windows x86-64 | `zell-<version>-windows-amd64.tar.xz` / `.tar.gz` |

Each archive is self-contained, like Zig's: `bin/zell`, `bin/zell-lsp`,
`bin/zell-mcp`, the linker `bin/lld`, `std/`, `targets/<triple>/` with the
link inputs of **every** target (runtime, compiler-rt, crt objects, glibc
2.31 stubs, musl, mingw-w64, `libSystem.tbd`) and `tools/` (lldb/gdb
formatters, the VS Code client). One archive therefore cross-compiles to
all targets, and nothing else is needed: no clang, no C library headers,
no mingw-w64, no macOS SDK.

Release candidates (`v0.1.0-rc.N`) are published as GitHub *pre-releases*;
a pre-release is never "latest", so the plain installer does not pick it up
and you ask for it by version.

**One line** (Linux, macOS):

```sh
curl -fsSL https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.sh | sh -s -- --version 0.1.0-rc.1   # a candidate
```

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.ps1 | iex
.\install.ps1 -Version 0.1.0-rc.1        # a candidate, from a saved copy of the script
```

`install.sh` downloads the archive for your host (from the GitHub release,
or from `$ZELL_DIST_URL`, a directory of release files, or `--from
<archive>`), checks its `.sha256`, unpacks it into
`~/.zell/toolchain/zell-<version>-<host>/`, points `~/.zell/toolchain/current`
at it and links `zell`, `zell-lsp` and `zell-mcp` into `~/.local/bin`
(`--dir`, `--bin`, `$ZELL_BIN_DIR` change the places). `install.ps1` does
the same under `%USERPROFILE%\.zell\toolchain` and adds `bin\` to the user
`PATH`.

**By hand**: download the archive and `SHA256SUMS`, check and unpack it,
and put `bin/` on `PATH`:

```sh
sha256sum -c --ignore-missing SHA256SUMS      # macOS: shasum -a 256 -c
tar xf zell-0.1.0-rc.1-linux-amd64.tar.xz
export PATH="$PWD/zell-0.1.0-rc.1-linux-amd64/bin:$PATH"
zell doctor                                   # checks the linker and every target's link inputs
```

**macOS:** the binaries are not signed yet. If Gatekeeper refuses to run
them, remove the quarantine attribute from the unpacked directory:

```sh
xattr -dr com.apple.quarantine ~/.zell/toolchain/zell-0.1.0-rc.1-macos-arm64
```

Linux executables (glibc) run on any system with glibc 2.31 or newer
(Ubuntu 20.04+, Debian 11+, RHEL 9+); `--target linux-amd64-musl` gives
fully static ones. `ZELL_LINKER=clang` links with the system clang
instead. `zell target add <target>` downloads extra targets into
`~/.zell/targets`. Archives are made with `scripts/bundle-targets.sh`,
`scripts/build-lld.sh` and `scripts/package-toolchain.sh`; licences of the
bundled parts are in `THIRD_PARTY.md`; details and the release process in
`docs/PLATFORMS.md`, "Дистрибутив" and "Релиз-кандидаты".

### Your first program

```sh
zell init hello && cd hello    # zell.toml + src/main.zl
zell run                       # build and run the project
zell test                      # tests, benchmarks and examples (@test functions)
zell fmt                       # format in the canonical style (--check in CI)
zell build --release                          # an optimized, stripped binary for this machine
zell build --release --target linux-arm64-musl   # cross-compile: a static arm64 Linux executable
zell target list                              # all targets: linux-*, macos-*, windows-amd64
```

The same cross-build works for `windows-amd64` (an `.exe`) and
`macos-arm64` (a Mach-O executable linked on any host). A step-by-step
version is in [Getting started](docs/guide/getting-started.md).

`zell init` is optional. `zell run` and `zell build` also take a single
file (`zell run main.zl`), a plain directory of `.zl` files (`zell run .`)
or the smallest possible project: a directory with `main.zl` and a
`zell.toml` beside it,

```toml
[package]
name = "hello"
version = "0.1.0"
entry = "main.zl"
```

where the files declare `namespace hello` (the package name is the
namespace root unless the manifest sets `namespace`). `zell init` just
writes this manifest and a `src/` layout for you.

### Editors

- **VS Code**: the extension (highlighting, `zell-lsp`, run/debug code
  lenses, debugging) is in `editors/vscode`; the toolchain archive carries
  its sources in `tools/vscode`. Install steps:
  [editors/vscode/README.md](editors/vscode/README.md).
- **IntelliJ IDEA, GoLand and other JetBrains IDEs**: the
  [zell-idea-plugin](https://github.com/zell-lang/zell-idea-plugin);
  download the zip from its
  [releases](https://github.com/zell-lang/zell-idea-plugin/releases) and
  install it with *Settings > Plugins > gear icon > Install Plugin from
  Disk*. A JetBrains Marketplace listing is planned. The plugin hosts
  `zell-lsp` through the platform LSP API, which exists only in the
  **commercial** IDEs (IntelliJ IDEA Ultimate, GoLand, ...); the free
  Community edition cannot run it. It gives Grammar-Kit PSI syntax
  highlighting, structure view and folding; LSP diagnostics, navigation,
  completion and rename; formatting with `zell fmt`; run and test
  configurations are in progress.
- Neovim, Helix, Zed, Emacs: see the table below.

### Examples

Runnable programs in [`examples/`](examples) and the showcase
repositories (a configuration library, a REST service, a CLI, a worker
pool) are described in [docs/EXAMPLES.md](docs/EXAMPLES.md).

### From source

Building from source works on Linux x86-64 or arm64, macOS and (through MSYS2) Windows. You
need Rust, LLVM 23, clang and lld (a checkout links with the system clang;
the bundled lld and link inputs come from `scripts/bundle-targets.sh`):

```sh
git clone https://github.com/zell-lang/zell && cd zell
./scripts/setup.sh                    # Ubuntu 24.04: LLVM 23 from apt.llvm.org (+ LLVM 18 for the bundle scripts; run as root)
make -C runtime                       # the C runtime (libzell_rt.a)
cargo build --release                 # target/release/zell, zell-lsp and zell-mcp
export PATH="$PWD/target/release:$PATH"
make -C runtime CROSS=x86_64-w64-mingw32   # optional: the Windows runtime (mingw-w64)
make -C runtime CROSS=aarch64-linux-gnu    # optional: linux-arm64 (gcc-aarch64-linux-gnu)
make -C runtime CROSS=x86_64-linux-musl    # optional: static musl binaries (musl-tools)
```

The compiler finds the standard library sources in the repository's
`std/` (or `$ZELL_STD`, or `<prefix>/std` next to an installed
`<prefix>/bin/zell`). Supported platforms and how each was verified:
`docs/PLATFORMS.md`. Check the installation:

```sh
zell version                 # zell 0.1.0 (x86_64)
zell-lsp --version           # the language server (also: zell lsp)
zell run examples/hello.zl   # Hello, Zell!
```

### The language server and other editors

`zell-lsp` is a standard LSP server over stdio (diagnostics with quick
fixes, go to definition/implementation/type definition, references, hover,
rename, completion, signature help, inlay hints, code actions, semantic
tokens, formatting, code lenses to run tests, workspace symbols; the method
list is in `docs/TOOLING.md`). Any editor with an LSP client can use it;
the server must be on `PATH` (or point the client at the binary).

| Editor | Setup |
|---|---|
| **VS Code** | `cd editors/vscode && npm install && npx @vscode/vsce package`, then `code --install-extension zell-*.vsix`. The extension starts `zell-lsp` from `PATH` (setting `zell.serverPath` to override), adds syntax highlighting, run/debug code lenses and debugging through `lldb-dap`. Not on the Marketplace yet. |
| **IntelliJ IDEA / GoLand / other JetBrains IDEs** | plugin repository [zell-lang/zell-idea-plugin](https://github.com/zell-lang/zell-idea-plugin): Grammar-Kit PSI for syntax, `zell-lsp` through the platform LSP API (Ultimate/commercial IDEs). Install the zip from its [releases](https://github.com/zell-lang/zell-idea-plugin/releases), or build it with `./gradlew buildPlugin` (`build/distributions/`); the toolchain is found on `PATH`, in `ZELL_HOME`, or set in Settings → Languages → Zell. |
| **Neovim** | `vim.lsp.start({ name = "zell", cmd = { "zell-lsp" }, root_dir = vim.fs.root(0, { "zell.toml", "zell.json", "zell.yaml", ".git" }) })` in an autocommand for `*.zl` (`vim.filetype.add({ extension = { zl = "zell" } })`). |
| **Helix** | in `languages.toml`: `[[language]] name = "zell" scope = "source.zell" file-types = ["zl"] roots = ["zell.toml"] language-servers = ["zell-lsp"]` and `[language-server.zell-lsp] command = "zell-lsp"`. |
| **Zed, Sublime, Emacs (eglot/lsp-mode)** | register `zell-lsp` as the server for the `zl` extension; no dedicated extension exists yet. |
| **AI coding agents** | Claude Code plugin with the LSP, `zell-mcp` for the rest: see [AI coding agents](#ai-coding-agents). |

Server options go in the client's `initializationOptions`: `inlayHints`
(`types`, `parameterNames`) and `diagnostics.mode` (`auto` by default: pull
diagnostics when the client supports them, otherwise push; set `"push"` for
clients that advertise pull support but never pull).

## Hello, world

```zell
namespace hello

import io

func main() {
    println("Hello, Zell!")
}
// Output:
// Hello, Zell!
```

```sh
zell run hello.zl                    # build and run one file
zell init myapp && cd myapp && zell run    # a project: zell.toml + src/main.zl
zell build --release                 # an optimized, stripped binary
```

## Learn

| | |
|---|---|
| [A tour of Zell](docs/guide/tour.md) | the language in twenty minutes |
| [Errors](docs/guide/errors.md) | `T!`, `?`, `try`, typed errors, wrapping, traces |
| [Async](docs/guide/async.md) | tasks, channels, mutexes, cancellation, timeouts |
| [Memory](docs/guide/memory.md) | ARC, `Drop`, `weak`, arenas, interior pointers |
| [Packages](docs/guide/packages.md) | projects, dependencies, workspaces |
| [Testing](docs/guide/testing.md) | `zell test`, benchmarks, runnable examples |
| [Debugging](docs/guide/debugging.md) | traces, logging, leak checks, the debugger, profiles |
| [Getting started](docs/guide/getting-started.md) | install, first project, tests, cross-builds, editors |
| [FAQ](docs/guide/faq.md) | differences from Go and C# |
| API reference | `zell doc strings.Split`, or `zell doc std --html out/` for a searchable site |

Every code block in these guides is compiled and run by the test suite
(`compiler/tests/guide.rs`). The language specification is
[DESIGN.md](DESIGN.md) (in Russian), with [docs/TOOLING.md](docs/TOOLING.md),
[docs/PACKAGES.md](docs/PACKAGES.md), [docs/FRAMEWORK.md](docs/FRAMEWORK.md),
[docs/STDLIB.md](docs/STDLIB.md) and [docs/PLATFORMS.md](docs/PLATFORMS.md).
API documentation is written in English, in the comments above the
declarations of `std/`.

### The documentation site

`zell doc --site site` builds the whole site from the repository: this
README as the landing page, the guides, the design documents under
`design/` and the standard library reference under `std/`, with one
search index and relative links. `scripts/docs-serve.sh` builds it and
serves it at `http://localhost:8000/`.

The workflow `.github/workflows/docs.yml` publishes it to GitHub Pages on
every push to `master` (`zell doc --site site --base /<repo>/`). To
enable it once: **Settings → Pages → Source: GitHub Actions**; the site
then lives at `https://<user>.github.io/<repo>/`
(for this repository, `https://zell-lang.github.io/zell/`).

## Status

Zell is young: the language is usable and heavily tested, but not yet
stable; binary releases are at the release-candidate stage.

| Area | Status |
|---|---|
| Language core: namespaces, structs, interfaces (default methods, `sealed`), enums with data, closures, `defer`, `switch`, operator overloading, iteration protocol | works |
| Generics (functions, structs, enums, methods; constraints) | works |
| Errors: `T!`, `?`, `try` (expression and block), traces | works |
| `async`/`await`, scheduler, channels, cancellation, task context | works |
| Memory: ARC with ownership, `Drop`, `weak`, arenas, interior-pointer rules, `--leak-check` | works |
| Attributes and reflection (compile-time and run-time) | works |
| Standard library | most of Go's standard library has a counterpart; see [docs/STDLIB.md](docs/STDLIB.md) for each package |
| Networking: TCP/UDP, HTTP/1.1 + HTTP/2 client and server, WebSocket, TLS 1.2/1.3 | works |
| Data: SQL (PostgreSQL, SQLite), Redis, Kafka, S3 clients | works |
| Attribute-routed web framework (routing, binding, DI, OpenAPI) | moved out until `reflect.TypesWith`; returns as the package `zell-lang/web` (in-tree sources: git tag `web-in-tree`) |
| Tools: `fmt`, `test` (with examples), `doc`, LSP + VS Code, debugger (in-process agent, DAP, lldb/gdb) | works |
| Packages: manifests, workspaces, git modules with MVS, lock file, module proxy | works |
| Platforms: Linux x86-64/arm64 | works; macOS and Windows (mingw-w64 cross builds) in progress |
| FFI, a public package index | not yet |

The test suite (`cargo test --workspace`) builds and runs 47 example
programs byte-exact, a specification corpus of 850+ programs, the
standard library's own examples (`zell test std`) and the guides.

## Performance

Benchmarks against equivalent Go programs (`benchmarks/`, `--release`,
x86-64, wall time, best of 5; Go 1.24):

| fib | map | nbody | sieve | strings | json_like | struct_map |
|---|---|---|---|---|---|---|
| 0.57x | 0.56x | 0.97x | 0.61x | 0.80x | 0.62x | 0.86x |

(Zell time / Go time: below 1 is faster.) A TCP echo of 2 GB through
1000 connections runs at about 1.9 GB/s against Go's 1.6 GB/s, and the
`net.http` server handles about the same keep-alive load as Go's
`net/http`. Details and methodology: [benchmarks/README.md](benchmarks/README.md).

## Command line

`zell --help` lists the commands and `zell <command> --help` shows the
options of one.

```
zell run   [path] [options] [-- args]   # build and run (a file, a directory, or the project)
zell build [path] [options]             # compile (default output: ./<name>, or target/<profile>/<pkg>)
zell check [path] [--dump-hir]          # type-check only
zell test  [path] [-v] [--filter re] [--json] [--bench] [--parallel N]   # tests, benchmarks, examples
zell doc   [path|namespace|symbol] [--html dir] [--json] [--check]      # documentation
zell fmt   [paths] [--check] [--diff] [--stdin]   # format in the canonical style
zell init  [dir] [--name n] [--format toml|json|yaml] [--lib] [--agents claude,codex,...]
zell install | update | tree | why <module> | vendor   # dependencies
zell store verify|prune|path | env [NAME] | proxy serve --dir <cache>
zell lsp                                # language server on stdio
zell debug run [path] | debug attach [host:]port | debug [--lldb|--gdb] [path]
zell tool pprof [args] <profile>        # inspect a diag CPU profile
zell target list | add <target>...      # prebuilt runtimes for cross-compilation
```

`path` is a file (one program), a directory without a manifest (all its
`.zl` files form one program) or a project (the nearest `zell.toml`,
`zell.json` or `zell.yaml`; a `[workspace]` root builds its members).

| Build option | Effect |
|---|---|
| `-o <path>` | output file |
| `--release` | `-O2`, `--gc-sections`, stripped, no debug info |
| `-g` / `--strip` | keep DWARF in a release build / no debug info at all |
| `--debug-agent` / `--no-debug-agent` | the in-process debugger hooks (default: on in debug builds) |
| `-O0` … `-O3`, `--static` | optimisation level, fully static executable |
| `--target <triple\|alias>` | cross-compile (`windows-amd64`, `linux-arm64`, `linux-amd64-musl`, `macos-arm64`, ...) |
| `--emit=exe\|obj\|llvm-ir` | what to produce |
| `--leak-check` | report objects still alive at exit (exit status 66) |
| `--workspace`, `-p <name>` | all members / one member of a workspace |
| `--locked`, `--offline` | fail if `zell.lock` would change / use only the lock and the store |

## AI coding agents

Claude Code, Codex CLI, Gemini CLI, Cursor, Windsurf, Cline, Aider, Zed
and Copilot get Zell language intelligence from [`agents/`](agents/README.md):
a Claude Code plugin (`zell-lsp` diagnostics and navigation after every
edit, a format/check hook, Zell skills; `claude plugin marketplace add
zell-lang/zell`, then `claude plugin install zell@zell`), the `zell-mcp`
MCP server (diagnostics, definition, references, rename, format, tests,
std docs) for every MCP-capable agent, and `zell init --agents
claude,codex,cursor,...` to write the instructions and MCP configuration
into a project.

## Repository layout

| Path | What |
|---|---|
| `compiler/` | the `zell` compiler (Rust, inkwell/LLVM 23): lexer, parser, semantic analysis, codegen, build, packages, `doc`, `fmt`, `test` |
| `runtime/` | the C runtime: allocator, refcounting, strings, slices, maps, scheduler, I/O, the C parts of the stdlib (`runtime/ABI.md`) |
| `std/` | the standard library sources (written by `compiler/build.rs` from `compiler/src/sema/stdlib_src/`), plus its tests and examples (`*_test.zl`) |
| `lsp/`, `editors/vscode/`, `grammar/` | language server, VS Code extension, Grammar-Kit grammar |
| `agents/` | AI agent tooling: Claude Code plugin, `zell-mcp` MCP server, agent configuration snippets |
| `examples/` | runnable programs with byte-exact expected output |
| `tests/spec/` | the specification corpus |
| `benchmarks/` | Zell vs Go programs and scripts |
| `docs/` | design documents and the user guides (`docs/guide/`) |

Contributors (human or AI) should read [CLAUDE.md](CLAUDE.md) first: it
describes the architecture, the rules (spec first, ABI contracts, output
format) and how to build and test.

Programs link only the Zell runtime, libc, libm and pthreads, plus a
system library when a module needs one (`database.sqlite` links
`-lsqlite3`). TLS, crypto, compression and the PostgreSQL driver are
self-contained.
