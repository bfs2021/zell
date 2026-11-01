//! `zell build` / `run` / `check` / `test`: front end -> LLVM module ->
//! verification -> optimisation (new pass manager) -> object file(s) ->
//! link against `libzell_rt.a`: with the toolchain's bundled lld and link
//! inputs, or the system `clang` (see [`crate::crosslink`]).
//!
//! Two kinds of builds:
//!
//! - **Plain**: a single file or a directory without a manifest. The whole
//!   program becomes one object file, as before.
//! - **Package**: a project found through `zell.toml|json|yaml` (see
//!   [`crate::package`]). Sema runs over the target and its dependency
//!   closure with strict isolation; the resulting module is split into one
//!   module per package ([`split_module`]), each compiled to its own object
//!   `target/zell/<pkg>-<hash>.o`. The hash covers the package's split IR,
//!   the flags and the compiler, so an unchanged package is not recompiled.
//!
//! Every object gets one section per function and global (the equivalent
//! of `-ffunction-sections -fdata-sections`); `--release` links with
//! `-Wl,--gc-sections -s`, `--static` with `-static`.

use crate::codegen;
use crate::diagnostics::Diagnostics;
use crate::driver;
use crate::hir::Program;
use crate::package::{self, lock, PkgId, Project};
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{FileType, TargetMachine};
use inkwell::types::{AnyTypeEnum, BasicTypeEnum};
use inkwell::values::{AsValueRef, GlobalValue};
use inkwell::{GlobalVisibility, OptimizationLevel};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What `zell build` produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emit {
    /// A linked executable (default).
    Exe,
    /// Optimised textual LLVM IR (`.ll`).
    LlvmIr,
    /// An object file (`.o`).
    Obj,
}

/// Options of `zell build` (and `run`, `check`, `test`).
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// File, directory or project directory; `None` = find the project
    /// from the current directory.
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub emit: Emit,
    /// Optimisation level 0..=3.
    pub opt: u8,
    /// Release build: no `-rdynamic`, `--gc-sections`, stripped.
    pub release: bool,
    /// Fully static executable (`clang -static`).
    pub static_link: bool,
    /// Log cache decisions and tool invocations to stderr.
    pub verbose: bool,
    /// Fail instead of updating `zell.lock`.
    pub locked: bool,
    /// `--offline`: module dependencies only from `zell.lock` and the store.
    pub offline: bool,
    /// Build every workspace member.
    pub workspace: bool,
    /// Build these members (`-p name`).
    pub packages: Vec<String>,
    /// `--leak-check`: at exit the program reports objects still alive
    /// (with their type names unless `release`) and exits non-zero.
    pub leak_check: bool,
    /// `--no-error-trace`: errors do not record their return trace.
    pub no_error_trace: bool,
    /// `--target <triple|alias>`: the target of `@cfg`, file suffixes and
    /// `os.Name`/`os.Arch` (docs/PLATFORMS.md); `None` = the host. Only
    /// `check` accepts a target other than the host so far.
    pub target: Option<crate::target::Target>,
    /// `-g`: DWARF debug information in a `--release` build (debug builds
    /// always have it, docs/DEBUGGER.md).
    pub debug_info: bool,
    /// `--strip`: no debug information and a stripped executable.
    pub strip: bool,
    /// `--debug-agent` / `--no-debug-agent`: statement hooks and tables of
    /// the in-process debugger (docs/DEBUGGER.md stage 2); `None` = on in
    /// debug builds, off in release builds.
    pub debug_agent: Option<bool>,
}

impl BuildOptions {
    pub fn new(input: impl Into<PathBuf>) -> BuildOptions {
        BuildOptions {
            input: Some(input.into()),
            output: None,
            emit: Emit::Exe,
            opt: 0,
            release: false,
            static_link: false,
            verbose: false,
            locked: false,
            offline: false,
            workspace: false,
            packages: Vec::new(),
            leak_check: false,
            no_error_trace: false,
            target: None,
            debug_info: false,
            strip: false,
            debug_agent: None,
        }
    }

    /// Whether the build links the debug agent (see [`Self::debug_agent`]).
    pub fn debug_agent_on(&self) -> bool {
        self.debug_agent.unwrap_or(!self.release) && self.emit != Emit::Obj
    }

    /// Whether the build emits DWARF: debug builds unless `--strip`,
    /// release builds only with `-g`.
    pub fn emits_debug_info(&self) -> bool {
        !self.strip && (!self.release || self.debug_info)
    }
}

/// Parses `build`/`run`/`check`/`test` flags; returns the options and the
/// remaining arguments after `--`.
pub fn parse_args(args: &[String]) -> Result<(BuildOptions, Vec<String>), String> {
    let mut o = BuildOptions::new("");
    o.input = None;
    let mut opt = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--" => {
                rest = args[i + 1..].to_vec();
                break;
            }
            "-o" => {
                i += 1;
                o.output = Some(PathBuf::from(args.get(i).ok_or("-o needs a path")?));
            }
            "--target" => {
                i += 1;
                let t = args.get(i).ok_or("--target needs a triple (x86_64-unknown-linux-gnu) or an alias (linux-amd64)")?;
                o.target = Some(crate::target::Target::parse(t)?);
            }
            "-p" | "--package" => {
                i += 1;
                o.packages.push(args.get(i).ok_or("-p needs a package name")?.clone());
            }
            "--release" => o.release = true,
            "-g" | "--debug-info" => o.debug_info = true,
            "--strip" => o.strip = true,
            "--debug-agent" => o.debug_agent = Some(true),
            "--no-debug-agent" => o.debug_agent = Some(false),
            "--leak-check" => o.leak_check = true,
            "--deny-warnings" => crate::sema::cycles::set_deny_warnings(true),
            "--fix" => crate::sema::lint::update_config(|c| c.fix = true),
            _ if a.starts_with("--lint=") => {
                let v = &a["--lint=".len()..];
                let level = crate::sema::lint::Level::parse(v).filter(|l| matches!(l, crate::sema::lint::Level::Off | crate::sema::lint::Level::Is(crate::diagnostics::Severity::Warning | crate::diagnostics::Severity::Error)));
                let level = level.ok_or_else(|| format!("--lint takes off, warn or error, not `{v}`"))?;
                crate::sema::lint::update_config(|c| c.global = Some(level));
            }
            "--no-error-trace" => o.no_error_trace = true,
            "--static" => o.static_link = true,
            "--verbose" | "-v" => o.verbose = true,
            "--locked" => o.locked = true,
            "--offline" => o.offline = true,
            "--workspace" => o.workspace = true,
            "--emit=llvm-ir" => o.emit = Emit::LlvmIr,
            "--emit=obj" => o.emit = Emit::Obj,
            "--emit=exe" => o.emit = Emit::Exe,
            "-O0" | "-O1" | "-O2" | "-O3" => opt = Some(a[2..].parse::<u8>().unwrap()),
            _ if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            _ => {
                if o.input.is_some() {
                    return Err(format!("unexpected argument `{a}`"));
                }
                o.input = Some(PathBuf::from(a));
            }
        }
        i += 1;
    }
    o.opt = opt.unwrap_or(if o.release { 2 } else { 0 });
    if o.workspace && !o.packages.is_empty() {
        return Err("--workspace and -p are mutually exclusive".into());
    }
    if o.output.is_some() && o.workspace {
        return Err("-o cannot be used with --workspace".into());
    }
    Ok((o, rest))
}

/// Locates `libzell_rt.a`: `$ZELL_RUNTIME` (the archive or its
/// directory), an installed toolchain's `targets/<host triple>/` (and
/// `$ZELL_TARGETS/<host triple>/`), next to the compiler
/// (`<exe>/../../runtime`), then the repository the compiler was built from.
pub fn find_runtime() -> Result<PathBuf, String> {
    let mut cands = Vec::new();
    if let Ok(p) = std::env::var("ZELL_RUNTIME") {
        let p = PathBuf::from(p);
        cands.push(if p.is_dir() { p.join("libzell_rt.a") } else { p });
    }
    let host = crate::target::Target::host().triple;
    if let Some(b) = crate::toolchain::bundled_targets_dir() {
        cands.push(b.join(&host).join("libzell_rt.a"));
    }
    cands.push(crate::toolchain::targets_dir().join(&host).join("libzell_rt.a"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("../../runtime/libzell_rt.a"));
            cands.push(dir.join("../../../runtime/libzell_rt.a"));
        }
    }
    cands.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime/libzell_rt.a"));
    for c in &cands {
        if c.is_file() {
            return Ok(crate::paths::canonical(&c).unwrap_or(c.clone()));
        }
    }
    Err(format!(
        "cannot find libzell_rt.a (set ZELL_RUNTIME or run `make -C runtime`); looked in: {}",
        cands.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

/// The program that merges partial objects (`<prog> -r -nostdlib -o out
/// parts...`, [`crate::parcodegen`]): the system clang, else the bundled
/// `ld.lld`.
fn partial_linker() -> Option<String> {
    if crate::crosslink::linker_choice() != crate::crosslink::LinkerChoice::Clang {
        if let Some(p) = crate::crosslink::find_lld().filter(|l| l.multi).and_then(|l| l.elf_program()) {
            return Some(p.display().to_string());
        }
    }
    find_clang().ok().or_else(|| crate::crosslink::find_lld().and_then(|l| l.elf_program()).map(|p| p.display().to_string()))
}

fn find_clang() -> Result<String, String> {
    // Probed once per process (each probe is a process start).
    static CLANG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CLANG
        .get_or_init(|| ["clang-23", "clang"].into_iter().find(|c| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success())).map(str::to_string))
        .clone()
        .ok_or_else(|| "cannot find clang (tried clang-23, clang)".into())
}

/// Whether a `--release` host build links the program and the runtime with
/// ThinLTO: opt-in with `ZELL_LTO=1` (docs/PLATFORMS.md «LTO»). The
/// program is then emitted as bitcode and linked by `clang -flto=thin
/// -fuse-ld=lld` with the bitcode runtime of `make -C runtime LTO=1`
/// ([`lto_runtime`]) when there is one. Off by default: it needs the
/// system clang and lld, and the link takes several seconds.
fn lto_enabled(opts: &BuildOptions) -> bool {
    !tsan_enabled() && opts.release && !cfg!(target_os = "windows") && std::env::var("ZELL_LTO").is_ok_and(|v| v == "1")
}

/// The bitcode runtime archive beside the native one `rt`
/// (`runtime/libzell_rt.a` -> `runtime/build/lto/libzell_rt.a`, or
/// `<dir>/lto/libzell_rt.a` for an installed `<dir>/libzell_rt.a`).
fn lto_runtime(rt: &Path) -> Option<PathBuf> {
    let dir = rt.parent()?;
    [dir.join("build/lto/libzell_rt.a"), dir.join("lto/libzell_rt.a")].into_iter().find(|p| p.is_file())
}

/// Whether host links use lld (`ZELL_LINKER=lld`, Linux, `ld.lld` on PATH).
fn use_lld() -> bool {
    static LLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LLD.get_or_init(|| {
        cfg!(target_os = "linux") && std::env::var("ZELL_LINKER").is_ok_and(|v| v == "lld") && Command::new("ld.lld").arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

/// Default output path of a plain build: the input's stem (or directory
/// name) in the current directory, with the emit kind's extension.
fn default_output(input: &Path, emit: Emit) -> PathBuf {
    let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "out".into());
    with_emit_ext(PathBuf::from(stem), emit)
}

fn with_emit_ext(mut p: PathBuf, emit: Emit) -> PathBuf {
    match emit {
        Emit::Exe => {
            // `hello.exe` for Windows (an explicit `-o` is kept as given)
            p.as_mut_os_string().push(crate::target::current().exe_suffix());
        }
        Emit::LlvmIr => {
            p.set_extension("ll");
        }
        Emit::Obj => {
            p.set_extension("o");
        }
    }
    p
}

fn opt_level(n: u8) -> OptimizationLevel {
    match n {
        0 => OptimizationLevel::None,
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    }
}

fn log(opts: &BuildOptions, msg: impl AsRef<str>) {
    if opts.verbose {
        eprintln!("zell: {}", msg.as_ref());
    }
}

// -- what to build -------------------------------------------------------------

/// The resolved input of a command.
enum Plan {
    /// A single file or a directory without a manifest.
    Plain(PathBuf),
    /// A project and the members to build.
    Project(Project, Vec<PkgId>),
}

fn plan(opts: &BuildOptions) -> Result<Plan, String> {
    package::resolve::set_options(package::resolve::Options { offline: opts.offline, update: package::resolve::Update::None });
    let project = match &opts.input {
        Some(p) if p.is_file() => return Ok(Plan::Plain(p.clone())),
        Some(p) if p.is_dir() => {
            if package::manifest::manifest_in(p)?.is_none() {
                if opts.workspace || !opts.packages.is_empty() {
                    return Err(format!("{} has no manifest; --workspace and -p need a project", p.display()));
                }
                return Ok(Plan::Plain(p.clone()));
            }
            package::discover(Some(p))?.expect("manifest exists")
        }
        Some(p) => return Err(format!("cannot read {}: no such file or directory", p.display())),
        None => match package::discover(None)? {
            Some(p) => p,
            None => {
                return Err(format!(
                    "could not find {} in the current directory or any parent; pass a file or directory, or create a project with `zell init`",
                    package::manifest::MANIFEST_NAMES.join(", ")
                ))
            }
        },
    };
    let targets = if opts.workspace {
        project.members.clone()
    } else if !opts.packages.is_empty() {
        let mut t = Vec::new();
        for name in &opts.packages {
            match project.find_member(name) {
                Some(id) => t.push(id),
                None => {
                    let names: Vec<&str> = project.members.iter().map(|&m| project.packages[m].name.as_str()).collect();
                    return Err(format!("no workspace member named `{name}` (members: {})", names.join(", ")));
                }
            }
        }
        t
    } else if let Some(c) = project.current.or(project.root_package) {
        vec![c]
    } else {
        project.members.clone()
    };
    if targets.is_empty() {
        return Err(format!("{}: the workspace has no members", project.root_manifest.path.display()));
    }
    let targets = if opts.output.is_some() && targets.len() > 1 {
        // `-o` names one executable: the only binary among the targets.
        let bins: Vec<PkgId> = targets.iter().copied().filter(|&t| project.packages[t].is_bin()).collect();
        if bins.len() != 1 {
            return Err("-o needs a single package; select one with -p".into());
        }
        bins
    } else {
        targets
    };
    Ok(Plan::Project(project, targets))
}

fn sync_lock(project: &Project, opts: &BuildOptions) -> Result<(), String> {
    if lock::sync(project, opts.locked)? == lock::LockStatus::Written {
        log(opts, format!("wrote {}", project.lock_path().display()));
    }
    Ok(())
}

// -- commands -------------------------------------------------------------------

/// Makes `--target` the process-wide target ([`crate::target::set_current`]):
/// `@cfg`, `os.Name`, the LLVM triple and data layout of code generation,
/// and the runtime and linker (`crate::crosslink`) all follow it.
fn use_target(opts: &BuildOptions, codegen: bool) -> Result<(), String> {
    let _ = codegen;
    let Some(t) = &opts.target else { return Ok(()) };
    crate::target::set_current(t.clone());
    Ok(())
}

/// `zell build`: builds every selected target; returns the produced files
/// (executables, or nothing for libraries).
pub fn build(opts: &BuildOptions) -> Result<Vec<PathBuf>, String> {
    use_target(opts, true)?;
    match plan(opts)? {
        Plan::Plain(input) => Ok(vec![build_plain(&input, opts)?]),
        Plan::Project(project, targets) => {
            sync_lock(&project, opts)?;
            let mut out = Vec::new();
            for t in targets {
                out.extend(build_package(&project, t, opts)?);
            }
            Ok(out)
        }
    }
}

/// `zell run`: builds the program and runs it; returns its exit code.
pub fn run(mut opts: BuildOptions, args: &[String]) -> Result<i32, String> {
    opts.emit = Emit::Exe;
    use_target(&opts, true)?;
    let (exe, tmp) = match plan(&opts)? {
        Plan::Plain(input) => {
            let dir = std::env::temp_dir().join(format!("zell-run-{}", std::process::id()));
            std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            opts.output = Some(dir.join(format!("prog{}", crate::target::current().exe_suffix())));
            match build_plain(&input, &opts) {
                Ok(exe) => (exe, Some(dir)),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(e);
                }
            }
        }
        Plan::Project(project, targets) => {
            let bins: Vec<PkgId> = targets.iter().copied().filter(|&t| project.packages[t].is_bin()).collect();
            let t = match bins.as_slice() {
                [t] => *t,
                [] => return Err("nothing to run: the selected packages are libraries (no `entry`)".into()),
                _ => {
                    let names: Vec<&str> = bins.iter().map(|&b| project.packages[b].name.as_str()).collect();
                    return Err(format!("several binaries ({}); choose one with -p", names.join(", ")));
                }
            };
            sync_lock(&project, &opts)?;
            let exe = build_package(&project, t, &opts)?.ok_or("internal error: binary package produced no executable")?;
            (exe, None)
        }
    };
    let target = crate::target::current();
    let Some(mut runner) = crate::crosslink::run_command(&target, &exe) else {
        if let Some(d) = tmp {
            let _ = std::fs::remove_dir_all(d);
        }
        return Err(format!("cannot run a {} executable on this machine (use `zell build --target {}`)", target.triple, target.triple));
    };
    let status = runner.args(args).status().map_err(|e| format!("cannot run {}: {e}", exe.display()));
    if let Some(d) = tmp {
        let _ = std::fs::remove_dir_all(d);
    }
    let status = status?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }
    #[cfg(not(unix))]
    Ok(status.code().unwrap_or(1))
}

/// `zell check` (`dev`: `zell test`, dev-dependencies visible): runs the
/// front end; with `dump_hir`, prints the HIR of each target.
pub fn check(opts: &BuildOptions, dump_hir: bool, dev: bool) -> Result<usize, String> {
    use_target(opts, false)?;
    match plan(opts)? {
        Plan::Plain(input) => {
            let (files, program) = front_end_plain(&input)?;
            let _ = files;
            if dump_hir {
                print!("{}", program.dump());
            }
            Ok(1)
        }
        Plan::Project(project, targets) => {
            sync_lock(&project, opts)?;
            for &t in &targets {
                let program = front_end_package(&project, t, dev)?.1;
                if dump_hir {
                    print!("{}", program.dump());
                }
            }
            Ok(targets.len())
        }
    }
}

// -- front end ------------------------------------------------------------------

/// `ZELL_TIME=1`: prints the duration of each build phase to stderr
/// (compile-time measurements, benchmarks/README.md).
pub(crate) struct PhaseTimer(&'static str, std::time::Instant);

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        eprintln!("zell: time {:<10} {:9.1} ms", self.0, self.1.elapsed().as_secs_f64() * 1e3);
    }
}

pub(crate) fn phase(name: &'static str) -> Option<PhaseTimer> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ON.get_or_init(|| std::env::var_os("ZELL_TIME").is_some()).then(|| PhaseTimer(name, std::time::Instant::now()))
}

fn front_end_plain(input: &Path) -> Result<(Vec<crate::span::SourceFile>, Program), String> {
    let _t = phase("front-end");
    let paths = driver::collect_files(input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;
    if paths.is_empty() {
        return Err(format!("no .zl files in {}", input.display()));
    }
    let files = driver::load(&paths).map_err(|e| format!("cannot read source: {e}"))?;
    let mut diags = Diagnostics::new();
    let program = driver::check(&files, &mut diags);
    if !diags.is_empty() {
        eprint!("{}", diags.render_all_files(&files));
    }
    crate::sema::lint::apply_fixes_if_asked(&diags, &files);
    match program {
        Some(p) if !diags.has_errors() => Ok((files, p)),
        _ => Err("compilation failed".into()),
    }
}

fn front_end_package(project: &Project, target: PkgId, dev: bool) -> Result<(package::Checked, Program), String> {
    let _t = phase("front-end");
    let mut diags = Diagnostics::new();
    let mut checked = package::check(project, target, dev, &mut diags)?;
    if !diags.is_empty() {
        eprint!("{}", diags.render_all_files(&checked.files));
    }
    crate::sema::lint::apply_fixes_if_asked(&diags, &checked.files);
    match checked.program.take() {
        Some(p) if !diags.has_errors() => Ok((checked, p)),
        _ => Err(format!("could not compile `{}`", project.packages[target].name)),
    }
}

// -- plain builds -----------------------------------------------------------------

fn build_plain(input: &Path, opts: &BuildOptions) -> Result<PathBuf, String> {
    let (_, program) = crate::sema::debug_agent::with(opts.debug_agent_on(), || front_end_plain(input))?;
    let output = opts.output.clone().unwrap_or_else(|| default_output(input, opts.emit));
    let name = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "main".into());
    emit_plain(&program, &name, output, opts)
}

/// `zell test`: builds an executable from in-memory sources (the user's
/// files, with `main` renamed, plus the generated test driver; see
/// `main.rs`/`testrun.rs`). Paths stay those of the original files, so
/// diagnostics and `@embed` resolve as usual.
pub fn build_sources(files: Vec<crate::span::SourceFile>, output: &Path, opts: &BuildOptions) -> Result<PathBuf, String> {
    use_target(opts, true)?;
    let mut diags = Diagnostics::new();
    let program = crate::sema::debug_agent::with(opts.debug_agent_on(), || driver::check(&files, &mut diags));
    if !diags.is_empty() {
        eprint!("{}", diags.render_all_files(&files));
    }
    let program = match program {
        Some(p) if !diags.has_errors() => p,
        _ => return Err("compilation failed".into()),
    };
    let mut opts = opts.clone();
    opts.emit = Emit::Exe;
    emit_plain(&program, "test", output.to_path_buf(), &opts)
}

/// `zell test` in a project: like [`build_sources`], but `files` (the
/// target package's sources, then generated ones from `generated_from`
/// on) are checked together with the package's dependency closure,
/// dev-dependencies included, the way `zell build` resolves it. Without a
/// manifest it is [`build_sources`].
pub fn build_test_sources(files: Vec<crate::span::SourceFile>, generated_from: usize, output: &Path, opts: &BuildOptions) -> Result<PathBuf, String> {
    use_target(opts, true)?;
    let (project, targets) = match plan(opts)? {
        Plan::Plain(_) => return build_sources(files, output, opts),
        Plan::Project(p, t) => (p, t),
    };
    sync_lock(&project, opts)?;
    let target = *targets.first().ok_or("no package to test")?;
    let mut diags = Diagnostics::new();
    let own = package::TargetSources { files, generated_from };
    let checked = crate::sema::debug_agent::with(opts.debug_agent_on(), || package::check_with(&project, target, true, &mut diags, Some(own)))?;
    if !diags.is_empty() {
        eprint!("{}", diags.render_all_files(&checked.files));
    }
    let program = match checked.program {
        Some(p) if !diags.has_errors() => p,
        _ => return Err("compilation failed".into()),
    };
    let mut opts = opts.clone();
    opts.emit = Emit::Exe;
    emit_plain(&program, "test", output.to_path_buf(), &opts)
}

/// Code generation, optimisation and linking of a checked plain program.
fn emit_plain(program: &Program, name: &str, output: PathBuf, opts: &BuildOptions) -> Result<PathBuf, String> {
    if program.entry.is_none() && opts.emit == Emit::Exe {
        return Err("no `main` function: cannot build an executable".into());
    }
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let ctx = Context::create();
    let module = lower(program, &ctx, name, &output, opts)?;
    let machine = codegen::host_machine(opt_level(opts.opt))?;
    set_sections(&module);
    optimise(&module, &machine, opts.opt)?;
    match opts.emit {
        Emit::LlvmIr => {
            module.print_to_file(&output).map_err(|e| format!("cannot write {}: {}", output.display(), e.to_string()))?;
            return Ok(output);
        }
        Emit::Obj => {
            write_object(&machine, &module, &output)?;
            return Ok(output);
        }
        Emit::Exe => {}
    }
    let libs = module_link_libs(&module);
    let lto = lto_enabled(opts);
    let obj = output.with_extension(format!("{}.{}", std::process::id(), if lto { "bc" } else { "o" }));
    if lto {
        // Bitcode: the ThinLTO link inlines runtime helpers into the program.
        let _t = phase("bitcode");
        if !module.write_bitcode_to_path(&obj) {
            return Err(format!("cannot write {}", obj.display()));
        }
    } else {
        if crate::lines::supported(&crate::target::current()) {
            crate::lines::expose(&module, None);
        }
        write_object_par(&machine, &module, &obj, opts)?;
    }
    let r = link(&[obj.clone()], &output, &libs, opts, &crate::lines::Names::of(program));
    if r.is_ok() && opts.emits_debug_info() {
        macos_dsym(&output, opts);
    }
    let _ = std::fs::remove_file(&obj);
    r?;
    Ok(output)
}

/// On macOS the DWARF stays in the object files (the executable has only a
/// debug map), so before the temporary object is removed `dsymutil` copies
/// it into `<output>.dSYM`, where lldb finds it. Without dsymutil the build
/// still succeeds, without debug info.
fn macos_dsym(output: &Path, opts: &BuildOptions) {
    if crate::target::current().os != "macos" {
        return;
    }
    let mut tools: Vec<String> = ["dsymutil", "llvm-dsymutil", "llvm-dsymutil-23"].iter().map(|s| s.to_string()).collect();
    if let Some(p) = std::env::var_os("LLVM_SYS_180_PREFIX") {
        tools.push(Path::new(&p).join("bin/dsymutil").to_string_lossy().into_owned());
    }
    for t in &tools {
        if let Ok(out) = Command::new(t).arg(output).output() {
            if out.status.success() {
                log(opts, format!("debug info: {}.dSYM ({t})", output.display()));
            } else {
                eprintln!("warning: {t} failed, {} has no debug info:\n{}", output.display(), String::from_utf8_lossy(&out.stderr));
            }
            return;
        }
    }
    eprintln!("warning: dsymutil not found, {} has no debug info (install the Xcode command line tools)", output.display());
}

/// Codegen + verification of the whole program.
fn lower<'ctx>(program: &Program, ctx: &'ctx Context, name: &str, output: &Path, opts: &BuildOptions) -> Result<Module<'ctx>, String> {
    let _t = phase("codegen");
    let leak_check = match (opts.leak_check, opts.release) {
        (false, _) => 0,
        (true, true) => 1,
        (true, false) => 2,
    };
    let module = match codegen::emit_with(program, ctx, name, codegen::Options { leak_check, no_error_trace: opts.no_error_trace, debug_info: opts.emits_debug_info(), line_tables: crate::lines::supported(&crate::target::current()), optimized: opts.opt > 0, inline_rc: std::env::var_os("ZELL_NO_INLINE_RC").is_none(), debug_agent: opts.debug_agent_on() }) {
        Ok(m) => m,
        Err(errs) => {
            for e in &errs {
                eprintln!("error: {e}");
            }
            return Err("code generation failed".into());
        }
    };
    keep_frame_pointers(ctx, &module);
    if let Err(e) = module.verify() {
        let ll = output.with_extension("ll");
        let _ = module.print_to_file(&ll);
        return Err(format!("internal compiler error: LLVM module verification failed (IR written to {}):\n{}", ll.display(), e.to_string()));
    }
    Ok(module)
}

/// `ZELL_SANITIZE=thread`: the program is instrumented by LLVM's
/// ThreadSanitizer pass (every defined function gets `sanitize_thread`, so
/// its plain accesses and atomics, e.g. the inline RC and mutex CAS of
/// codegen, become `__tsan_*` calls) and linked with `-fsanitize=thread`.
/// Link it with a runtime built by `make -C runtime tsan-lib`
/// (`ZELL_RUNTIME=runtime/build/tsan`); `scripts/tsan-run.sh` does both.
pub fn tsan_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("ZELL_SANITIZE").is_ok_and(|v| v == "thread"))
}

/// Instruments `module` for ThreadSanitizer ([`tsan_enabled`]).
fn instrument_tsan(module: &Module, machine: &TargetMachine) -> Result<(), String> {
    let attr = module.get_context().create_enum_attribute(inkwell::attributes::Attribute::get_named_enum_kind_id("sanitize_thread"), 0);
    for f in module.get_functions() {
        if f.count_basic_blocks() > 0 {
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, attr);
        }
    }
    module.run_passes("tsan-module,function(tsan)", machine, PassBuilderOptions::create()).map_err(|e| format!("internal compiler error: tsan instrumentation failed: {}", e.to_string()))
}

/// Every defined function keeps its frame pointer (`"frame-pointer"="all"`,
/// like the runtime's `-fno-omit-frame-pointer`): the diag CPU profiler
/// walks stacks through them (docs/FRAMEWORK.md section 5).
fn keep_frame_pointers(ctx: &Context, module: &Module) {
    let fp = ctx.create_string_attribute("frame-pointer", "all");
    for f in module.get_functions() {
        if f.count_basic_blocks() > 0 {
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, fp);
        }
    }
}

fn optimise(module: &Module, machine: &TargetMachine, opt: u8) -> Result<(), String> {
    let _t = phase("optimise");
    // Code generation emits every stdlib function a program could reach
    // (over 1000 for hello); drop the unreferenced ones first, so neither
    // the optimisation pipeline nor the code generator sees them.
    module.run_passes("globaldce", machine, PassBuilderOptions::create()).map_err(|e| format!("internal compiler error: globaldce failed: {}", e.to_string()))?;
    let pipeline = format!("default<O{}>", opt.min(3));
    module.run_passes(&pipeline, machine, PassBuilderOptions::create()).map_err(|e| format!("internal compiler error: optimisation failed: {}", e.to_string()))?;
    codegen::dedup_declares(module);
    codegen::coro_frame_fixup(module);
    codegen::lower_unwind(module);
    if tsan_enabled() {
        instrument_tsan(module, machine)?;
    }
    // `ZELL_VERIFY_UNWIND=1`: verify the module after the lowering of the
    // landing pads (codegen/unwind_lower.rs; the compiler's own tests set it).
    if std::env::var_os("ZELL_VERIFY_UNWIND").is_some() {
        if let Err(e) = module.verify() {
            return Err(format!("internal compiler error: unwind lowering produced invalid IR:\n{}", e.to_string()));
        }
    }
    Ok(())
}

/// [`write_object`] for an executable's object: on several threads when
/// the module is large ([`crate::parcodegen`]).
fn write_object_par(machine: &TargetMachine, module: &Module, path: &Path, opts: &BuildOptions) -> Result<(), String> {
    {
        let _t = phase("object-par");
        match crate::parcodegen::write_object_parallel(module, opt_level(opts.opt), path, partial_linker) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => log(opts, format!("parallel code generation failed, retrying serially: {e}")),
        }
    }
    write_object(machine, module, path)
}

fn write_object(machine: &TargetMachine, module: &Module, path: &Path) -> Result<(), String> {
    let _t = phase("object");
    machine.write_to_file(module, FileType::Object, path).map_err(|e| format!("cannot write {}: {}", path.display(), e.to_string()))
}

/// The system libraries (`stdlib::LINK_LIBS`) that the runtime functions
/// declared in `module` need.
fn module_link_libs(module: &Module) -> Vec<&'static str> {
    let names: Vec<String> = module.get_functions().filter(|f| f.count_basic_blocks() == 0).map(|f| f.get_name().to_string_lossy().into_owned()).collect();
    crate::sema::stdlib::link_libs(names.iter().map(|s| s.as_str()))
}

/// Links `objs` with the runtime (and the system libraries `libs`) into
/// the executable `output`: with the bundled lld when the runtime comes
/// with link inputs (an installed toolchain, [`crate::crosslink`]), else
/// with the system clang driver.
fn link(objs: &[PathBuf], output: &Path, libs: &[&str], opts: &BuildOptions, names: &crate::lines::Names) -> Result<(), String> {
    let _t = phase("link");
    let target = crate::target::current();
    let clang = find_clang().unwrap_or_else(|_| "clang".into());
    let strip = opts.strip || (opts.release && !opts.debug_info);
    let lo = crate::crosslink::LinkOpts { release: opts.release, strip, static_link: opts.static_link, export_dynamic: !opts.release && !opts.static_link, clang: &clang };
    let cross = !target.is_host() || target.os == "windows";
    let lto = !cross && lto_enabled(opts);
    let rt = if cross {
        // no runtime for the target: say first what else the clang link would lack (the macOS SDK, ...)
        crate::crosslink::find_runtime(&target).or_else(|e| crate::crosslink::preflight(&target).and(Err(e)))?
    } else if lto {
        let rt = find_runtime()?;
        match lto_runtime(&rt) {
            Some(bc) => bc,
            None => {
                log(opts, format!("no bitcode runtime beside {} (make -C runtime LTO=1): ThinLTO over the program only", rt.display()));
                rt
            }
        }
    } else {
        find_runtime()?
    };
    // The line table (ABI.md 8.2): one more object, built from the DWARF of
    // the program's objects (not of bitcode: ThinLTO builds have none).
    let mut objs = objs.to_vec();
    let lines_obj = output.with_extension(format!("{}.lines.o", std::process::id()));
    if crate::lines::supported(&target) && !lto {
        let _t = phase("lines");
        let machine = codegen::host_machine(OptimizationLevel::None)?;
        if let Some(n) = crate::lines::write_object(&objs, names, &machine, &lines_obj)? {
            log(opts, format!("line table: {n} bytes"));
            objs.push(lines_obj.clone());
        }
    }
    let r = link_command(&objs, &rt, output, libs, opts, &target, &clang, &lo, cross, lto).and_then(|(mut cmd, linker)| {
        log(opts, format!("linking {} for {} ({} object(s), runtime {}) with {linker}{}", output.display(), target.triple, objs.len(), rt.display(), if lto { " (ThinLTO)" } else { "" }));
        let out = cmd.output().map_err(|e| format!("cannot run {}: {e}", cmd.get_program().to_string_lossy()))?;
        if !out.status.success() {
            return Err(format!("linking failed:\n{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout)));
        }
        Ok(())
    });
    let _ = std::fs::remove_file(&lines_obj);
    r
}

/// The link command of [`link`] and the linker's name.
#[allow(clippy::too_many_arguments)]
fn link_command(objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], opts: &BuildOptions, target: &crate::target::Target, clang: &str, lo: &crate::crosslink::LinkOpts, cross: bool, lto: bool) -> Result<(Command, String), String> {
    let strip = lo.strip;
    Ok(if lto {
        (host_clang_command(objs, rt, output, libs, opts, &find_clang()?, true, strip), clang.to_string())
    } else if let Some(r) = crate::crosslink::bundled_link(target, objs, rt, output, libs, lo) {
        r?
    } else if cross {
        // Cross link with the system clang (docs/PLATFORMS.md): the
        // target's runtime and C library (on a Windows host too: the MinGW
        // runtime and clang driver).
        find_clang()?;
        crate::crosslink::link_command(target, objs, rt, output, libs, lo)?
    } else {
        (host_clang_command(objs, rt, output, libs, opts, &find_clang()?, false, strip), clang.to_string())
    })
}

/// The host link through the system clang driver (a runtime without
/// bundled link inputs, e.g. `runtime/libzell_rt.a` of the repository).
fn host_clang_command(objs: &[PathBuf], rt: &Path, output: &Path, libs: &[&str], opts: &BuildOptions, clang: &str, lto: bool, strip: bool) -> Command {
    let mut cmd = Command::new(clang);
    cmd.args(objs).arg(rt);
    for l in libs {
        cmd.arg(format!("-l{l}"));
    }
    cmd.arg("-lpthread").arg("-lm").arg("-o").arg(output);
    if tsan_enabled() {
        cmd.arg("-fsanitize=thread");
    }
    if lto {
        // ThinLTO over the program's bitcode and the bitcode runtime
        // (`lto_enabled`); lld runs the LTO backend itself.
        cmd.arg("-flto=thin").arg("-fuse-ld=lld").arg("-O2");
    } else if use_lld() {
        // Opt-in: lld links faster and lays out smaller executables (no
        // page padding between segments: hello 10 KB instead of 15 KB), but
        // its code layout cost 15% on benchmarks/fib, so it is not the default.
        cmd.arg("-fuse-ld=lld");
    }
    // ld64 (macOS) spells these differently and links no static executables.
    let macos = cfg!(target_os = "macos");
    if opts.release {
        cmd.arg(if macos { "-Wl,-dead_strip" } else { "-Wl,--gc-sections" });
    } else if !opts.static_link {
        cmd.arg("-rdynamic");
    }
    // Release builds are stripped unless `-g` asked for debug info.
    if strip {
        cmd.arg(if macos { "-Wl,-S,-x" } else { "-s" });
    }
    if opts.static_link && !macos {
        cmd.arg("-static");
    }
    cmd
}

// -- package builds ---------------------------------------------------------------

/// Builds member `target`: its executable (binary) or just the objects of
/// its closure (library, returns `None`).
fn build_package(project: &Project, target: PkgId, opts: &BuildOptions) -> Result<Option<PathBuf>, String> {
    let pkg = &project.packages[target];
    let (checked, program) = crate::sema::debug_agent::with(opts.debug_agent_on(), || front_end_package(project, target, false))?;
    let profile = if opts.release { "release" } else { "debug" };
    let out_dir = project.target_dir().join(profile);
    let output = opts.output.clone().unwrap_or_else(|| with_emit_ext(out_dir.join(&pkg.name), opts.emit));
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let ctx = Context::create();
    let module = lower(&program, &ctx, &pkg.name, &output, opts)?;
    let machine = codegen::host_machine(opt_level(opts.opt))?;
    if opts.emit != Emit::Exe {
        // Whole-program artefacts are not split.
        set_sections(&module);
        optimise(&module, &machine, opts.opt)?;
        if opts.emit == Emit::LlvmIr {
            module.print_to_file(&output).map_err(|e| format!("cannot write {}: {}", output.display(), e.to_string()))?;
        } else {
            write_object(&machine, &module, &output)?;
        }
        return Ok(Some(output));
    }
    let libs = module_link_libs(&module);
    let cache = project.target_dir().join("zell");
    std::fs::create_dir_all(&cache).map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
    let compiler = compiler_id();
    let mut objs = Vec::new();
    for (i, &id) in checked.closure.iter().enumerate() {
        let name = &project.packages[id].name;
        let part = split_module(&module, &program, &checked.file_pkg, i, name)?;
        set_sections(&part);
        let mut h = Sha256::new();
        h.update(compiler.as_bytes());
        h.update(format!("\0opt={}\0release={}\0leak={}\0trace={}\0debug={}\0triple={}\0inline_rc={}\0open_defer={}\0lines=2\0", opts.opt, opts.release, opts.leak_check, !opts.no_error_trace, opts.emits_debug_info(), machine.get_triple(), std::env::var_os("ZELL_NO_INLINE_RC").is_none(), std::env::var_os("ZELL_NO_OPEN_DEFER").is_none()).as_bytes());
        h.update(format!("agent={}\0tsan={}\0", opts.debug_agent_on(), tsan_enabled()).as_bytes());
        h.update(part.print_to_string().to_bytes());
        let key = lock::hex(&h.finalize()[..8]);
        let obj = cache.join(format!("{name}-{key}.o"));
        if obj.is_file() {
            log(opts, format!("cache hit: {name} ({})", obj.display()));
        } else {
            log(opts, format!("compiling {name} -> {}", obj.display()));
            optimise(&part, &machine, opts.opt)?;
            if crate::lines::supported(&crate::target::current()) {
                crate::lines::expose(&part, Some(name));
            }
            let tmp = obj.with_extension(format!("{}.tmp", std::process::id()));
            write_object_par(&machine, &part, &tmp, opts)?;
            std::fs::rename(&tmp, &obj).map_err(|e| format!("cannot write {}: {e}", obj.display()))?;
        }
        objs.push(obj);
    }
    if !pkg.is_bin() {
        log(opts, format!("library {} checked and compiled ({} object(s))", package::describe(pkg), objs.len()));
        return Ok(None);
    }
    link(&objs, &output, &libs, opts, &crate::lines::Names::of(&program))?;
    Ok(Some(output))
}

/// A digest identifying this compiler binary (version, size, mtime), so a
/// rebuilt compiler does not reuse objects of an older one.
fn compiler_id() -> String {
    let mut id = format!("zell {}", env!("CARGO_PKG_VERSION"));
    if let Some(m) = std::env::current_exe().ok().and_then(|e| std::fs::metadata(e).ok()) {
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        id.push_str(&format!(" {} {mtime}", m.len()));
    }
    id
}

/// The part of the whole-program `module` that belongs to package `pkg`
/// (a position in the build closure; `file_pkg` maps HIR files to it):
///
/// - user functions and globals of `pkg` keep their bodies/initialisers
///   and become external symbols (`zell_<namespace>__<name>` is unique
///   because namespace roots are unique);
/// - those of other packages become external declarations;
/// - `main` stays only in the package that defines the entry point;
/// - compiler-generated helpers (type infos, vtables, rc/drop helpers,
///   thunks) stay in every part as `linkonce_odr` in a comdat, so the
///   linker keeps one copy and run-time type identity (the address of a
///   type info) holds across objects. Names agree because every part is
///   cut from the same module, and a cached object is reused only when its
///   IR is byte-identical to the current part. Unreferenced helpers are
///   then dropped by `globaldce`.
fn split_module<'ctx>(module: &Module<'ctx>, program: &Program, file_pkg: &[usize], pkg: usize, name: &str) -> Result<Module<'ctx>, String> {
    let _t = phase("split");
    let part = module.clone();
    part.set_name(name);
    part.set_source_file_name(name);
    let mut user = std::collections::HashSet::new();
    for f in &program.funcs {
        user.insert(f.symbol.clone());
        let Some(fv) = part.get_function(&f.symbol) else { continue };
        if file_pkg[f.file] == pkg {
            fv.set_linkage(Linkage::External);
        } else if fv.count_basic_blocks() > 0 {
            // Replace the definition by a declaration of the same type.
            let decl = part.add_function(&format!("{}.decl", f.symbol), fv.get_type(), Some(Linkage::External));
            fv.replace_all_uses_with(decl);
            unsafe { fv.delete() };
            decl.as_global_value().set_name(&f.symbol);
        }
    }
    for g in &program.globals {
        user.insert(g.symbol.clone());
        let Some(gv) = part.get_global(&g.symbol) else { continue };
        gv.set_linkage(Linkage::External);
        if file_pkg[g.file] != pkg {
            // SAFETY: a null initializer turns the definition into a declaration.
            unsafe { llvm_sys::core::LLVMSetInitializer(gv.as_value_ref(), std::ptr::null_mut()) };
        }
    }
    let entry_pkg = program.entry.map(|e| file_pkg[program.funcs[e.0 as usize].file]);
    if entry_pkg != Some(pkg) {
        if let Some(main) = part.get_function("main") {
            unsafe { main.delete() };
        }
    }
    let make_shared = |gv: GlobalValue<'ctx>| {
        let n = gv.get_name().to_string_lossy().to_string();
        if gv.is_declaration() || user.contains(&n) || gv.get_linkage() != Linkage::Internal {
            return;
        }
        gv.set_linkage(Linkage::LinkOnceODR);
        gv.set_visibility(GlobalVisibility::Hidden);
        if crate::target::current().object_format() != crate::target::ObjectFormat::MachO {
            gv.set_comdat(part.get_or_insert_comdat(&n));  /* Mach-O has no comdats: weak definitions suffice */
        }
    };
    for f in part.get_functions() {
        make_shared(f.as_global_value());
    }
    for g in part.get_globals() {
        make_shared(g);
    }
    let machine = codegen::host_machine(OptimizationLevel::None)?;
    part.run_passes("globaldce", &machine, PassBuilderOptions::create()).map_err(|e| format!("internal compiler error: globaldce failed: {}", e.to_string()))?;
    if let Err(e) = part.verify() {
        return Err(format!("internal compiler error: split module of `{name}` is invalid:\n{}", e.to_string()));
    }
    Ok(part)
}

/// One section per defined function and global (`.text.<name>`,
/// `.data.<name>`, `.bss.<name>`, `.rodata.<name>`, `.data.rel.ro.<name>`),
/// like clang's `-ffunction-sections -fdata-sections`, so the linker's
/// `--gc-sections` can drop unused code and data.
fn set_sections(module: &Module) {
    if crate::target::current().object_format() == crate::target::ObjectFormat::MachO {
        return;  /* Mach-O names sections "segment,section"; ld64 -dead_strip works per symbol (subsections) */
    }
    for f in module.get_functions() {
        if f.count_basic_blocks() > 0 {
            let n = f.get_name().to_string_lossy().to_string();
            f.set_section(Some(&format!(".text.{n}")));
        }
    }
    for g in module.get_globals() {
        if g.is_declaration() {
            continue;
        }
        let n = g.get_name().to_string_lossy().to_string();
        if n.starts_with("llvm.") {
            continue;
        }
        let zero = g.get_initializer().is_some_and(|i| unsafe { llvm_sys::core::LLVMIsNull(i.as_value_ref()) != 0 });
        let prefix = if g.is_constant() {
            if has_pointers(g.get_value_type()) {
                ".data.rel.ro"
            } else {
                ".rodata"
            }
        } else if zero {
            ".bss"
        } else {
            ".data"
        };
        g.set_section(Some(&format!("{prefix}.{n}")));
    }
}

/// Whether values of `t` may hold addresses (which need relocations, so
/// read-only data goes to `.data.rel.ro`).
fn has_pointers(t: AnyTypeEnum) -> bool {
    fn basic(t: BasicTypeEnum) -> bool {
        match t {
            BasicTypeEnum::PointerType(_) => true,
            BasicTypeEnum::StructType(s) => s.get_field_types().into_iter().any(basic),
            BasicTypeEnum::ArrayType(a) => basic(a.get_element_type()),
            BasicTypeEnum::VectorType(v) => basic(v.get_element_type()),
            BasicTypeEnum::ScalableVectorType(v) => basic(v.get_element_type()),
            BasicTypeEnum::IntType(_) | BasicTypeEnum::FloatType(_) => false,
        }
    }
    match BasicTypeEnum::try_from(t) {
        Ok(b) => basic(b),
        Err(_) => true,
    }
}
