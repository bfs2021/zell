//! The standard library surface known to the compiler.
//!
//! Every ordinary stdlib function is an *extern*: a C runtime function
//! named `zell_<ns>_<snake_name>` called with the Zell ABI types
//! (`string`, `[]T`, `error`, `T!`, `Task<T>` as defined by codegen/runtime).
//! A few functions need compiler support and are lowered specially
//! ([`Special`]).
//!
//! # Runtime symbols
//!
//! | Zell | C symbol | Signature |
//! |-------|----------|-----------|
//! | `io.ReadFile` | `zell_io_read_file` | `(string) string!` |
//! | `io.WriteFile` | `zell_io_write_file` | `(string, string) void!` |
//! | `io.ReadLine` | `zell_io_read_line` | `() string!` |
//! | `fmt.Format` | `zell_fmt_format` | `(string, []string) string` (args pre-formatted) |
//! | `strings.Split` | `zell_strings_split` | `(string, string) []string` |
//! | `strings.Join` | `zell_strings_join` | `([]string, string) string` |
//! | `strings.Trim` | `zell_strings_trim` | `(string) string` |
//! | `strings.Contains` | `zell_strings_contains` | `(string, string) bool` |
//! | `strings.Replace` | `zell_strings_replace` | `(string, string, string) string` |
//! | `strings.ToUpper` | `zell_strings_to_upper` | `(string) string` |
//! | `strings.ToLower` | `zell_strings_to_lower` | `(string) string` |
//! | `strings.HasPrefix` | `zell_strings_has_prefix` | `(string, string) bool` |
//! | `strings.HasSuffix` | `zell_strings_has_suffix` | `(string, string) bool` |
//! | `strings.Index` | `zell_strings_index` | `(string, string) int` |
//! | `strings.Repeat` | `zell_strings_repeat` | `(string, int) string` |
//! | `strings.ToInt` | `zell_strings_to_int` | `(string) int!` |
//! | `strings.ToFloat` | `zell_strings_to_float` | `(string) float!` |
//! | `math.Sqrt` | `zell_math_sqrt` | `(float) float` |
//! | `math.Abs` | `zell_math_abs` | `(float) float` |
//! | `math.Pow` | `zell_math_pow` | `(float, float) float` |
//! | `math.Floor` | `zell_math_floor` | `(float) float` |
//! | `math.Ceil` | `zell_math_ceil` | `(float) float` |
//! | `math.Min` | `zell_math_min` | `(float, float) float` |
//! | `math.Max` | `zell_math_max` | `(float, float) float` |
//! | `math.Sin` / `Cos` / `Tan` | `zell_math_sin` / `_cos` / `_tan` | `(float) float` |
//! | `math.Log` / `Exp` | `zell_math_log` / `_exp` | `(float) float` |
//! | `errors.Wrap` | `zell_errors_wrap` | `(error, string) error` |
//! | `errors.Unwrap` | `zell_errors_unwrap` | `(error) error` (nil if not a wrapper) |
//! | `errors.Is` | `zell_errors_is` | `(error, error) bool` |
//! | `errors.Trace` | `zell_errors_trace` | `(error) string` (the return trace, see DESIGN.md) |
//! | `errors.Frames` | `zell_errors_frames` | `(error) []errors.Frame` (special: struct result) |
//! | `task.Delay` | `zell_task_delay` | `(int) Task<void>` |
//!
//!
//! The second batch (`time`, `os`, `fs`, `random`,
//! `crypto.{hash,hmac,rand}`, `encoding.{base64,hex}`, `uuid`
//! and the additions to `strings`/`math`/`io`) follows the same
//! rule, with dots of nested namespaces turned into `_`
//! (`crypto.hash.Sha256Hex` -> `zell_crypto_hash_sha256_hex`); the
//! full list with C signatures is `runtime/ABI.md` section 14.3 and the
//! [`FUNCS`] table below. Int versions of the float `math.Min/Max/Abs` are
//! `IMin/IMax/IAbs` (no overloading).
//!
//! Constants ([`CONSTS`]): `math.Pi`, `math.E` (untyped float),
//! `math.MaxInt`/`MinInt`/`MaxInt32`/`MinInt32`/`MaxUint32`/`MaxUint`
//! (untyped int), all inlined, and the sentinel error `fs.ErrNotFound`
//! (runtime `zell_fs_err_not_found()`, wrapped by every "file not found"
//! error of `io`/`fs`), the `net` sentinels `ErrClosed`,
//! `ErrTimeout`, `ErrRefused`, `ErrReset`, `ErrUnreachable`, `ErrAddrInUse`,
//! `ErrNotFound`, `ErrEOF` (`zell_net_err_*()`, runtime/ABI.md 16.2); the
//! target constants `os.Name`,
//! `Arch`, `PathSeparator`, `LineSeparator` (`string`, folded at compile
//! time from [`crate::target::current`]).
//!
//! Specials (no extern; see `hir`):
//! `io.println` / `io.print` → [`crate::hir::ExprKind::Print`]
//! (runtime `zell_print_{int,uint,float,bool,rune,str,space,newline}`),
//! `fmt.Format` → extern `zell_fmt_format` with a `[]string` of
//! [`crate::hir::ExprKind::ToString`] arguments,
//! `task.All` → [`crate::hir::ExprKind::TaskAll`],
//! `task.Any` → [`crate::hir::Builtin::TaskAny`],
//! `errors.New` → [`crate::hir::Builtin::ErrorNew`],
//! `errors.As` → [`crate::hir::Builtin::ErrorsAs`].
//!
//! Zell-source stdlib (`task` types, `net`, `net.http`,
//! `crypto.x509`, `net.tls`; the C crypto primitives behind the
//! latter are the `crypto.*` rows below, `runtime/ABI.md` section 19):
//! [`super::stdsrc`]; their runtime primitives are the `rt*` externs below
//! (internal; `runtime/ABI.md` 10.10, 10.11 and 16).
//!
//! Builtins always in scope (no import): `error`, `len`, `append`,
//! `delete`, `panic`, conversions `int(x)`, `float(x)`, `string(c)` (and
//! the other numeric type names), `typeof(x)` (the `*reflect.TypeInfo` of
//! `x`, see [`super::reflect`]).
//!
//! `reflect`, `encoding.json` (apart from the externs `Quote` and
//! `scan` below) and `strings.Builder` are Zell sources loaded on first
//! use ([`super::reflect`]).

use crate::ast;
use crate::span::Span;
use crate::types::{TypeId, TypeTable};
use std::sync::OnceLock;

/// Synthesized declarations of [`STRUCTS`] (shared by every analysis).
fn std_struct_decls() -> &'static [ast::StructDecl] {
    static DECLS: OnceLock<Vec<ast::StructDecl>> = OnceLock::new();
    DECLS.get_or_init(|| {
        let ident = |n: &str| ast::Ident { name: n.to_string(), span: Span::DUMMY };
        STRUCTS
            .iter()
            .map(|(_, name, fields)| ast::StructDecl {
                attrs: Vec::new(),
                name: ident(name),
                generics: Vec::new(),
                fields: fields
                    .iter()
                    .map(|(f, t)| ast::Field {
                        attrs: Vec::new(),
                        name: ident(f),
                        ty: ast::TypeExpr {
                            kind: ast::TypeKind::Named { path: ast::Path { segments: vec![ident(t)], span: Span::DUMMY }, generics: Vec::new() },
                            span: Span::DUMMY,
                        },
                        default: None,
                        span: Span::DUMMY,
                    })
                    .collect(),
                embeds: Vec::new(),
                methods: Vec::new(),
                implements: Vec::new(),
                span: Span::DUMMY,
            })
            .collect()
    })
}

impl<'a> super::Sema<'a> {
    /// Registers the [`STRUCTS`] (after the user declarations, so user
    /// struct ids are unchanged). Needs at least one file.
    pub(crate) fn add_std_structs(&mut self) {
        if self.files.is_empty() {
            return;
        }
        for ((ns, name, _), decl) in STRUCTS.iter().zip(std_struct_decls()) {
            let id = self.structs.len() as u32;
            self.structs.push(crate::hir::Struct {
                name: name.to_string(),
                full_name: format!("{ns}.{name}"),
                namespace: crate::hir::NamespaceId(0),
                fields: Vec::new(),
                methods: Vec::new(),
                attrs: Vec::new(),
                is_attribute: false,
                exported: true,
                file: 0,
                span: Span::DUMMY,
            });
            self.struct_src.push(decl);
            let nsi = self.ns_index[*ns];
            self.nss[nsi].members.insert(name.to_string(), (super::Member::Struct(id), Span::DUMMY, 0));
        }
    }

    /// The id of builtin struct `ns.name` (see [`STRUCTS`]).
    pub(crate) fn std_struct(&self, ns: &str, name: &str) -> Option<u32> {
        match self.nss[*self.ns_index.get(ns)?].members.get(name)? {
            (super::Member::Struct(id), _, _) => Some(*id),
            _ => None,
        }
    }
}

/// The root namespaces of the standard library (DESIGN.md "Правила
/// импорта"): every stdlib namespace is one of these or nested in one.
/// They are top-level names like package roots, and reserved: user code
/// and packages cannot declare a namespace under them.
pub const STD_ROOTS: &[&str] = &[
    "archive", "arena", "bytes", "cli", "cloud", "cmp", "collections", "compress", "crypto", "data", "database", "debug", "diag", "embed", "encoding", "errors", "expr", "fmt", "fs", "hash", "html", "io",
    "log", "maps", "math", "mime", "net", "os", "plugin", "process", "random", "reflect", "regex", "runtime", "slices", "strconv", "strings", "sync", "task", "testing", "text", "thread", "time",
    "unicode", "unsafe", "uuid",
];

/// Whether `path` (a namespace path or a full name such as `io.File` or
/// `collections.List<int>`) belongs to the standard library: its first
/// segment is one of [`STD_ROOTS`].
pub fn is_std_namespace(path: &str) -> bool {
    let root = path.split(['.', '<']).next().unwrap_or(path);
    STD_ROOTS.contains(&root)
}

/// The removed `std` root (`import std.io` before the stdlib namespaces
/// became top-level): the message for a path whose first segment is
/// `std`, naming the path to use instead.
pub fn removed_std_root(path: &str) -> Option<String> {
    let rest = if path == "std" { "" } else { path.strip_prefix("std.")? };
    Some(if rest.is_empty() {
        "the `std` namespace no longer exists: standard library namespaces are top-level (`io.println`, `import io`)".to_string()
    } else {
        format!("the `std` namespace no longer exists: use `{rest}` instead of `{path}`")
    })
}

/// Stdlib namespaces (each under one of [`STD_ROOTS`]).
/// Nested namespaces (`crypto.hash`) need no extra support: the
/// resolver creates every parent (`crypto`) as a namespace member of its
/// own parent, so `import crypto` gives `hash.Sha256Hex` and the full
/// path is `crypto.hash.Sha256Hex`.
pub const NAMESPACES: &[&str] = &[
    "io",
    "fmt",
    "strings",
    "math",
    "errors",
    "task",
    "time",
    "os",
    "fs",
    "random",
    "crypto",
    "crypto.hash",
    "crypto.hmac",
    "crypto.rand",
    "encoding",
    "encoding.base64",
    "encoding.hex",
    "encoding.json",
    "reflect",
    "uuid",
    "net",
    "net.http",
    "net.http.hpack",
    "net.http.test",
    "net.ws",
    // TLS and the crypto primitives behind it (runtime/ABI.md section 19)
    "net.tls",
    "crypto.aes",
    "crypto.chacha",
    "crypto.x25519",
    "crypto.ecdh",
    "crypto.ecdsa",
    "crypto.ed25519",
    "crypto.rsa",
    "crypto.hkdf",
    "crypto.x509",
    "log",
    "diag",
    "runtime",
    "arena",
    // >>> tier 2 stdlib (docs/STDLIB.md level 2)
    "regex",
    "compress",
    "compress.deflate",
    "compress.zlib",
    "compress.gzip",
    "compress.zstd",
    "compress.brotli",
    "archive",
    "archive.zip",
    "archive.tar",
    "encoding.csv",
    "encoding.xml",
    "encoding.yaml",
    "encoding.toml",
    "html",
    "html.template",
    "text",
    "text.template",
    "mime",
    "mime.multipart",
    "mime.quotedprintable",
    "net.textproto",
    "net.url",
    "text.scanner",
    "text.table",
    // <<< tier 2 stdlib
    // >>> tier 3 stdlib (docs/STDLIB.md level 3)
    "database",
    "database.sql",
    "database.sqlite",
    "database.postgres",
    "net.mail",
    "crypto.kdf",
    "crypto.password",
    // <<< tier 3 stdlib
    // >>> tier 3b stdlib (docs/STDLIB.md level 3b: infrastructure clients)
    "data",
    "data.redis",
    "data.kafka",
    "cloud",
    "cloud.s3",
    "cloud.blob",
    // <<< tier 3b stdlib
    // tier 0/1 stdlib (docs/STDLIB.md)
    "slices",
    "maps",
    "cmp",
    "expr",
    "bytes",
    "strconv",
    "unicode",
    "unicode.utf8",
    "unicode.utf16",
    "math.bits",
    "math.complex",
    "math.big",
    "hash",
    "hash.fnv",
    "hash.crc32",
    "hash.crc64",
    "hash.adler32",
    "hash.maphash",
    "encoding.binary",
    "encoding.base32",
    "encoding.pem",
    "encoding.asn1",
    "sync",
    "os.signal",
    "process",
    "os.user",
    "cli",
    "testing",
    "embed",
    // >>> debugger (docs/DEBUGGER.md stage 2, runtime/ABI.md section 22)
    "debug",
    "unsafe",
    "thread",
    "plugin",
    // <<< debugger
];

/// An ordinary stdlib function implemented by the runtime.
#[derive(Debug, Clone, Copy)]
pub struct StdFunc {
    /// Namespace path, e.g. `strings`.
    pub ns: &'static str,
    /// Zell name, e.g. `Split`.
    pub name: &'static str,
    /// Comma-separated parameter types in Zell syntax.
    pub params: &'static str,
    /// Result type in Zell syntax.
    pub ret: &'static str,
    /// C runtime symbol.
    pub symbol: &'static str,
    /// Documentation (the optional sixth argument of `f!`; empty: see
    /// [`StdFunc::doc`] for the fallback table).
    pub doc: &'static str,
}

/// Stdlib functions that the compiler lowers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// `io.println(args ...any)`.
    Println,
    /// `io.print(args ...any)`: like `println` without the newline.
    Print,
    /// `fmt.Format(template string, args ...any) string`.
    Format,
    /// `fmt.Sprintf(format string, args ...any) string` (Go verbs):
    /// extern [`SPRINTF_SYMBOL`] with per-argument kinds and values.
    Sprintf,
    /// `task.All(tasks ...)`.
    TaskAll,
    /// `task.Any(tasks ...)` → [`crate::hir::Builtin::TaskAny`].
    TaskAny,
    /// `errors.New(msg string) error`.
    ErrorsNew,
    /// `errors.As(err error, target *T) bool`.
    ErrorsAs,
    /// `errors.Frames(err error) []errors.Frame` (extern
    /// `zell_errors_frames`; the result type names a [`STRUCTS`] entry).
    ErrorsFrames,
    /// `arena.rtArenaNew<T>(h int) *T` (internal, used by
    /// `Arena.New<T>`) → [`crate::hir::Builtin::ArenaNew`].
    ArenaNew,
    /// `arena.rtArenaAlloc<T>(h int, n int) []T` (internal, used by
    /// `Arena.Alloc<T>`) → [`crate::hir::Builtin::ArenaSlice`].
    ArenaAlloc,
    /// `encoding.json.rtMake<T>(n int) []T` (internal, used by the typed
    /// decoder to size a slice before filling its elements in place) →
    /// [`crate::hir::Builtin::SliceMake`].
    JsonMake,
    /// `task.rtChanNew<T>(capacity int) int` (internal, used by
    /// `Channel<T>.New`) → [`crate::hir::Builtin::ChanNew`]: a runtime
    /// channel of `T`, given the layouts of `T`, `T!` and `(T, bool)`.
    ChanNew,
    /// `task.rtChanSend<T>(h int, v T) Task<void>` →
    /// [`crate::hir::Builtin::ChanSend`].
    ChanSend,
    /// `task.rtChanRecv<T>(h int) Task<T!>` → [`crate::hir::Builtin::ChanRecv`].
    ChanRecv,
    /// `task.rtChanNext<T>(h int) Task<(T, bool)>` →
    /// [`crate::hir::Builtin::ChanNext`] (the iteration protocol).
    ChanNext,
    /// `task.rtChanRecvAny<T>(hs []int) Task<(int, T, bool)>` →
    /// [`crate::hir::Builtin::ChanRecvAny`] (`task.RecvAny`).
    ChanRecvAny,
    /// `task.rtAllCancel(ts []Task<T>, fallible bool) Task<int>` →
    /// [`crate::hir::Builtin::TaskAllCancel`] (the slice forms of `task.All`).
    TaskAllCancel,
}

/// Special functions by namespace and name.
pub const SPECIALS: &[(&str, &str, Special)] = &[
    ("io", "println", Special::Println),
    ("io", "print", Special::Print),
    ("fmt", "Format", Special::Format),
    ("fmt", "Sprintf", Special::Sprintf),
    ("task", "All", Special::TaskAll),
    ("task", "Any", Special::TaskAny),
    ("errors", "New", Special::ErrorsNew),
    ("errors", "As", Special::ErrorsAs),
    ("errors", "Frames", Special::ErrorsFrames),
    ("arena", "rtArenaNew", Special::ArenaNew),
    ("arena", "rtArenaAlloc", Special::ArenaAlloc),
    ("encoding.json", "rtMake", Special::JsonMake),
    ("task", "rtChanNew", Special::ChanNew),
    ("task", "rtChanSend", Special::ChanSend),
    ("task", "rtChanRecv", Special::ChanRecv),
    ("task", "rtChanNext", Special::ChanNext),
    ("task", "rtChanRecvAny", Special::ChanRecvAny),
    ("task", "rtAllCancel", Special::TaskAllCancel),
];

/// Builtin stdlib struct types: (namespace, name, fields as (name, type)).
/// They become ordinary [`crate::hir::Struct`]s (after the user structs)
/// whose C layout the runtime shares.
pub const STRUCTS: &[(&str, &str, &[(&str, &str)])] = &[
    // One error-trace record (runtime `frame_rec` in error.c, 32 bytes).
    ("errors", "Frame", &[("File", "string"), ("Line", "int"), ("Func", "string"), ("Context", "string")]),
];

/// Symbol used for the lowered `fmt.Format`.
pub const FORMAT_SYMBOL: &str = "zell_fmt_format";

/// Symbol of the lowered `fmt.Sprintf`: `(format string, kinds string,
/// ints []int, floats []float, texts []string) string`, see
/// `runtime/src/std_fmt_sprintf.c`.
pub const SPRINTF_SYMBOL: &str = "zell_fmt_sprintf";

/// Value of a stdlib constant ([`CONSTS`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StdConst {
    /// Untyped float constant (inlined).
    Float(f64),
    /// Untyped integer constant (inlined).
    Int(i128),
    /// Sentinel error: a zero-argument runtime function returning the
    /// immortal error object (so `errors.Is` compares identity).
    Error(&'static str),
    /// `string` constant describing the compilation target
    /// ([`crate::target::current`]): `os.Name`, `os.Arch`,
    /// `os.PathSeparator`, `os.LineSeparator`.
    Target(TargetConst),
}

/// Target-dependent `os` string constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetConst {
    Name,
    Arch,
    PathSeparator,
    LineSeparator,
    /// A fixed string constant (not target dependent): `time.RFC3339`.
    Str(&'static str),
}

impl TargetConst {
    /// The value for the current target.
    pub fn value(self) -> String {
        let t = crate::target::current();
        match self {
            TargetConst::Name => t.os.clone(),
            TargetConst::Arch => t.arch.clone(),
            TargetConst::PathSeparator => t.path_separator().to_string(),
            TargetConst::LineSeparator => t.line_separator().to_string(),
            TargetConst::Str(v) => v.to_string(),
        }
    }
}

impl std::fmt::Display for StdConst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdConst::Float(v) => write!(f, "{v} (untyped float)"),
            StdConst::Int(v) => write!(f, "{v} (untyped int)"),
            StdConst::Error(_) => write!(f, "error (sentinel)"),
            StdConst::Target(t) => write!(f, "{:?} (string)", t.value()),
        }
    }
}

/// Stdlib constants: (namespace, name, value).
pub const CONSTS: &[(&str, &str, StdConst)] = &[
    ("math", "Pi", StdConst::Float(std::f64::consts::PI)),
    ("math", "E", StdConst::Float(std::f64::consts::E)),
    ("math", "MaxInt", StdConst::Int(i64::MAX as i128)),
    ("math", "MinInt", StdConst::Int(i64::MIN as i128)),
    ("math", "MaxInt32", StdConst::Int(i32::MAX as i128)),
    ("math", "MinInt32", StdConst::Int(i32::MIN as i128)),
    ("math", "MaxUint32", StdConst::Int(u32::MAX as i128)),
    ("math", "MaxUint", StdConst::Int(u64::MAX as i128)),
    ("fs", "ErrNotFound", StdConst::Error("zell_fs_err_not_found")),
    ("task", "ErrCancelled", StdConst::Error("zell_task_err_cancelled")),
    ("task", "ErrPanic", StdConst::Error("zell_task_err_panic")),
    // net sentinels (runtime/src/std_net.c); every net error wraps one
    // of them when its cause is of that kind
    ("net", "ErrClosed", StdConst::Error("zell_net_err_closed")),
    ("net", "ErrTimeout", StdConst::Error("zell_net_err_timeout")),
    ("net", "ErrRefused", StdConst::Error("zell_net_err_refused")),
    ("net", "ErrReset", StdConst::Error("zell_net_err_reset")),
    ("net", "ErrUnreachable", StdConst::Error("zell_net_err_unreachable")),
    ("net", "ErrAddrInUse", StdConst::Error("zell_net_err_addr_in_use")),
    ("net", "ErrNotFound", StdConst::Error("zell_net_err_not_found")),
    ("net", "ErrEOF", StdConst::Error("zell_net_err_eof")),
    // net.http / net.ws sentinels (runtime/src/std_http.c)
    ("net.http", "ErrBodyTooLarge", StdConst::Error("zell_net_http_err_body_too_large")),
    ("net.http", "ErrHijacked", StdConst::Error("zell_net_http_err_hijacked")),
    ("net.http", "ErrAbortHandler", StdConst::Error("zell_net_http_err_abort_handler")),
    ("net.http", "ErrServerClosed", StdConst::Error("zell_net_http_err_server_closed")),
    ("net.http", "ErrHandlerTimeout", StdConst::Error("zell_net_http_err_handler_timeout")),
    ("net.http", "ErrBodyClosed", StdConst::Error("zell_net_http_err_body_closed")),
    ("net.ws", "ErrClosed", StdConst::Error("zell_net_ws_err_closed")),
    ("net.ws", "ErrBadHandshake", StdConst::Error("zell_net_ws_err_bad_handshake")),
    ("net.ws", "TextMessage", StdConst::Int(1)),
    ("net.ws", "BinaryMessage", StdConst::Int(2)),
    ("net.ws", "CloseNormal", StdConst::Int(1000)),
    ("net.ws", "CloseGoingAway", StdConst::Int(1001)),
    ("net.ws", "CloseProtocolError", StdConst::Int(1002)),
    ("net.ws", "CloseUnsupportedData", StdConst::Int(1003)),
    ("net.ws", "CloseNoStatus", StdConst::Int(1005)),
    ("net.ws", "CloseAbnormal", StdConst::Int(1006)),
    ("net.ws", "CloseInvalidPayload", StdConst::Int(1007)),
    ("net.ws", "ClosePolicyViolation", StdConst::Int(1008)),
    ("net.ws", "CloseMessageTooBig", StdConst::Int(1009)),
    ("net.ws", "CloseMandatoryExtension", StdConst::Int(1010)),
    ("net.ws", "CloseInternalError", StdConst::Int(1011)),
    // net.tls: protocol versions and ServerConfig.ClientAuth modes
    ("net.tls", "V12", StdConst::Int(0x0303)),
    ("net.tls", "V13", StdConst::Int(0x0304)),
    ("net.tls", "NoClientCert", StdConst::Int(0)),
    ("net.tls", "RequestClientCert", StdConst::Int(1)),
    ("net.tls", "RequireAnyClientCert", StdConst::Int(2)),
    ("net.tls", "VerifyClientCertIfGiven", StdConst::Int(3)),
    ("net.tls", "RequireAndVerifyClientCert", StdConst::Int(4)),
    // tier 0/1 stdlib (docs/STDLIB.md)
    ("strconv", "ErrSyntax", StdConst::Error("zell_strconv_err_syntax")),
    ("strconv", "ErrRange", StdConst::Error("zell_strconv_err_range")),
    ("unicode", "MaxRune", StdConst::Int(0x10FFFF)),
    ("unicode", "ReplacementChar", StdConst::Int(0xFFFD)),
    ("unicode.utf8", "RuneError", StdConst::Int(0xFFFD)),
    ("unicode.utf8", "UTFMax", StdConst::Int(4)),
    ("unicode.utf8", "RuneSelf", StdConst::Int(0x80)),
    ("math.bits", "UintSize", StdConst::Int(64)),
    ("hash.crc32", "IEEE", StdConst::Int(0xEDB88320)),
    ("hash.crc32", "Castagnoli", StdConst::Int(0x82F63B78)),
    ("hash.crc32", "Koopman", StdConst::Int(0xEB31D82E)),
    ("hash.crc64", "ISO", StdConst::Int(0xD800000000000000)),
    ("hash.crc64", "ECMA", StdConst::Int(0xC96C5795D7870F42)),
    ("encoding.binary", "MaxVarintLen16", StdConst::Int(3)),
    ("encoding.binary", "MaxVarintLen32", StdConst::Int(5)),
    ("encoding.binary", "MaxVarintLen64", StdConst::Int(10)),
    // time layouts (Go reference layouts)
    ("time", "Layout", StdConst::Target(TargetConst::Str("01/02 03:04:05PM '06 -0700"))),
    ("time", "ANSIC", StdConst::Target(TargetConst::Str("Mon Jan _2 15:04:05 2006"))),
    ("time", "UnixDate", StdConst::Target(TargetConst::Str("Mon Jan _2 15:04:05 MST 2006"))),
    ("time", "RubyDate", StdConst::Target(TargetConst::Str("Mon Jan 02 15:04:05 -0700 2006"))),
    ("time", "RFC822", StdConst::Target(TargetConst::Str("02 Jan 06 15:04 MST"))),
    ("time", "RFC822Z", StdConst::Target(TargetConst::Str("02 Jan 06 15:04 -0700"))),
    ("time", "RFC850", StdConst::Target(TargetConst::Str("Monday, 02-Jan-06 15:04:05 MST"))),
    ("time", "RFC1123", StdConst::Target(TargetConst::Str("Mon, 02 Jan 2006 15:04:05 MST"))),
    ("time", "RFC1123Z", StdConst::Target(TargetConst::Str("Mon, 02 Jan 2006 15:04:05 -0700"))),
    ("time", "RFC3339", StdConst::Target(TargetConst::Str("2006-01-02T15:04:05Z07:00"))),
    ("time", "RFC3339Nano", StdConst::Target(TargetConst::Str("2006-01-02T15:04:05.999999999Z07:00"))),
    ("time", "Kitchen", StdConst::Target(TargetConst::Str("3:04PM"))),
    ("time", "Stamp", StdConst::Target(TargetConst::Str("Jan _2 15:04:05"))),
    ("time", "StampMilli", StdConst::Target(TargetConst::Str("Jan _2 15:04:05.000"))),
    ("time", "StampMicro", StdConst::Target(TargetConst::Str("Jan _2 15:04:05.000000"))),
    ("time", "StampNano", StdConst::Target(TargetConst::Str("Jan _2 15:04:05.000000000"))),
    ("time", "DateTime", StdConst::Target(TargetConst::Str("2006-01-02 15:04:05"))),
    ("time", "DateOnly", StdConst::Target(TargetConst::Str("2006-01-02"))),
    ("time", "TimeOnly", StdConst::Target(TargetConst::Str("15:04:05"))),
    // >>> tier 2 stdlib constants
    ("compress", "NoCompression", StdConst::Int(0)),
    ("compress", "BestSpeed", StdConst::Int(1)),
    ("compress", "BestCompression", StdConst::Int(9)),
    ("compress", "DefaultCompression", StdConst::Int(-1)),
    ("archive.zip", "Store", StdConst::Int(0)),
    ("archive.zip", "Deflate", StdConst::Int(8)),
    ("archive.tar", "TypeReg", StdConst::Int(48)),
    ("archive.tar", "TypeLink", StdConst::Int(49)),
    ("archive.tar", "TypeSymlink", StdConst::Int(50)),
    ("archive.tar", "TypeDir", StdConst::Int(53)),
    ("text.scanner", "EOF", StdConst::Int(-1)),
    ("text.scanner", "Ident", StdConst::Int(-2)),
    ("text.scanner", "Int", StdConst::Int(-3)),
    ("text.scanner", "Float", StdConst::Int(-4)),
    ("text.scanner", "Char", StdConst::Int(-5)),
    ("text.scanner", "String", StdConst::Int(-6)),
    ("text.scanner", "RawString", StdConst::Int(-7)),
    ("text.scanner", "Comment", StdConst::Int(-8)),
    ("text.scanner", "ScanIdents", StdConst::Int(4)),
    ("text.scanner", "ScanInts", StdConst::Int(8)),
    ("text.scanner", "ScanFloats", StdConst::Int(16)),
    ("text.scanner", "ScanChars", StdConst::Int(32)),
    ("text.scanner", "ScanStrings", StdConst::Int(64)),
    ("text.scanner", "ScanRawStrings", StdConst::Int(128)),
    ("text.scanner", "ScanComments", StdConst::Int(256)),
    ("text.scanner", "SkipComments", StdConst::Int(512)),
    ("text.scanner", "GoTokens", StdConst::Int(1012)),
    ("text.scanner", "GoWhitespace", StdConst::Int(4294977024)),
    ("text.table", "Plain", StdConst::Int(0)),
    ("text.table", "ASCII", StdConst::Int(1)),
    ("text.table", "Unicode", StdConst::Int(2)),
    ("text.table", "Markdown", StdConst::Int(3)),
    ("text.table", "Left", StdConst::Int(0)),
    ("text.table", "Right", StdConst::Int(1)),
    ("text.table", "Center", StdConst::Int(2)),
    // <<< tier 2 stdlib constants
    // >>> tier 3 stdlib constants
    ("database.sql", "ErrNoRows", StdConst::Error("zell_database_sql_err_no_rows")),
    ("database.sql", "ErrTxDone", StdConst::Error("zell_database_sql_err_tx_done")),
    ("database.sql", "ErrConnDone", StdConst::Error("zell_database_sql_err_conn_done")),
    ("database.sql", "PlaceholderQuestion", StdConst::Int(0)),
    ("database.sql", "PlaceholderDollar", StdConst::Int(1)),
    // <<< tier 3 stdlib constants
    // >>> tier 3b stdlib constants (runtime/src/std_infra.c)
    ("data.redis", "Nil", StdConst::Error("zell_data_redis_err_nil")),
    ("data.redis", "ErrClosed", StdConst::Error("zell_data_redis_err_closed")),
    ("data.redis", "ErrTxFailed", StdConst::Error("zell_data_redis_err_tx_failed")),
    ("data.redis", "ErrPoolTimeout", StdConst::Error("zell_data_redis_err_pool_timeout")),
    ("data.redis", "ErrCrossSlot", StdConst::Error("zell_data_redis_err_cross_slot")),
    ("data.kafka", "ErrClosed", StdConst::Error("zell_data_kafka_err_closed")),
    ("data.kafka", "ErrNoBrokers", StdConst::Error("zell_data_kafka_err_no_brokers")),
    ("cloud.s3", "ErrNoSuchKey", StdConst::Error("zell_cloud_s3_err_no_such_key")),
    ("cloud.s3", "ErrNoSuchBucket", StdConst::Error("zell_cloud_s3_err_no_such_bucket")),
    ("cloud.s3", "ErrAccessDenied", StdConst::Error("zell_cloud_s3_err_access_denied")),
    ("cloud.s3", "ErrPreconditionFailed", StdConst::Error("zell_cloud_s3_err_precondition")),
    ("cloud.blob", "ErrNotFound", StdConst::Error("zell_cloud_blob_err_not_found")),
    // <<< tier 3b stdlib constants
    ("io", "EOF", StdConst::Error("zell_io_err_eof")),
    ("io", "SeekStart", StdConst::Int(0)),
    ("io", "SeekCurrent", StdConst::Int(1)),
    ("io", "SeekEnd", StdConst::Int(2)),
    ("os", "O_RDONLY", StdConst::Int(0)),
    ("os", "O_WRONLY", StdConst::Int(1)),
    ("os", "O_RDWR", StdConst::Int(2)),
    ("os", "O_APPEND", StdConst::Int(8)),
    ("os", "O_CREATE", StdConst::Int(64)),
    ("os", "O_EXCL", StdConst::Int(128)),
    ("os", "O_SYNC", StdConst::Int(256)),
    ("os", "O_TRUNC", StdConst::Int(512)),
    ("encoding.asn1", "TagBoolean", StdConst::Int(1)),
    ("encoding.asn1", "TagInteger", StdConst::Int(2)),
    ("encoding.asn1", "TagBitString", StdConst::Int(3)),
    ("encoding.asn1", "TagOctetString", StdConst::Int(4)),
    ("encoding.asn1", "TagNull", StdConst::Int(5)),
    ("encoding.asn1", "TagOID", StdConst::Int(6)),
    ("encoding.asn1", "TagEnum", StdConst::Int(10)),
    ("encoding.asn1", "TagUTF8String", StdConst::Int(12)),
    ("encoding.asn1", "TagSequence", StdConst::Int(16)),
    ("encoding.asn1", "TagSet", StdConst::Int(17)),
    ("encoding.asn1", "TagNumericString", StdConst::Int(18)),
    ("encoding.asn1", "TagPrintableString", StdConst::Int(19)),
    ("encoding.asn1", "TagT61String", StdConst::Int(20)),
    ("encoding.asn1", "TagIA5String", StdConst::Int(22)),
    ("encoding.asn1", "TagUTCTime", StdConst::Int(23)),
    ("encoding.asn1", "TagGeneralizedTime", StdConst::Int(24)),
    ("encoding.asn1", "TagBMPString", StdConst::Int(30)),
    ("encoding.asn1", "ClassUniversal", StdConst::Int(0)),
    ("encoding.asn1", "ClassApplication", StdConst::Int(1)),
    ("encoding.asn1", "ClassContextSpecific", StdConst::Int(2)),
    ("encoding.asn1", "ClassPrivate", StdConst::Int(3)),
    // os.signal numbers (Linux values)
    ("os.signal", "SIGHUP", StdConst::Int(1)),
    ("os.signal", "SIGINT", StdConst::Int(2)),
    ("os.signal", "SIGQUIT", StdConst::Int(3)),
    ("os.signal", "SIGABRT", StdConst::Int(6)),
    ("os.signal", "SIGKILL", StdConst::Int(9)),
    ("os.signal", "SIGUSR1", StdConst::Int(10)),
    ("os.signal", "SIGUSR2", StdConst::Int(12)),
    ("os.signal", "SIGPIPE", StdConst::Int(13)),
    ("os.signal", "SIGALRM", StdConst::Int(14)),
    ("os.signal", "SIGTERM", StdConst::Int(15)),
    ("os.signal", "SIGCHLD", StdConst::Int(17)),
    ("os.signal", "SIGWINCH", StdConst::Int(28)),
    ("process", "Inherit", StdConst::Int(0)),
    ("process", "Pipe", StdConst::Int(1)),
    ("process", "Null", StdConst::Int(2)),
    ("os", "Name", StdConst::Target(TargetConst::Name)),
    ("os", "Arch", StdConst::Target(TargetConst::Arch)),
    ("os", "PathSeparator", StdConst::Target(TargetConst::PathSeparator)),
    ("os", "LineSeparator", StdConst::Target(TargetConst::LineSeparator)),
];

macro_rules! f {
    ($ns:literal, $name:literal, $params:literal, $ret:literal, $sym:literal) => {
        StdFunc { ns: $ns, name: $name, params: $params, ret: $ret, symbol: $sym, doc: "" }
    };
    ($ns:literal, $name:literal, $params:literal, $ret:literal, $sym:literal, $doc:literal) => {
        StdFunc { ns: $ns, name: $name, params: $params, ret: $ret, symbol: $sym, doc: $doc }
    };
}

/// All extern stdlib functions.
pub const FUNCS: &[StdFunc] = &[
    f!("io", "ReadFile", "string", "string!", "zell_io_read_file"),
    f!("io", "WriteFile", "string, string", "void!", "zell_io_write_file"),
    f!("io", "ReadLine", "", "string!", "zell_io_read_line"),
    f!("strings", "Split", "string, string", "[]string", "zell_strings_split"),
    f!("strings", "Join", "[]string, string", "string", "zell_strings_join"),
    f!("strings", "Trim", "string", "string", "zell_strings_trim"),
    f!("strings", "Contains", "string, string", "bool", "zell_strings_contains"),
    f!("strings", "Replace", "string, string, string", "string", "zell_strings_replace"),
    f!("strings", "ToUpper", "string", "string", "zell_strings_to_upper"),
    f!("strings", "ToLower", "string", "string", "zell_strings_to_lower"),
    f!("strings", "HasPrefix", "string, string", "bool", "zell_strings_has_prefix"),
    f!("strings", "HasSuffix", "string, string", "bool", "zell_strings_has_suffix"),
    f!("strings", "Index", "string, string", "int", "zell_strings_index"),
    f!("strings", "Repeat", "string, int", "string", "zell_strings_repeat"),
    f!("strings", "ToInt", "string", "int!", "zell_strings_to_int"),
    f!("strings", "ToFloat", "string", "float!", "zell_strings_to_float"),
    f!("math", "Sqrt", "float", "float", "zell_math_sqrt"),
    f!("math", "Abs", "float", "float", "zell_math_abs"),
    f!("math", "Pow", "float, float", "float", "zell_math_pow"),
    f!("math", "Floor", "float", "float", "zell_math_floor"),
    f!("math", "Ceil", "float", "float", "zell_math_ceil"),
    f!("math", "Min", "float, float", "float", "zell_math_min"),
    f!("math", "Max", "float, float", "float", "zell_math_max"),
    f!("math", "Sin", "float", "float", "zell_math_sin"),
    f!("math", "Cos", "float", "float", "zell_math_cos"),
    f!("math", "Tan", "float", "float", "zell_math_tan"),
    f!("math", "Log", "float", "float", "zell_math_log"),
    f!("math", "Exp", "float", "float", "zell_math_exp"),
    f!("errors", "Wrap", "error, string", "error", "zell_errors_wrap"),
    f!("errors", "Unwrap", "error", "error", "zell_errors_unwrap"),
    f!("errors", "Is", "error, error", "bool", "zell_errors_is"),
    f!("errors", "Trace", "error", "string", "zell_errors_trace"),
    f!("arena", "rtArenaCreate", "", "int", "zell_arena_create"),
    f!("arena", "rtArenaDestroy", "int", "void", "zell_arena_destroy"),
    f!("arena", "rtArenaFree", "int", "void", "zell_arena_free"),
    f!("arena", "rtArenaString", "int, string", "string", "zell_arena_string"),
    f!("arena", "rtArenaLen", "int", "int", "zell_arena_objects"),
    f!("task", "Delay", "int", "Task<void>", "zell_task_delay"),
    f!("task", "Yield", "", "Task<void>", "zell_task_yield"),
    f!("task", "PanicMessage", "error", "string", "zell_task_panic_message"),
    f!("task", "PanicTrace", "error", "string", "zell_task_panic_trace"),
    // internal primitives of the Zell-source task (sema/stdlib_src/task.zl)
    f!("task", "rtSignal", "", "Task<void>", "zell_task_rt_signal"),
    // fmt.Sprintf's lowered form (SPRINTF_SYMBOL) for fmt.zl's sprintfSpread.
    f!("fmt", "rtSprintf", "string, string, []int, []float, []string", "string", "zell_fmt_sprintf"),
    f!("task", "rtWaker", "", "Task<void>", "zell_task_rt_waker"),
    f!("task", "IsCancelled", "", "bool", "zell_task_cur_cancelled"),
    f!("task", "rtCancel", "Task<void>", "void", "zell_task_cancel"),
    f!("task", "rtScopeCancelled", "Task<void>", "bool", "zell_task_is_cancelled"),
    f!("task", "rtSetDeadline", "Task<void>, int", "void", "zell_task_set_deadline"),
    f!("task", "rtCurrent", "", "Task<void>", "zell_task_current"),
    f!("task", "rtNewScope", "", "Task<void>", "zell_task_new_scope"),
    f!("task", "rtEnterScope", "Task<void>", "Task<void>", "zell_task_enter_scope"),
    f!("task", "rtLeaveScope", "Task<void>", "void", "zell_task_leave_scope"),
    f!("task", "rtNextKey", "", "int", "zell_task_next_key"),
    f!("task", "rtFire", "Task<void>", "void", "zell_task_rt_fire"),
    f!("task", "rtLock", "*int", "void", "zell_task_spin_lock"),
    f!("task", "rtUnlock", "*int", "void", "zell_task_spin_unlock"),
    f!("task", "rtGetCtx", "", "map[string]any", "zell_task_get_ctx"),
    f!("task", "rtSetCtx", "map[string]any", "void", "zell_task_set_ctx"),
    // task.Channel<T> and task.Mutex (runtime/src/task_sync.c, ABI.md 10.6, 10.7);
    // rtChanNew/Send/Recv/Next are typed specials (SPECIALS)
    f!("task", "rtChanRelease", "int", "void", "zell_task_chan_release"),
    f!("task", "rtChanClose", "int", "void", "zell_task_chan_close"),
    f!("task", "rtChanLen", "int", "int", "zell_task_chan_len"),
    f!("task", "rtChanCap", "int", "int", "zell_task_chan_cap"),
    f!("task", "rtChanIsClosed", "int", "bool", "zell_task_chan_is_closed"),
    f!("task", "rtMutexLock", "*int", "Task<void>", "zell_task_mutex_lock"),
    f!("task", "rtMutexTryLock", "*int", "bool", "zell_task_mutex_try_lock"),
    f!("task", "rtMutexUnlock", "*int", "void", "zell_task_mutex_unlock"),
    // strings (more)
    f!("strings", "Fields", "string", "[]string", "zell_strings_fields"),
    f!("strings", "Count", "string, string", "int", "zell_strings_count"),
    f!("strings", "LastIndex", "string, string", "int", "zell_strings_last_index"),
    f!("strings", "TrimPrefix", "string, string", "string", "zell_strings_trim_prefix"),
    f!("strings", "TrimSuffix", "string, string", "string", "zell_strings_trim_suffix"),
    f!("strings", "TrimLeft", "string, string", "string", "zell_strings_trim_left"),
    f!("strings", "TrimRight", "string, string", "string", "zell_strings_trim_right"),
    f!("strings", "SplitN", "string, string, int", "[]string", "zell_strings_split_n"),
    f!("strings", "EqualFold", "string, string", "bool", "zell_strings_equal_fold"),
    f!("strings", "Reverse", "string", "string", "zell_strings_reverse"),
    f!("strings", "PadLeft", "string, int, string", "string", "zell_strings_pad_left"),
    f!("strings", "PadRight", "string, int, string", "string", "zell_strings_pad_right"),
    f!("strings", "Runes", "string", "[]char", "zell_strings_runes"),
    f!("strings", "FromRunes", "[]char", "string", "zell_strings_from_runes"),
    f!("strings", "Bytes", "string", "[]byte", "zell_strings_bytes"),
    f!("strings", "FromBytes", "[]byte", "string", "zell_strings_from_bytes"),
    f!("strings", "Compare", "string, string", "int", "zell_strings_compare"),
    // byte-buffer primitives of the Zell-source strings.Builder and io
    // (reflect_src/builder.zl, stdlib_src/io.zl); bounds are clamped
    f!("strings", "rtCopy", "[]byte, int, []byte, int, int", "void", "zell_strings_rt_copy"),
    f!("strings", "rtCopyString", "[]byte, int, string, int, int", "void", "zell_strings_rt_copy_string"),
    f!("strings", "rtIndexByte", "[]byte, int, int, int", "int", "zell_strings_rt_index_byte"),
    f!("strings", "rtMakeBytes", "int", "[]byte", "zell_strings_rt_make_bytes"),
    f!("strings", "rtSort", "[]string", "void", "zell_strings_rt_sort"),
    f!("strings", "Title", "string", "string", "zell_strings_title"),
    // math (more); int helpers are I-prefixed (no overloading)
    f!("math", "Round", "float", "float", "zell_math_round"),
    f!("math", "Trunc", "float", "float", "zell_math_trunc"),
    f!("math", "Mod", "float, float", "float", "zell_math_mod"),
    f!("math", "Hypot", "float, float", "float", "zell_math_hypot"),
    f!("math", "Atan2", "float, float", "float", "zell_math_atan2"),
    f!("math", "Log2", "float", "float", "zell_math_log2"),
    f!("math", "Log10", "float", "float", "zell_math_log10"),
    f!("math", "IsNaN", "float", "bool", "zell_math_is_nan"),
    f!("math", "IsInf", "float, int", "bool", "zell_math_is_inf"),
    f!("math", "Inf", "int", "float", "zell_math_inf"),
    f!("math", "NaN", "", "float", "zell_math_nan"),
    f!("math", "IMin", "int, int", "int", "zell_math_imin"),
    f!("math", "IMax", "int, int", "int", "zell_math_imax"),
    f!("math", "IAbs", "int", "int", "zell_math_iabs"),
    // io (more)
    f!("io", "ReadAll", "", "string!", "zell_io_read_all"),
    f!("io", "ReadLines", "string", "[]string!", "zell_io_read_lines"),
    f!("io", "WriteLines", "string, []string", "void!", "zell_io_write_lines"),
    f!("io", "Eprint", "string", "void", "zell_io_eprint"),
    f!("io", "Eprintln", "string", "void", "zell_io_eprintln"),
    // time: int Unix milliseconds, UTC calendar
    f!("time", "Now", "", "int", "zell_time_now"),
    f!("time", "NowNanos", "", "int", "zell_time_now_nanos"),
    f!("time", "Sleep", "int", "void", "zell_time_sleep"),
    f!("time", "Since", "int", "int", "zell_time_since"),
    f!("time", "Format", "int, string", "string", "zell_time_format"),
    f!("time", "Parse", "string, string", "int!", "zell_time_parse"),
    f!("time", "Year", "int", "int", "zell_time_year"),
    f!("time", "Month", "int", "int", "zell_time_month"),
    f!("time", "Day", "int", "int", "zell_time_day"),
    f!("time", "Hour", "int", "int", "zell_time_hour"),
    f!("time", "Minute", "int", "int", "zell_time_minute"),
    f!("time", "Second", "int", "int", "zell_time_second"),
    f!("time", "Weekday", "int", "int", "zell_time_weekday"),
    // os
    f!("os", "Args", "", "[]string", "zell_os_args"),
    f!("os", "Getenv", "string", "string", "zell_os_getenv"),
    f!("os", "Setenv", "string, string", "void!", "zell_os_setenv"),
    f!("os", "Environ", "", "[]string", "zell_os_environ"),
    f!("os", "Exit", "int", "void", "zell_os_exit"),
    f!("os", "Hostname", "", "string!", "zell_os_hostname"),
    f!("os", "Pid", "", "int", "zell_os_pid"),
    f!("os", "Cwd", "", "string!", "zell_os_cwd"),
    f!("os", "Chdir", "string", "void!", "zell_os_chdir"),
    // fs
    f!("fs", "Exists", "string", "bool", "zell_fs_exists"),
    f!("fs", "IsDir", "string", "bool", "zell_fs_is_dir"),
    f!("fs", "ReadDir", "string", "[]string!", "zell_fs_read_dir"),
    f!("fs", "Mkdir", "string", "void!", "zell_fs_mkdir"),
    f!("fs", "MkdirAll", "string", "void!", "zell_fs_mkdir_all"),
    f!("fs", "Remove", "string", "void!", "zell_fs_remove"),
    f!("fs", "RemoveAll", "string", "void!", "zell_fs_remove_all"),
    f!("fs", "Rename", "string, string", "void!", "zell_fs_rename"),
    f!("fs", "Size", "string", "int!", "zell_fs_size"),
    f!("fs", "ModTime", "string", "int!", "zell_fs_mod_time"),
    f!("fs", "Copy", "string, string", "void!", "zell_fs_copy"),
    f!("fs", "Join", "string, string", "string", "zell_fs_join"),
    f!("fs", "Base", "string", "string", "zell_fs_base"),
    f!("fs", "Dir", "string", "string", "zell_fs_dir"),
    f!("fs", "Ext", "string", "string", "zell_fs_ext"),
    f!("fs", "Abs", "string", "string!", "zell_fs_abs"),
    f!("fs", "TempDir", "", "string", "zell_fs_temp_dir"),
    f!("fs", "AppendFile", "string, string", "void!", "zell_fs_append_file"),
    // random (non-cryptographic)
    f!("random", "Seed", "int", "void", "zell_random_seed"),
    f!("random", "Int", "", "int", "zell_random_int"),
    f!("random", "IntN", "int", "int", "zell_random_int_n"),
    f!("random", "Float", "", "float", "zell_random_float"),
    f!("random", "Bytes", "int", "[]byte", "zell_random_bytes"),
    f!("random", "Shuffle", "[]int", "void", "zell_random_shuffle"),
    // crypto.*
    f!("crypto.hash", "Sha256", "string", "[]byte", "zell_crypto_hash_sha256"),
    f!("crypto.hash", "Sha256Hex", "string", "string", "zell_crypto_hash_sha256_hex"),
    f!("crypto.hash", "Sha512", "string", "[]byte", "zell_crypto_hash_sha512"),
    f!("crypto.hash", "Sha512Hex", "string", "string", "zell_crypto_hash_sha512_hex"),
    f!("crypto.hash", "Sha1", "string", "[]byte", "zell_crypto_hash_sha1"),
    f!("crypto.hash", "Sha1Hex", "string", "string", "zell_crypto_hash_sha1_hex"),
    f!("crypto.hash", "Md5", "string", "[]byte", "zell_crypto_hash_md5"),
    f!("crypto.hash", "Md5Hex", "string", "string", "zell_crypto_hash_md5_hex"),
    f!("crypto.hash", "Crc32", "string", "int", "zell_crypto_hash_crc32"),
    f!("crypto.hmac", "Sha256", "string, string", "[]byte", "zell_crypto_hmac_sha256"),
    f!("crypto.hmac", "Sha256Hex", "string, string", "string", "zell_crypto_hmac_sha256_hex"),
    f!("crypto.hmac", "Equal", "[]byte, []byte", "bool", "zell_crypto_hmac_equal"),
    f!("crypto.rand", "Bytes", "int", "[]byte!", "zell_crypto_rand_bytes"),
    f!("crypto.rand", "Int", "", "int!", "zell_crypto_rand_int"),
    f!("crypto.rand", "Hex", "int", "string!", "zell_crypto_rand_hex"),
    // encoding.*
    f!("encoding.base64", "Encode", "[]byte", "string", "zell_encoding_base64_encode"),
    f!("encoding.base64", "EncodeString", "string", "string", "zell_encoding_base64_encode_string"),
    f!("encoding.base64", "Decode", "string", "[]byte!", "zell_encoding_base64_decode"),
    f!("encoding.base64", "DecodeString", "string", "string!", "zell_encoding_base64_decode_string"),
    f!("encoding.base64", "EncodeURL", "[]byte", "string", "zell_encoding_base64_encode_url"),
    f!("encoding.base64", "DecodeURL", "string", "[]byte!", "zell_encoding_base64_decode_url"),
    f!("encoding.hex", "Encode", "[]byte", "string", "zell_encoding_hex_encode"),
    f!("encoding.hex", "EncodeString", "string", "string", "zell_encoding_hex_encode_string"),
    f!("encoding.hex", "Decode", "string", "[]byte!", "zell_encoding_hex_decode"),
    f!("encoding.hex", "DecodeString", "string", "string!", "zell_encoding_hex_decode_string"),
    // encoding.json: the C half (std_json.c, ABI.md 17.4): the parsed
    // document (rtParse .. rtIndex; handles are runtime addresses, node 0
    // is null) and the text writer (rtW*); the rest is Zell
    // (sema/reflect_src/json.zl)
    f!("encoding.json", "Quote", "string", "string", "zell_encoding_json_quote"),
    f!("encoding.json", "rtParse", "string", "int!", "zell_encoding_json_rt_parse"),
    f!("encoding.json", "rtFree", "int", "void", "zell_encoding_json_rt_free"),
    f!("encoding.json", "rtRoot", "int", "int", "zell_encoding_json_rt_root"),
    f!("encoding.json", "rtKind", "int", "int", "zell_encoding_json_rt_kind"),
    f!("encoding.json", "rtIsInt", "int", "bool", "zell_encoding_json_rt_is_int"),
    f!("encoding.json", "rtBool", "int", "bool", "zell_encoding_json_rt_bool"),
    f!("encoding.json", "rtInt", "int", "int", "zell_encoding_json_rt_int"),
    f!("encoding.json", "rtFloat", "int", "float", "zell_encoding_json_rt_float"),
    f!("encoding.json", "rtStr", "int", "string", "zell_encoding_json_rt_str"),
    f!("encoding.json", "rtLen", "int", "int", "zell_encoding_json_rt_len"),
    f!("encoding.json", "rtFirst", "int", "int", "zell_encoding_json_rt_first"),
    f!("encoding.json", "rtNext", "int", "int", "zell_encoding_json_rt_next"),
    f!("encoding.json", "rtGet", "int, string", "int", "zell_encoding_json_rt_get"),
    f!("encoding.json", "rtGetFold", "int, string", "int", "zell_encoding_json_rt_get_fold"),
    f!("encoding.json", "rtIndex", "int, int, int", "int", "zell_encoding_json_rt_index"),
    f!("encoding.json", "rtWNew", "", "int", "zell_encoding_json_rt_w_new"),
    f!("encoding.json", "rtWFinish", "int", "string", "zell_encoding_json_rt_w_finish"),
    f!("encoding.json", "rtWRaw", "int, string", "void", "zell_encoding_json_rt_w_raw"),
    f!("encoding.json", "rtWKey", "int, string, bool", "void", "zell_encoding_json_rt_w_key"),
    f!("encoding.json", "rtWStr", "int, string", "void", "zell_encoding_json_rt_w_str"),
    f!("encoding.json", "rtWInt", "int, int", "void", "zell_encoding_json_rt_w_int"),
    f!("encoding.json", "rtWUint", "int, uint", "void", "zell_encoding_json_rt_w_uint"),
    f!("encoding.json", "rtWFloat", "int, float", "void", "zell_encoding_json_rt_w_float"),
    f!("encoding.json", "rtWBool", "int, bool", "void", "zell_encoding_json_rt_w_bool"),
    f!("encoding.json", "rtWNode", "int, int", "void", "zell_encoding_json_rt_w_node"),
    f!("encoding.json", "rtIndent", "string, string, string", "string", "zell_encoding_json_rt_indent"),
    f!("encoding.json", "rtWKeyInt", "int, string, bool, int", "void", "zell_encoding_json_rt_w_key_int"),
    f!("encoding.json", "rtWKeyStr", "int, string, bool, string", "void", "zell_encoding_json_rt_w_key_str"),
    f!("encoding.json", "rtWKeyFloat", "int, string, bool, float", "void", "zell_encoding_json_rt_w_key_float"),
    f!("encoding.json", "rtWKeyBool", "int, string, bool, bool", "void", "zell_encoding_json_rt_w_key_bool"),
    f!("encoding.json", "rtWStrs", "int, []string", "void", "zell_encoding_json_rt_w_strs"),
    // internal primitives of the Zell-source net (sema/stdlib_src/net.zl, net_udp.zl);
    // sockets are runtime handles (ABI.md 16)
    f!("net", "rtOpError", "error, string, error", "error", "zell_net_rt_op_error"),
    f!("net", "rtDial", "string, int, string, int", "int!", "zell_net_rt_dial"),
    f!("net", "rtDialUnix", "string", "int!", "zell_net_rt_dial_unix"),
    f!("net", "rtConnectResult", "int", "void!", "zell_net_rt_connect_result"),
    f!("net", "rtConnectCheck", "int", "int!", "zell_net_rt_connect_check"),
    f!("net", "rtWaitAnyWritable", "[]int, int", "Task<int>", "zell_net_rt_wait_any_writable"),
    f!("net", "rtListen", "string, int, int, int", "int!", "zell_net_rt_listen"),
    f!("net", "rtListenUnix", "string, int", "int!", "zell_net_rt_listen_unix"),
    f!("net", "rtAccept", "int", "int!", "zell_net_rt_accept"),
    f!("net", "rtRead", "int, int", "[]byte!", "zell_net_rt_read"),
    f!("net", "rtReadInto", "int, []byte, int", "int!", "zell_net_rt_read_into"),
    f!("net", "rtAgain", "", "bool", "zell_net_rt_again"),
    f!("net", "rtWrite", "int, []byte, int", "int!", "zell_net_rt_write"),
    f!("net", "rtWriteStr", "int, string, int", "int!", "zell_net_rt_write_str"),
    f!("net", "rtWriteV", "int, [][]byte, int", "int!", "zell_net_rt_write_v"),
    f!("net", "rtShutdown", "int, int", "void!", "zell_net_rt_shutdown"),
    f!("net", "rtClose", "int", "void", "zell_net_rt_close"),
    f!("net", "rtFree", "int", "void", "zell_net_rt_free"),
    f!("net", "rtIsClosed", "int", "bool", "zell_net_rt_is_closed"),
    f!("net", "rtFd", "int", "int", "zell_net_rt_fd"),
    f!("net", "rtWaitReadable", "int, int", "Task<int>", "zell_net_rt_wait_readable"),
    f!("net", "rtWaitWritable", "int, int", "Task<int>", "zell_net_rt_wait_writable"),
    f!("net", "rtLocalAddr", "int", "string", "zell_net_rt_local_addr"),
    f!("net", "rtRemoteAddr", "int", "string", "zell_net_rt_remote_addr"),
    f!("net", "rtPort", "int", "int", "zell_net_rt_port"),
    f!("net", "rtSetOpt", "int, int, int", "void!", "zell_net_rt_set_opt"),
    f!("net", "rtGetOpt", "int, int", "int!", "zell_net_rt_get_opt"),
    f!("net", "rtMcast", "int, bool, string, string", "void!", "zell_net_rt_mcast"),
    f!("net", "rtOpenFile", "string", "int!", "zell_net_rt_open_file"),
    f!("net", "rtFileSize", "int", "int", "zell_net_rt_file_size"),
    f!("net", "rtCloseFile", "int", "void", "zell_net_rt_close_file"),
    f!("net", "rtSendFile", "int, int, int, int", "int!", "zell_net_rt_send_file"),
    f!("net", "rtParseIP", "string", "[]byte", "zell_net_rt_parse_ip"),
    f!("net", "rtFormatIP", "[]byte", "string", "zell_net_rt_format_ip"),
    f!("net", "rtIsIP", "string", "bool", "zell_net_rt_is_ip"),
    f!("net", "rtIfIndex", "string", "int", "zell_net_rt_if_index"),
    f!("net", "rtIfName", "int", "string", "zell_net_rt_if_name"),
    f!("net", "rtInterfaces", "", "[]string!", "zell_net_rt_interfaces"),
    f!("net", "rtLookupPort", "string, string", "int", "zell_net_rt_lookup_port"),
    f!("net", "rtResolve", "string, int", "Task<int>", "zell_net_rt_resolve"),
    f!("net", "rtReverse", "string", "Task<int>", "zell_net_rt_reverse"),
    f!("net", "rtResolveTake", "int", "[]string!", "zell_net_rt_resolve_take"),
    f!("net", "rtResolveSync", "string, int", "[]string!", "zell_net_rt_resolve_sync"),
    f!("net", "rtUdp", "string, int, int", "int!", "zell_net_rt_udp"),
    f!("net", "rtUdpDial", "string, int, string, int", "int!", "zell_net_rt_udp_dial"),
    f!("net", "rtRecvFrom", "int, int", "[]byte!", "zell_net_rt_recv_from"),
    f!("net", "rtReadFrom", "int, []byte", "int!", "zell_net_rt_read_from"),
    f!("net", "rtLastFrom", "", "string", "zell_net_rt_last_from"),
    f!("net", "rtLastFromPort", "", "int", "zell_net_rt_last_from_port"),
    f!("net", "rtLastTrunc", "", "bool", "zell_net_rt_last_trunc"),
    f!("net", "rtSendTo", "int, []byte, string, int", "int!", "zell_net_rt_send_to"),
    f!("net", "rtBytes", "int", "[]byte", "zell_net_rt_bytes"),
    f!("net", "rtCopyBytes", "[]byte, int, []byte, int, int", "int", "zell_net_rt_copy_bytes"),
    f!("net", "rtIndexByte", "[]byte, int, int", "int", "zell_net_rt_index_byte"),
    f!("net", "rtJoin", "[]byte, []byte, int", "[]byte", "zell_net_rt_join"),
    // net.http / net.ws hot loops (runtime/src/std_http.c, ABI.md 16.5)
    f!("net.http", "rtCanonical", "string", "string", "zell_net_http_rt_canonical"),
    f!("net.http", "rtHeadEnd", "[]byte, int, int", "int", "zell_net_http_rt_head_end"),
    f!("net.http", "rtParseHead", "[]byte, int, int, int, map[string]string", "[]string!", "zell_net_http_rt_parse_head"),
    f!("net.http", "rtHeadInfo", "", "int", "zell_net_http_rt_head_info"),
    f!("net.http", "rtHasToken", "string, string", "bool", "zell_net_http_rt_has_token"),
    f!("net.http", "rtStatusText", "int", "string", "zell_net_http_rt_status_text"),
    f!("net.http", "rtDate", "", "string", "zell_net_http_rt_date"),
    f!("net.http", "rtFormatHead", "int, string, map[string]string, int, int, string", "string", "zell_net_http_rt_format_head"),
    f!("net.http", "rtMask", "[]byte, int, int, int, int", "void", "zell_net_http_rt_mask"),
    f!("net.http.hpack", "rtHuffLen", "string", "int", "zell_net_http_hpack_rt_huff_len"),
    f!("net.http.hpack", "rtHuffEncode", "string", "[]byte", "zell_net_http_hpack_rt_huff_encode"),
    f!("net.http.hpack", "rtHuffDecode", "[]byte, int, int", "string!", "zell_net_http_hpack_rt_huff_decode"),
    f!("net.http", "rtItoa", "int", "string", "zell_net_http_rt_itoa"),
    f!("net.http", "rtAtoi", "string", "int", "zell_net_http_rt_atoi"),
    f!("net.http", "rtHttpTime", "int", "string", "zell_net_http_rt_http_time"),
    f!("net.http", "rtParseHttpTime", "string", "int", "zell_net_http_rt_parse_http_time"),
    // internal primitives of the Zell-source diag / log
    // (sema/stdlib_src/{diag,log}.zl; runtime diag.c, profile.c)
    f!("diag", "rtSlots", "", "map[string]any", "zell_diag_rt_slots"),
    f!("diag", "rtSetSlots", "map[string]any", "void", "zell_diag_rt_set_slots"),
    f!("diag", "rtLock", "", "void", "zell_diag_rt_lock"),
    f!("diag", "rtUnlock", "", "void", "zell_diag_rt_unlock"),
    f!("diag", "rtLiveObjects", "", "int", "zell_diag_rt_live_objects"),
    f!("diag", "rtAllocations", "", "int", "zell_diag_rt_allocations"),
    f!("diag", "rtLiveTasks", "", "int", "zell_diag_rt_live_tasks"),
    f!("diag", "rtQueueLen", "", "int", "zell_diag_rt_queue_len"),
    f!("diag", "rtWorkers", "", "int", "zell_diag_rt_workers"),
    f!("diag", "rtMonoNanos", "", "int", "zell_diag_rt_mono_nanos"),
    f!("diag", "rtHeapInUse", "", "int", "zell_diag_rt_heap_in_use"),
    f!("diag", "rtHeapRetained", "", "int", "zell_diag_rt_heap_retained"),
    f!("diag", "rtHeapReleased", "", "int", "zell_diag_rt_heap_released"),
    f!("runtime", "FreeOSMemory", "", "void", "zell_runtime_free_os_memory"),
    f!("runtime", "Workers", "", "int", "zell_runtime_workers"),
    f!("runtime", "CPULimit", "", "int", "zell_runtime_cpu_limit"),
    f!("runtime", "MemLimit", "", "int", "zell_runtime_mem_limit"),
    f!("runtime", "SetMemLimit", "int", "int", "zell_runtime_set_mem_limit"),
    f!("testing", "rtCapture", "func()", "int", "zell_testing_rt_capture"),
    f!("testing", "rtCaptureMessage", "", "string", "zell_testing_rt_capture_message"),
    f!("runtime", "rtCollectCycles", "", "int", "zell_runtime_rt_collect_cycles"),
    f!("runtime", "rtCycleStat", "int", "int", "zell_runtime_rt_cycle_stat"),
    f!("diag", "rtStartCPUProfile", "string, int", "void!", "zell_diag_rt_start_cpu_profile"),
    f!("diag", "rtStopCPUProfile", "", "int!", "zell_diag_rt_stop_cpu_profile"),
    f!("diag", "rtHeapText", "", "string", "zell_diag_rt_heap_text"),
    f!("diag", "rtHeapTypes", "", "string", "zell_diag_rt_heap_types"),
    f!("diag", "rtHeapCounting", "", "bool", "zell_diag_rt_heap_counting"),
    f!("diag", "rtWriteHeapProfile", "string", "void!", "zell_diag_rt_write_heap_profile"),
    f!("log", "rtOpen", "string", "int!", "zell_log_rt_open"),
    f!("log", "rtWrite", "int, string", "void!", "zell_log_rt_write"),
    f!("log", "rtClose", "int", "void", "zell_log_rt_close"),
    f!("log", "rtIsatty", "int", "bool", "zell_log_rt_isatty"),
    // crypto primitives (runtime/src/crypto/, ABI.md section 19)
    f!("crypto.hash", "Sha384", "string", "[]byte", "zell_crypto_hash_sha384"),
    f!("crypto.hash", "Sha384Hex", "string", "string", "zell_crypto_hash_sha384_hex"),
    f!("crypto.hash", "Sum", "string, []byte", "[]byte!", "zell_crypto_hash_sum"),
    f!("crypto.hash", "rtNew", "string", "[]byte!", "zell_crypto_hash_rt_new"),
    f!("crypto.hash", "rtWrite", "[]byte, []byte", "void", "zell_crypto_hash_rt_write"),
    f!("crypto.hash", "rtSum", "[]byte", "[]byte", "zell_crypto_hash_rt_sum"),
    f!("crypto.hmac", "Sum", "string, []byte, []byte", "[]byte!", "zell_crypto_hmac_sum"),
    f!("crypto.hkdf", "Extract", "string, []byte, []byte", "[]byte!", "zell_crypto_hkdf_extract"),
    f!("crypto.hkdf", "Expand", "string, []byte, []byte, int", "[]byte!", "zell_crypto_hkdf_expand"),
    f!("crypto.aes", "GcmSeal", "[]byte, []byte, []byte, []byte", "[]byte!", "zell_crypto_aes_gcm_seal"),
    f!("crypto.aes", "GcmOpen", "[]byte, []byte, []byte, []byte", "[]byte!", "zell_crypto_aes_gcm_open"),
    f!("crypto.aes", "EncryptBlock", "[]byte, []byte", "[]byte!", "zell_crypto_aes_encrypt_block"),
    f!("crypto.aes", "HasHardware", "", "bool", "zell_crypto_aes_has_hardware"),
    f!("crypto.chacha", "Seal", "[]byte, []byte, []byte, []byte", "[]byte!", "zell_crypto_chacha_seal"),
    f!("crypto.chacha", "Open", "[]byte, []byte, []byte, []byte", "[]byte!", "zell_crypto_chacha_open"),
    f!("crypto.x25519", "GenerateKey", "", "[]byte!", "zell_crypto_x25519_generate_key"),
    f!("crypto.x25519", "PublicKey", "[]byte", "[]byte!", "zell_crypto_x25519_public_key"),
    f!("crypto.x25519", "SharedSecret", "[]byte, []byte", "[]byte!", "zell_crypto_x25519_shared_secret"),
    f!("crypto.ecdh", "GenerateKey", "", "[]byte!", "zell_crypto_ecdh_generate_key"),
    f!("crypto.ecdh", "PublicKey", "[]byte", "[]byte!", "zell_crypto_ecdh_public_key"),
    f!("crypto.ecdh", "SharedSecret", "[]byte, []byte", "[]byte!", "zell_crypto_ecdh_shared_secret"),
    f!("crypto.ecdsa", "GenerateKey", "", "[]byte!", "zell_crypto_ecdsa_generate_key"),
    f!("crypto.ecdsa", "PublicKey", "[]byte", "[]byte!", "zell_crypto_ecdsa_public_key"),
    f!("crypto.ecdsa", "Sign", "[]byte, []byte", "[]byte!", "zell_crypto_ecdsa_sign"),
    f!("crypto.ecdsa", "Verify", "[]byte, []byte, []byte", "bool", "zell_crypto_ecdsa_verify"),
    f!("crypto.ed25519", "GenerateKey", "", "[]byte!", "zell_crypto_ed25519_generate_key"),
    f!("crypto.ed25519", "PublicKey", "[]byte", "[]byte!", "zell_crypto_ed25519_public_key"),
    f!("crypto.ed25519", "Sign", "[]byte, []byte", "[]byte!", "zell_crypto_ed25519_sign"),
    f!("crypto.ed25519", "Verify", "[]byte, []byte, []byte", "bool", "zell_crypto_ed25519_verify"),
    f!("crypto.rsa", "VerifyPKCS1v15", "[]byte, []byte, string, []byte, []byte", "bool", "zell_crypto_rsa_verify_pkcs1v15"),
    f!("crypto.rsa", "VerifyPSS", "[]byte, []byte, string, []byte, []byte", "bool", "zell_crypto_rsa_verify_pss"),
    f!("crypto.rsa", "SignPKCS1v15", "[]byte, string, []byte", "[]byte!", "zell_crypto_rsa_sign_pkcs1v15"),
    f!("crypto.rsa", "SignPSS", "[]byte, string, []byte", "[]byte!", "zell_crypto_rsa_sign_pss"),
    // internal primitives of the Zell-source crypto.x509 / net.tls
    // (sema/stdlib_src/{x509,tls}.zl)
    f!("crypto.x509", "rtSystemRootsPath", "", "string", "zell_crypto_x509_rt_system_roots_path"),
    f!("net.tls", "rtConcat", "[]byte, []byte", "[]byte", "zell_net_tls_rt_concat"),
    f!("net.tls", "rtZeros", "int", "[]byte", "zell_net_tls_rt_zeros"),
    f!("net.tls", "rtCopy", "[]byte, int, []byte", "void", "zell_net_tls_rt_copy"),
    f!("net.tls", "rtAeadNew", "int, []byte", "[]byte!", "zell_net_tls_rt_aead_new"),
    f!("net.tls", "rtAeadSeal", "[]byte, []byte, []byte, []byte", "[]byte", "zell_net_tls_rt_aead_seal"),
    f!("net.tls", "rtAeadOpen", "[]byte, []byte, []byte, []byte", "[]byte!", "zell_net_tls_rt_aead_open"),
    // tier 0/1 stdlib (docs/STDLIB.md): strconv (std_strconv.c), unicode (std_unicode.c)
    f!("strconv", "rtItoa", "int", "string", "zell_strconv_rt_itoa"),
    f!("strconv", "rtUtoa", "uint", "string", "zell_strconv_rt_utoa"),
    f!("strconv", "rtFormatFloat","float, int, int", "string", "zell_strconv_rt_format_float"),
    f!("strconv", "rtParseFloat", "string", "float!", "zell_strconv_rt_parse_float"),
    f!("unicode", "IsLetter", "char", "bool", "zell_unicode_is_letter"),
    f!("unicode", "IsDigit", "char", "bool", "zell_unicode_is_digit"),
    f!("unicode", "IsNumber", "char", "bool", "zell_unicode_is_number"),
    f!("unicode", "IsSpace", "char", "bool", "zell_unicode_is_space"),
    f!("unicode", "IsUpper", "char", "bool", "zell_unicode_is_upper"),
    f!("unicode", "IsLower", "char", "bool", "zell_unicode_is_lower"),
    f!("unicode", "IsTitle", "char", "bool", "zell_unicode_is_title"),
    f!("unicode", "IsPunct", "char", "bool", "zell_unicode_is_punct"),
    f!("unicode", "IsSymbol", "char", "bool", "zell_unicode_is_symbol"),
    f!("unicode", "IsMark", "char", "bool", "zell_unicode_is_mark"),
    f!("unicode", "IsControl", "char", "bool", "zell_unicode_is_control"),
    f!("unicode", "IsPrint", "char", "bool", "zell_unicode_is_print"),
    f!("unicode", "IsGraphic", "char", "bool", "zell_unicode_is_graphic"),
    f!("unicode", "ToUpper", "char", "char", "zell_unicode_to_upper"),
    f!("unicode", "ToLower", "char", "char", "zell_unicode_to_lower"),
    f!("unicode", "ToTitle", "char", "char", "zell_unicode_to_title"),
    f!("unicode", "SimpleFold", "char", "char", "zell_unicode_simple_fold"),
    // math.bits (std_math_bits.c)
    f!("math", "Float64bits", "float", "uint", "zell_math_float64bits"),
    f!("math", "Float64frombits", "uint", "float", "zell_math_float64frombits"),
    f!("math", "Float32bits", "float32", "uint32", "zell_math_float32bits"),
    f!("math", "Float32frombits", "uint32", "float32", "zell_math_float32frombits"),
    f!("math.bits", "LeadingZeros", "uint", "int", "zell_math_bits_leading_zeros"),
    f!("math.bits", "LeadingZeros32", "uint32", "int", "zell_math_bits_leading_zeros32"),
    f!("math.bits", "TrailingZeros", "uint", "int", "zell_math_bits_trailing_zeros"),
    f!("math.bits", "TrailingZeros32", "uint32", "int", "zell_math_bits_trailing_zeros32"),
    f!("math.bits", "OnesCount", "uint", "int", "zell_math_bits_ones_count"),
    f!("math.bits", "OnesCount32", "uint32", "int", "zell_math_bits_ones_count32"),
    f!("math.bits", "Len", "uint", "int", "zell_math_bits_len"),
    f!("math.bits", "Len32", "uint32", "int", "zell_math_bits_len32"),
    f!("math.bits", "RotateLeft", "uint, int", "uint", "zell_math_bits_rotate_left"),
    f!("math.bits", "RotateLeft32", "uint32, int", "uint32", "zell_math_bits_rotate_left32"),
    f!("math.bits", "Reverse", "uint", "uint", "zell_math_bits_reverse"),
    f!("math.bits", "Reverse32", "uint32", "uint32", "zell_math_bits_reverse32"),
    f!("math.bits", "ReverseBytes", "uint", "uint", "zell_math_bits_reverse_bytes"),
    f!("math.bits", "ReverseBytes32", "uint32", "uint32", "zell_math_bits_reverse_bytes32"),
    f!("math.bits", "ReverseBytes16", "uint16", "uint16", "zell_math_bits_reverse_bytes16"),
    f!("math.bits", "rtMulHi", "uint, uint", "uint", "zell_math_bits_rt_mul_hi"),
    f!("math.bits", "rtDivQuo", "uint, uint, uint", "uint", "zell_math_bits_rt_div_quo"),
    f!("math.bits", "rtDivRem", "uint, uint, uint", "uint", "zell_math_bits_rt_div_rem"),
    // >>> tier 2 stdlib externs
    // regex: the C half (std_regex.c); the engine is Zell (sema/stdlib_src/regex*.zl)
    f!("regex", "rtUnicode", "string", "[]int", "zell_regex_rt_unicode"),
    f!("regex", "rtFold", "int", "int", "zell_regex_rt_fold"),
    f!("regex", "rtInts", "int", "[]int", "zell_regex_rt_ints"),
    f!("regex", "rtBytes", "int", "[]byte", "zell_regex_rt_bytes"),
    f!("regex", "rtIndex", "string, string, int", "int", "zell_regex_rt_index"),
    f!("regex", "rtTryLock", "*int", "bool", "zell_regex_rt_try_lock"),
    f!("regex", "rtUnlock", "*int", "void", "zell_regex_rt_unlock"),
    // compress: DEFLATE coder and checksums (std_compress.c), zstd decoder (std_zstd.c)
    f!("compress", "rtCrc32", "int, []byte", "int", "zell_compress_rt_crc32"),
    f!("compress", "rtAdler32", "int, []byte", "int", "zell_compress_rt_adler32"),
    f!("compress", "rtInflateNew", "", "[]byte", "zell_compress_rt_inflate_new"),
    f!("compress", "rtInflate", "[]byte, []byte", "[]byte!", "zell_compress_rt_inflate"),
    f!("compress", "rtInflateDone", "[]byte", "bool", "zell_compress_rt_inflate_done"),
    f!("compress", "rtInflateTail", "[]byte", "[]byte", "zell_compress_rt_inflate_tail"),
    f!("compress", "rtInflateUnused", "[]byte", "int", "zell_compress_rt_inflate_unused"),
    f!("compress", "rtDeflate", "[]byte, []byte, int, bool", "[]byte", "zell_compress_rt_deflate"),
    f!("compress", "rtConcat", "[]byte, []byte", "[]byte", "zell_compress_rt_concat"),
    f!("compress", "rtCopy", "[]byte, int, int", "[]byte", "zell_compress_rt_copy"),
    f!("compress", "rtZstdDecode", "[]byte", "[]byte!", "zell_compress_rt_zstd_decode"),
    f!("compress", "rtZstdFrameLen", "[]byte", "int", "zell_compress_rt_zstd_frame_len"),
    f!("compress", "rtXxh64", "[]byte", "int", "zell_compress_rt_xxh64"),
    f!("compress", "rtZeros", "int", "[]byte", "zell_compress_rt_zeros"),
    // encoding.xml: the tokenizer (std_xml.c); the rest is Zell (sema/stdlib_src/xml.zl)
    f!("encoding.xml", "rtScan", "string, bool", "[]string!", "zell_encoding_xml_rt_scan"),
    f!("encoding.xml", "rtEscape", "string, bool", "string", "zell_encoding_xml_rt_escape"),
    // html (std_html.c)
    f!("html", "Escape", "string", "string", "zell_html_escape"),
    f!("html", "Unescape", "string", "string", "zell_html_unescape"),
    // mime extension table (std_mime.c)
    f!("mime", "rtType", "string", "string", "zell_mime_rt_type"),
    f!("mime", "rtAdd", "string, string", "void", "zell_mime_rt_add"),
    f!("mime", "rtExts", "string", "[]string", "zell_mime_rt_exts"),
    // <<< tier 2 stdlib externs
    // >>> tier 3 stdlib externs (docs/STDLIB.md level 3)
    // database.sqlite over the system libsqlite3 (std_sqlite.c; LINK_LIBS adds -lsqlite3)
    f!("database.sqlite", "rtAvailable", "", "bool", "zell_database_sqlite_rt_available"),
    f!("database.sqlite", "rtVersion", "", "string", "zell_database_sqlite_rt_version"),
    f!("database.sqlite", "rtOpen", "string, int", "int!", "zell_database_sqlite_rt_open"),
    f!("database.sqlite", "rtClose", "int", "void", "zell_database_sqlite_rt_close"),
    f!("database.sqlite", "rtBusyTimeout", "int, int", "void", "zell_database_sqlite_rt_busy_timeout"),
    f!("database.sqlite", "rtExec", "int, string", "void!", "zell_database_sqlite_rt_exec"),
    f!("database.sqlite", "rtPrepare", "int, string", "int!", "zell_database_sqlite_rt_prepare"),
    f!("database.sqlite", "rtFinalize", "int", "void", "zell_database_sqlite_rt_finalize"),
    f!("database.sqlite", "rtReset", "int", "void", "zell_database_sqlite_rt_reset"),
    f!("database.sqlite", "rtParamCount", "int", "int", "zell_database_sqlite_rt_param_count"),
    f!("database.sqlite", "rtParamIndex", "int, string", "int", "zell_database_sqlite_rt_param_index"),
    f!("database.sqlite", "rtBindNull", "int, int", "void!", "zell_database_sqlite_rt_bind_null"),
    f!("database.sqlite", "rtBindInt", "int, int, int", "void!", "zell_database_sqlite_rt_bind_int"),
    f!("database.sqlite", "rtBindFloat", "int, int, float", "void!", "zell_database_sqlite_rt_bind_float"),
    f!("database.sqlite", "rtBindText", "int, int, string", "void!", "zell_database_sqlite_rt_bind_text"),
    f!("database.sqlite", "rtBindBlob", "int, int, []byte", "void!", "zell_database_sqlite_rt_bind_blob"),
    f!("database.sqlite", "rtStep", "int", "int!", "zell_database_sqlite_rt_step"),
    f!("database.sqlite", "rtColumnCount", "int", "int", "zell_database_sqlite_rt_column_count"),
    f!("database.sqlite", "rtColumnName", "int, int", "string", "zell_database_sqlite_rt_column_name"),
    f!("database.sqlite", "rtColumnDecltype", "int, int", "string", "zell_database_sqlite_rt_column_decltype"),
    f!("database.sqlite", "rtColumnType", "int, int", "int", "zell_database_sqlite_rt_column_type"),
    f!("database.sqlite", "rtColumnInt", "int, int", "int", "zell_database_sqlite_rt_column_int"),
    f!("database.sqlite", "rtColumnFloat", "int, int", "float", "zell_database_sqlite_rt_column_float"),
    f!("database.sqlite", "rtColumnText", "int, int", "string", "zell_database_sqlite_rt_column_text"),
    f!("database.sqlite", "rtColumnBlob", "int, int", "[]byte", "zell_database_sqlite_rt_column_blob"),
    f!("database.sqlite", "rtChanges", "int", "int", "zell_database_sqlite_rt_changes"),
    f!("database.sqlite", "rtLastInsertId", "int", "int", "zell_database_sqlite_rt_last_insert_id"),
    f!("database.sqlite", "rtErrcode", "int", "int", "zell_database_sqlite_rt_errcode"),
    f!("database.sqlite", "rtAutocommit", "int", "bool", "zell_database_sqlite_rt_autocommit"),
    // crypto tier-3 additions (crypto/api2.c, crypto/{kdf,cbc,p384}.c, std_argon2.c; ABI.md 22)
    f!("crypto.kdf", "Pbkdf2", "[]byte, []byte, int, int, string", "[]byte!", "zell_crypto_kdf_pbkdf2"),
    f!("crypto.kdf", "Scrypt", "[]byte, []byte, int, int, int, int", "[]byte!", "zell_crypto_kdf_scrypt"),
    f!("crypto.kdf", "rtArgon2", "int, []byte, []byte, []byte, []byte, int, int, int, int", "[]byte!", "zell_crypto_kdf_rt_argon2"),
    f!("crypto.hash", "Blake2b", "[]byte, int, []byte", "[]byte!", "zell_crypto_hash_blake2b"),
    f!("crypto.ecdh", "GenerateKeyP384", "", "[]byte!", "zell_crypto_ecdh_generate_key_p384"),
    f!("crypto.ecdh", "PublicKeyP384", "[]byte", "[]byte!", "zell_crypto_ecdh_public_key_p384"),
    f!("crypto.ecdh", "SharedSecretP384", "[]byte, []byte", "[]byte!", "zell_crypto_ecdh_shared_secret_p384"),
    f!("crypto.ecdsa", "GenerateKeyP384", "", "[]byte!", "zell_crypto_ecdsa_generate_key_p384"),
    f!("crypto.ecdsa", "PublicKeyP384", "[]byte", "[]byte!", "zell_crypto_ecdsa_public_key_p384"),
    f!("crypto.ecdsa", "SignP384", "[]byte, []byte", "[]byte!", "zell_crypto_ecdsa_sign_p384"),
    f!("crypto.ecdsa", "SignP384Raw", "[]byte, []byte", "[]byte!", "zell_crypto_ecdsa_sign_p384_raw"),
    f!("crypto.ecdsa", "VerifyP384", "[]byte, []byte, []byte", "bool", "zell_crypto_ecdsa_verify_p384"),
    f!("crypto.aes", "DecryptBlock", "[]byte, []byte", "[]byte!", "zell_crypto_aes_decrypt_block"),
    f!("crypto.aes", "CbcEncrypt", "[]byte, []byte, []byte", "[]byte!", "zell_crypto_aes_cbc_encrypt"),
    f!("crypto.aes", "CbcDecrypt", "[]byte, []byte, []byte", "[]byte!", "zell_crypto_aes_cbc_decrypt"),
    f!("crypto.aes", "CbcEncryptRaw", "[]byte, []byte, []byte", "[]byte!", "zell_crypto_aes_cbc_encrypt_raw"),
    f!("crypto.aes", "CbcDecryptRaw", "[]byte, []byte, []byte", "[]byte!", "zell_crypto_aes_cbc_decrypt_raw"),
    f!("crypto.aes", "Pkcs7Pad", "[]byte, int", "[]byte!", "zell_crypto_aes_pkcs7_pad"),
    f!("crypto.aes", "Pkcs7Unpad", "[]byte, int", "[]byte!", "zell_crypto_aes_pkcs7_unpad"),
    f!("crypto.rsa", "EncryptOAEP", "[]byte, []byte, string, []byte, []byte", "[]byte!", "zell_crypto_rsa_encrypt_oaep"),
    f!("crypto.rsa", "DecryptOAEP", "[]byte, string, []byte, []byte", "[]byte!", "zell_crypto_rsa_decrypt_oaep"),
    f!("crypto.rsa", "rtGenPrime", "int, int", "[]byte!", "zell_crypto_rsa_rt_gen_prime"),
    f!("crypto.rsa", "IsProbablePrime", "[]byte, int", "bool", "zell_crypto_rsa_is_probable_prime"),
    // compress.zstd encoder (std_zstd_enc.c)
    f!("compress", "rtZstdEncode", "[]byte, int, bool", "[]byte!", "zell_compress_rt_zstd_encode"),
    // compress.brotli decoder (std_brotli.c)
    f!("compress", "rtBrotliDecode", "[]byte", "[]byte!", "zell_compress_rt_brotli_decode"),
    // <<< tier 3 stdlib externs
    // >>> tier 3b stdlib externs (runtime/src/std_infra.c, ABI.md section 24)
    f!("data", "rtAppend", "[]byte, []byte", "[]byte", "zell_data_rt_append"),
    f!("data", "rtAppendStr", "[]byte, string", "[]byte", "zell_data_rt_append_str"),
    f!("data", "rtCrc16", "string", "int", "zell_data_rt_crc16"),
    f!("data", "rtCrc32c", "int, []byte", "int", "zell_data_rt_crc32c"),
    f!("data", "rtMurmur2", "[]byte", "int", "zell_data_rt_murmur2"),
    // <<< tier 3b stdlib externs
    // math.big limb arithmetic (std_math_big.c)
    f!("math.big", "rtAdd", "[]uint32, []uint32", "[]uint32", "zell_math_big_rt_add"),
    f!("math.big", "rtSub", "[]uint32, []uint32", "[]uint32", "zell_math_big_rt_sub"),
    f!("math.big", "rtCmp", "[]uint32, []uint32", "int", "zell_math_big_rt_cmp"),
    f!("math.big", "rtMul", "[]uint32, []uint32", "[]uint32", "zell_math_big_rt_mul"),
    f!("math.big", "rtDivMod", "[]uint32, []uint32", "[]uint32", "zell_math_big_rt_divmod"),
    f!("math.big", "rtShl", "[]uint32, int", "[]uint32", "zell_math_big_rt_shl"),
    f!("math.big", "rtShr", "[]uint32, int", "[]uint32", "zell_math_big_rt_shr"),
    f!("math.big", "rtBitLen", "[]uint32", "int", "zell_math_big_rt_bit_len"),
    f!("math.big", "rtFormat", "[]uint32, int", "string", "zell_math_big_rt_format"),
    f!("math.big", "rtParse", "string, int", "[]uint32!", "zell_math_big_rt_parse"),
    f!("math.big", "rtFromBytes", "[]byte", "[]uint32", "zell_math_big_rt_from_bytes"),
    f!("math.big", "rtToBytes", "[]uint32", "[]byte", "zell_math_big_rt_to_bytes"),
    f!("math.big", "rtFromUint", "uint", "[]uint32", "zell_math_big_rt_from_uint"),
    // hash.maphash (std_hash.c)
    f!("hash.maphash", "rtSip13", "uint, uint, []byte", "uint", "zell_hash_maphash_rt_sip13"),
    f!("hash.maphash", "rtSip13Str", "uint, uint, string", "uint", "zell_hash_maphash_rt_sip13_str"),
    // time types (time.zl): the local zone's TZif bytes (std_time_zone.c)
    f!("time", "rtLocalTZif", "", "string!", "zell_time_rt_local_tzif"),
    f!("time", "rtZoneTZif", "string", "string!", "zell_time_rt_zone_tzif"),
    // os files (os.zl over std_os_file.c)
    f!("os", "rtOpen", "string, int, int", "int!", "zell_os_rt_open"),
    f!("os", "rtRead", "int, []byte", "int!", "zell_os_rt_read"),
    f!("os", "rtPread", "int, []byte, int", "int!", "zell_os_rt_pread"),
    f!("os", "rtWrite", "int, []byte", "int!", "zell_os_rt_write"),
    f!("os", "rtWriteStr", "int, string", "int!", "zell_os_rt_write_str"),
    f!("os", "rtSeek", "int, int, int", "int!", "zell_os_rt_seek"),
    f!("os", "rtClose", "int", "void!", "zell_os_rt_close"),
    f!("os", "rtSync", "int", "void!", "zell_os_rt_sync"),
    f!("os", "rtTruncate", "int, int", "void!", "zell_os_rt_truncate"),
    f!("os", "rtFstat", "int", "[]int!", "zell_os_rt_fstat"),
    f!("os", "rtStat", "string, bool", "[]int!", "zell_os_rt_stat"),
    f!("os", "rtReadDir", "string", "[]string!", "zell_os_rt_read_dir"),
    f!("os", "rtChmod", "string, int", "void!", "zell_os_rt_chmod"),
    // sync atomics (std_sync.c)
    f!("sync", "rtLoad", "*int", "int", "zell_sync_rt_load"),
    f!("sync", "rtStore", "*int, int", "void", "zell_sync_rt_store"),
    f!("sync", "rtAdd", "*int, int", "int", "zell_sync_rt_add"),
    f!("sync", "rtSwap", "*int, int", "int", "zell_sync_rt_swap"),
    f!("sync", "rtCas", "*int, int, int", "bool", "zell_sync_rt_cas"),
    f!("sync", "rtYield", "", "void", "zell_sync_rt_yield"),
    // os.signal (std_os_signal.c)
    f!("os.signal", "rtWait", "int", "Task<void>", "zell_os_signal_rt_wait"),
    f!("os.signal", "rtIgnore", "int", "void", "zell_os_signal_rt_ignore"),
    f!("os.signal", "rtReset", "int", "void", "zell_os_signal_rt_reset"),
    f!("os.signal", "rtRaise", "int", "void", "zell_os_signal_rt_raise"),
    f!("os.signal", "rtPending", "int", "int", "zell_os_signal_rt_pending"),
    f!("os.signal", "rtCatch", "int", "void", "zell_os_signal_rt_catch"),
    // process (std_process.c)
    f!("process", "rtSpawn", "string, []string, []string, bool, string, int, int, int", "[]int!", "zell_process_rt_spawn"),
    f!("process", "rtRead", "int, []byte", "int!", "zell_process_rt_read"),
    f!("process", "rtWrite", "int, []byte, int", "int!", "zell_process_rt_write"),
    f!("process", "rtClose", "int", "void", "zell_process_rt_close"),
    f!("process", "rtWaitPid", "int", "int!", "zell_process_rt_wait_pid"),
    f!("process", "rtKill", "int, int", "void!", "zell_process_rt_kill"),
    f!("process", "rtExecutable", "string", "bool", "zell_process_rt_executable"),
    f!("process", "rtWaitReadable", "int, int", "Task<int>", "zell_process_rt_wait_readable"),
    f!("process", "rtWaitWritable", "int, int", "Task<int>", "zell_process_rt_wait_writable"),
    // os.user (std_os_user.c)
    f!("os.user", "rtLookup", "string, int, bool", "[]string!", "zell_os_user_rt_lookup"),
    f!("os.user", "rtGroup", "string, int, bool", "[]string!", "zell_os_user_rt_group"),
    f!("os.user", "rtUid", "", "int", "zell_os_user_rt_uid"),
    // uuid
    f!("uuid", "New", "", "string", "zell_uuid_new"),
    f!("uuid", "IsValid", "string", "bool", "zell_uuid_is_valid"),
    // >>> debugger externs (runtime/src/debug.c, std_unsafe.c, std_thread.c, std_plugin.c; ABI.md section 22)
    f!("debug", "rtTables", "", "int", "zell_debug_rt_tables"),
    f!("debug", "rtSetHook", "func(int;int;int;int) int", "void", "zell_debug_rt_set_hook"),
    f!("debug", "rtSetExitHook", "func(int)", "void", "zell_debug_rt_set_exit_hook"),
    f!("debug", "rtSetBp", "int, bool", "void", "zell_debug_rt_set_bp"),
    f!("debug", "rtPathCanon", "string", "string", "zell_debug_rt_path_canon"),
    f!("debug", "rtPathMatch", "string, string", "int", "zell_debug_rt_path_match"),
    f!("debug", "rtWriteFloat", "int, int, float", "void", "zell_debug_rt_write_float"),
    f!("debug", "rtSetString", "int, string", "void", "zell_debug_rt_set_string"),
    f!("debug", "rtOnError", "bool", "void", "zell_debug_rt_on_error"),
    f!("debug", "rtStopNext", "", "void", "zell_debug_rt_stop_next"),
    f!("debug", "rtFreeze", "bool", "void", "zell_debug_rt_freeze"),
    f!("debug", "rtResume", "int, int, bool", "bool", "zell_debug_rt_resume"),
    f!("debug", "rtReleaseAll", "", "void", "zell_debug_rt_release_all"),
    f!("debug", "rtTasks", "", "[]int", "zell_debug_rt_tasks"),
    f!("debug", "rtTaskState", "int", "int", "zell_debug_rt_task_state"),
    f!("debug", "rtTaskFrame", "int", "int", "zell_debug_rt_task_frame"),
    f!("debug", "rtTaskFunc", "int", "int", "zell_debug_rt_task_func"),
    f!("debug", "rtTaskReason", "int", "int", "zell_debug_rt_task_reason"),
    f!("debug", "rtLock", "int", "void", "zell_debug_rt_lock"),
    f!("debug", "rtUnlock", "int", "void", "zell_debug_rt_unlock"),
    f!("debug", "rtWaitReady", "", "void", "zell_debug_rt_wait_ready"),
    f!("debug", "rtSignalReady", "", "void", "zell_debug_rt_signal_ready"),
    f!("debug", "rtIgnoreHooks", "", "void", "zell_debug_rt_ignore_hooks"),
    f!("debug", "rtPanicText", "", "string", "zell_debug_rt_panic_text"),
    f!("debug", "rtCurError", "", "int", "zell_debug_rt_cur_error"),
    f!("debug", "rtErrorText", "int", "string", "zell_debug_rt_error_text"),
    f!("debug", "rtTypeName", "int", "string", "zell_debug_rt_type_name"),
    f!("debug", "rtAwaitInt", "Task<int>", "int", "zell_debug_rt_await_int"),
    f!("debug", "rtTaskDone", "int", "bool", "zell_debug_rt_task_done"),
    f!("debug", "rtTaskSlot", "int", "int", "zell_debug_rt_task_slot"),
    f!("debug", "rtSymName", "int", "string", "zell_debug_rt_sym_name"),
    f!("debug", "rtExit", "int", "void", "zell_debug_rt_exit"),
    f!("debug", "rtNowMs", "", "int", "zell_debug_rt_now_ms"),
    f!("debug", "rtString", "int", "string", "zell_debug_rt_string"),
    f!("debug", "rtMapLen", "int", "int", "zell_debug_rt_map_len"),
    f!("debug", "rtMapEntries", "int, int, int", "[]int", "zell_debug_rt_map_entries"),
    f!("debug", "rtErrorType", "int", "string", "zell_debug_rt_error_type"),
    f!("debug", "rtListen", "string, int", "int!", "zell_debug_rt_listen"),
    f!("debug", "rtSockPort", "int", "int", "zell_debug_rt_sock_port"),
    f!("debug", "rtAccept", "int", "int", "zell_debug_rt_accept"),
    f!("debug", "rtRecv", "int, int", "[]byte", "zell_debug_rt_recv"),
    f!("debug", "rtSend", "int, string", "bool", "zell_debug_rt_send"),
    f!("debug", "rtClose", "int", "void", "zell_debug_rt_close"),
    f!("debug", "rtEprint", "string", "void", "zell_debug_rt_eprint"),
    f!("unsafe", "ReadInt", "int, int", "int", "zell_unsafe_read_int"),
    f!("unsafe", "ReadUint", "int, int", "int", "zell_unsafe_read_uint"),
    f!("unsafe", "ReadFloat", "int, int", "float", "zell_unsafe_read_float"),
    f!("unsafe", "WriteInt", "int, int, int", "void", "zell_unsafe_write_int"),
    f!("unsafe", "ReadBytes", "int, int", "[]byte", "zell_unsafe_read_bytes"),
    f!("unsafe", "WriteBytes", "int, []byte", "void", "zell_unsafe_write_bytes"),
    f!("unsafe", "CString", "int", "string", "zell_unsafe_c_string"),
    f!("unsafe", "AddrOf", "any", "int", "zell_unsafe_addr_of"),
    f!("unsafe", "TypeOf", "any", "int", "zell_unsafe_type_of"),
    f!("unsafe", "CallFunc", "int, []int", "int", "zell_unsafe_call_func"),
    f!("thread", "Spawn", "func()", "int", "zell_thread_spawn"),
    f!("thread", "Join", "int", "void", "zell_thread_join"),
    f!("thread", "Sleep", "int", "void", "zell_thread_sleep"),
    f!("plugin", "rtOpen", "string", "int!", "zell_plugin_rt_open"),
    f!("plugin", "rtSym", "int, string", "int!", "zell_plugin_rt_sym"),
    f!("plugin", "rtClose", "int", "void", "zell_plugin_rt_close"),
    // <<< debugger externs
];

/// Parses a type in the table's mini syntax: builtin names, `[]T`, `T!`,
/// `Task<T>`, `*T` (passed as a plain pointer: only `&place` arguments of
/// the internal `rt*` externs use it), `map[K]V`.
pub fn parse_type(tt: &mut TypeTable, s: &str) -> TypeId {
    let s = s.trim();
    if let Some(inner) = s.strip_suffix('!') {
        let t = parse_type(tt, inner);
        return tt.fallible(t);
    }
    if let Some(inner) = s.strip_prefix("[]") {
        let t = parse_type(tt, inner);
        return tt.slice(t);
    }
    if let Some(inner) = s.strip_prefix("Task<").and_then(|r| r.strip_suffix('>')) {
        let t = parse_type(tt, inner);
        return tt.task(t);
    }
    if let Some(inner) = s.strip_prefix('*') {
        let t = parse_type(tt, inner);
        return tt.pointer(t);
    }
    // `func(int;int) int`: closure types of callbacks (`;` separates the
    // parameters, because the table splits parameter lists on ", ").
    if let Some((ps, ret)) = s.strip_prefix("func(").and_then(|r| r.split_once(')')) {
        let params = ps.split(';').map(str::trim).filter(|p| !p.is_empty()).map(|p| parse_type(tt, p)).collect();
        let ret = if ret.trim().is_empty() { tt.void } else { parse_type(tt, ret) };
        return tt.func(params, ret);
    }
    if let Some((k, v)) = s.strip_prefix("map[").and_then(|r| r.split_once(']')) {
        let (k, v) = (parse_type(tt, k), parse_type(tt, v));
        return tt.map(k, v);
    }
    tt.builtin(s).unwrap_or_else(|| panic!("stdlib table: unknown type `{s}`"))
}

impl StdFunc {
    /// Parameter and result types.
    pub fn signature(&self, tt: &mut TypeTable) -> (Vec<TypeId>, TypeId) {
        let params = if self.params.trim().is_empty() {
            Vec::new()
        } else {
            self.params.split(", ").map(|p| parse_type(tt, p)).collect()
        };
        (params, parse_type(tt, self.ret))
    }

    /// Full Zell path, e.g. `strings.Split`.
    pub fn path(&self) -> String {
        format!("{}.{}", self.ns, self.name)
    }

    /// The documentation: the row's own, else [`super::stdlib_docs::DOCS`].
    pub fn doc(&self) -> Option<&'static str> {
        if !self.doc.is_empty() {
            return Some(self.doc);
        }
        super::stdlib_docs::doc_of(&self.path()).map(|(_, d)| d)
    }
}

// -- @embed source paths (sema/attrs.rs embed_global) ----------------------------

thread_local! {
    /// Source paths of the files being analysed (by file index), for
    /// `@embed` paths; set by the driver around analysis.
    static SOURCE_PATHS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Line-start offsets of the same files (`t.Must` call locations).
    static SOURCE_LINES: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with the source paths of the files being analysed known to
/// `@embed` (relative embed paths resolve against their directories;
/// without them, against the current directory).
/// `lines` are the line-start offsets of each file (see
/// [`source_location`]).
pub fn with_source_paths<R>(paths: Vec<String>, lines: Vec<Vec<u32>>, f: impl FnOnce() -> R) -> R {
    let saved = SOURCE_PATHS.with(|p| std::mem::replace(&mut *p.borrow_mut(), paths));
    let saved_lines = SOURCE_LINES.with(|p| std::mem::replace(&mut *p.borrow_mut(), lines));
    let r = f();
    SOURCE_PATHS.with(|p| *p.borrow_mut() = saved);
    SOURCE_LINES.with(|p| *p.borrow_mut() = saved_lines);
    r
}

/// `path:line` of byte `offset` in source file `file`, the path relative
/// to the current directory when it is under it (as error traces print
/// it); `?` for an unknown file (analysis without paths, e.g. the LSP).
pub(crate) fn source_location(file: usize, offset: u32) -> String {
    let path = source_path(file);
    if path.is_empty() {
        return "?".into();
    }
    let line = SOURCE_LINES.with(|l| l.borrow().get(file).map_or(0, |starts| starts.partition_point(|&s| s <= offset).max(1)));
    let shown = std::env::current_dir()
        .ok()
        .and_then(|cwd| std::path::Path::new(&path).strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or(path);
    format!("{shown}:{line}")
}

/// The path of source file `file` (empty if unknown).
pub(crate) fn source_path(file: usize) -> String {
    SOURCE_PATHS.with(|p| p.borrow().get(file).cloned().unwrap_or_default())
}

/// The directory of source file `file` (for `@embed`; empty if unknown).
pub(crate) fn embed_source_dir(file: usize) -> std::path::PathBuf {
    SOURCE_PATHS.with(|p| {
        p.borrow()
            .get(file)
            .and_then(|f| std::path::Path::new(f).parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_types_parse_and_symbols_follow_convention() {
        let mut tt = TypeTable::new();
        for f in FUNCS {
            f.signature(&mut tt);
            let ns = f.ns.replace('.', "_");
            assert!(f.symbol.starts_with(&format!("zell_{ns}_")), "{}", f.symbol);
            assert!(NAMESPACES.contains(&f.ns), "{} not in NAMESPACES", f.ns);
        }
    }

    #[test]
    fn every_stdlib_namespace_has_a_reserved_root() {
        let srcs = super::super::stdsrc::MODULES.iter().map(|m| m.0).chain(super::super::reflect::MODULES.iter().map(|m| m.0));
        let all: Vec<&str> = NAMESPACES.iter().copied().chain(srcs).chain(["collections"]).collect();
        for ns in &all {
            assert!(is_std_namespace(ns), "{ns}: root not in STD_ROOTS");
        }
        for r in STD_ROOTS {
            assert!(all.iter().any(|ns| ns.split('.').next() == Some(r)), "STD_ROOTS: {r} has no namespace");
        }
        assert!(is_std_namespace("collections.List<app.X>") && !is_std_namespace("app.io") && !is_std_namespace("iox"));
        assert!(removed_std_root("std.io").unwrap().contains("use `io`") && removed_std_root("stdx").is_none());
    }

    #[test]
    fn no_duplicate_names_or_symbols() {
        let mut seen = std::collections::HashSet::new();
        let mut syms = std::collections::HashSet::new();
        for f in FUNCS {
            assert!(seen.insert(f.path()), "duplicate {}", f.path());
            assert!(syms.insert(f.symbol), "duplicate symbol {}", f.symbol);
        }
    }
}

// -- per-module system libraries (build.rs link) --------------------------------

/// System libraries some stdlib modules need at link time, by the prefix of
/// the runtime symbols that use them: a program is linked with `-l<lib>`
/// only if its module declares (calls) a runtime function whose symbol
/// starts with the prefix, so programs that do not use the module do not
/// depend on the library. `database.sqlite` binds the system SQLite.
pub const LINK_LIBS: &[(&str, &str)] = &[("zell_database_sqlite_", "sqlite3")];

/// The [`LINK_LIBS`] libraries needed by a module declaring `symbols`, in
/// table order, without duplicates.
pub fn link_libs<'a>(symbols: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let syms: Vec<&str> = symbols.into_iter().collect();
    let mut libs: Vec<&'static str> = Vec::new();
    for (prefix, lib) in LINK_LIBS {
        if !libs.contains(lib) && syms.iter().any(|s| s.starts_with(prefix)) {
            libs.push(lib);
        }
    }
    libs
}

#[cfg(test)]
mod link_lib_tests {
    use super::link_libs;

    #[test]
    fn libraries_only_for_used_modules() {
        assert!(link_libs(["zell_print_int", "zell_database_sql_err_no_rows"]).is_empty());
        assert_eq!(link_libs(["zell_database_sqlite_rt_open", "zell_database_sqlite_rt_step"]), vec!["sqlite3"]);
    }
}
