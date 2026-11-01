//! Linking executables (docs/PLATFORMS.md, "Кросс-компиляция" and
//! "Дистрибутив"): where the runtime and link inputs of a target live, the
//! link command, and how an executable is run on this machine (Wine for
//! Windows, QEMU for another CPU).
//!
//! Two linkers:
//!
//! - **Bundled lld** (the default whenever the target's link inputs are
//!   bundled): the toolchain archive carries `bin/lld` (one multi-flavour
//!   binary: ELF, COFF/MinGW, Mach-O) and `targets/<triple>/` with
//!   everything a link needs: `libzell_rt.a`, compiler-rt builtins, crt
//!   objects, and the C library as glibc 2.31 stub shared objects (dynamic,
//!   runs on glibc >= 2.31), musl `libc.a` (fully static), mingw-w64 CRT and
//!   import libraries, or a `libSystem.tbd` text stub (macOS). A directory
//!   is bundled if it holds a `BUNDLE` file (`scripts/bundle-targets.sh`
//!   writes it). [`link_command`] then builds the whole lld command line:
//!   nothing else (clang, libc6-dev, mingw, an SDK) is needed on the machine.
//! - **clang** (`ZELL_LINKER=clang`, or a runtime without link inputs, e.g.
//!   `runtime/libzell_rt.a` of a repository checkout): the system's clang
//!   driver finds the C library, as before: `clang -target <cpu>-w64-mingw32`
//!   for Windows (mingw-w64 of Debian/Ubuntu or MSYS2), `-isysroot $SDKROOT`
//!   for macOS, `--sysroot=$ZELL_SYSROOT` for other ELF targets, `musl-gcc`
//!   (`$ZELL_MUSL_CC`) for musl on the same CPU. Needed for `--static` with
//!   glibc (libc.a is not bundled).
//!
//! Runtime and link inputs of target `t` are looked for in
//! `<prefix>/targets/<triple>/` next to the installed `bin/zell`, then
//! `$ZELL_TARGETS/<triple>/` (default `~/.zell/targets`, filled by `zell
//! target add`), then the repository (`runtime/build/<triple>/` from `make
//! -C runtime CROSS=...`, `build/targets/<triple>/` from
//! `scripts/bundle-targets.sh`).

use crate::target::{ObjectFormat, Target};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Windows system libraries the runtime imports from.
pub const WINDOWS_LIBS: &[&str] = &["ws2_32", "iphlpapi", "bcrypt", "shell32", "advapi32"];

/// The file that marks a directory of bundled link inputs.
pub const BUNDLE_MARK: &str = "BUNDLE";

/// The directory of prebuilt target runtimes (`$ZELL_TARGETS`, default
/// `$ZELL_HOME/targets` = `~/.zell/targets`; `zell target add` fills it).
pub fn targets_dir() -> Option<PathBuf> {
    Some(crate::toolchain::targets_dir())
}

/// The repository the compiler was built from or runs in (`<exe>/../..` of
/// `target/<profile>/zell`), for development builds.
fn repo_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("../.."));
            v.push(dir.join("../../.."));
        }
    }
    v.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."));
    v
}

/// Every place `libzell_rt.a` for `t` (not the host) is looked for, in order.
pub fn runtime_candidates(t: &Target) -> Vec<PathBuf> {
    let mut cands = crate::toolchain::runtime_candidates(&t.triple);
    for r in repo_dirs() {
        cands.push(r.join("runtime/build").join(&t.triple).join("libzell_rt.a"));
        cands.push(r.join("build/targets").join(&t.triple).join("libzell_rt.a"));
    }
    cands
}

/// Locates `libzell_rt.a` built for `t` (not the host): the toolchain's own
/// `targets/`, `$ZELL_TARGETS`, then the repository.
pub fn find_runtime(t: &Target) -> Result<PathBuf, String> {
    let cands = runtime_candidates(t);
    for c in &cands {
        if c.is_file() {
            return Ok(crate::paths::canonical(&c).unwrap_or(c.clone()));
        }
    }
    let hint = if t.os == "windows" { "make -C runtime CROSS=x86_64-w64-mingw32".to_string() } else { format!("make -C runtime CROSS={} (needs the target's C headers)", t.triple) };
    Err(format!(
        "no Zell runtime for {} (download it with `zell target add {}`, or build it with `{hint}`); looked in: {}",
        t.triple,
        t.triple,
        cands.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

/// The directory of bundled link inputs that runtime `rt` belongs to, if it
/// is one (holds a [`BUNDLE_MARK`] file).
pub fn bundle_of(rt: &Path) -> Option<PathBuf> {
    let dir = rt.parent()?;
    dir.join(BUNDLE_MARK).is_file().then(|| dir.to_path_buf())
}

/// What `ZELL_LINKER` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkerChoice {
    /// Unset: the bundled lld when the target's link inputs are bundled, else clang.
    Auto,
    /// `ZELL_LINKER=lld`: the bundled lld (without link inputs, clang `-fuse-ld=lld`).
    Lld,
    /// `ZELL_LINKER=clang`: the system clang driver.
    Clang,
}

/// `ZELL_LINKER`.
pub fn linker_choice() -> LinkerChoice {
    match std::env::var("ZELL_LINKER").unwrap_or_default().as_str() {
        "clang" | "cc" => LinkerChoice::Clang,
        "lld" | "bundle" | "bundled" => LinkerChoice::Lld,
        _ => LinkerChoice::Auto,
    }
}

/// lld flavours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// ELF and, with `-m i386pep`, MinGW (`ld.lld`).
    Gnu,
    /// Mach-O (`ld64.lld`).
    Darwin,
}

/// The lld binary that links: `$ZELL_LLD`, `bin/lld` next to `zell`, then
/// `ld.lld`/`ld64.lld` (also `-18`) on PATH.
#[derive(Clone, Debug)]
pub struct Lld {
    /// `bin/lld` (multi-flavour, takes `-flavor`), or a flavour-named binary.
    pub program: PathBuf,
    /// True for a multi-flavour `lld` that needs `-flavor <f>` first.
    pub multi: bool,
}

impl Lld {
    /// A command running lld as flavour `f`.
    pub fn command(&self, f: Flavor) -> Command {
        if self.multi {
            let mut c = Command::new(&self.program);
            c.arg("-flavor").arg(match f {
                Flavor::Gnu => "gnu",
                Flavor::Darwin => "darwin",
            });
            return c;
        }
        let name = self.program.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let want = if f == Flavor::Darwin { "ld64.lld" } else { "ld.lld" };
        if name.starts_with(want) {
            return Command::new(&self.program);
        }
        // a flavour-named binary of the other flavour: its sibling
        let suffix = name.split_once(".lld").map(|(_, s)| s.to_string()).unwrap_or_default();
        Command::new(self.program.with_file_name(format!("{want}{suffix}")))
    }

    /// The ELF linker as one program (for `-r` merges of partial objects).
    pub fn elf_program(&self) -> Option<PathBuf> {
        if !self.multi {
            return Some(self.program.clone()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ld.lld")));
        }
        let sib = self.program.with_file_name(format!("ld.lld{}", std::env::consts::EXE_SUFFIX));
        sib.is_file().then_some(sib)
    }
}

fn runs(p: &Path, flavor_arg: bool) -> bool {
    let mut c = Command::new(p);
    if flavor_arg {
        c.args(["-flavor", "gnu"]);
    }
    c.arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Finds lld ([`Lld`]); probed once per process.
pub fn find_lld() -> Option<Lld> {
    static LLD: std::sync::OnceLock<Option<Lld>> = std::sync::OnceLock::new();
    LLD.get_or_init(|| {
        let exe_suffix = std::env::consts::EXE_SUFFIX;
        if let Some(p) = std::env::var_os("ZELL_LLD").filter(|v| !v.is_empty()) {
            let p = PathBuf::from(p);
            let multi = p.file_stem().is_some_and(|n| n == "lld");
            return Some(Lld { program: p, multi });
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            let p = dir.join(format!("lld{exe_suffix}"));
            if p.is_file() && runs(&p, true) {
                return Some(Lld { program: p, multi: true });
            }
        }
        for name in ["ld.lld-23", "ld.lld"] {
            if let Some(p) = which(name) {
                if runs(&p, false) {
                    return Some(Lld { program: p, multi: false });
                }
            }
        }
        None
    })
    .clone()
}

/// `name` on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The macOS SDK: `$SDKROOT`, else `xcrun --show-sdk-path` (on a Mac).
fn macos_sdk() -> Option<String> {
    std::env::var("SDKROOT").ok().filter(|s| !s.is_empty()).or_else(|| {
        let out = Command::new("xcrun").args(["--sdk", "macosx", "--show-sdk-path"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    })
}

/// Checks that an executable for `t` can be linked by the clang driver
/// (before looking for its runtime), with the reason and the way out if not.
pub fn preflight(t: &Target) -> Result<(), String> {
    match t.object_format() {
        ObjectFormat::MachO if macos_sdk().is_none() => Err(format!(
            "linking for {} without the bundled link inputs needs the macOS SDK (set SDKROOT to a MacOSX.sdk directory, or link on a Mac); `zell target add {}` installs them, `--emit=obj` builds the object file without either",
            t.triple, t.triple
        )),
        ObjectFormat::Coff if t.is_msvc() => Err(format!(
            "linking for {} (MSVC ABI) needs the Windows SDK and an MSVC-built runtime, not supported yet; use windows-amd64 (MinGW ABI) or --emit=obj",
            t.triple
        )),
        ObjectFormat::Wasm => Err(format!("linking for {} is not supported yet; use --emit=obj", t.triple)),
        _ => Ok(()),
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success())
}

/// The musl-gcc wrapper for a musl target of the host's CPU
/// (`$ZELL_MUSL_CC`, else `musl-gcc` when installed).
fn musl_cc(t: &Target) -> Option<String> {
    if let Ok(cc) = std::env::var("ZELL_MUSL_CC") {
        return Some(cc);
    }
    let host_cpu = std::env::consts::ARCH;
    let cpu = t.triple.split('-').next().unwrap_or("");
    (cpu == host_cpu && have("musl-gcc")).then(|| "musl-gcc".to_string())
}

/// The clang `-target` of `t` for linking: MinGW triples are spelled the
/// way the mingw-w64 sysroot is named.
fn clang_target(t: &Target) -> String {
    if t.os == "windows" && !t.is_msvc() {
        let cpu = t.triple.split('-').next().unwrap_or("x86_64");
        return format!("{cpu}-w64-mingw32");
    }
    t.triple.clone()
}

/// Options of a link that depend on the build.
pub struct LinkOpts<'a> {
    pub release: bool,
    /// Strip symbols and debug information (`-s`).
    pub strip: bool,
    pub static_link: bool,
    /// Export the executable's symbols (`-rdynamic`: names in backtraces of
    /// debug builds through `dladdr`).
    pub export_dynamic: bool,
    /// The clang driver (`ZELL_LINKER=clang` and links without bundled inputs).
    pub clang: &'a str,
}

/// True if the link of `t` with bundled inputs needs clang anyway:
/// `--static` with glibc (libc.a is not bundled; musl is the static target).
pub fn bundle_needs_clang(t: &Target, o: &LinkOpts) -> bool {
    o.static_link && t.os == "linux" && !t.is_musl()
}

/// The CPU part of `t`'s triple (`x86_64`, `aarch64`).
fn cpu(t: &Target) -> &str {
    t.triple.split('-').next().unwrap_or("")
}

/// The bundled-lld command for executable `output` of target `t` from
/// `objs`, the runtime `rt`, the link inputs in `dir` ([`bundle_of`]) and
/// the libraries `libs` (stdlib `LINK_LIBS`: `-l<name>` against `dir`).
pub fn lld_command(lld: &Lld, t: &Target, dir: &Path, objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], o: &LinkOpts) -> Result<Command, String> {
    let f = |name: &str| dir.join(name);
    let need = |name: &str| -> Result<PathBuf, String> {
        let p = f(name);
        if p.is_file() {
            Ok(p)
        } else {
            Err(format!("incomplete link inputs for {} in {}: no {name} (reinstall the toolchain, `zell doctor` lists what is missing)", t.triple, dir.display()))
        }
    };
    let builtins = need("libclang_rt.builtins.a")?;
    match t.object_format() {
        ObjectFormat::Elf if t.os == "linux" => {
            let mut c = lld.command(Flavor::Gnu);
            let emu = match cpu(t) {
                "x86_64" => "elf_x86_64",
                "aarch64" => "aarch64linux",
                other => return Err(format!("no bundled link recipe for CPU {other} ({})", t.triple)),
            };
            c.args(["-m", emu, "--hash-style=gnu", "--build-id", "--eh-frame-hdr", "-z", "relro"]);
            c.arg("-o").arg(output);
            if t.is_musl() {
                // fully static: musl's libc.a, no interpreter
                c.arg("-static");
                c.arg(need("crt1.o")?).arg(need("crti.o")?).arg(need("crtbegin.o")?);
                c.args(objs).arg(rt);
                c.arg("--start-group").arg(need("libc.a")?).arg(&builtins).arg("--end-group");
                c.arg(need("crtend.o")?).arg(need("crtn.o")?);
            } else {
                let (interp, ldso) = match cpu(t) {
                    "x86_64" => ("/lib64/ld-linux-x86-64.so.2", "ld-linux-x86-64.so.2"),
                    _ => ("/lib/ld-linux-aarch64.so.1", "ld-linux-aarch64.so.1"),
                };
                c.args(["-pie", "-dynamic-linker", interp]);
                c.arg(need("Scrt1.o")?).arg(need("crti.o")?).arg(need("crtbeginS.o")?);
                c.arg("-L").arg(dir);
                c.args(objs).arg(rt);
                c.arg("--as-needed");
                for l in libs {
                    c.arg(format!("-l{l}"));
                }
                c.args(["-lpthread", "-ldl", "-lrt", "-lm"]);
                c.arg(&builtins);
                c.arg("-lc").arg(need("libc_nonshared.a")?).arg(need(ldso)?);
                c.arg("--no-as-needed");
                c.arg(&builtins);
                c.arg(need("crtendS.o")?).arg(need("crtn.o")?);
                if o.export_dynamic {
                    c.arg("--export-dynamic");
                }
            }
            if o.release {
                c.arg("--gc-sections");
            }
            if o.strip {
                c.arg("-s");
            }
            Ok(c)
        }
        ObjectFormat::Coff if !t.is_msvc() => {
            // ld.lld's MinGW driver (what `clang -target x86_64-w64-mingw32
            // -fuse-ld=lld` runs), compiler-rt in place of libgcc
            let mut c = lld.command(Flavor::Gnu);
            c.args(["-m", "i386pep", "-Bdynamic"]);
            c.arg("-o").arg(output);
            c.arg(need("crt2.o")?);
            c.arg("-L").arg(dir);
            c.args(objs).arg(rt);
            for l in WINDOWS_LIBS {
                c.arg(format!("-l{l}"));
            }
            // SQLite is bound at run time from winsqlite3.dll / sqlite3.dll
            // (runtime/src/platform/sqlite_dyn.h): no import library
            for l in libs.iter().filter(|l| **l != "sqlite3") {
                c.arg(format!("-l{l}"));
            }
            for _ in 0..2 {
                c.args(["-lmingw32"]).arg(&builtins).args(["-lmoldname", "-lmingwex", "-lmsvcrt", "-ladvapi32", "-lshell32", "-luser32", "-lkernel32"]);
            }
            if o.release {
                c.arg("--gc-sections");
            }
            if o.strip {
                c.arg("-s");
            }
            Ok(c)
        }
        ObjectFormat::MachO => {
            let mut c = lld.command(Flavor::Darwin);
            let arch = if cpu(t) == "aarch64" || cpu(t) == "arm64" { "arm64" } else { "x86_64" };
            c.args(["-demangle", "-dynamic", "-arch", arch, "-platform_version", "macos", "12.3.0", "14.0"]);
            c.arg("-o").arg(output);
            c.args(objs).arg(rt);
            c.arg("-L").arg(dir);
            for l in libs {
                // libSystem.tbd lists what the runtime uses; other libraries
                // (libsqlite3) are found only if bundled as lib<name>.tbd
                if f(&format!("lib{l}.tbd")).is_file() {
                    c.arg(format!("-l{l}"));
                }
            }
            c.arg(&builtins);
            c.arg(need("libSystem.tbd")?);
            if o.release {
                c.arg("-dead_strip");
            }
            if o.strip {
                c.arg("-x").arg("-S");
            }
            Ok(c)
        }
        _ => Err(preflight(t).err().unwrap_or_else(|| format!("no bundled link recipe for {}", t.triple))),
    }
}

/// The clang option that makes it link with lld of flavour `f`: the full
/// path (`--ld-path=`) of the lld [`find_lld`] found (`ld.lld-18` on PATH is
/// not something `-fuse-ld=lld` finds), or `None` without one.
fn clang_lld_arg(f: Flavor) -> Option<String> {
    let lld = find_lld()?;
    let prog = PathBuf::from(lld.command(f).get_program());
    let prog = if lld.multi {
        // the multi-flavour binary through its flavour-named link
        let name = if f == Flavor::Darwin { "ld64.lld" } else { "ld.lld" };
        lld.program.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    } else {
        prog
    };
    prog.is_file().then(|| format!("--ld-path={}", prog.display()))
}

/// The clang-driver link command for executable `output` of target `t`
/// from `objs`, the target runtime `rt` and the system libraries `libs`.
pub fn clang_command(t: &Target, objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], o: &LinkOpts) -> Result<Command, String> {
    preflight(t)?;
    if t.is_musl() {
        if let Some(cc) = musl_cc(t) {
            let mut cmd = Command::new(cc);
            cmd.args(objs).arg(rt);
            for l in libs.iter().filter(|l| **l != "sqlite3") {
                cmd.arg(format!("-l{l}"));
            }
            cmd.arg("-static").arg("-lpthread").arg("-lm");
            if o.release {
                cmd.arg("-Wl,--gc-sections");
            }
            if o.strip {
                cmd.arg("-s");
            }
            cmd.arg("-o").arg(output);
            return Ok(cmd);
        }
    }
    let mut cmd = Command::new(o.clang);
    cmd.arg("-target").arg(clang_target(t));
    cmd.args(objs).arg(rt);
    for l in libs {
        // Windows: SQLite is bound at run time from winsqlite3.dll / sqlite3.dll
        // (runtime/src/platform/sqlite_dyn.h), there is no import library to link;
        // musl: no static libsqlite3, the runtime is built without it.
        if (t.os == "windows" || t.is_musl()) && *l == "sqlite3" {
            continue;
        }
        cmd.arg(format!("-l{l}"));
    }
    match t.object_format() {
        ObjectFormat::Coff => {
            if let Some(a) = clang_lld_arg(Flavor::Gnu) {
                cmd.arg(a);
            }
            for l in WINDOWS_LIBS {
                cmd.arg(format!("-l{l}"));
            }
            if o.release {
                cmd.arg("-Wl,--gc-sections");
            }
            if o.static_link {
                cmd.arg("-static");
            }
        }
        ObjectFormat::MachO => {
            let Some(sdk) = macos_sdk() else { return Err(preflight(t).unwrap_err()) };
            cmd.arg("-isysroot").arg(sdk);
            if let Some(a) = clang_lld_arg(Flavor::Darwin) {
                cmd.arg(a);
            }
            if o.release {
                cmd.arg("-Wl,-dead_strip");
            }
        }
        ObjectFormat::Elf => {
            cmd.arg(clang_lld_arg(Flavor::Gnu).unwrap_or_else(|| "-fuse-ld=lld".into()));
            if let Ok(s) = std::env::var("ZELL_SYSROOT") {
                cmd.arg(format!("--sysroot={s}"));
            }
            cmd.arg("-lpthread");
            if o.release {
                cmd.arg("-Wl,--gc-sections");
            }
            if o.static_link || t.is_musl() {
                cmd.arg("-static");
            }
        }
        ObjectFormat::Wasm => return Err(preflight(t).unwrap_err()),
    }
    cmd.arg("-lm").arg("-o").arg(output);
    if o.strip {
        cmd.arg("-s");
    }
    Ok(cmd)
}

/// A link command for target `t` (not the host, or Windows): the bundled
/// lld if `rt` comes with link inputs ([`bundle_of`]) and `ZELL_LINKER` does
/// not ask for clang, else the clang driver. Returns the command and a
/// name of the linker for messages.
pub fn link_command(t: &Target, objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], o: &LinkOpts) -> Result<(Command, String), String> {
    if let Some(r) = bundled_link(t, objs, rt, output, libs, o) {
        return r;
    }
    let cmd = clang_command(t, objs, rt, output, libs, o)?;
    Ok((cmd, o.clang.to_string()))
}

/// The bundled-lld link of [`link_command`], or `None` when it does not apply
/// (no link inputs next to `rt`, `ZELL_LINKER=clang`, `--static` with glibc
/// and clang present).
pub fn bundled_link(t: &Target, objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], o: &LinkOpts) -> Option<Result<(Command, String), String>> {
    let choice = linker_choice();
    if choice == LinkerChoice::Clang {
        return None;
    }
    let dir = bundle_of(rt)?;
    if bundle_needs_clang(t, o) {
        if which(o.clang).is_some() || have(o.clang) {
            return None;
        }
        return Some(Err(format!(
            "--static for {} needs glibc's libc.a, which the toolchain does not bundle: build a static executable with `--target {}` (musl), or install clang and the C library's static archives",
            t.triple,
            if cpu(t) == "aarch64" { "linux-arm64-musl" } else { "linux-amd64-musl" }
        )));
    }
    let Some(lld) = find_lld() else {
        return Some(Err(format!("linking for {} with the bundled inputs in {} needs lld: bin/lld of the toolchain is missing (set ZELL_LLD, or ZELL_LINKER=clang)", t.triple, dir.display())));
    };
    Some(lld_command(&lld, t, &dir, objs, rt, output, libs, o).map(|c| (c, format!("{} (bundled inputs {})", lld.program.display(), dir.display()))))
}

/// A command that runs executable `exe` of target `t` here: directly for
/// the host, through Wine (`$ZELL_WINE`, else `wine`) for Windows on
/// another OS; `None` if this machine cannot run it.
pub fn run_command(t: &Target, exe: &Path) -> Option<Command> {
    if t.is_host() {
        return Some(Command::new(exe));
    }
    let host = Target::host();
    if t.os == "linux" && host.os == "linux" {
        if t.arch == host.arch {
            return Some(Command::new(exe)); // musl: a static executable
        }
        // another CPU: user-mode QEMU, with the target's dynamic loader
        let cpu = t.triple.split('-').next().unwrap_or("");
        let cands = match std::env::var("ZELL_QEMU") {
            Ok(q) => vec![q],
            Err(_) => vec![format!("qemu-{cpu}-static"), format!("qemu-{cpu}")],
        };
        let qemu = cands.into_iter().find(|q| Command::new(q).arg("--version").output().is_ok_and(|o| o.status.success()))?;
        let mut c = Command::new(qemu);
        let root = std::env::var("ZELL_SYSROOT").unwrap_or_else(|_| format!("/usr/{cpu}-linux-gnu"));
        if !t.is_musl() && Path::new(&root).is_dir() {
            c.arg("-L").arg(root);
        }
        c.arg(exe);
        return Some(c);
    }
    if t.os == "windows" && t.arch == "amd64" && !cfg!(windows) {
        let wine = std::env::var("ZELL_WINE").unwrap_or_else(|_| "wine".into());
        if Command::new(&wine).arg("--version").output().is_ok_and(|o| o.status.success()) {
            let mut c = Command::new(wine);
            c.arg(exe);
            if std::env::var_os("WINEDEBUG").is_none() {
                c.env("WINEDEBUG", "-all");
            }
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().to_string()).collect()
    }

    /// A fake bundle directory with every file the recipes name.
    fn fake_bundle(name: &str, files: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zell-crosslink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in files.iter().chain(&[BUNDLE_MARK, "libzell_rt.a", "libclang_rt.builtins.a"]) {
            std::fs::write(d.join(f), b"").unwrap();
        }
        d
    }

    fn opts() -> LinkOpts<'static> {
        LinkOpts { release: true, strip: true, static_link: false, export_dynamic: false, clang: "clang" }
    }

    #[test]
    fn lld_recipes() {
        let lld = Lld { program: PathBuf::from("/tc/bin/lld"), multi: true };
        let objs = vec![PathBuf::from("a.o")];
        let d = fake_bundle("gnu", &["Scrt1.o", "crti.o", "crtn.o", "crtbeginS.o", "crtendS.o", "libc_nonshared.a", "ld-linux-x86-64.so.2"]);
        let t = Target::parse("linux-amd64").unwrap();
        let rt = d.join("libzell_rt.a");
        assert_eq!(bundle_of(&rt).as_deref(), Some(d.as_path()));
        let c = lld_command(&lld, &t, &d, &objs, &rt, Path::new("out"), &["sqlite3"], &opts()).unwrap();
        let a = args(&c);
        assert_eq!(&a[..2], ["-flavor", "gnu"]);
        for want in ["-pie", "/lib64/ld-linux-x86-64.so.2", "-lsqlite3", "-lc", "--gc-sections", "-s", "elf_x86_64"] {
            assert!(a.iter().any(|x| x == want), "{want} in {a:?}");
        }
        assert!(!a.iter().any(|x| x == "-static"));
        assert!(bundle_needs_clang(&t, &LinkOpts { static_link: true, ..opts() }));

        let d = fake_bundle("musl", &["crt1.o", "crti.o", "crtn.o", "crtbegin.o", "crtend.o", "libc.a"]);
        let t = Target::parse("linux-arm64-musl").unwrap();
        let a = args(&lld_command(&lld, &t, &d, &objs, &d.join("libzell_rt.a"), Path::new("out"), &["sqlite3"], &opts()).unwrap());
        assert!(a.iter().any(|x| x == "-static") && a.iter().any(|x| x == "aarch64linux"), "{a:?}");
        assert!(!a.iter().any(|x| x == "-lsqlite3" || x == "-pie"), "{a:?}");
        assert!(!bundle_needs_clang(&t, &LinkOpts { static_link: true, ..opts() }));

        let d = fake_bundle("win", &["crt2.o"]);
        let t = Target::parse("windows-amd64").unwrap();
        let a = args(&lld_command(&lld, &t, &d, &objs, &d.join("libzell_rt.a"), Path::new("out.exe"), &["sqlite3"], &opts()).unwrap());
        assert!(a.windows(2).any(|w| w == ["-m", "i386pep"]) && a.iter().any(|x| x == "-lws2_32") && a.iter().any(|x| x == "-lmsvcrt"), "{a:?}");
        assert!(!a.iter().any(|x| x == "-lsqlite3"));

        let d = fake_bundle("mac", &["libSystem.tbd"]);
        let t = Target::parse("macos-arm64").unwrap();
        let a = args(&lld_command(&lld, &t, &d, &objs, &d.join("libzell_rt.a"), Path::new("out"), &[], &opts()).unwrap());
        assert_eq!(&a[..2], ["-flavor", "darwin"]);
        assert!(a.windows(2).any(|w| w == ["-arch", "arm64"]) && a.iter().any(|x| x == "-dead_strip"), "{a:?}");

        // a missing input is reported by name
        let d = fake_bundle("broken", &[]);
        let t = Target::parse("linux-amd64").unwrap();
        let e = lld_command(&lld, &t, &d, &objs, &d.join("libzell_rt.a"), Path::new("out"), &[], &opts()).unwrap_err();
        assert!(e.contains("no Scrt1.o"), "{e}");
    }

    #[test]
    fn flavour_named_binaries() {
        let l = Lld { program: PathBuf::from("/usr/bin/ld.lld-18"), multi: false };
        assert_eq!(Path::new(l.command(Flavor::Gnu).get_program()), l.program);
        assert_eq!(Path::new(l.command(Flavor::Darwin).get_program()), l.program.with_file_name("ld64.lld-18"));
        assert_eq!(l.elf_program().as_deref(), Some(Path::new("/usr/bin/ld.lld-18")));
    }
}
