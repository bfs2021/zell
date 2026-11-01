use std::io::Write;
use std::path::PathBuf;
use std::process::exit;

mod testrun;

use zell::diagnostics::Diagnostics;
use zell::package::{self, Format};

const USAGE: &str = "\
usage: zell <command> [args]

commands:
  init [dir] [--name n] [--format toml|json|yaml] [--lib] [--agents list]
                                 create a package (manifest + src/main.zl or src/lib.zl);
                                 --agents claude,codex,gemini,cursor,windsurf,cline,aider,zed,
                                 copilot,claude-skills or all: AI agent configuration (zell-mcp,
                                 instructions; also in an existing package, see agents/README.md)
  build [path] [options]         build the project found from the current directory,
                                 a project directory, a plain directory or a single file
  run [path] [options] [-- args] build and run
  check [path] [--dump-hir] [options]
                                 type-check without generating code
  test [path] [--filter re] [--json] [--bench] [-v] [--parallel n] [options]
                                 run the @test functions (and @test benchmarks
                                 taking *testing.B with --bench), see testing
                                 (--debug [--port N] [--wait]: all selected tests in one
                                 process under the debug agent, for an editor to attach)
  doc [path|namespace|symbol] [--html dir] [--json] [--check [--min N]] [--private]
                                 documentation from doc comments: `zell doc strings.Split`,
                                 `zell doc std --html out/` (the stdlib reference), --check: coverage
  debug [--lldb|--gdb] [path] [options] [-- args]
                                 build with debug info and start lldb (default) or gdb
                                 with the Zell formatters loaded (docs/DEBUGGER.md)
                                 (--source <file>: run debugger commands, --batch: then exit)
  debug --formatters lldb|gdb    print the path of the formatter script
  debug run [path] [--port N] [--headless [--wait]] [options] [-- args]
                                 build with the in-process debug agent and debug it
                                 (gdb-like commands; --headless: only start the agent,
                                 for an editor to attach)
  debug attach [host:]port       debug a program started with ZELL_DEBUG=[host]:port
  install [--offline] [--locked] fetch module dependencies into the store and .zell/deps/,
                                 write zell.lock (--offline: only from the lock and the store)
  update [module|name ...]       newest versions within the constraints (all, or the named)
  tree                           print the dependency graph
  why <module|name>              print every chain of requirements leading to a package
  vendor                         copy all modules into vendor/ (builds then use it)
  store verify|prune|path        check, clean up or locate the module store
  env [NAME ...]                 print ZELL_STD, ZELL_HOME, ZELL_STORE, ZELL_LINKER, ZELL_LTO, ZELL_TARGETS, ...
  doctor                         check that the toolchain (linker, std, every target's link inputs) is complete
  proxy serve --dir <cache> [--addr host:port] [--upstream direct|off|<url>] [--sumdb]
                                 serve a module proxy (static layout) and checksum log
  manifest schema                print the manifest JSON Schema
  manifest convert <toml|json|yaml> [dir]
                                 rewrite the manifest in another format
  data migration add <Name> | update [target] | script | list
                                 data migrations: diff the model against
                                 migrations/snapshot.json, apply, print SQL (zell data help)
  parse <file.zl>                print the AST
  fmt [paths] [--check] [--diff] [--stdin]
                                 format .zl files in place (canonical style)
  lsp                            run the language server on stdio (execs zell-lsp)
  tool pprof [args] <profile>    inspect a diag profile (runs `go tool pprof`)
  target list                    known targets and where their runtime is
  target add <target>... [--url template]
                                 download prebuilt runtimes into ~/.zell/targets
                                 ($ZELL_TARGETS_URL; docs/PLATFORMS.md)

  version                      print the compiler version

build options:
  -o <path>            output file (single package)
  --release            -O2, no -rdynamic, --gc-sections, stripped
  -g                   keep DWARF debug info in a --release build (debug builds have it)
  --strip              no debug info, stripped executable
  -O0..-O3             optimisation level
  --static             fully static executable
  ZELL_LTO=1           (env) ThinLTO of a --release build with the runtime (docs/PLATFORMS.md)
  --leak-check         report objects still alive at exit, exit non-zero
  --no-error-trace     do not record error return traces (errors.Trace is empty)
  --deny-warnings      treat warnings (W0001 reference cycles, ...) as errors
  --lint=off|warn|error  disable the lints (L0001..., docs/TOOLING.md), keep their levels, or make them errors
  --fix                apply the fixes of lint findings to the source files
  --emit=exe|obj|llvm-ir
  --workspace          all workspace members
  -p, --package <name> one workspace member (repeatable)
  --target <t>         target for @cfg, file suffixes and os.Name/os.Arch
                       (triple or alias such as linux-amd64; non-host only for check)
  --locked             fail if zell.lock would change
  --offline            module dependencies only from zell.lock and the store
  -v, --verbose        log the object cache and the linker";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || matches!(args[1].as_str(), "-h" | "--help" | "help") {
        eprintln!("{USAGE}");
        exit(if args.len() < 2 { 2 } else { 0 });
    }
    let rest = &args[2..];
    // `zell <command> --help` (commands with their own help handle it).
    if matches!(rest.first().map(String::as_str), Some("-h" | "--help")) && !matches!(args[1].as_str(), "doc" | "fmt" | "data") {
        match command_usage(&args[1]) {
            Some(u) => {
                println!("{u}");
                exit(0);
            }
            None => usage_error(&format!("unknown command `{}`", args[1])),
        }
    }
    match args[1].as_str() {
        "version" | "--version" | "-V" => {
            println!("zell {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH);
        }
        "parse" => match rest.first() {
            Some(p) => parse(p),
            None => usage_error("parse needs a file"),
        },
        "test" => exit(testrun::run(rest)),
        "doc" => exit(zell::doc::run(rest)),
        "check" => {
            let dump = rest.iter().any(|a| a == "--dump-hir");
            let flags: Vec<String> = rest.iter().filter(|a| *a != "--dump-hir").cloned().collect();
            let opts = parse_build_args(&flags).0;
            match zell::build::check(&opts, dump, false) {
                Ok(_) => {}
                Err(e) => fail(&e),
            }
        }
        "build" => {
            let opts = parse_build_args(rest).0;
            if let Err(e) = zell::build::build(&opts) {
                fail(&e);
            }
        }
        "run" => {
            let (opts, extra) = parse_build_args(rest);
            match zell::build::run(opts, &extra) {
                Ok(code) => exit(code),
                Err(e) => fail(&e),
            }
        }
        "debug" => debug(rest),
        "init" => init(rest),
        "manifest" => manifest(rest),
        "lsp" => lsp(rest),
        "tool" => tool(rest),
        "target" => print(zell::toolchain::run(rest)),
        "doctor" => print(zell::toolchain::doctor(rest)),
        "fmt" => exit(zell::fmt::cli::run(rest)),
        "data" => match zell::datacli::run(rest) {
            Ok(code) => exit(code),
            Err(e) => fail(&e),
        },
        "install" => done(package::cli::install(rest)),
        "update" => done(package::cli::update(rest)),
        "vendor" => done(package::cli::vendor(rest)),
        "tree" => print(package::cli::tree(rest)),
        "why" => print(package::cli::why(rest)),
        "store" => print(package::cli::store_cmd(rest)),
        "env" => print(package::cli::env(rest)),
        "proxy" => done(package::cli::proxy(rest)),
        cmd => {
            eprintln!("error: command `{cmd}` is not implemented yet\n{USAGE}");
            exit(2);
        }
    }
}

fn done(r: Result<(), String>) {
    if let Err(e) = r {
        fail(&e);
    }
}

fn print(r: Result<String, String>) {
    match r {
        Ok(out) => print!("{out}"),
        Err(e) => fail(&e),
    }
}

fn fail(e: &str) -> ! {
    eprintln!("error: {e}");
    exit(1);
}

/// The lines of [`USAGE`] describing `cmd` (`init`, `debug`, ...) plus the
/// build options for the commands that take them; `None` for an unknown
/// command.
fn command_usage(cmd: &str) -> Option<String> {
    let (commands, options) = USAGE.split_once("\nbuild options:\n")?;
    let mut out = String::new();
    let mut inside = false;
    for line in commands.lines() {
        let is_entry = line.starts_with("  ") && !line.starts_with("   ");
        if is_entry {
            let first = line.trim_start().split([' ', '\t']).next().unwrap_or("");
            inside = first == cmd;
        } else if !line.starts_with("   ") {
            inside = false;
        }
        if inside {
            out.push_str(&format!("{line}\n"));
        }
    }
    if out.is_empty() {
        return None;
    }
    let out = format!("usage: zell {cmd} ...\n\n{out}");
    if matches!(cmd, "build" | "run" | "check" | "test" | "debug") {
        return Some(format!("{out}\nbuild options:\n{options}"));
    }
    Some(out.trim_end().to_string())
}

fn usage_error(e: &str) -> ! {
    eprintln!("error: {e}\n\n{USAGE}");
    exit(2);
}

fn parse_build_args(args: &[String]) -> (zell::build::BuildOptions, Vec<String>) {
    zell::build::parse_args(args).unwrap_or_else(|e| usage_error(&e))
}

fn init(args: &[String]) {
    let mut dir: Option<PathBuf> = None;
    let mut name = None;
    let mut format = Format::Toml;
    let mut lib = false;
    let mut agents = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lib" => lib = true,
            "--agents" => {
                i += 1;
                let list = args.get(i).unwrap_or_else(|| usage_error("--agents needs a list such as claude,codex (or all)"));
                agents = Some(package::agents::parse_list(list).unwrap_or_else(|e| usage_error(&e)));
            }
            a if a.starts_with("--agents=") => agents = Some(package::agents::parse_list(&a["--agents=".len()..]).unwrap_or_else(|e| usage_error(&e))),
            "--name" => {
                i += 1;
                name = Some(args.get(i).cloned().unwrap_or_else(|| usage_error("--name needs a value")));
            }
            "--format" => {
                i += 1;
                let f = args.get(i).unwrap_or_else(|| usage_error("--format needs toml, json or yaml"));
                format = Format::parse(f).unwrap_or_else(|| usage_error(&format!("unknown manifest format `{f}` (toml, json, yaml)")));
            }
            a if a.starts_with("--format=") => {
                let f = &a["--format=".len()..];
                format = Format::parse(f).unwrap_or_else(|| usage_error(&format!("unknown manifest format `{f}` (toml, json, yaml)")));
            }
            a if a.starts_with('-') => usage_error(&format!("unknown option `{a}`")),
            a => {
                if dir.is_some() {
                    usage_error(&format!("unexpected argument `{a}`"));
                }
                dir = Some(PathBuf::from(a));
            }
        }
        i += 1;
    }
    // `zell init name` creates ./name; the package is named after the directory.
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    // With --agents, an existing package only gets the agent files.
    let existing = agents.is_some() && !package::manifest::manifests_in(&dir).is_empty();
    if !existing {
        match package::init::init(&dir, name.as_deref(), format, lib) {
            Ok(files) => {
                for f in files {
                    eprintln!("created {}", f.display());
                }
            }
            Err(e) => fail(&e),
        }
    }
    if let Some(agents) = agents {
        let report = package::agents::write(&dir, &agents).unwrap_or_else(|e| fail(&e));
        for (f, outcome) in &report.files {
            let f = f.display();
            match outcome {
                package::agents::Outcome::Created => eprintln!("created {f}"),
                package::agents::Outcome::Updated => eprintln!("updated {f}"),
                package::agents::Outcome::Unchanged => eprintln!("unchanged {f}"),
                package::agents::Outcome::Skipped(note) => eprintln!("skipped {f}: {note}"),
            }
        }
        for n in &report.notes {
            eprintln!("note: {n}");
        }
        eprintln!("agents use zell-mcp (and zell-lsp for Claude Code) from PATH: install the toolchain or build them (cargo build --release)");
    }
}

fn manifest(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("schema") => {
            let _ = std::io::stdout().write_all(package::Manifest::json_schema().as_bytes());
        }
        Some("convert") => {
            let Some(f) = args.get(1) else { usage_error("manifest convert needs a format (toml, json, yaml)") };
            let format = Format::parse(f).unwrap_or_else(|| usage_error(&format!("unknown manifest format `{f}` (toml, json, yaml)")));
            let dir = match args.get(2) {
                Some(d) => PathBuf::from(d),
                None => {
                    let cwd = std::env::current_dir().unwrap_or_else(|e| fail(&e.to_string()));
                    match package::manifest::find_upwards(&cwd) {
                        Ok(Some(p)) => p.parent().unwrap().to_path_buf(),
                        Ok(None) => fail("no manifest in the current directory or any parent"),
                        Err(e) => fail(&e),
                    }
                }
            };
            match package::manifest::convert(&dir, format) {
                Ok((old, new)) if old == new => eprintln!("{} is already {format}", new.display()),
                Ok((old, new)) => eprintln!("converted {} -> {}", old.display(), new.display()),
                Err(e) => fail(&e),
            }
        }
        _ => usage_error("usage: zell manifest schema | zell manifest convert <toml|json|yaml> [dir]"),
    }
}

fn read(path: &str) -> zell::span::SourceFile {
    match std::fs::read_to_string(path) {
        Ok(t) => zell::span::SourceFile::new(path.to_string(), t),
        Err(e) => fail(&format!("cannot read {path}: {e}")),
    }
}

fn parse(path: &str) {
    let file = read(path);
    let mut diags = Diagnostics::new();
    let tokens = zell::lexer::lex(&file.text, &mut diags);
    let ast = if diags.has_errors() { None } else { zell::parser::parse(tokens, &mut diags) };
    if !diags.is_empty() {
        eprint!("{}", diags.render_all(&file));
    }
    if diags.has_errors() {
        exit(1);
    }
    // Ignore write errors such as a closed pipe (`zell parse f | head`).
    let _ = writeln!(std::io::stdout(), "{:#?}", ast.unwrap());
}

/// `zell tool <name> [args]`: diagnostics tools. `pprof` shells out to
/// `go tool pprof` (or a `pprof` binary on PATH), which reads the
/// `profile.proto` files written by `diag.StopCPUProfile` and
/// `diag.WriteHeapProfile`.
fn tool(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("pprof") => {
            let attempts: [(&str, &[&str]); 2] = [("go", &["tool", "pprof"]), ("pprof", &[])];
            for (program, pre) in attempts {
                match std::process::Command::new(program).args(pre).args(&args[1..]).status() {
                    Ok(status) => exit(status.code().unwrap_or(1)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => fail(&format!("cannot start {program}: {e}")),
                }
            }
            fail("zell tool pprof needs `go` (go tool pprof) or `pprof` on PATH")
        }
        Some(t) => usage_error(&format!("unknown tool `{t}` (available: pprof)")),
        None => usage_error("tool needs a name (available: pprof)"),
    }
}

/// `zell lsp`: runs the `zell-lsp` binary (next to this executable, else
/// on PATH) with stdio inherited, so editors can launch either command.
fn lsp(args: &[String]) {
    let exe = if cfg!(windows) { "zell-lsp.exe" } else { "zell-lsp" };
    let sibling = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(exe))).filter(|p| p.is_file());
    let program = sibling.unwrap_or_else(|| PathBuf::from(exe));
    match std::process::Command::new(&program).args(args).status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => fail(&format!("cannot start {}: {e} (build it with `cargo build -p zell-lsp`)", program.display())),
    }
}

/// The lldb formatters and gdb printers (tools/), embedded so that an
/// installed compiler works without the repository.
const LLDB_FORMATTERS: &str = include_str!("../../tools/lldb/zell_formatters.py");
const GDB_PRINTERS: &str = include_str!("../../tools/gdb/zell_printers.py");

/// Path of the formatter script for `which` (`lldb` or `gdb`): `$ZELL_TOOLS`,
/// `tools/` of the source tree next to the compiler, or the embedded copy
/// written to the temporary directory.
fn formatter_script(which: &str) -> PathBuf {
    let (rel, embedded) = match which {
        "gdb" => ("gdb/zell_printers.py", GDB_PRINTERS),
        _ => ("lldb/zell_formatters.py", LLDB_FORMATTERS),
    };
    let mut roots = Vec::new();
    if let Ok(t) = std::env::var("ZELL_TOOLS") {
        roots.push(PathBuf::from(t));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
        roots.push(dir.join("../../tools"));
        roots.push(dir.join("../../../tools"));
        roots.push(dir.join("../share/zell/tools"));
    }
    roots.push(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../tools")));
    for r in roots {
        let p = r.join(rel);
        if p.is_file() {
            return zell::paths::canonical(&p).unwrap_or(p);
        }
    }
    let dir = std::env::temp_dir().join(format!("zell-tools-{}", env!("CARGO_PKG_VERSION")));
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::read_to_string(&p).ok().as_deref() != Some(embedded) {
        if let Err(e) = std::fs::write(&p, embedded) {
            fail(&format!("cannot write {}: {e}", p.display()));
        }
    }
    p
}

/// `zell debug [--lldb|--gdb] [path] [build options] [-- args]`: builds
/// with debug info and runs the debugger on the executable with the Zell
/// formatters loaded. `zell debug --formatters lldb|gdb` prints the script
/// path (used by the VS Code extension for `initCommands`).
fn debug(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("run") => return debug_agent_run(&args[1..]),
        Some("attach") => return debug_agent_attach(&args[1..]),
        _ => {}
    }
    let mut gdb = false;
    let mut batch = false;
    let mut source: Option<String> = None;
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                flags.extend_from_slice(&args[i..]);
                break;
            }
            "--lldb" => gdb = false,
            "--gdb" => gdb = true,
            "--batch" => batch = true,
            "--source" => {
                i += 1;
                source = Some(args.get(i).cloned().unwrap_or_else(|| usage_error("--source needs a command file")));
            }
            "--formatters" => {
                let which = args.get(i + 1).map(String::as_str).unwrap_or("lldb");
                if which != "lldb" && which != "gdb" {
                    usage_error("--formatters takes lldb or gdb");
                }
                println!("{}", formatter_script(which).display());
                return;
            }
            a => flags.push(a.to_string()),
        }
        i += 1;
    }
    let (mut opts, prog_args) = parse_build_args(&flags);
    opts.emit = zell::build::Emit::Exe;
    opts.strip = false;
    opts.debug_info = true;
    let tmp = std::env::temp_dir().join(format!("zell-debug-{}", std::process::id()));
    let plain = opts.input.as_ref().is_some_and(|p| p.is_file());
    if plain && opts.output.is_none() {
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            fail(&format!("cannot create {}: {e}", tmp.display()));
        }
        let stem = opts.input.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "prog".into());
        opts.output = Some(tmp.join(stem));
    }
    let exes = zell::build::build(&opts).unwrap_or_else(|e| fail(&e));
    let exe = match exes.as_slice() {
        [e] => e.clone(),
        [] => fail("nothing to debug: no executable was built"),
        _ => fail("several executables were built; choose one with -p"),
    };
    let mut cmd = if gdb {
        let mut c = std::process::Command::new("gdb");
        c.arg("-q").arg("-ex").arg(format!("source {}", formatter_script("gdb").display()));
        if batch {
            c.arg("--batch");
        }
        if let Some(s) = &source {
            c.arg("-x").arg(s);
        }
        c.arg("--args").arg(&exe).args(&prog_args);
        c
    } else {
        let lldb = ["lldb", "lldb-23"].into_iter().find(|l| std::process::Command::new(l).arg("--version").output().is_ok_and(|o| o.status.success())).unwrap_or("lldb");
        let mut c = std::process::Command::new(lldb);
        c.arg("-O").arg(format!("command script import {}", formatter_script("lldb").display()));
        if batch {
            c.arg("--batch");
        }
        if let Some(s) = &source {
            c.arg("-s").arg(s);
        }
        c.arg("--").arg(&exe).args(&prog_args);
        c
    };
    let status = cmd.status();
    let _ = std::fs::remove_dir_all(&tmp);
    match status {
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => fail(&format!("cannot start the debugger: {e}")),
    }
}

/// `zell debug run [path] [--port N] [--headless [--wait]] [build options] [-- args]`:
/// builds with the debug agent and starts the program with
/// `ZELL_DEBUG`; without `--headless`, debugs it with the command-line
/// client (`zell::dap::repl`).
fn debug_agent_run(args: &[String]) {
    let mut port = 0u16;
    let mut headless = false;
    let mut wait = false;
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                flags.extend_from_slice(&args[i..]);
                break;
            }
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|p| p.parse().ok()).unwrap_or_else(|| usage_error("--port needs a number"));
            }
            "--headless" => headless = true,
            "--wait" => wait = true,
            a => flags.push(a.to_string()),
        }
        i += 1;
    }
    let (mut opts, prog_args) = parse_build_args(&flags);
    opts.emit = zell::build::Emit::Exe;
    opts.debug_agent = Some(true);
    let tmp = std::env::temp_dir().join(format!("zell-debug-{}", std::process::id()));
    let plain = opts.input.as_ref().is_some_and(|p| p.is_file());
    if plain && opts.output.is_none() {
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            fail(&format!("cannot create {}: {e}", tmp.display()));
        }
        let stem = opts.input.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "prog".into());
        opts.output = Some(tmp.join(stem));
    }
    let exes = zell::build::build(&opts).unwrap_or_else(|e| fail(&e));
    let exe = match exes.as_slice() {
        [e] => e.clone(),
        [] => fail("nothing to debug: no executable was built"),
        _ => fail("several executables were built; choose one with -p"),
    };
    let code = if headless {
        let spec = if wait { format!("127.0.0.1:{port},wait") } else { format!("127.0.0.1:{port}") };
        let st = std::process::Command::new(&exe).args(&prog_args).env("ZELL_DEBUG", spec).status();
        match st {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => fail(&format!("cannot run {}: {e}", exe.display())),
        }
    } else {
        let (child, addr) = zell::dap::spawn(&exe, &prog_args, port, true, false).unwrap_or_else(|e| fail(&e));
        let c = zell::dap::Client::connect(&addr, std::time::Duration::from_secs(10)).unwrap_or_else(|e| fail(&format!("cannot connect to the debug agent at {addr}: {e}")));
        zell::dap::repl(c, Some(child))
    };
    let _ = std::fs::remove_dir_all(&tmp);
    exit(code);
}

/// `zell debug attach [host:]port`: the command-line client for a
/// program already running with `ZELL_DEBUG`.
fn debug_agent_attach(args: &[String]) {
    let Some(a) = args.first() else { usage_error("debug attach needs [host:]port") };
    let addr = if a.starts_with(':') { format!("127.0.0.1{a}") } else if a.contains(':') { a.clone() } else { format!("127.0.0.1:{a}") };
    let c = zell::dap::Client::connect(&addr, std::time::Duration::from_secs(10)).unwrap_or_else(|e| fail(&format!("cannot connect to the debug agent at {addr}: {e}")));
    exit(zell::dap::repl(c, None));
}
