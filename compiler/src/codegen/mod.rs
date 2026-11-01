//! LLVM code generation: lowers a typed [`hir::Program`] to an LLVM module
//! that links against the C runtime (`runtime/libzell_rt.a`, contract in
//! `runtime/ABI.md`).
//!
//! # Value representation (compiler-side ABI, v0.1)
//!
//! | Zell type | LLVM | Notes |
//! |---|---|---|
//! | `int`, `uint`, `intN`, `uintN`, `byte` | `i64`, `iN` | signedness only in instructions |
//! | `float` / `float32` | `double` / `float` | |
//! | `bool` | `i1` | `zeroext` at every C call boundary; 1 byte in memory |
//! | `char` | `i32` | code point |
//! | `string` | `ptr` | runtime `zell_string*` (`{hdr, len, data ptr}`, ABI.md §4.1: owned or a view), NULL is `""`; literals are immortal constant globals whose `data` points at their own bytes |
//! | `*T` | `ptr` | address of the `T` itself: the **payload** of a heap object (object + 16), owning one reference; see "Pointers" |
//! | `weak *T` | `ptr` | runtime weak box (ABI.md §3.4), NULL = nil |
//! | struct | named LLVM struct by value | fields in HIR order |
//! | `[N]T` | `[N x T]` | by value |
//! | `[]T` | `{ptr data, i64 len, i64 cap, ptr backing}` | ABI.md §5, the slice value owns one reference to `backing` |
//! | `map[K]V` | `ptr` | runtime `zell_map*`; `string`/`int` keys use the fast paths |
//! | `error` | `ptr` | runtime `zell_error*`, NULL = no error |
//! | `T!` | `{T, ptr err}` | `void!` is just the `ptr err` |
//! | tuple | anonymous LLVM struct | |
//! | plain enum | `i32` tag | variant index |
//! | data enum | `{i32 tag, [W x i64] payload}` | payload is the variant's fields as an anonymous struct, 8-aligned |
//! | interface | `{ptr data, ptr vtable}` | vtable `{ptr typeid, i64 boxed, ptr method0, ...}` (methods in interface order) |
//! | `any` | `{ptr data, ptr typeinfo}` | boxed, except bool/integers/floats/char, whose value bits are the data word (ABI.md §3.7) |
//! | `func(..)` | `{ptr fn, ptr env}` | `fn(env, args...)`; `env` is NULL or an env object |
//! | `Task<T>` | `ptr` | runtime `zell_task*` |
//!
//! **Heap objects.** Every heap allocation made by compiled code is an ABI.md
//! §3 object (`zell_object_new`): `new T`, boxes (struct values stored in
//! interfaces / `any` / errors), *cells* and closure environments. One
//! `zell_type_info` per boxed type `T` (`ti.<T>`) has a `drop` that releases
//! the references inside the `T` payload at offset 16. Its address is also
//! the run-time type id used by type switches and `errors.As`; the type id of
//! a stored `*S` is `ti.<*S>`.
//!
//! **Reflection** ([`reflect`], ABI.md §17). When the program uses
//! `reflect` (`Program::reflect`), every type id is instead the payload
//! of an immortal `reflect.TypeInfo` object whose first three words are
//! the `zell_type_info` (`name`, `size`, `drop`), so the same pointer is
//! the box type, `vtable[0]`, the `any` type word and a Zell
//! `*reflect.TypeInfo`. Tables reference fields, methods (packed `invoke`
//! thunks and closure thunks), variants and attribute values (constant
//! structs) as immortal objects through NULL-backed slices; a method lists
//! its parameters (`ParamInfo`, receiver excluded) and a struct/enum/
//! interface table its declaring namespace (`Package`, appended at +232).
//! `reflect.TypesWith<A>()` returns a fresh heap slice of the marked
//! types' tables per call (`Reflect::types_with`). Functions
//! listed in `Reflect::intrinsics` get their bodies from
//! `CodeGen::emit_intrinsic` instead of their HIR.
//!
//! **Interfaces.** Storing a struct value boxes a copy (`boxed = 1`);
//! storing a `*S` stores the pointer (`boxed = 0`). Either way a non-nil
//! interface value owns one reference to the object whose payload `data`
//! is (released with `zell_ptr_release(data)`). Method `m` is `vtable[2 + m](data, args...)`;
//! methods promoted from embedded structs get an adjusting thunk. An
//! interface default method is one function `F(iface this, args...)`; the
//! slot of a type that does not declare the method holds a thunk
//! `(data, args) -> F({data, vtable of the type for the declaring
//! interface}, args)`. Enum values are boxed like struct values; their slots
//! are thunks loading the enum from `data` (enum methods take the value).
//! The slot of an `async` method (for an interface method returning
//! `Task<T>`) is a thunk `(data, args) -> ptr task` calling the ramp with a
//! local `{task, value}` slot and a NULL awaiter: `res.task`, or
//! `zell_task_completed` over `res.value` when the call never suspended.
//! `case I x:` on an `any` looks the vtable up with `iface.conv.N(typeid)`
//! (NULL = no match) and unwraps a stored `*S` when `boxed = 0`; on an
//! `error` the same lookup takes the user type at +32 (every struct boxed
//! into an error is a candidate) and binds `{payload of the box at +40,
//! vtable}`. Storing a
//! struct in an `error` boxes it and calls `zell_error_new_typed` with a
//! `Message()` thunk; when the struct has `Unwrap() error`, its result
//! (+1) is stored into the new error's `inner` (+24). `error` method 0 is
//! `zell_error_message`.
//!
//! **Arenas.** `Builtin::ArenaNew` (`*T`) and `ArenaSlice` (`[]T`) call
//! `zell_arena_object` / `zell_arena_slice` (runtime/ABI.md 3.6): the
//! objects carry refcount `ZELL_RC_ARENA`, so the ordinary retain/release
//! calls on them are no-ops and no special casing is emitted.
//!
//! **Leak check names.** With `--leak-check` type names (mode 2), C `main`
//! calls `leak.fields`, which registers the `*T` field offsets of every
//! struct type info as `"Type.Field"` (`zell_leak_fields`, ABI.md 3.5).
//!
//! **Closures.** A closure is `{fn, env}`. `env` is NULL for closures without
//! captures, else an object `{hdr, i64 n, ptr cell[n]}` whose drop releases
//! the cells. Captured locals (`Local::captured`) live in *cells*: objects
//! `{hdr, T value}`; the function's slot holds the cell pointer, every
//! execution of the `Let` creates a fresh cell, and closures retain the
//! cells, so variables are shared by reference. Named functions used as
//! values get a thunk `fn(env, args) -> f(args)`. Lifted lambdas take the
//! env pointer as an extra first parameter.
//! *Value environments* (expression trees, `ExprKind::EnvClosures`,
//! `Function::value_env`): the env is an ordinary box of a tuple of the
//! captured values (`{hdr, (T0, T1, ...)}`, dropped by the tuple's type
//! info), shared by several closures; upvalue `i` is tuple element `i`
//! and the box stands in for the cell wherever a cell is pinned.
//!
//! **Address-taken locals** (`Local::address_taken`) also live in cells,
//! so `&x` is a whole-object pointer, unless escape analysis
//! ([`crate::hir::ownership::stack_locals`]) proves the address does not
//! escape: then the local (`StackKind::Cell`), or the object of a
//! non-escaping `var p = new T{...}` (`StackKind::New`), is an entry alloca
//! `{i64 -1, ptr null, T}` with an immortal header, and `&x` / `p` point at
//! its payload. Leaving the scope of such a `p` releases the fields.
//!
//! **Constants need no reference counting.** A constant pointer operand
//! (NULL, or the address of an immortal global: a string literal, a
//! reflection table, an attribute value [`ExprKind::AttrConst`]) is never
//! retained or released (`rc_val`), and a local of a pointer type that
//! only ever held such constants (`FnState::const_only`, cleared by the
//! first other store; a store at the loop depth of its `Let` also skips
//! releasing the old constant) is not released at the end of its block or
//! at the function exit, the two points where every store to it has
//! already been emitted. `len(s)` of a string is the inline load of the
//! length word (NULL is 0). The error-trace calls of `?` run only when the
//! error is not NULL (`trace::when_error`).
//!
//! **Reference counting fast paths.** With `Options::inline_rc` (the
//! default; `ZELL_NO_INLINE_RC=1` at build time turns it off)
//! `call_rt` routes `zell_retain`/`zell_release`/`zell_ptr_*` to internal
//! `alwaysinline` helpers (`rc.rs`, `rc_inline_fn`) that update the count
//! inline while the process is single-threaded, or the owner's 16-bit half
//! of a biased count owned by this thread (the runtime's thread-local
//! `zell_rc_owner`, declared `external thread_local`), and call the
//! runtime otherwise (ABI.md 3.1, 3.2).
//!
//! **Fallible results** travel as first-class aggregates between Zell
//! functions. Stdlib externs follow `zell_rt.h`: `[]T` params as
//! `const zell_slice*`, a `[]T` result through a trailing out-pointer,
//! `T!` as `T` plus a trailing `zell_error **err` (`void!`: only the
//! out-error). Errors record a return trace (ABI.md §7.4, [`trace`]):
//! creation sites and every `?` / catching `try` pass a static
//! `zell_error_site` to the runtime. A shared error (a sentinel) keeps its
//! trace in a thread-local pending record while it propagates; the end of
//! the propagation settles it: `try ... catch err` calls `zell_error_caught`
//! with `keep` = "the handler can observe the trace" (any use of `err`
//! other than `Message()` / `errors.Is`), `try ... else` and a catch without
//! a binding call `zell_error_discarded`, and an async result is settled
//! before `zell_task_complete`. A `?` inside a block `try` branches
//! to its `catch` ([`TryTarget`]) instead of returning.
//!
//! **Functions.** Every function has a single exit block: `return` stores
//! the result into a slot and branches there; the exit block runs deferred
//! closures (LIFO, up to 64 per call, kept in an alloca'd array), releases
//! the locals and returns. `async` functions are LLVM switched-resume
//! coroutines following ABI.md §10.3 (see [`async_`]): `void @f(params,
//! ptr %res)` with `%res -> { ptr task; T value }`; the caller stores
//! into `res.task` NULL or, for a direct `await` inside a coroutine, its
//! own frame handle. With NULL the coroutine makes its `Task<T>`
//! (`zell_task_lazy`) only at its first suspension and stores it into
//! `res.task` (the cancel hooks of the waits the body started before, linked
//! on the caller's task after the mark `zell_coro_enter` saved at the
//! ramp's start, move to the new task), while a body that returns without suspending leaves NULL
//! there and moves its result into `res.value` (the synchronous fast
//! path: no task, and no frame when the body has no suspend point,
//! through `llvm.coro.alloc`). With a frame handle `f` never makes a task
//! (frame-linked await, ABI.md 10.3): at its first suspension it keeps
//! the awaiter in its task slot (handle + 1), leaves `LINKED` (1) in
//! `res.task` and defers its registration (`zell_coro_defer`); the frames
//! above it link the same way, and the last frame of the synchronous chain
//! to suspend publishes it (`zell_coro_register`). `f`'s return moves the
//! result into `res.value` and stores the awaiter into the thread-local
//! `zell_coro_next`, which `run_job` resumes; a panic leaves its
//! `task.Panic` error in `res.task` (`zell_coro_unwound_linked`, read back
//! by `zell_coro_link_panic`). Frames come from
//! `zell_coro_frame_alloc_uninit` (not zeroed: the slots the destroy path
//! reads are stored and escaped at entry; [`coro_frame_fixup`] switches a
//! frame whose destroy function loads LLVM's suspend index to the zeroing
//! `zell_coro_frame_alloc` after optimisation) and go back with
//! `zell_coro_frame_free_sized`; at -O1 and above [`inline_frame_cache`]
//! then inlines the per-thread frame cache into those calls (ABI.md 10.3
//! "Frame cache"). The ramp's `zell_coro_enter` / `zell_coro_leave`
//! are inlined on the thread-local `zell_coro_tls` `{ ctx, cur,
//! orphan_seq, orphan_on }` (ABI.md 10.2).
//! An async function that never suspends ([`hir::suspend::plain_async`]:
//! it awaits only direct calls of such functions) is a plain function with
//! the same ABI (ABI.md 10.3 "Never-suspending functions"): no frame, task
//! or coroutine chain node (its landing pads as for any synchronous
//! function, below), the caller's context saved and given back unless the body is
//! context-neutral, NULL into `res.task` and the moved result into
//! `res.value`; a direct `await` of it reads `res.value` only. It carries
//! the string attribute `"zell-plain-async"` instead of
//! `presplitcoroutine`.
//! An `await` of a task tests its state word inline (+24 == 1, acquire)
//! and takes the result at +224 when it did not panic (+200 NULL; ABI.md
//! 10.2 "Task layout").
//! `Mutex.Lock` / `Unlock` are a CAS 0 -> 1 / 1 -> 0 of the state word,
//! with the runtime call only when it fails (ABI.md 10.6: 0 is free with
//! nothing to do, 1 locked with no waiter, woken waiter, starvation or
//! guard bit; barging, spinning and hand-off stay in the runtime).
//! Callers awaiting a direct call outside a coroutine read `res.value` when
//! `res.task` is NULL; callers keeping the task, and thunks of async
//! functions used as values, pass a NULL awaiter and materialise a done
//! task with `zell_task_completed` (so do the vtable slots of async
//! methods). The C `main` of an
//! `async func main` hands the ramp and its `main.res` slot to
//! `zell_task_run_main(ramp, res)` (ABI.md §10.2, §11), which runs the
//! ramp as a job on a worker and returns once it did; a non-NULL
//! `res.task` is then waited for with `zell_task_await_main`. The build pipeline
//! (`crate::build`) runs `default<On>`, which in LLVM 18 includes the
//! coroutine passes (coro-early, coro-split, coro-elide, coro-cleanup; at
//! O0 via the CoroConditionalWrapper), so coroutines are lowered at every
//! optimisation level.
//!
//! # Reference counting
//!
//! Managed: `string`, `[]T`, `map`, `error`, `Task`, `*T`, `weak *T`,
//! closures (their env), interface/`any` values, and aggregates (structs,
//! tuples, arrays, `T!`, data enums) containing those or a struct with a
//! destructor, via generated per-type retain/release helpers (also used as
//! `zell_value_ops` for containers). `*T` uses `zell_ptr_retain/release`
//! (header = payload - 16) and `zell_ops_ptr`.
//!
//! * Expression results are *owned* (+1: calls, constructors, `new`,
//!   concatenation) or *borrowed* (loads from places, literals). Storing a
//!   borrowed value into a location retains it; owned temporaries are
//!   released after their use.
//! * Stores release the previous value (after retaining the new one).
//! * **Parameters** (see `hir::ownership`): *borrowed* by default (no
//!   retain on entry, no release at exit, the slot is marked in
//!   `FnState::borrowed`); *owned* (retained on entry) when the function is
//!   `async` or reassigns the parameter; *moved* (owned, not retained) for
//!   types with a destructor. Callers (`CodeGen::arg` in `expr.rs`) pass owned
//!   temporaries and stable values (non-cell locals, constants) as they
//!   are and retain ("pin") any other borrowed value for the call; unknown
//!   callees (closures, interface methods) are treated as borrowing.
//! * **Locals** are released at function exit (last declared first, after
//!   the deferred calls); `Let` locals of nested blocks are also released
//!   (and zeroed) at the end of their block and by `break`/`continue` out
//!   of it, see [`cleanup`]. Re-executing a `Let` releases the old value.
//!   Stack values with a destructor have a drop flag (`FnState::flags`).
//!   *Borrowed locals* ([`crate::hir::borrow`], DESIGN.md "Память") of
//!   heap-pure functions (`CodeGen::pure_fns`) are stored without a retain
//!   and never released (`FnState::borrow_local`, also marked in
//!   `FnState::borrowed`). *Borrow-returning functions*
//!   (`hir::borrow::borrow_returning`, `CodeGen::borrow_ret`) are emitted
//!   twice: the body under their symbol returns its parameter paths
//!   without a retain (`FnState::borrow_ret`, `borrowed_return`) and is
//!   called directly when the arguments are stable for the statement
//!   (`borrow_call`; the result is then a borrowed `Val`), and a `.rc`
//!   wrapper (`funcs[fid]`, `emit_rc_wrapper`) retains the result for every
//!   other use.
//! * **Temporaries** held while later operands are evaluated live on the
//!   cleanup stack ([`cleanup`]) and are released on early exits (`?`,
//!   `try ... else return/break/continue`, `return`, `break`, `continue`
//!   inside `catch` blocks).
//! * Results are returned +1; runtime borrowed results are retained.
//! * `s += x`, `s = s + x` (`zell_string_append`) and `xs = append(xs,
//!   x)` on a local move the old value into the runtime call, which grows
//!   uniquely referenced strings/backings in place.
//! * **Destructors**: the release helper of a struct with `func Drop()`
//!   calls it first, then releases the fields.
//!
//! # Pointers
//!
//! A stored `*T` always points at a whole object (`new`, a variable's cell,
//! a box). Interior pointers (`&s.f`, `&a[i]`, `&slice[i]`, `&global`) are
//! borrowed-only: sema (`sema::ownership`) only lets them be read through
//! or passed to non-escaping parameters, and `CodeGen::place_ex`
//! pins their owners (the dereferenced pointer, the slice) for the call.
//! An interior pointer stored in a local (`var p = &xs[i]`) is never
//! retained itself: the local's hidden pin slots hold its owners
//! (`interior.rs`). Sub-slices of a local array own the array's cell.
//!
//! Not released (by design): strong reference cycles (use `weak`; detected
//! by `--leak-check`; freed by the cycle collector, below).
//!
//! **Cycle collector** ([`cycles`], ABI.md 3.8). Types on a cycle of the
//! type reference graph ([`crate::sema::cycles`]) are allocated with
//! `zell_cycles_new_entry(entry, size)` (it sets the flag), where `entry` is a
//! per-type registry entry (type info, payload visitor, destructor-only and
//! release-only helpers, the `ti.<*S>` alias) that the runtime registers
//! on first use; closure environments capturing such cells get a
//! per-lambda type info and entry (alias: the lambda's function); every
//! compiled `zell_value_ops` has a third `visit` word (non-NULL for
//! element types holding references to cyclic objects). Pins of a `*T` of
//! a cyclic type (`expr.rs` `arg`/`keep`) read `zell_cycles_epoch` and are
//! released with `zell_ptr_release_noroot` (`FnState::pins`).
//!
//! **Leak check.** With [`Options::leak_check`] the C `main` calls
//! `zell_leak_check_enable` first; after the entry point returns, the
//! globals are released (reverse initialisation order) before
//! `zell_runtime_shutdown`, which reports live objects.
//!
//! **Line table** (ABI.md 8.2). With [`Options::line_tables`] (every
//! DWARF target: ELF, Mach-O, MinGW COFF) the module carries DWARF line
//! tables even without `-g`; every function, helpers and C `main`
//! included, has a subprogram, and calls without a location get line 0.
//! After optimisation `crate::lines::expose` gives internal functions
//! hidden external linkage; before the link `crate::lines::write_object`
//! turns the objects' `.debug_line` into a second object holding the
//! `zell_lines` table (function addresses as 32-bit offsets from the table,
//! resolved by the linker) and a constructor calling
//! `zell_lines_register(table, len)`, which the runtime's backtrace
//! printer reads.
//!
//! **Task panics** (ABI.md 8.1): the `zell_coro_chain` pushes and pops are
//! emitted for every target (the thread-local is native TLS everywhere,
//! MinGW included). Synchronous functions emit the calls that may panic
//! as `invoke`s with cleanup landing pads (the held temporaries, the
//! defers, the locals: [`unwind`]); after optimisation [`unwind_lower`]
//! turns each function that keeps one into a node on that chain (an
//! outlined cleanup on x86-64, a setjmp elsewhere; ABI.md 8.1 item 4a).
//! Coroutines spill held temporaries to frame slots for their destroy
//! path ([`cleanup`]).

mod async_;
mod cleanup;
mod cycles;
mod debug;
mod debug_agent;
mod frame_cache;
pub use debug::dedup_declares;
pub use async_::coro_frame_fixup;
pub use frame_cache::inline_frame_cache;
pub use unwind_lower::{lower_unwind, unwind_parent};
mod expr;
mod funcs;
mod iface;
mod interior;
mod ops;
mod print;
mod rc;
mod reflect;
mod runtime;
mod stmt;
mod trace;
mod types;
mod unwind;
mod unwind_lower;

use crate::hir::{self, FuncId, Program};
use crate::types::TypeId;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetData, TargetMachine};
use inkwell::types::StructType;
use inkwell::values::{BasicValueEnum, FunctionValue, GlobalValue, PointerValue};
use inkwell::OptimizationLevel;
use std::collections::HashMap;

/// Codegen result; the error is a diagnostic message.
pub(crate) type R<T> = Result<T, String>;

/// "not supported in codegen yet: X".
pub(crate) fn unsupported<T>(what: impl std::fmt::Display) -> R<T> {
    Err(format!("not supported in codegen yet: {what}"))
}

/// A value produced by expression codegen, with its ownership.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Val<'ctx> {
    /// `None` for `void` expressions.
    pub v: Option<BasicValueEnum<'ctx>>,
    /// True if the value carries a +1 reference the consumer must
    /// release or move.
    pub owned: bool,
}

impl<'ctx> Val<'ctx> {
    pub fn owned(v: BasicValueEnum<'ctx>) -> Self {
        Val { v: Some(v), owned: true }
    }
    pub fn borrowed(v: BasicValueEnum<'ctx>) -> Self {
        Val { v: Some(v), owned: false }
    }
    pub fn void() -> Self {
        Val { v: None, owned: false }
    }
    pub fn get(&self) -> BasicValueEnum<'ctx> {
        self.v.expect("codegen: void value used")
    }
}

/// Storage of a local variable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalSlot<'ctx> {
    /// The alloca: holds the value, or (for cells) the cell pointer.
    pub ptr: PointerValue<'ctx>,
    pub cell: bool,
    /// `StackKind::New` (escape analysis): the payload of the stack object
    /// that `var p = new T{...}` initialises; the slot holds it or NULL.
    pub stack_obj: Option<PointerValue<'ctx>>,
}

/// A `break`/`continue` target.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoopTarget<'ctx> {
    pub label: hir::Label,
    pub brk: BasicBlock<'ctx>,
    pub cont: Option<BasicBlock<'ctx>>,
    /// Cleanup-stack depth at the loop (or switch) entry.
    pub depth: usize,
}

/// The `catch` of an enclosing block `try` (target of `?` inside it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct TryTarget<'ctx> {
    /// The handler block; it loads the error from `slot`.
    pub catch: BasicBlock<'ctx>,
    /// Holds the (owned) error while jumping to `catch`.
    pub slot: PointerValue<'ctx>,
    /// Cleanup-stack depth at the `try` statement.
    pub depth: usize,
}

/// An entry of the per-function cleanup stack (see [`cleanup`]).
#[derive(Clone, Debug)]
pub(crate) enum Cleanup<'ctx> {
    /// An owned temporary that is still to be consumed; released when an
    /// early exit (`?`, `try ... else return/break/continue`, `return`,
    /// `break`, `continue`) leaves the expression.
    /// With the slot it is spilled to while live, so that a panic
    /// unwinding the frame releases it ([`unwind`]).
    Temp(TypeId, BasicValueEnum<'ctx>, Option<TempSlot<'ctx>>),
    /// A block scope with the locals its `Let`s declared so far; released
    /// at the end of the block and by `break`/`continue` out of it.
    Scope(Vec<hir::LocalId>),
    /// A consumed entry.
    Done,
}

/// The frame slot an owned temporary is spilled to while it is held
/// ([`unwind`]): the value (zeroed once consumed), or for a type with a
/// destructor the value and a live flag.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TempSlot<'ctx> {
    pub ptr: PointerValue<'ctx>,
    pub flag: Option<PointerValue<'ctx>>,
    pub ty: TypeId,
}

/// Code generation options.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// 0: off; 1: report the number of live objects at exit; 2: also their
    /// type names (`zell_leak_check_enable`).
    pub leak_check: i32,
    /// Do not record error traces (`--no-error-trace`): the trace calls are
    /// elided; `errors.Trace` then returns "".
    pub no_error_trace: bool,
    /// Emit DWARF debug information ([`debug`]): compile units, subprograms,
    /// variables, types and line locations. Does not change the generated
    /// code.
    pub debug_info: bool,
    /// Without [`Options::debug_info`]: emit DWARF line tables only
    /// (`LineTablesOnly`: subprograms and statement locations), which the
    /// build turns into the `zell_lines` table of panic backtraces
    /// (runtime/ABI.md 8.2); stripped builds drop the DWARF. Does not
    /// change the code.
    pub line_tables: bool,
    /// The module will be optimised (`DW_AT_..._optimized` in the debug info).
    pub optimized: bool,
    /// Emit the reference-counting fast paths inline (`rc.rs`,
    /// `rc_inline_fn`; runtime/ABI.md 3.2). On at every `-O` level unless
    /// the build runs with `ZELL_NO_INLINE_RC=1`, which emits the plain
    /// runtime calls (the escape hatch; they grow large programs' code by
    /// up to a third and their optimisation time by half, for 5-25% on
    /// refcount-heavy loops; benchmarks/README.md).
    pub inline_rc: bool,
    /// Hooks and tables of the in-process debug agent (`--debug-agent`,
    /// [`debug_agent`], runtime/ABI.md §22).
    pub debug_agent: bool,
}

/// Coroutine state of an `async` function being emitted ([`async_`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Coro<'ctx> {
    pub hdl: PointerValue<'ctx>,
    /// The result type `T` of `Task<T>`, its size and value ops.
    pub ret: TypeId,
    pub ret_size: inkwell::values::IntValue<'ctx>,
    pub ret_ops: PointerValue<'ctx>,
    /// Frame slot holding the coroutine's task (NULL until it suspends),
    /// or the awaiter's handle + 1 once it suspended linked (ABI.md 10.3).
    pub task_slot: PointerValue<'ctx>,
    /// Frame slot holding the caller's context (+1) to restore.
    pub ctx_slot: PointerValue<'ctx>,
    /// Frame slot holding the current task's cancel-hook mark at the
    /// ramp's start (`zell_coro_enter`), passed to `zell_task_lazy`.
    pub mark_slot: PointerValue<'ctx>,
    /// The caller's result slot `{ ptr word; T value }` (last parameter).
    pub res: PointerValue<'ctx>,
    pub cleanup: BasicBlock<'ctx>,
    pub done: BasicBlock<'ctx>,
    /// The destroy edge of every suspend point: a panic unwinding the frame
    /// runs the pending defers and completes the task as panicked (ABI.md 8.1).
    pub unwind: BasicBlock<'ctx>,
    /// This frame's node `{ prev, hdl }` on the thread's `zell_coro_chain`
    /// and the chain itself (None on Windows: no task panics there).
    pub chain: Option<(PointerValue<'ctx>, PointerValue<'ctx>)>,
    /// Frame slot holding the task an `await` waits for while the frame
    /// owns it (NULL otherwise): the unwind path releases it.
    pub awaiting: PointerValue<'ctx>,
}

/// Per-function emission state.
pub(crate) struct FnState<'a, 'ctx> {
    pub func: FunctionValue<'ctx>,
    /// The HIR function, if any (`None` for C `main` and helpers).
    pub hir: Option<&'a hir::Function>,
    pub init: BasicBlock<'ctx>,
    pub alloca_b: Builder<'ctx>,
    pub init_b: Builder<'ctx>,
    pub locals: Vec<Option<LocalSlot<'ctx>>>,
    pub env: Option<PointerValue<'ctx>>,
    /// Result slot and its (representation) type.
    pub ret_slot: Option<(PointerValue<'ctx>, TypeId)>,
    pub exit: Option<BasicBlock<'ctx>>,
    pub loops: Vec<LoopTarget<'ctx>>,
    /// Defer array (`[64 x {ptr, ptr}]`) and count (heap `defer`s).
    pub defers: Option<(PointerValue<'ctx>, PointerValue<'ctx>)>,
    /// Open-coded `defer`s ([`hir::StmtKind::Defer`]) in statement order:
    /// the "armed" flag (an `i1` alloca, zeroed at entry, set by the
    /// statement) and the call the exit block emits under it (a pointer
    /// into the function's HIR, which outlives the codegen: the statement
    /// emitters are not generic over the HIR lifetime).
    pub open_defers: Vec<(PointerValue<'ctx>, *const hir::Expr)>,
    /// Zero-initialised slots released at exit (loop sequences).
    pub scoped: Vec<(PointerValue<'ctx>, TypeId)>,
    pub coro: Option<Coro<'ctx>>,
    /// A never-suspending async function lowered as a plain function
    /// ([`hir::suspend::plain_async`], ABI.md 10.3): the caller's context
    /// (+1, given back at the exit; None when the body is context-neutral)
    /// and the result slot `res`.
    pub plain: Option<(Option<PointerValue<'ctx>>, PointerValue<'ctx>)>,
    /// Owned temporaries and block scopes, innermost last.
    pub cleanups: Vec<Cleanup<'ctx>>,
    /// Cleanup entries that are *pins* of a `*T` of a cyclic type (a
    /// borrowed value retained for a call or a field access): the
    /// collection epoch read when it was taken, so the paired release can
    /// skip recording a possible cycle root (`zell_ptr_release_noroot`).
    pub pins: HashMap<usize, inkwell::values::IntValue<'ctx>>,
    /// Per local: the slot holds a borrowed parameter (never released).
    pub borrowed: Vec<bool>,
    /// Per local: a borrowed local ([`hir::borrow`]): stored without a
    /// retain and never released.
    pub borrow_local: Vec<bool>,
    /// The function returns a value borrowed from its parameters
    /// (`CodeGen::borrow_ret`): `return e` does not retain.
    pub borrow_ret: bool,
    /// Per local (string, `*T`, map, error ... slots that are not cells,
    /// interior or address-taken): every value stored so far was an LLVM
    /// constant pointer (NULL or the address of an immortal global: a
    /// literal, a reflection table, an attribute value), so the slot holds
    /// nothing to release. Cleared by the first other store; checked by
    /// `release_local`.
    pub const_only: Vec<bool>,
    /// Per local: the loop depth (`loops.len()`) at its `Let`, when that
    /// stored a constant. A later store at the same depth (no loop between
    /// them) always finds that constant in the slot, so it need not
    /// release the old value; a store inside a deeper loop may find the
    /// value of a previous iteration.
    pub const_depth: Vec<usize>,
    /// Per local: drop flag of a stack value with a destructor (`i1`, set
    /// while the slot holds a live value).
    pub flags: Vec<Option<PointerValue<'ctx>>>,
    /// Enclosing block `try`s, innermost last.
    pub try_targets: Vec<TryTarget<'ctx>>,
    /// Interior pointer variables and their pins (`interior.rs`).
    pub interior: Option<Box<interior::InteriorState<'a, 'ctx>>>,
    /// Interface locals whose dynamic type is known here (inside the arm
    /// of a type switch on them): calls through them use the constant
    /// vtable of that type, which LLVM turns into direct calls.
    pub known_dyn: Vec<(hir::LocalId, TypeId)>,
    /// Held temporaries are spilled to [`TempSlot`]s (a coroutine: its
    /// destroy path releases them, ABI.md 8.1).
    pub spill_temps: bool,
    /// Every temporary slot, and the one reused per (cleanup index, type).
    pub temp_slots: Vec<TempSlot<'ctx>>,
    pub temp_slot_of: HashMap<(usize, TypeId), usize>,
    /// Calls that may panic are `invoke`s with a cleanup landing pad
    /// ([`unwind`]): a synchronous function with a HIR body.
    pub lpad_mode: bool,
    /// The function has state to clean up on any panic (a `defer`, a local
    /// that may own something, a saved context): every call that may
    /// panic gets a landing pad, not only those under a held temporary.
    pub needs_cleanup: bool,
    /// Nesting of code that emits plain calls only (landing pads, the
    /// unwind block, the release of the locals at the exit).
    pub no_invoke: u32,
    /// The shared tail of the landing pads: defers, locals, `resume`.
    pub unwind_bb: Option<BasicBlock<'ctx>>,
    /// Landing pads per set of live held temporaries (their values).
    pub lpads: HashMap<Vec<usize>, BasicBlock<'ctx>>,
    /// The `landingpad` values reaching the unwind block, and their blocks.
    pub lpad_vals: Vec<(inkwell::values::BasicValueEnum<'ctx>, BasicBlock<'ctx>)>,
    /// Nesting of cold code (landing pads, the unwind block): reference
    /// counts are released by runtime calls, not the inline fast path, so
    /// that the cleanup code stays small for the inliner.
    pub cold: u32,
}

/// The code generator.
pub struct CodeGen<'a, 'ctx> {
    pub(crate) ctx: &'ctx Context,
    pub(crate) module: Module<'ctx>,
    pub(crate) b: Builder<'ctx>,
    pub(crate) prog: &'a Program,
    pub(crate) td: TargetData,
    pub(crate) slice_struct: StructType<'ctx>,
    pub(crate) struct_tys: HashMap<u32, StructType<'ctx>>,
    pub(crate) enum_tys: HashMap<u32, StructType<'ctx>>,
    pub(crate) funcs: Vec<FunctionValue<'ctx>>,
    /// Per function: its result is borrowed from its parameters
    /// (`hir::borrow::borrow_returning`): per parameter, whether the
    /// result reads through it.
    pub(crate) borrow_ret: Vec<Option<Vec<bool>>>,
    /// For a borrow-returning function: the body under the borrowed-result
    /// convention, called directly (`funcs[fid]` is then the `.rc` wrapper
    /// that retains the result, for every indirect use: vtables, function
    /// values, reflection).
    pub(crate) funcs_borrow: HashMap<u32, FunctionValue<'ctx>>,
    pub(crate) globals: Vec<GlobalValue<'ctx>>,
    pub(crate) strings: HashMap<String, PointerValue<'ctx>>,
    pub(crate) cstrings: HashMap<String, PointerValue<'ctx>>,
    /// Error-trace site records by `file\0func\0line` (see [`trace`]).
    pub(crate) error_sites: HashMap<String, PointerValue<'ctx>>,
    /// Interface-to-interface conversion functions by target interface
    /// (bodies emitted last, see `iface::finish_iface_convs`).
    pub(crate) iface_convs: Vec<(TypeId, FunctionValue<'ctx>)>,
    /// `iface.anybox(typeid, ptr) -> box`: boxes the `*S` stored in an
    /// interface for an `any` (body emitted with the conversions).
    pub(crate) iface_anybox: Option<FunctionValue<'ctx>>,
    /// Run-time type ids by type (with reflection: `TypeInfo` payloads).
    pub(crate) type_infos: HashMap<TypeId, PointerValue<'ctx>>,
    /// Reflection tables of builtin types under another spelling (`int64`).
    pub(crate) reflect_aliases: HashMap<(TypeId, String), PointerValue<'ctx>>,
    pub(crate) rc_helpers: HashMap<TypeId, (FunctionValue<'ctx>, FunctionValue<'ctx>)>,
    pub(crate) value_ops: HashMap<TypeId, PointerValue<'ctx>>,
    pub(crate) vtables: HashMap<(TypeId, TypeId), GlobalValue<'ctx>>,
    /// Vtable slots calling an interface default method: (dynamic type, default).
    pub(crate) default_thunks: HashMap<(TypeId, FuncId), FunctionValue<'ctx>>,
    /// Dynamic types stored in an `any` (candidates of `case I x:` on `any`).
    pub(crate) any_dyn_tys: Vec<TypeId>,
    /// Scalar types stored inline in an `any` somewhere in the module
    /// (`iface::make_any`; the body of `any.inline` tests for them).
    pub(crate) any_inline_tys: Vec<TypeId>,
    /// `--leak-check` with type names: registers the `*T` fields of every
    /// struct type info (`zell_leak_fields`); body emitted last.
    pub(crate) leak_fields_fn: Option<FunctionValue<'ctx>>,
    pub(crate) thunks: HashMap<FuncId, FunctionValue<'ctx>>,
    pub(crate) method_thunks: HashMap<(u32, String), FunctionValue<'ctx>>,
    /// Vtable slots of `async` methods (struct or enum id, name, is enum):
    /// `(data, args...) -> Task<R>` over the ramp (`iface::async_entry`).
    pub(crate) async_thunks: HashMap<(u32, String, bool), FunctionValue<'ctx>>,
    /// Vtable slots of enum methods (they take the enum value, not a pointer).
    pub(crate) enum_thunks: HashMap<(u32, String), FunctionValue<'ctx>>,
    pub(crate) msg_thunks: HashMap<u32, FunctionValue<'ctx>>,
    pub(crate) tostr_fns: HashMap<TypeId, FunctionValue<'ctx>>,
    pub(crate) enum_names: HashMap<u32, GlobalValue<'ctx>>,
    /// Map descriptors by `(key type, value type)` (packed).
    pub(crate) map_descs: HashMap<u64, PointerValue<'ctx>>,
    pub(crate) env_ti: Option<GlobalValue<'ctx>>,
    /// Types on reference cycles ([`crate::sema::cycles`]).
    pub(crate) cycles: crate::sema::cycles::Cycles,
    /// Cycle-collector helpers ([`cycles`]).
    pub(crate) cyc: cycles::CycState<'ctx>,
    pub(crate) f: Option<FnState<'a, 'ctx>>,
    pub(crate) counter: u32,
    pub(crate) opts: Options,
    /// Parameter modes (owned / borrowed / moved) and escapes.
    pub(crate) facts: hir::ownership::ParamFacts,
    /// Escape facts that also treat runtime-call arguments as escaping
    /// (for stack allocation, [`hir::ownership::stack_locals`]).
    pub(crate) strict_facts: hir::ownership::ParamFacts,
    /// Heap-pure functions ([`hir::borrow::pure_functions`]).
    pub(crate) pure_fns: Vec<bool>,
    /// Async functions lowered as plain functions
    /// ([`hir::suspend::plain_async`]).
    pub(crate) plain_async: hir::suspend::PlainAsync,
    /// Memoised [`rc::RcKind`] per type.
    pub(crate) rc_cache: std::cell::RefCell<HashMap<TypeId, rc::RcKind>>,
    /// Debug-info state when [`Options::debug_info`] is set.
    pub(crate) dbg: Option<Box<debug::DebugInfo>>,
    /// Debug-agent state when [`Options::debug_agent`] is set.
    pub(crate) dbga: Option<Box<debug_agent::Agent<'ctx>>>,
}

/// Creates a target machine for the current target
/// ([`crate::target::current`]: the host unless `--target` chose another).
/// The host gets its own CPU and features; another target a baseline CPU
/// (`x86-64`, `apple-m1` for macOS arm64, `generic` otherwise). The module's
/// triple and data layout come from this machine, so type sizes, alignment
/// and the C calling convention (SysV, Win64, AAPCS64) are the target's.
/// The ABI boundary only passes scalars and pointers (`zell_slice` and
/// other aggregates go by pointer, ABI.md), so the Win64 rules for structs
/// by value and varargs never come into play.
pub fn host_machine(opt: OptimizationLevel) -> Result<TargetMachine, String> {
    target_machine(&crate::target::current(), opt)
}

/// [`host_machine`] for an explicit target.
pub fn target_machine(t: &crate::target::Target, opt: OptimizationLevel) -> Result<TargetMachine, String> {
    if t.is_host() {
        Target::initialize_native(&InitializationConfig::default())?;
        // The compiler's own triple (`x86_64-pc-windows-gnu`, `aarch64-apple-darwin`),
        // not LLVM's default one: a zell.exe built with the MSVC LLVM defaults to
        // `x86_64-pc-windows-msvc`, whose code (`__chkstk`, `_fltused`, MSVC
        // relocations) does not link with the bundled MinGW runtime.
        let triple = inkwell::targets::TargetTriple::create(&t.triple);
        let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
        // ZELL_TARGET_CPU=x86-64-v3 (say) instead of the host's CPU and
        // features: for tools such as Valgrind that lack AVX-512.
        if let Some(cpu) = std::env::var("ZELL_TARGET_CPU").ok().filter(|c| !c.is_empty()) {
            return target
                .create_target_machine(&triple, &cpu, "", opt, RelocMode::PIC, CodeModel::Default)
                .ok_or_else(|| format!("cannot create a target machine for CPU {cpu}"));
        }
        let cpu = TargetMachine::get_host_cpu_name();
        let features = TargetMachine::get_host_cpu_features();
        return target
            .create_target_machine(&triple, cpu.to_str().unwrap_or("generic"), features.to_str().unwrap_or(""), opt, RelocMode::PIC, CodeModel::Default)
            .ok_or_else(|| "cannot create a target machine for the host".to_string());
    }
    let cfg = InitializationConfig::default();
    Target::initialize_x86(&cfg);
    Target::initialize_aarch64(&cfg);
    Target::initialize_webassembly(&cfg);
    let triple = inkwell::targets::TargetTriple::create(&t.triple);
    let target = Target::from_triple(&triple).map_err(|e| format!("unsupported target {}: {}", t.triple, e.to_string()))?;
    let cpu = match (t.arch.as_str(), t.os.as_str()) {
        ("amd64", _) => "x86-64",
        ("arm64", "macos") => "apple-m1",
        _ => "generic",
    };
    target
        .create_target_machine(&triple, cpu, "", opt, RelocMode::PIC, CodeModel::Default)
        .ok_or_else(|| format!("cannot create a target machine for {}", t.triple))
}

/// Lowers `program` into a new LLVM module named `module_name` for the host
/// target. Returns the diagnostics of unsupported constructs on failure.
pub fn emit<'ctx>(program: &Program, ctx: &'ctx Context, module_name: &str) -> Result<Module<'ctx>, Vec<String>> {
    emit_with(program, ctx, module_name, Options::default())
}

/// [`emit`] with options.
pub fn emit_with<'ctx>(program: &Program, ctx: &'ctx Context, module_name: &str, opts: Options) -> Result<Module<'ctx>, Vec<String>> {
    let machine = host_machine(OptimizationLevel::None).map_err(|e| vec![e])?;
    let module = ctx.create_module(module_name);
    module.set_triple(&machine.get_triple());
    let td = machine.get_target_data();
    module.set_data_layout(&td.get_data_layout());
    let ptr = ctx.ptr_type(inkwell::AddressSpace::default());
    let slice_struct = ctx.opaque_struct_type("zell_slice");
    slice_struct.set_body(&[ptr.into(), ctx.i64_type().into(), ctx.i64_type().into(), ptr.into()], false);
    let mut cg = CodeGen {
        ctx,
        module,
        b: ctx.create_builder(),
        prog: program,
        td,
        slice_struct,
        struct_tys: HashMap::new(),
        enum_tys: HashMap::new(),
        funcs: Vec::new(),
        borrow_ret: Vec::new(),
        funcs_borrow: HashMap::new(),
        globals: Vec::new(),
        strings: HashMap::new(),
        cstrings: HashMap::new(),
        error_sites: HashMap::new(),
        iface_convs: Vec::new(),
        iface_anybox: None,
        type_infos: HashMap::new(),
        reflect_aliases: HashMap::new(),
        rc_helpers: HashMap::new(),
        value_ops: HashMap::new(),
        vtables: HashMap::new(),
        default_thunks: HashMap::new(),
        any_dyn_tys: Vec::new(),
        any_inline_tys: Vec::new(),
        leak_fields_fn: None,
        thunks: HashMap::new(),
        method_thunks: HashMap::new(),
        async_thunks: HashMap::new(),
        enum_thunks: HashMap::new(),
        msg_thunks: HashMap::new(),
        tostr_fns: HashMap::new(),
        enum_names: HashMap::new(),
        map_descs: HashMap::new(),
        env_ti: None,
        cycles: crate::sema::cycles::analyze(program),
        cyc: Default::default(),
        f: None,
        counter: 0,
        opts,
        facts: hir::ownership::param_facts(program),
        strict_facts: hir::ownership::strict_param_facts(program),
        pure_fns: hir::borrow::pure_functions(program),
        plain_async: hir::suspend::plain_async(program),
        rc_cache: Default::default(),
        dbg: None,
        dbga: None,
    };
    cg.init_debug();
    cg.init_agent();
    let mut errors = Vec::new();
    let br = hir::borrow::borrow_returning(program, &|f, k| !cg.facts.owned(f, k) && !cg.facts.moved(f, k), &|t| cg.cycles.holds_cyclic(t));
    cg.borrow_ret = br;
    if let Err(e) = cg.declare_all() {
        return Err(vec![e]);
    }
    let intrinsics = cg.intrinsics();
    for k in intrinsics.keys() {
        if let Some(b) = cg.plain_async.plain.get_mut(k.0 as usize) {
            *b = false;
        }
    }
    for (i, f) in program.funcs.iter().enumerate() {
        let fid = FuncId(i as u32);
        let r = match intrinsics.get(&fid) {
            Some(k) => cg.emit_intrinsic(fid, *k),
            None => cg.emit_function(fid),
        };
        if let Err(e) = r {
            errors.push(format!("{e} (in {})", f.full_name));
            // Leave a valid (trapping) body so the module stays well formed.
            cg.f = None;
            let fv = cg.funcs[i];
            for bb in fv.get_basic_blocks() {
                unsafe {
                    let _ = bb.delete();
                }
            }
        }
    }
    if program.entry.is_some() {
        if let Err(e) = cg.emit_c_main() {
            errors.push(e);
        }
    }
    if let Err(e) = cg.finish_any_tostr() {
        errors.push(e);
    }
    if let Err(e) = cg.finish_iface_convs() {
        errors.push(e);
    }
    if let Err(e) = cg.finish_cycles() {
        errors.push(e);
    }
    if let Err(e) = cg.finish_any_inline() {
        errors.push(e);
    }
    if let Err(e) = cg.finish_leak_fields() {
        errors.push(e);
    }
    cg.finish_agent();
    cg.finish_debug();
    if errors.is_empty() {
        Ok(cg.module)
    } else {
        Err(errors)
    }
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Function `i` is an async function lowered as a plain function.
    pub(crate) fn is_plain_async(&self, i: usize) -> bool {
        self.plain_async.plain.get(i).copied().unwrap_or(false)
    }

    /// Declares every function and global.
    fn declare_all(&mut self) -> R<()> {
        for (i, f) in self.prog.funcs.iter().enumerate() {
            let fty = self.fn_llty(f)?;
            let fv = self.module.add_function(&f.symbol, fty, Some(Linkage::Internal));
            if f.is_async && self.is_plain_async(i) {
                // Never suspends (ABI.md 10.3): a plain function with the
                // coroutine's ABI, marked for `--emit=llvm-ir` readers.
                let a = self.ctx.create_string_attribute("zell-plain-async", "");
                fv.add_attribute(inkwell::attributes::AttributeLoc::Function, a);
            } else if f.is_async {
                fv.add_attribute(inkwell::attributes::AttributeLoc::Function, self.attr("presplitcoroutine"));
            }
            if self.borrow_ret.get(i).is_some_and(|b| b.is_some()) {
                // The body returns borrowed; `.rc` retains for indirect uses.
                self.funcs_borrow.insert(i as u32, fv);
                let w = self.module.add_function(&format!("{}.rc", f.symbol), fty, Some(Linkage::Internal));
                self.funcs.push(w);
            } else {
                self.funcs.push(fv);
            }
        }
        for g in &self.prog.globals {
            let ty = self.llty(g.ty);
            let gv = self.module.add_global(ty, None, &g.symbol);
            gv.set_linkage(Linkage::Internal);
            gv.set_initializer(&ty.const_zero());
            self.globals.push(gv);
        }
        Ok(())
    }

    /// A fresh name suffix.
    pub(crate) fn fresh(&mut self) -> u32 {
        self.counter += 1;
        self.counter
    }

    pub(crate) fn fs(&self) -> &FnState<'a, 'ctx> {
        self.f.as_ref().expect("codegen: no current function")
    }

    pub(crate) fn fs_mut(&mut self) -> &mut FnState<'a, 'ctx> {
        self.f.as_mut().expect("codegen: no current function")
    }

    /// Starts emitting function `func`: creates the entry (allocas), init
    /// (zeroing) and body blocks and positions the builder in the body.
    pub(crate) fn begin_fn(&mut self, func: FunctionValue<'ctx>, hir: Option<&'a hir::Function>) -> Option<FnState<'a, 'ctx>> {
        let entry = self.ctx.append_basic_block(func, "entry");
        let init = self.ctx.append_basic_block(func, "init");
        let body = self.ctx.append_basic_block(func, "body");
        let alloca_b = self.ctx.create_builder();
        alloca_b.position_at_end(entry);
        let init_b = self.ctx.create_builder();
        init_b.position_at_end(init);
        let st = FnState {
            func,
            hir,
            init,
            alloca_b,
            init_b,
            locals: Vec::new(),
            env: None,
            ret_slot: None,
            exit: None,
            loops: Vec::new(),
            defers: None,
            open_defers: Vec::new(),
            scoped: Vec::new(),
            coro: None,
            plain: None,
            cleanups: Vec::new(),
            pins: HashMap::new(),
            borrowed: Vec::new(),
            borrow_local: Vec::new(),
            borrow_ret: false,
            const_only: Vec::new(),
            const_depth: Vec::new(),
            flags: Vec::new(),
            try_targets: Vec::new(),
            interior: None,
            known_dyn: Vec::new(),
            spill_temps: false,
            temp_slots: Vec::new(),
            temp_slot_of: HashMap::new(),
            lpad_mode: false,
            needs_cleanup: false,
            no_invoke: 0,
            unwind_bb: None,
            lpads: HashMap::new(),
            lpad_vals: Vec::new(),
            cold: 0,
        };
        let saved = self.f.replace(st);
        self.b.position_at_end(body);
        self.dbg_begin_fn(func, hir);
        saved
    }

    /// Finishes the current function: links entry -> init -> body and
    /// terminates dangling blocks with `unreachable`; restores `saved`.
    pub(crate) fn end_fn(&mut self, saved: Option<FnState<'a, 'ctx>>, saved_pos: Option<BasicBlock<'ctx>>) {
        let st = self.f.take().expect("codegen: end_fn without function");
        // entry -> (prologue chain) is set up by the caller via `init`.
        let blocks = st.func.get_basic_blocks();
        let body = blocks[2];
        st.alloca_b.build_unconditional_branch(st.init).unwrap();
        // The init chain may have grown blocks (a coroutine prologue):
        // `init_b` sits at the end of its last one.
        let init_tail = st.init_b.get_insert_block().unwrap_or(st.init);
        if init_tail.get_terminator().is_none() {
            st.init_b.build_unconditional_branch(body).unwrap();
        }
        for bb in st.func.get_basic_blocks() {
            if bb.get_terminator().is_none() {
                self.b.position_at_end(bb);
                self.b.build_unreachable().unwrap();
            }
        }
        self.f = saved;
        self.dbg_end_fn();
        if let Some(bb) = saved_pos {
            self.b.position_at_end(bb);
        }
    }

    /// An alloca in the entry block; zero-initialised (in the init block)
    /// when `zero`.
    pub(crate) fn entry_alloca(&mut self, ty: inkwell::types::BasicTypeEnum<'ctx>, name: &str, zero: bool) -> PointerValue<'ctx> {
        let st = self.fs();
        let p = st.alloca_b.build_alloca(ty, name).unwrap();
        if zero {
            st.init_b.build_store(p, ty.const_zero()).unwrap();
        }
        p
    }

    /// Spills `v` into a fresh entry alloca and returns its address.
    pub(crate) fn spill(&mut self, v: BasicValueEnum<'ctx>) -> PointerValue<'ctx> {
        let p = self.entry_alloca(v.get_type(), "spill", false);
        self.b.build_store(p, v).unwrap();
        p
    }

    /// Continues emission in a new block without predecessors (after a
    /// terminator such as `br`, `ret` or `unreachable`).
    pub(crate) fn dead_block(&mut self) {
        let f = self.fs().func;
        let bb = self.ctx.append_basic_block(f, "dead");
        self.b.position_at_end(bb);
    }

    /// Appends a block to the current function.
    pub(crate) fn block(&mut self, name: &str) -> BasicBlock<'ctx> {
        let f = self.fs().func;
        self.ctx.append_basic_block(f, name)
    }

    /// Branches to `bb` unless the current block is already terminated.
    pub(crate) fn br(&mut self, bb: BasicBlock<'ctx>) {
        if self.b.get_insert_block().and_then(|b| b.get_terminator()).is_none() {
            self.b.build_unconditional_branch(bb).unwrap();
        }
    }

    /// Emits the C `main(argc, argv)` (ABI.md §11).
    fn emit_c_main(&mut self) -> R<()> {
        let i32t = self.ctx.i32_type();
        let fty = i32t.fn_type(&[i32t.into(), self.ptr_ty().into()], false);
        let main = self.module.add_function("main", fty, Some(Linkage::External));
        let saved = self.begin_fn(main, None);
        let argc = main.get_nth_param(0).unwrap();
        let argv = main.get_nth_param(1).unwrap();
        if self.opts.leak_check != 0 {
            let m = self.ctx.i32_type().const_int(self.opts.leak_check as u64, false);
            self.call_rt("zell_leak_check_enable", &[m.into()]);
            if self.opts.leak_check >= 2 {
                let f = self.module.add_function("leak.fields", self.ctx.void_type().fn_type(&[], false), Some(Linkage::Internal));
                self.leak_fields_fn = Some(f);
                self.b.build_call(f, &[], "").unwrap();
            }
        }
        self.call_rt("zell_runtime_init", &[argc.into(), argv.into()]);
        self.dbga_boot();
        for gid in &self.prog.global_init_order {
            let g = &self.prog.globals[gid.0 as usize];
            let gv = self.globals[gid.0 as usize].as_pointer_value();
            let val = match (&g.init, &g.value) {
                (Some(init), _) => self.expr_owned(init)?,
                (None, Some(c)) => {
                    let e = hir::Expr::new(hir::ExprKind::Const(c.clone()), g.ty, g.span);
                    self.expr_owned(&e)?
                }
                (None, None) => continue,
            };
            self.b.build_store(gv, val).unwrap();
            self.release_temps_from(0);
        }
        let entry = self.prog.entry.unwrap();
        let ef = self.funcs[entry.0 as usize];
        // `main() void!`: the error (owned) is reported before shutdown.
        let fallible = matches!(self.prog.types.get(self.prog.funcs[entry.0 as usize].ret), crate::types::Type::Fallible(_));
        let err_slot = fallible.then(|| self.entry_alloca(self.ptr_ty().into(), "main.err", true));
        if self.prog.funcs[entry.0 as usize].is_async {
            // `async func main`: the ramp runs as a job on a worker (the
            // main thread is none) and the main thread blocks until it
            // returned. Its result slot then holds either the task to
            // wait for or, when it never suspended, the result itself.
            let ret = self.prog.funcs[entry.0 as usize].ret;
            let rty = self.coro_res_ty(ret);
            let res = self.entry_alloca(rty.into(), "main.res", true);
            self.coro_res_init(ret, res, None);
            let ramp = ef.as_global_value().as_pointer_value();
            self.call_rt("zell_task_run_main", &[ramp.into(), res.into()]);
            let tp = self.b.build_struct_gep(rty, res, 0, "").unwrap();
            let t = self.b.build_load(self.ptr_ty(), tp, "main.task").unwrap().into_pointer_value();
            let sync = self.b.build_is_null(t, "").unwrap();
            let fast = self.block("main.sync");
            let slow = self.block("main.await");
            let join = self.block("main.join");
            self.b.build_conditional_branch(sync, fast, slow).unwrap();
            self.b.position_at_end(fast);
            if let Some(es) = err_slot {
                let vp = self.b.build_struct_gep(rty, res, 1, "").unwrap();
                let e = self.b.build_load(self.ptr_ty(), vp, "").unwrap();
                self.b.build_store(es, e).unwrap();
            }
            self.b.build_unconditional_branch(join).unwrap();
            self.b.position_at_end(slow);
            let slot = self.call_rt_v("zell_task_await_main", &[t.into()]).into_pointer_value();
            if let Some(es) = err_slot {
                let e = self.b.build_load(self.ptr_ty(), slot, "").unwrap();
                self.call_rt("zell_retain", &[e.into()]);
                self.b.build_store(es, e).unwrap();
            }
            self.call_rt("zell_release", &[t.into()]);
            self.b.build_unconditional_branch(join).unwrap();
            self.b.position_at_end(join);
        } else {
            let r = self.b.build_call(ef, &[], "").unwrap().try_as_basic_value().basic();
            if let (Some(es), Some(e)) = (err_slot, r) {
                self.b.build_store(es, e).unwrap();
            }
        }
        // Globals are released in reverse initialisation order once no task
        // runs any more (their destructors run before the final shutdown).
        if self.prog.globals.iter().any(|g| self.needs_rc(g.ty)) {
            self.call_rt("zell_runtime_drain", &[]);
        }
        for gid in self.prog.global_init_order.iter().rev() {
            let g = &self.prog.globals[gid.0 as usize];
            if self.needs_rc(g.ty) {
                let gv = self.globals[gid.0 as usize].as_pointer_value();
                self.rc_slot(g.ty, gv, false);
            }
        }
        // The returned error is reported and released before the shutdown,
        // so that `--leak-check` neither counts it (nor what it holds) as
        // leaked nor exits before the message is printed.
        let mut code = i32t.const_zero();
        if let Some(es) = err_slot {
            let e = self.b.build_load(self.ptr_ty(), es, "").unwrap().into_pointer_value();
            let failed = self.block("main.failed");
            let ok = self.block("main.ok");
            let isnull = self.b.build_is_null(e, "").unwrap();
            self.b.build_conditional_branch(isnull, ok, failed).unwrap();
            self.b.position_at_end(failed);
            self.call_rt("zell_error_report", &[e.into()]);
            self.call_rt("zell_release", &[e.into()]);
            self.b.build_unconditional_branch(ok).unwrap();
            self.b.position_at_end(ok);
            code = self.b.build_select(isnull, i32t.const_zero(), i32t.const_int(1, false), "").unwrap().into_int_value();
        }
        self.dbga_exit(code);
        self.call_rt("zell_runtime_shutdown", &[]);
        self.b.build_return(Some(&code)).unwrap();
        self.end_fn(saved, None);
        Ok(())
    }
}
