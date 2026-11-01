//! Calls: user functions, methods (with implicit `&` receivers and
//! interface dispatch), closures, enum variant constructors, builtins,
//! conversions and the special stdlib functions.

use super::coerce::Ctx;
use super::resolve::PathRes;
use super::stdlib::Special;
use super::*;
use crate::hir::{Builtin, Callee, ConvKind, ExprKind, PrintItem, PrintOp};

impl<'a> Sema<'a> {
    /// `callee(args)`.
    pub(crate) fn call(&mut self, callee: &ast::Expr, args: &[ast::Expr], span: Span, expected: Option<TypeId>) -> hir::Expr {
        let target = expr::strip_parens(callee);
        if let ast::ExprKind::TypeArgs { base, args: targs } = &target.kind {
            return self.call_with_type_args(base, targs, args, span);
        }
        if let ast::ExprKind::Field { base, name } = &target.kind {
            if let Some(ty) = self.type_args_type(base) {
                return self.type_member_call(ty, name, args, span);
            }
            // `T.New(...)` with `T` a type parameter bound to a concrete
            // struct (instances of stdlib templates, e.g. generated
            // code): a static method of that struct.
            if let ast::ExprKind::Ident(id) = &expr::strip_parens(base).kind {
                if self.lookup_local(&id.name, false).is_none() {
                    if let Some(ty) = self.gen.tenv.iter().rev().find(|(n, _)| *n == id.name).map(|x| x.1) {
                        if !self.has_param(ty) && matches!(self.tt.get(ty), Type::Struct(_)) {
                            return self.type_member_call(ty, name, args, span);
                        }
                    }
                }
            }
        }
        if let Some(segs) = Self::flatten_path(target) {
            let last = *segs.last().unwrap();
            let head_local = self.lookup_local(&segs[0].name, false).is_some();
            if head_local {
                if segs.len() == 1 {
                    let f = self.path_expr(&segs, callee.span, None);
                    return self.call_value(f, args, span);
                }
                let base = self.path_expr(&segs[..segs.len() - 1], callee.span, None);
                return self.method_call(base, last, args, span);
            }
            let Some((res, n)) = self.resolve_segments(&segs, true) else {
                if self.lookup_global(segs[0], false).is_none() && !self.diags.has_errors_at(segs[0].span, self.cur_file) {
                    self.unknown_ident(segs[0]);
                }
                self.check_args_loose(args);
                return self.invalid(span);
            };
            if n == segs.len() {
                return self.call_res(res, last, args, span, expected);
            }
            let sp = segs[0].span.to(segs[n - 1].span);
            let mut base = self.path_value(res, &segs[n - 1].name, sp, None);
            for seg in &segs[n..segs.len() - 1] {
                base = self.field_access(base, seg);
            }
            return self.method_call(base, last, args, span);
        }
        if let ast::ExprKind::Field { base, name } = &target.kind {
            let b = self.value(base, None);
            return self.method_call(b, name, args, span);
        }
        let f = self.value(callee, None);
        self.call_value(f, args, span)
    }

    /// Checks arguments without a signature (after an earlier error).
    pub(crate) fn check_args_loose(&mut self, args: &[ast::Expr]) {
        for a in args {
            // `(u) => ...` cannot be typed without a callee (already reported)
            if matches!(&expr::strip_parens(a).kind, ast::ExprKind::Lambda { params, .. } if params.iter().any(|p| matches!(p.ty.kind, ast::TypeKind::Infer))) {
                continue;
            }
            match &a.kind {
                ast::ExprKind::Spread(x) => self.expr(x, None),
                _ => self.expr(a, None),
            };
        }
    }

    /// Checks call arguments against parameter types.
    pub(crate) fn check_args(&mut self, params: &[TypeId], args: &[ast::Expr], fname: &str, span: Span) -> Vec<hir::Expr> {
        if let Some(a) = args.iter().find(|a| matches!(a.kind, ast::ExprKind::Spread(_))) {
            self.err(format!("cannot use ... in call to non-variadic {fname}"), a.span);
            self.check_args_loose(args);
            return Vec::new();
        }
        if params.len() != args.len() {
            if !args.iter().any(|a| a.is_recovered()) {
                let ps: Vec<String> = params.iter().map(|p| self.tn(*p)).collect();
                let help = format!("{fname} takes ({})", ps.join(", "));
                self.err_help(format!("wrong number of arguments in call to {fname}: expected {}, got {}", params.len(), args.len()), span, help);
            }
            self.check_args_loose(args);
            return Vec::new();
        }
        args.iter().zip(params).enumerate().map(|(i, (a, p))| self.expr_to(a, *p, Ctx::Arg(i, fname.to_string()))).collect()
    }

    /// [`check_args`](Self::check_args) for user function `f`: a variadic
    /// last parameter `...T` (a `[]T`) takes the remaining arguments as a
    /// new slice, or one slice spread with `xs...` (Go rules).
    pub(crate) fn check_fn_args(&mut self, f: FuncId, params: &[TypeId], args: &[ast::Expr], fname: &str, span: Span) -> Vec<hir::Expr> {
        if !self.variadic.contains(&f.0) {
            return self.check_args(params, args, fname, span);
        }
        let fixed = params.len() - 1;
        let sl = params[fixed];
        let Type::Slice(el) = *self.tt.get(sl) else { return self.check_args(params, args, fname, span) };
        let spread_at = args.iter().position(|a| matches!(a.kind, ast::ExprKind::Spread(_)));
        if let Some(i) = spread_at {
            if i + 1 != args.len() || i != fixed {
                let msg = if i + 1 != args.len() {
                    "can only use ... with the final argument".to_string()
                } else {
                    format!("have {} arguments before ..., {fname} takes {fixed} before its variadic parameter", i)
                };
                self.err(msg, args[i].span);
                self.check_args_loose(args);
                return Vec::new();
            }
        } else if args.len() < fixed {
            if !args.iter().any(|a| a.is_recovered()) {
                let ps: Vec<String> = params.iter().enumerate().map(|(i, p)| if i == fixed { format!("...{}", self.tn(el)) } else { self.tn(*p) }).collect();
                let help = format!("{fname} takes ({})", ps.join(", "));
                self.err_help(format!("not enough arguments in call to {fname}: expected at least {fixed}, got {}", args.len()), span, help);
            }
            self.check_args_loose(args);
            return Vec::new();
        }
        let mut out: Vec<hir::Expr> = args[..fixed].iter().zip(params).enumerate().map(|(i, (a, p))| self.expr_to(a, *p, Ctx::Arg(i, fname.to_string()))).collect();
        if let Some(i) = spread_at {
            let ast::ExprKind::Spread(x) = &args[i].kind else { unreachable!() };
            out.push(self.expr_to(x, sl, Ctx::Arg(i, fname.to_string())));
        } else {
            let elems: Vec<hir::Expr> = args[fixed..].iter().enumerate().map(|(i, a)| self.expr_to(a, el, Ctx::Arg(fixed + i, fname.to_string()))).collect();
            out.push(mk(ExprKind::SliceLit(elems), sl, span));
        }
        out
    }

    /// The one slice argument of `task.All` / `task.Any` (`ts` or `ts...`,
    /// a `[]Task<T>`), checked; `Err` for the variadic form, with the
    /// first argument when it was already lowered.
    fn task_slice_arg(&mut self, what: &str, args: &[ast::Expr]) -> Result<hir::Expr, Option<hir::Expr>> {
        let spread = args.iter().position(|a| matches!(a.kind, ast::ExprKind::Spread(_)));
        if args.len() != 1 {
            if let Some(i) = spread {
                self.err(format!("{what} takes tasks or one slice of tasks: ... cannot follow other arguments"), args[i].span);
            }
            return Err(None);
        }
        let a = match &args[0].kind {
            ast::ExprKind::Spread(x) => &**x,
            _ => &args[0],
        };
        // A slice literal of tasks is the slice form too.
        if spread.is_none() && !matches!(expr::strip_parens(a).kind, ast::ExprKind::SliceLit(_)) {
            // Decided by the type: lowered once here, then kept.
            let v = self.value(a, None);
            return match *self.tt.get(v.ty) {
                Type::Slice(t) if matches!(self.tt.get(t), Type::Task(_)) => Ok(v),
                _ => Err(Some(v)),
            };
        }
        let v = self.value(a, None);
        match *self.tt.get(v.ty) {
            Type::Slice(t) if matches!(self.tt.get(t), Type::Task(_)) => Ok(v),
            Type::Invalid => Ok(v),
            _ => {
                self.err(format!("{what} of a slice needs a []Task<T>, found {}", self.tn(v.ty)), v.span);
                Ok(self.invalid(v.span))
            }
        }
    }

    /// `task.All(ts)` over a slice: the `task.allSlice*` helpers of
    /// `stdlib_src/task.zl` (every task awaited, results in input order,
    /// the first error in input order for fallible tasks).
    fn task_all_slice(&mut self, v: hir::Expr, span: Span) -> hir::Expr {
        let Type::Slice(tt) = *self.tt.get(v.ty) else { return self.invalid(span) };
        let Type::Task(r) = *self.tt.get(tt) else { return self.invalid(span) };
        let (name, targs) = match self.tt.fallible_inner(r) {
            Some(u) if matches!(self.tt.get(u), Type::Void) => ("allSliceVoidErr", vec![]),
            Some(u) => ("allSliceErr", vec![u]),
            None if matches!(self.tt.get(r), Type::Void) => ("allSliceVoid", vec![]),
            None => ("allSlice", vec![r]),
        };
        let Some(t) = self.std_template("task", name) else { return self.invalid(span) };
        let Some(fid) = self.instantiate_func(t, targs, span) else { return self.invalid(span) };
        let ty = self.tt.task(self.sigs[fid.0 as usize].ret);
        mk(ExprKind::Call { callee: Callee::Func(fid), args: vec![v] }, ty, span)
    }

    /// The template of stdlib Zell-source function `ns.name` (loading its
    /// module).
    pub(crate) fn std_template(&mut self, ns: &str, name: &str) -> Option<u32> {
        let nsi = *self.ns_index.get(ns)?;
        self.std_lookup_hook(nsi, name);
        match self.nss[nsi].members.get(name)? {
            (Member::Generic(t), _, _) => Some(*t),
            _ => None,
        }
    }

    /// `fmt.Sprintf(format, args...)`: `args` is a `[]any` (Go); the kinds
    /// are taken from the dynamic types at run time (`fmt.sprintfSpread`).
    fn sprintf_spread(&mut self, args: &[ast::Expr], i: usize, span: Span) -> hir::Expr {
        if i != 1 || args.len() != 2 {
            self.err("fmt.Sprintf takes a format and either values or one []any spread with ...", args[i].span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        let fmt = self.expr_to(&args[0], self.tt.string, Ctx::Arg(0, "fmt.Sprintf".into()));
        let ast::ExprKind::Spread(x) = &args[1].kind else { unreachable!() };
        let anys = self.tt.slice(self.tt.any);
        let xs = self.expr_to(x, anys, Ctx::Arg(1, "fmt.Sprintf".into()));
        let Some(t) = self.std_template("fmt", "sprintfSpread") else { return self.invalid(span) };
        let Some(fid) = self.instantiate_func(t, Vec::new(), span) else { return self.invalid(span) };
        mk(ExprKind::Call { callee: Callee::Func(fid), args: vec![fmt, xs] }, self.tt.string, span)
    }

    /// Call of a symbolically resolved callee.
    pub(crate) fn call_res(&mut self, res: PathRes, name: &ast::Ident, args: &[ast::Expr], span: Span, expected: Option<TypeId>) -> hir::Expr {
        match res {
            PathRes::Member(Member::Func(f)) | PathRes::StaticMethod(f) => {
                let sig = self.sigs[f.0 as usize].clone();
                let fname = self.funcs[f.0 as usize].name.clone();
                let args = self.check_fn_args(f, &sig.params, args, &fname, span);
                if self.reflect_error_call(f, &args, span) && args.len() == 1 {
                    // Never runs when reachable (compile error); a terminating statement.
                    return mk(ExprKind::Builtin { op: Builtin::Panic, args }, self.tt.void, span);
                }
                let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
                mk(ExprKind::Call { callee: Callee::Func(f), args }, ty, span)
            }
            // `T.M(x, args)`: method expression called with the receiver first.
            PathRes::MethodExpr(f, sid) => {
                let sig = self.sigs[f.0 as usize].clone();
                let sname = self.structs[sid as usize].name.clone();
                if args.len() == sig.params.len() {
                    self.err_help(
                        format!("{} is not a static method of {sname}", name.name),
                        name.span,
                        format!("call it on a value (`x.{0}(...)`), or pass the receiver first: `{sname}.{0}(&x, ...)`", name.name),
                    );
                    self.check_args_loose(args);
                    return self.invalid(span);
                }
                let st = self.struct_ty(sid);
                let mut params = vec![self.tt.pointer(st)];
                params.extend(sig.params.iter().copied());
                let fname = format!("{sname}.{}", name.name);
                let args = self.check_args(&params, args, &fname, span);
                let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
                mk(ExprKind::Call { callee: Callee::Func(f), args }, ty, span)
            }
            PathRes::Member(Member::StdFunc(i)) => {
                let sf = stdlib::FUNCS[i];
                let (params, ret) = sf.signature(&mut self.tt);
                let args = self.check_args(&params, args, &sf.path(), span);
                let x = self.extern_id(sf.path(), sf.symbol, params, ret);
                mk(ExprKind::Call { callee: Callee::Extern(x), args }, ret, span)
            }
            PathRes::Member(Member::Special(sp)) => self.special(sp, args, span),
            PathRes::Builtin(b) => self.builtin(b, args, span, expected),
            PathRes::BuiltinType(t) => self.conversion(t, args, span),
            // `Celsius(x)` with `type Celsius = float`: the aliased type's conversion.
            PathRes::Member(Member::Alias(a)) => {
                let t = self.alias_type(a);
                self.conversion(t, args, span)
            }
            PathRes::Variant(e, v) => {
                let fields: Vec<TypeId> = self.enums[e as usize].variants[v as usize].fields.iter().map(|f| f.1).collect();
                let vname = format!("{}.{}", self.enums[e as usize].name, name.name);
                if fields.is_empty() {
                    self.err(format!("variant {vname} has no fields; write it without parentheses"), span);
                }
                let args = self.check_args(&fields, args, &vname, span);
                let ty = self.enum_ty(e);
                mk(ExprKind::EnumLit { enum_id: e, variant: v, args }, ty, span)
            }
            PathRes::Member(Member::Global(_)) => {
                let f = self.path_value(res, &name.name, name.span, None);
                self.call_value(f, args, span)
            }
            PathRes::Member(Member::StdConst(_)) => {
                self.err(format!("{} is a constant, not a function", name.name), name.span);
                self.invalid(span)
            }
            PathRes::Member(Member::Struct(_) | Member::Interface(_) | Member::Enum(_)) => {
                self.err(format!("cannot call type {} (use a struct literal {}{{...}} or a static constructor)", name.name, name.name), name.span);
                self.check_args_loose(args);
                self.invalid(span)
            }
            PathRes::Member(Member::Ns(_)) => {
                self.err(format!("{} is a namespace, not a function", name.name), name.span);
                self.invalid(span)
            }
            PathRes::Member(Member::Generic(t)) => {
                // `log.Info("...", args)`: compiled message template (logtmpl.rs).
                if let Some(e) = self.log_func_call(t, args, span) {
                    return e;
                }
                self.gen.ret_hint = expected;
                self.generic_call(t, None, None, args, span)
            }
        }
    }

    /// Call of a closure / function value.
    pub(crate) fn call_value(&mut self, f: hir::Expr, args: &[ast::Expr], span: Span) -> hir::Expr {
        match self.tt.get(f.ty).clone() {
            Type::Func(sig) => {
                let args = self.check_args(&sig.params, args, "function value", span);
                mk(ExprKind::Call { callee: Callee::Value(Box::new(f)), args }, sig.ret, span)
            }
            Type::Invalid => {
                self.check_args_loose(args);
                self.invalid(span)
            }
            _ => {
                self.err(format!("cannot call non-function value of type {}", self.tn(f.ty)), f.span);
                self.check_args_loose(args);
                self.invalid(span)
            }
        }
    }

    /// `base.name(args)`: struct methods (implicit `&`, promotion through
    /// embedded fields), func-typed fields, interface methods.
    pub(crate) fn method_call(&mut self, base: hir::Expr, name: &ast::Ident, args: &[ast::Expr], span: Span) -> hir::Expr {
        self.method_call_targs(base, name, None, args, span)
    }

    /// [`method_call`](Self::method_call) with optional explicit type
    /// arguments (generic methods).
    pub(crate) fn method_call_targs(&mut self, base: hir::Expr, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> hir::Expr {
        if self.tt.is_invalid(base.ty) {
            self.check_args_loose(args);
            return self.invalid(span);
        }
        self.ide_member_recv(name.span, base.ty);
        let base = match self.reflect_method_call(base, name, targs, args, span) {
            Ok(e) => return e,
            Err(b) => b,
        };
        // log level methods: compiled message templates (logtmpl.rs).
        let base = match self.log_method_call(base, name, targs, args, span) {
            Ok(e) => return e,
            Err(b) => b,
        };
        // `t.Must(x)` / `t.MustFail(x)` on a `testing.T` (testing.rs).
        let base = match self.testing_method_call(base, name, targs, args, span) {
            Ok(e) => return e,
            Err(b) => b,
        };
        // `t.Cancel()` on a `Task<T>`: cancels that task and its descendants
        // (DESIGN.md «Отмена»); idempotent, a no-op on a completed task.
        if matches!(self.tt.get(base.ty), Type::Task(_)) && name.name == "Cancel" {
            self.ide_ref(name.span, index::SymKey::Std("task.Task.Cancel".into()));
            if !self.arity("Task.Cancel", args, 0, span) {
                return self.invalid(span);
            }
            let tv = self.tt.task(self.tt.void);
            let x = self.extern_id("task.rtCancel".into(), "zell_task_cancel", vec![tv], self.tt.void);
            return mk(ExprKind::Call { callee: Callee::Extern(x), args: vec![base] }, self.tt.void, span);
        }
        if let Some(sid) = self.struct_of(base.ty) {
            if let Some(&t) = self.gen.methods.get(&(sid, name.name.clone())) {
                let recv = if matches!(self.tt.get(base.ty), Type::Pointer(_)) { base } else { self.addr_of(base) };
                return self.generic_call(t, targs, Some(recv), args, span);
            }
            // A generic method promoted through embedded fields (`ctx.Raw<T>()`
            // with `data.Context` embedded).
            if self.find_method(sid, &name.name).is_none() {
                if let Some((t, path)) = self.promoted_generic_method(sid, &name.name, 0) {
                    let mut b = base;
                    for idx in path {
                        let d = self.auto_deref(b);
                        let Type::Struct(sd) = *self.tt.get(d.ty) else { unreachable!() };
                        let fty = self.structs[sd.0 as usize].fields[idx as usize].ty;
                        let sp = d.span;
                        b = mk(ExprKind::Field { base: Box::new(d), index: idx }, fty, sp);
                    }
                    let recv = if matches!(self.tt.get(b.ty), Type::Pointer(_)) { b } else { self.addr_of(b) };
                    return self.generic_call(t, targs, Some(recv), args, span);
                }
            }
            if targs.is_some() {
                let sname = self.structs[sid as usize].name.clone();
                self.err(format!("method {sname}.{} is not generic (it takes no type arguments)", name.name), name.span);
                self.check_args_loose(args);
                return self.invalid(span);
            }
            self.check_std_method(sid, &name.name, name.span);
            if let Some((fid, path)) = self.find_method(sid, &name.name) {
                self.ide_method_ref(name.span, fid);
                let owner = self.funcs[fid.0 as usize].namespace;
                self.check_member_visibility(owner, &name.name, name.span, true);
                let is_ptr = matches!(self.tt.get(base.ty), Type::Pointer(_));
                let recv = if path.is_empty() && is_ptr {
                    base
                } else {
                    let mut b = base;
                    for idx in path {
                        let d = self.auto_deref(b);
                        let Type::Struct(sd) = *self.tt.get(d.ty) else { unreachable!() };
                        let fty = self.structs[sd.0 as usize].fields[idx as usize].ty;
                        let sp = d.span;
                        b = mk(ExprKind::Field { base: Box::new(d), index: idx }, fty, sp);
                    }
                    if matches!(self.tt.get(b.ty), Type::Pointer(_)) {
                        b
                    } else {
                        self.addr_of(b)
                    }
                };
                let sig = self.sigs[fid.0 as usize].clone();
                let fname = format!("{}.{}", self.structs[sid as usize].name, name.name);
                let mut hargs = vec![recv];
                hargs.extend(self.check_fn_args(fid, &sig.params, args, &fname, span));
                let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
                return mk(ExprKind::Call { callee: Callee::Func(fid), args: hargs }, ty, span);
            }
            if self.find_field(sid, &name.name).is_some() {
                let f = self.field_access(base, name);
                return self.call_value(f, args, span);
            }
            if let Some(iface) = self.default_method_iface(base.ty, name) {
                return self.default_method_call(base, iface, name, args, span);
            }
            let sname = self.structs[sid as usize].name.clone();
            let members = self.struct_member_names(sid);
            let help = self.did_you_mean(&name.name, &members);
            self.err_maybe_help(format!("struct {sname} has no method {}", name.name), name.span, help);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        if let Type::Enum(d) = *self.tt.get(base.ty) {
            if let Some(&fid) = self.enum_methods.get(&(d.0, name.name.clone())) {
                self.ide_ref(name.span, index::SymKey::Func(fid.0));
                let sig = self.sigs[fid.0 as usize].clone();
                let fname = format!("{}.{}", self.enums[d.0 as usize].name, name.name);
                let mut hargs = vec![base];
                hargs.extend(self.check_fn_args(fid, &sig.params, args, &fname, span));
                let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
                return mk(ExprKind::Call { callee: Callee::Func(fid), args: hargs }, ty, span);
            }
            if let Some(iface) = self.default_method_iface(base.ty, name) {
                return self.default_method_call(base, iface, name, args, span);
            }
        }
        if let Some(methods) = self.param_methods(base.ty) {
            let Some(m) = methods.iter().find(|m| m.name == name.name).cloned() else {
                self.err(format!("type parameter {} has no method {} (its constraint does not declare it)", self.tn(base.ty), name.name), name.span);
                self.check_args_loose(args);
                return self.invalid(span);
            };
            let idx = methods.iter().position(|x| x.name == m.name).unwrap() as u32;
            let fname = format!("{}.{}", self.tn(base.ty), m.name);
            let args = self.check_args(&m.params, args, &fname, span);
            return mk(ExprKind::Call { callee: Callee::Interface { recv: Box::new(base), method: idx }, args }, m.ret, span);
        }
        if self.tt.is_interface(base.ty) {
            let methods = self.iface_methods(base.ty);
            let Some(idx) = methods.iter().position(|m| m.name == name.name) else {
                let names: Vec<String> = methods.iter().map(|m| m.name.clone()).collect();
                let help = self.did_you_mean(&name.name, &names);
                self.err_maybe_help(format!("interface {} has no method {}", self.tn(base.ty), name.name), name.span, help);
                self.check_args_loose(args);
                return self.invalid(span);
            };
            if let Type::Interface(d) = *self.tt.get(base.ty) {
                self.ide_ref(name.span, index::SymKey::IfaceMethod(d.0, idx as u32));
            }
            let m = methods[idx].clone();
            let fname = format!("{}.{}", self.tn(base.ty), m.name);
            let args = self.check_args(&m.params, args, &fname, span);
            return mk(ExprKind::Call { callee: Callee::Interface { recv: Box::new(base), method: idx as u32 }, args }, m.ret, span);
        }
        let help = match name.name.as_str() {
            "Len" | "Length" => "use the builtin `len(x)`".to_string(),
            "String" => "convert with `fmt.Format(\"{0}\", x)` or `strconv.Itoa(x)`".to_string(),
            _ => "methods exist only on structs, enums and interfaces".to_string(),
        };
        self.err_help(format!("cannot call method {} on value of type {}", name.name, self.tn(base.ty)), name.span, help);
        self.check_args_loose(args);
        self.invalid(span)
    }

    /// The template of generic method `name` reachable through embedded
    /// fields of struct `sid`, with the field path (shortest first).
    fn promoted_generic_method(&self, sid: u32, name: &str, depth: usize) -> Option<(u32, Vec<u32>)> {
        if depth > 8 {
            return None;
        }
        let mut best: Option<(u32, Vec<u32>)> = None;
        for (i, f) in self.structs[sid as usize].fields.iter().enumerate().filter(|(_, f)| f.embedded) {
            let Type::Struct(d) = *self.tt.get(f.ty) else { continue };
            let found = match self.gen.methods.get(&(d.0, name.to_string())) {
                Some(&t) => Some((t, vec![i as u32])),
                None => self.promoted_generic_method(d.0, name, depth + 1).map(|(t, mut p)| {
                    p.insert(0, i as u32);
                    (t, p)
                }),
            };
            if let Some(x) = found {
                if best.as_ref().is_none_or(|b| x.1.len() < b.1.len()) {
                    best = Some(x);
                }
            }
        }
        best
    }

    /// Calls default method `name` of interface `iface` on `base` (a struct
    /// or enum value implementing it) converted to the interface.
    fn default_method_call(&mut self, base: hir::Expr, iface: TypeId, name: &ast::Ident, args: &[ast::Expr], span: Span) -> hir::Expr {
        let methods = self.iface_methods(iface);
        let idx = methods.iter().position(|m| m.name == name.name).unwrap();
        let m = methods[idx].clone();
        let fname = format!("{}.{}", self.tn(iface), m.name);
        // A struct variable is converted by address (its methods take
        // `this` by pointer), so the default method acts on it, not a copy.
        let base = if matches!(self.tt.get(base.ty), Type::Struct(_)) && self.is_place(&base, false) { self.addr_of(base) } else { base };
        let recv = self.coerce(base, iface, Ctx::Other);
        let args = self.check_args(&m.params, args, &fname, span);
        mk(ExprKind::Call { callee: Callee::Interface { recv: Box::new(recv), method: idx as u32 }, args }, m.ret, span)
    }

    /// The interface whose default method `name` a value of type `ty`
    /// (`S`, `*S` or an enum) gets: an interface with that default which
    /// the type implements (also how a generic body reaches the defaults
    /// of its constraint). Several interfaces with different defaults of
    /// that name are reported as ambiguous.
    fn default_method_iface(&mut self, ty: TypeId, name: &ast::Ident) -> Option<TypeId> {
        let mut found: Vec<(FuncId, TypeId)> = Vec::new();
        for iid in 0..self.interfaces.len() {
            // Unforced stdlib interfaces are consulted by their default table.
            if self.std_iface_lazy.contains_key(&(iid as u32)) {
                if !self.iface_defaults.contains_key(&(iid as u32, name.name.clone())) {
                    continue;
                }
                self.force_iface(iid as u32);
            }
            let Some(def) = self.interfaces[iid].methods.iter().find(|m| m.name == name.name).and_then(|m| m.default) else { continue };
            if found.iter().any(|(f, _)| *f == def) {
                continue;
            }
            let it = self.tt.intern(Type::Interface(crate::types::DeclId(iid as u32)));
            if self.missing_methods(ty, it).is_empty() {
                found.push((def, it));
            }
        }
        if found.len() > 1 {
            let names: Vec<String> = found.iter().map(|(_, t)| self.tn(*t)).collect();
            self.err(format!("ambiguous default method {}: provided by interfaces {}; convert the value to one of them", name.name, names.join(" and ")), name.span);
        }
        found.first().map(|(_, t)| *t)
    }

    pub(crate) fn arity(&mut self, name: &str, args: &[ast::Expr], n: usize, span: Span) -> bool {
        if args.len() != n {
            if !args.iter().any(|a| a.is_recovered()) {
                self.err(format!("wrong number of arguments in call to {name}: expected {n}, got {}", args.len()), span);
            }
            self.check_args_loose(args);
            return false;
        }
        true
    }

    fn builtin(&mut self, b: BuiltinFn, args: &[ast::Expr], span: Span, expected: Option<TypeId>) -> hir::Expr {
        let bi = |op, args, ty| mk(ExprKind::Builtin { op, args }, ty, span);
        match b {
            BuiltinFn::Error => {
                if !self.arity("error", args, 1, span) {
                    return self.invalid(span);
                }
                let m = self.expr_to(&args[0], self.tt.string, Ctx::Arg(0, "error".into()));
                bi(Builtin::ErrorNew, vec![m], self.tt.error)
            }
            BuiltinFn::Len => {
                if !self.arity("len", args, 1, span) {
                    return self.invalid(span);
                }
                if let Some(c) = self.ct_desc_len(&args[0]) {
                    return c;
                }
                let v = self.value(&args[0], None);
                let op = match self.tt.get(v.ty) {
                    Type::String => Builtin::LenString,
                    Type::Slice(_) => Builtin::LenSlice,
                    Type::Array(..) => Builtin::LenArray,
                    Type::Map(..) => Builtin::LenMap,
                    Type::Invalid => return self.invalid(span),
                    _ => {
                        self.err(format!("invalid argument for len: {} (want string, slice, array or map)", self.tn(v.ty)), v.span);
                        return self.invalid(span);
                    }
                };
                bi(op, vec![v], self.tt.int)
            }
            BuiltinFn::Append => {
                if args.len() < 2 {
                    self.err("append needs a slice and at least one value", span);
                    self.check_args_loose(args);
                    return self.invalid(span);
                }
                let s = self.value(&args[0], expected);
                let Type::Slice(el) = *self.tt.get(s.ty) else {
                    if !self.tt.is_invalid(s.ty) {
                        self.err(format!("first argument of append must be a slice, found {}", self.tn(s.ty)), s.span);
                    }
                    self.check_args_loose(&args[1..]);
                    return self.invalid(span);
                };
                let ty = s.ty;
                // `append(xs, ys...)`: all of the slice `ys`.
                if let Some(i) = args.iter().position(|a| matches!(a.kind, ast::ExprKind::Spread(_))) {
                    if i != 1 || args.len() != 2 {
                        self.err("append takes a slice and either values or one slice spread with ...", args[i].span);
                        self.check_args_loose(&args[1..]);
                        return self.invalid(span);
                    }
                    let ast::ExprKind::Spread(x) = &args[1].kind else { unreachable!() };
                    let o = self.expr_to(x, ty, Ctx::Arg(1, "append".into()));
                    return bi(Builtin::AppendSlice, vec![s, o], ty);
                }
                let mut hargs = vec![s];
                for (i, a) in args[1..].iter().enumerate() {
                    hargs.push(self.expr_to(a, el, Ctx::Arg(i + 1, "append".into())));
                }
                bi(Builtin::Append, hargs, ty)
            }
            BuiltinFn::Delete => {
                if !self.arity("delete", args, 2, span) {
                    return self.invalid(span);
                }
                let m = self.value(&args[0], None);
                let Type::Map(k, _) = *self.tt.get(m.ty) else {
                    if !self.tt.is_invalid(m.ty) {
                        self.err(format!("first argument of delete must be a map, found {}", self.tn(m.ty)), m.span);
                    }
                    return self.invalid(span);
                };
                let key = self.expr_to(&args[1], k, Ctx::Arg(1, "delete".into()));
                bi(Builtin::Delete, vec![m, key], self.tt.void)
            }
            BuiltinFn::Panic => {
                if !self.arity("panic", args, 1, span) {
                    return self.invalid(span);
                }
                let v = self.value(&args[0], Some(self.tt.string));
                let v = if v.ty == self.tt.string || v.ty == self.tt.error || self.tt.is_invalid(v.ty) {
                    v
                } else if self.implements(v.ty, self.tt.error) {
                    self.coerce(v, self.tt.error, Ctx::Other)
                } else {
                    self.err(format!("panic needs a string or an error, found {}", self.tn(v.ty)), v.span);
                    v
                };
                bi(Builtin::Panic, vec![v], self.tt.void)
            }
            BuiltinFn::Typeof => self.typeof_builtin(args, span),
        }
    }

    /// `T(x)` conversions between numeric types, chars, strings and enums.
    fn conversion(&mut self, t: TypeId, args: &[ast::Expr], span: Span) -> hir::Expr {
        let tname = self.tn(t);
        if !self.arity(&tname, args, 1, span) {
            return self.invalid(span);
        }
        let v = self.value(&args[0], Some(t));
        let v = self.default_type(v);
        if v.ty == t || self.tt.is_invalid(v.ty) {
            return mk(v.kind, t, span);
        }
        let (si, sf) = (self.tt.is_integer(v.ty), self.tt.is_float(v.ty));
        let (ti, tf) = (self.tt.is_integer(t), self.tt.is_float(t));
        let src = self.tt.get(v.ty).clone();
        let dst = self.tt.get(t).clone();
        let kind = if si && ti {
            Some(ConvKind::IntToInt)
        } else if si && tf {
            Some(ConvKind::IntToFloat)
        } else if sf && ti {
            Some(ConvKind::FloatToInt)
        } else if sf && tf {
            Some(ConvKind::FloatToFloat)
        } else {
            match (&src, &dst) {
                (Type::Enum(d), Type::Int(_)) if self.enums[d.0 as usize].is_plain => Some(ConvKind::EnumToInt),
                (Type::Char, Type::String) => Some(ConvKind::CharToString),
                (Type::Int(_), Type::Char) => Some(ConvKind::IntToChar),
                (Type::Char, Type::Int(_)) => Some(ConvKind::CharToInt),
                _ => None,
            }
        };
        match kind {
            Some(kind) => mk(ExprKind::Convert { kind, value: Box::new(v) }, t, span),
            None => {
                self.err(format!("cannot convert {} to {tname}", self.tn(v.ty)), span);
                self.invalid(span)
            }
        }
    }

    /// Whether values of `t` can be printed by `println` / formatted.
    fn printable(&self, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Int(_) | Type::Float32 | Type::Float64 | Type::Bool | Type::Char | Type::String | Type::Error | Type::Any | Type::Invalid => true,
            // An enum with data prints only through its String() method.
            Type::Enum(d) => self.enums[d.0 as usize].is_plain || self.has_stringer(t),
            // Checked again with the concrete type in each instantiation.
            Type::Param(_) => true,
            Type::Slice(e) | Type::Array(e, _) => self.printable(*e),
            Type::Tuple(es) => es.iter().all(|e| self.printable(*e)),
            Type::Struct(_) => self.has_stringer(t),
            Type::Pointer(inner) => matches!(self.tt.get(*inner), Type::Struct(_)) && self.has_stringer(*inner),
            _ => false,
        }
    }

    /// Whether struct or enum type `t` has a method `String() string` (own
    /// or promoted from an embedded field): `println` and the formatters
    /// print its result (Go's `fmt.Stringer`, DESIGN.md «Вывод»).
    pub(crate) fn has_stringer(&self, t: TypeId) -> bool {
        matches!(self.tt.get(t), Type::Struct(_) | Type::Enum(_))
            && self.op_method(t, "String").is_some_and(|m| m.params.is_empty() && m.ret == self.tt.string && !m.is_async)
    }

    /// Checks a variadic "any" argument for printing/formatting.
    fn print_arg(&mut self, a: &ast::Expr) -> hir::Expr {
        let v = self.value(a, None);
        let v = self.default_type(v);
        if !self.printable(v.ty) {
            self.err(format!("value of type {} cannot be printed (print its fields or convert it to a string)", self.tn(v.ty)), v.span);
        }
        v
    }

    fn stringify(&mut self, v: hir::Expr) -> hir::Expr {
        if v.ty == self.tt.string {
            return v;
        }
        let span = v.span;
        mk(ExprKind::ToString(Box::new(v)), self.tt.string, span)
    }

    fn special(&mut self, sp: Special, args: &[ast::Expr], span: Span) -> hir::Expr {
        match sp {
            Special::ArenaNew | Special::ArenaAlloc => {
                self.err("this arena intrinsic needs a type argument (use Arena.New<T>() or Arena.Alloc<T>(n))", span);
                self.check_args_loose(args);
                self.invalid(span)
            }
            Special::JsonMake => {
                self.err("rtMake needs a type argument (rtMake<T>(n))", span);
                self.check_args_loose(args);
                self.invalid(span)
            }
            Special::ChanNew | Special::ChanSend | Special::ChanRecv | Special::ChanNext | Special::ChanRecvAny => {
                self.err("this channel intrinsic needs a type argument (use task.Channel<T>)", span);
                self.check_args_loose(args);
                self.invalid(span)
            }
            Special::TaskAllCancel => {
                if args.len() != 2 {
                    self.err("rtAllCancel takes a slice of tasks and a bool", span);
                    self.check_args_loose(args);
                    return self.invalid(span);
                }
                let ts = self.value(&args[0], None);
                let fl = self.expr_to(&args[1], self.tt.bool_, Ctx::Other);
                let ty = self.tt.task(self.tt.int);
                mk(ExprKind::Builtin { op: Builtin::TaskAllCancel, args: vec![ts, fl] }, ty, span)
            }
            Special::Println | Special::Print => {
                let mut items = Vec::new();
                let mut prev_str: Option<bool> = None;
                for a in args {
                    let v = self.print_arg(a);
                    // println separates every operand; print (Go's fmt.Print)
                    // only operands where neither side is a string.
                    let is_str = v.ty == self.tt.string;
                    if let Some(p) = prev_str {
                        if sp == Special::Println || (!p && !is_str) {
                            items.push(PrintItem { op: PrintOp::Space, arg: None });
                        }
                    }
                    prev_str = Some(is_str);
                    let op = match self.tt.get(v.ty) {
                        Type::Int(k) if k.signed() => PrintOp::Int,
                        Type::Int(_) => PrintOp::Uint,
                        Type::Float32 | Type::Float64 => PrintOp::Float,
                        Type::Bool => PrintOp::Bool,
                        Type::Char => PrintOp::Char,
                        _ => PrintOp::Str,
                    };
                    let v = if op == PrintOp::Str { self.stringify(v) } else { v };
                    items.push(PrintItem { op, arg: Some(v) });
                }
                if sp == Special::Println {
                    items.push(PrintItem { op: PrintOp::Newline, arg: None });
                }
                mk(ExprKind::Print(items), self.tt.void, span)
            }
            Special::Format => {
                if args.is_empty() {
                    self.err("fmt.Format needs a template string", span);
                    return self.invalid(span);
                }
                // Fast path: Format("{0}", x) is just x converted to a string.
                if args.len() == 2 && matches!(&args[0].kind, ast::ExprKind::Str(s) if s == "{0}") {
                    let v = self.print_arg(&args[1]);
                    return self.stringify(v);
                }
                // Literal text on one side of a single "{0}" is one string concatenation
                // (one allocation) instead of a placeholder scan plus an argument slice.
                if args.len() == 2 {
                    if let ast::ExprKind::Str(s) = &args[0].kind {
                        if s.matches("{0}").count() == 1 {
                            let (pre, post) = s.split_once("{0}").unwrap();
                            if !pre.contains('{') && !post.contains('{') && (pre.is_empty() != post.is_empty()) {
                                let v = self.print_arg(&args[1]);
                                let v = self.stringify(v);
                                let string = self.tt.string;
                                let lit = |t: &str| mk(ExprKind::Const(hir::ConstValue::Str(t.to_string())), string, span);
                                let (lhs, rhs) = if post.is_empty() { (lit(pre), v) } else { (v, lit(post)) };
                                return mk(
                                    ExprKind::Binary { op: hir::BinOp::StrConcat, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                                    string,
                                    span,
                                );
                            }
                        }
                    }
                }
                let tmpl = self.expr_to(&args[0], self.tt.string, Ctx::Arg(0, "fmt.Format".into()));
                let mut parts = Vec::new();
                for a in &args[1..] {
                    let v = self.print_arg(a);
                    parts.push(self.stringify(v));
                }
                let sl = self.tt.slice(self.tt.string);
                let list = mk(ExprKind::SliceLit(parts), sl, span);
                let x = self.extern_id("fmt.Format".into(), stdlib::FORMAT_SYMBOL, vec![self.tt.string, sl], self.tt.string);
                mk(ExprKind::Call { callee: Callee::Extern(x), args: vec![tmpl, list] }, self.tt.string, span)
            }
            Special::Sprintf => {
                if args.is_empty() {
                    self.err("fmt.Sprintf needs a format string", span);
                    return self.invalid(span);
                }
                if let Some(i) = args.iter().position(|a| matches!(a.kind, ast::ExprKind::Spread(_))) {
                    return self.sprintf_spread(args, i, span);
                }
                let fmt = self.expr_to(&args[0], self.tt.string, Ctx::Arg(0, "fmt.Sprintf".into()));
                let (int, float, string) = (self.tt.int, self.tt.float, self.tt.string);
                let mut kinds = String::new();
                let (mut ints, mut floats, mut texts) = (Vec::new(), Vec::new(), Vec::new());
                for a in &args[1..] {
                    // Each argument is evaluated once, into the slot of its kind.
                    let v = self.print_arg(a);
                    let sp = v.span;
                    let zi = mk(ExprKind::Const(hir::ConstValue::Int(0)), int, sp);
                    let zf = mk(ExprKind::Const(hir::ConstValue::Float(0.0)), float, sp);
                    let zs = mk(ExprKind::Const(hir::ConstValue::Str(String::new())), string, sp);
                    let conv = |kind, v: hir::Expr, ty| mk(ExprKind::Convert { kind, value: Box::new(v) }, ty, sp);
                    let (k, i, f, t) = match self.tt.get(v.ty).clone() {
                        Type::Int(k) => {
                            let iv = if v.ty == int { v } else { conv(ConvKind::IntToInt, v, int) };
                            (if k.signed() { 'i' } else { 'u' }, iv, zf, zs)
                        }
                        Type::Float64 => ('f', zi, v, zs),
                        Type::Float32 => ('g', zi, conv(ConvKind::FloatToFloat, v, float), zs),
                        Type::Char => ('c', conv(ConvKind::CharToInt, v, int), zf, zs),
                        Type::String => ('s', zi, zf, v),
                        Type::Bool => ('b', zi, zf, self.stringify(v)),
                        _ => ('o', zi, zf, self.stringify(v)),
                    };
                    kinds.push(k);
                    ints.push(i);
                    floats.push(f);
                    texts.push(t);
                }
                let kinds = mk(ExprKind::Const(hir::ConstValue::Str(kinds)), string, span);
                let (si, sf, ss) = (self.tt.slice(int), self.tt.slice(float), self.tt.slice(string));
                let lists = vec![fmt, kinds, mk(ExprKind::SliceLit(ints), si, span), mk(ExprKind::SliceLit(floats), sf, span), mk(ExprKind::SliceLit(texts), ss, span)];
                let x = self.extern_id("fmt.Sprintf".into(), stdlib::SPRINTF_SYMBOL, vec![string, string, si, sf, ss], string);
                mk(ExprKind::Call { callee: Callee::Extern(x), args: lists }, string, span)
            }
            Special::TaskAll => {
                if args.is_empty() {
                    self.err("task.All needs at least one task", span);
                    return self.invalid(span);
                }
                let mut first = match self.task_slice_arg("task.All", args) {
                    Ok(v) if self.tt.is_invalid(v.ty) => return v,
                    Ok(v) => return self.task_all_slice(v, span),
                    Err(f) => f,
                };
                let mut vs = Vec::new();
                let mut inner = Vec::new();
                let mut fallible = false;
                for a in args {
                    let a = match &a.kind {
                        ast::ExprKind::Spread(x) => &**x,
                        _ => a,
                    };
                    let v = match first.take() {
                        Some(v) => v,
                        None => self.value(a, None),
                    };
                    match *self.tt.get(v.ty) {
                        Type::Task(t) => match self.tt.fallible_inner(t) {
                            Some(u) => {
                                fallible = true;
                                inner.push(u);
                            }
                            None => inner.push(t),
                        },
                        Type::Invalid => inner.push(v.ty),
                        _ => {
                            self.err(format!("task.All arguments must be tasks, found {}", self.tn(v.ty)), v.span);
                            inner.push(self.tt.invalid);
                        }
                    }
                    vs.push(v);
                }
                let tup = self.tt.tuple(inner);
                let res = if fallible { self.tt.fallible(tup) } else { tup };
                let ty = self.tt.task(res);
                mk(ExprKind::TaskAll(vs), ty, span)
            }
            Special::TaskAny => {
                if args.is_empty() {
                    self.err("task.Any needs at least one task", span);
                    return self.invalid(span);
                }
                let mut first = match self.task_slice_arg("task.Any", args) {
                    Ok(v) => {
                        let Type::Slice(t) = *self.tt.get(v.ty) else { return v };
                        return mk(ExprKind::Builtin { op: Builtin::TaskAny, args: vec![v] }, t, span);
                    }
                    Err(f) => f,
                };
                let mut vs: Vec<hir::Expr> = Vec::new();
                let mut res: Option<TypeId> = None;
                for a in args {
                    let a = match &a.kind {
                        ast::ExprKind::Spread(x) => &**x,
                        _ => a,
                    };
                    let v = match first.take() {
                        Some(v) => v,
                        None => self.value(a, None),
                    };
                    match *self.tt.get(v.ty) {
                        Type::Task(t) => match res {
                            None => res = Some(t),
                            Some(r) if r != t && !self.tt.is_invalid(r) => {
                                self.err(format!("task.Any arguments must have the same type: Task<{}> and Task<{}>", self.tn(r), self.tn(t)), v.span);
                            }
                            _ => {}
                        },
                        Type::Invalid => {}
                        _ => self.err(format!("task.Any arguments must be tasks, found {}", self.tn(v.ty)), v.span),
                    }
                    vs.push(v);
                }
                let ty = self.tt.task(res.unwrap_or(self.tt.invalid));
                mk(ExprKind::Builtin { op: Builtin::TaskAny, args: vs }, ty, span)
            }
            Special::ErrorsNew => {
                if !self.arity("errors.New", args, 1, span) {
                    return self.invalid(span);
                }
                let m = self.expr_to(&args[0], self.tt.string, Ctx::Arg(0, "errors.New".into()));
                mk(ExprKind::Builtin { op: Builtin::ErrorNew, args: vec![m] }, self.tt.error, span)
            }
            Special::ErrorsFrames => {
                if !self.arity("errors.Frames", args, 1, span) {
                    return self.invalid(span);
                }
                let err = self.expr_to(&args[0], self.tt.error, Ctx::Arg(0, "errors.Frames".into()));
                let Some(sid) = self.std_struct("errors", "Frame") else { return self.invalid(span) };
                let st = self.struct_ty(sid);
                let ret = self.tt.slice(st);
                let x = self.extern_id("errors.Frames".into(), "zell_errors_frames", vec![self.tt.error], ret);
                mk(ExprKind::Call { callee: Callee::Extern(x), args: vec![err] }, ret, span)
            }
            Special::ErrorsAs => {
                if !self.arity("errors.As", args, 2, span) {
                    return self.invalid(span);
                }
                let err = self.expr_to(&args[0], self.tt.error, Ctx::Arg(0, "errors.As".into()));
                let target = self.value(&args[1], None);
                let ok = match *self.tt.get(target.ty) {
                    Type::Pointer(p) => matches!(self.tt.get(p), Type::Struct(_)) && self.implements(p, self.tt.error),
                    Type::Invalid => true,
                    _ => false,
                };
                if !ok {
                    self.err(format!("errors.As target must be a pointer to a struct implementing error, found {}", self.tn(target.ty)), target.span);
                }
                mk(ExprKind::Builtin { op: Builtin::ErrorsAs, args: vec![err, target] }, self.tt.bool_, span)
            }
        }
    }
}
