//! Typed intrinsics: the `arena` ones, `rtArenaNew<T>(h)` and
//! `rtArenaAlloc<T>(h, n)`, used by `Arena.New<T>` / `Arena.Alloc<T>` in
//! `stdlib_src/arena.zl` (DESIGN.md "Арены", runtime/ABI.md 3.6); the
//! sized-slice intrinsic `encoding.json.rtMake<T>(n)` of the typed JSON
//! decoder (`reflect_src/json.zl`); and the `task` channel ones,
//! `rtChanNew<T>(cap)`, `rtChanSend<T>(h, v)`, `rtChanRecv<T>(h)` and
//! `rtChanNext<T>(h)` and `rtChanRecvAny<T>(hs)`, used by `task.Channel<T>` in `stdlib_src/task.zl`
//! (runtime/ABI.md 10.7). All take exactly one type argument, which the
//! Zell source spells out.

use super::coerce::Ctx;
use super::stdlib::Special;
use super::*;
use crate::hir::{Builtin, ExprKind};

impl<'a> Sema<'a> {
    /// A call of a special function with explicit type arguments, or `None`
    /// if `sp` takes none.
    pub(crate) fn typed_special(&mut self, sp: Special, targs: &[ast::TypeExpr], args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        let want = match sp {
            Special::ArenaNew | Special::JsonMake | Special::ChanNew | Special::ChanRecv | Special::ChanNext | Special::ChanRecvAny => 1,
            Special::ArenaAlloc | Special::ChanSend => 2,
            _ => return None,
        };
        if targs.len() != 1 {
            self.err(format!("wrong number of type arguments: want 1, got {}", targs.len()), span);
            self.check_args_loose(args);
            return Some(self.invalid(span));
        }
        let t = self.resolve_type(&targs[0]);
        if args.len() != want {
            self.err(format!("wrong number of arguments: want {want}, got {}", args.len()), span);
            self.check_args_loose(args);
            return Some(self.invalid(span));
        }
        let int = self.tt.int;
        // The first argument is an `int` handle (`[]int` handles for
        // `rtChanRecvAny`); `rtChanSend`'s second is a `T`.
        let first = if sp == Special::ChanRecvAny { self.tt.slice(int) } else { int };
        let mut hargs: Vec<hir::Expr> = vec![self.expr_to(&args[0], first, Ctx::Other)];
        if args.len() == 2 {
            let second = if sp == Special::ChanSend { t } else { int };
            hargs.push(self.expr_to(&args[1], second, Ctx::Other));
        }
        if self.tt.is_invalid(t) {
            return Some(self.invalid(span));
        }
        let bool_ = self.tt.bool_;
        let void = self.tt.void;
        let (op, ty) = match sp {
            Special::ArenaNew => (Builtin::ArenaNew, self.tt.pointer(t)),
            Special::JsonMake => (Builtin::SliceMake, self.tt.slice(t)),
            Special::ArenaAlloc => (Builtin::ArenaSlice, self.tt.slice(t)),
            Special::ChanNew => {
                let fallible = self.tt.fallible(t);
                let tuple = self.tt.tuple(vec![t, bool_]);
                (Builtin::ChanNew { elem: t, fallible, tuple }, int)
            }
            Special::ChanSend => (Builtin::ChanSend, self.tt.task(void)),
            Special::ChanRecv => {
                let fallible = self.tt.fallible(t);
                (Builtin::ChanRecv, self.tt.task(fallible))
            }
            Special::ChanRecvAny => {
                let tuple = self.tt.tuple(vec![int, t, bool_]);
                (Builtin::ChanRecvAny { tuple }, self.tt.task(tuple))
            }
            _ => {
                let tuple = self.tt.tuple(vec![t, bool_]);
                (Builtin::ChanNext, self.tt.task(tuple))
            }
        };
        Some(mk(ExprKind::Builtin { op, args: hargs }, ty, span))
    }
}
