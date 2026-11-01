//! Declarations of the C runtime (`runtime/include/zell_rt.h`) and of the
//! stdlib externs named by `sema::stdlib`, plus call helpers that apply the
//! C ABI attributes (`zeroext` on `bool`, `noreturn` on panics).
//!
//! Signatures are written in a tiny notation: `ret:params`, one letter per
//! type: `v` void, `p` ptr, `l` i64, `i` i32, `b` bool (`zeroext i1`),
//! `d` double, `f` float, `c` i8.

use super::CodeGen;
use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::module::Linkage;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue};

/// Runtime functions: (symbol, signature, noreturn).
const RUNTIME: &[(&str, &str, bool)] = &[
    ("zell_alloc", "p:l", false),
    ("zell_free", "v:p", false),
    ("zell_coro_frame_alloc", "p:l", false),
    ("zell_coro_frame_alloc_uninit", "p:l", false),
    ("zell_coro_frame_free", "v:p", false),
    ("zell_coro_frame_free_sized", "v:pl", false),
    ("zell_object_new", "p:pl", false),
    ("zell_cycles_new_entry", "p:pl", false),
    ("zell_cycles_new_dyn", "p:pl", false),
    ("zell_arena_object", "p:lpl", false),
    ("zell_arena_slice", "v:plllp", false),
    ("zell_task_chan_new", "l:llplpllpl", false),
    ("zell_task_chan_send", "p:lp", false),
    ("zell_task_chan_recv", "p:l", false),
    ("zell_task_chan_next", "p:l", false),
    ("zell_retain", "v:p", false),
    ("zell_release", "v:p", false),
    ("zell_ptr_retain", "v:p", false),
    ("zell_ptr_release", "v:p", false),
    ("zell_ptr_release_noroot", "v:pl", false),
    ("zell_weak_new", "p:p", false),
    ("zell_weak_get", "p:p", false),
    ("zell_leak_check_enable", "v:i", false),
    ("zell_leak_fields", "v:pp", false),
    ("zell_leak_acyclic", "v:pp", false),
    ("zell_string_concat", "p:pp", false),
    ("zell_string_append", "p:pp", false),
    ("zell_string_compare", "i:pp", false),
    ("zell_string_eq", "b:pp", false),
    ("zell_string_len", "l:p", false),
    ("zell_string_slice", "p:pll", false),
    ("zell_string_decode_rune", "i:plp", false),
    ("zell_string_from_int", "p:l", false),
    ("zell_string_from_uint", "p:l", false),
    ("zell_string_from_float", "p:d", false),
    ("zell_string_from_bool", "p:b", false),
    ("zell_string_from_rune", "p:i", false),
    ("zell_slice_new_ops", "v:plllp", false),
    ("zell_slice_append", "v:plpp", false),
    ("zell_slice_append_slice", "v:plpp", false),
    ("zell_slice_sub", "v:pplll", false),
    ("zell_map_new", "p:pl", false),
    ("zell_map_new_str", "p:lp", false),
    ("zell_map_new_int", "p:lp", false),
    ("zell_map_len", "l:p", false),
    ("zell_map_get", "p:pp", false),
    ("zell_map_set", "v:ppp", false),
    ("zell_map_get_or_insert", "p:pp", false),
    ("zell_map_delete", "b:pp", false),
    ("zell_map_get_str", "p:pp", false),
    ("zell_map_set_str", "v:ppp", false),
    ("zell_map_delete_str", "b:pp", false),
    ("zell_map_get_int", "p:pl", false),
    ("zell_map_set_int", "v:plp", false),
    ("zell_map_delete_int", "b:pl", false),
    ("zell_map_iter_init", "v:pp", false),
    ("zell_map_iter_next", "b:ppp", false),
    ("zell_hash_bytes", "l:pl", false),
    ("zell_error_new", "p:p", false),
    ("zell_error_new_typed", "p:ppp", false),
    ("zell_error_wrap", "p:pp", false),
    ("zell_error_message", "p:p", false),
    ("zell_error_as", "p:pp", false),
    ("zell_error_created", "p:pp", false),
    ("zell_error_returned", "p:ppp", false),
    ("zell_error_caught", "p:ppl", false),
    ("zell_error_discarded", "v:p", false),
    ("zell_error_report", "v:p", false),
    ("zell_panic", "v:p", true),
    ("zell_panic_str", "v:p", true),
    ("zell_panic_error", "v:p", true),
    ("zell_panic_nil", "v:", true),
    ("zell_unwind_resume", "v:", true),
    ("zell_panic_bounds", "v:ll", true),
    ("zell_print_str", "v:p", false),
    ("zell_print_int", "v:l", false),
    ("zell_print_uint", "v:l", false),
    ("zell_print_float", "v:d", false),
    ("zell_float32_shortest", "d:f", false),
    ("zell_print_bool", "v:b", false),
    ("zell_print_rune", "v:i", false),
    ("zell_print_space", "v:", false),
    ("zell_print_newline", "v:", false),
    ("zell_coro_resume", "v:p", false),
    ("zell_coro_leave", "v:p", false),
    ("zell_task_get_ctx", "p:", false),
    ("zell_task_lazy", "p:plppl", false),
    ("zell_coro_enter", "p:p", false),
    ("zell_task_completed", "p:lpp", false),
    ("zell_task_complete", "v:pp", false),
    ("zell_task_is_done", "b:p", false),
    ("zell_task_add_continuation", "b:ppp", false),
    ("zell_task_add_continuation_linked", "b:ppp", false),
    ("zell_coro_defer", "v:ppp", false),
    ("zell_coro_register", "v:b", false),
    ("zell_coro_unwound_linked", "v:ppp", false),
    ("zell_coro_link_panic", "v:ppll", false),
    ("zell_task_result", "p:p", false),
    ("zell_task_await", "p:p", false),
    ("zell_task_result_err", "p:pl", false),
    ("zell_task_await_err", "p:pl", false),
    ("zell_task_await_main", "p:p", false),
    ("zell_task_run_main", "v:pp", false),
    ("zell_coro_unwound", "v:pp", false),
    ("zell_task_any", "p:pl", false),
    ("zell_task_all_cancel", "p:plp", false),
    ("zell_task_all_cancel_uniform", "p:plb", false),
    ("zell_runtime_init", "v:ip", false),
    ("zell_runtime_shutdown", "v:", false),
    ("zell_runtime_drain", "v:", false),
    // debug agent hooks (debug_agent.rs, ABI.md §22)
    ("zell_dbg_enter", "v:pppi", false),
    ("zell_dbg_stmt", "v:pi", false),
    ("zell_dbg_leave", "v:p", false),
    ("zell_dbg_leave_linked", "v:p", false),
    ("zell_dbg_suspend", "v:p", false),
    ("zell_dbg_resume", "v:p", false),
    ("zell_dbg_boot", "v:pp", false),
    ("zell_dbg_exit", "v:i", false),
];

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    fn sig_ty(&self, c: char) -> Option<BasicTypeEnum<'ctx>> {
        Some(match c {
            'p' => self.ptr_ty().into(),
            'l' => self.i64_ty().into(),
            'i' => self.ctx.i32_type().into(),
            'b' => self.ctx.bool_type().into(),
            'd' => self.ctx.f64_type().into(),
            'f' => self.ctx.f32_type().into(),
            'c' => self.ctx.i8_type().into(),
            _ => return None,
        })
    }

    /// The declaration of runtime function `name` (declared on first use).
    pub(crate) fn rt(&mut self, name: &str) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let (_, sig, noret) = RUNTIME.iter().find(|r| r.0 == name).unwrap_or_else(|| panic!("codegen: unknown runtime function {name}"));
        let (ret, params) = sig.split_once(':').unwrap();
        let ps: Vec<char> = params.chars().filter(|c| !c.is_whitespace()).collect();
        let pts: Vec<BasicMetadataTypeEnum> = ps.iter().map(|c| self.sig_ty(*c).unwrap().into()).collect();
        let rc = ret.chars().next().unwrap();
        let fty = match self.sig_ty(rc) {
            None => self.ctx.void_type().fn_type(&pts, false),
            Some(t) => t.fn_type(&pts, false),
        };
        let f = self.module.add_function(name, fty, Some(Linkage::External));
        let zext = self.attr("zeroext");
        for (i, c) in ps.iter().enumerate() {
            if *c == 'b' {
                f.add_attribute(AttributeLoc::Param(i as u32), zext);
            }
        }
        if rc == 'b' {
            f.add_attribute(AttributeLoc::Return, zext);
        }
        if *noret {
            f.add_attribute(AttributeLoc::Function, self.attr("noreturn"));
            f.add_attribute(AttributeLoc::Function, self.attr("cold"));
        }
        f
    }

    pub(crate) fn attr(&self, name: &str) -> Attribute {
        self.ctx.create_enum_attribute(Attribute::get_named_enum_kind_id(name), 0)
    }

    /// Calls `f`, adding `zeroext` to every `i1` argument and result (C ABI).
    pub(crate) fn call_c(&mut self, f: FunctionValue<'ctx>, args: &[BasicMetadataValueEnum<'ctx>]) -> Option<BasicValueEnum<'ctx>> {
        let cs = self.call_fn(f, args);
        let zext = self.attr("zeroext");
        for (i, a) in args.iter().enumerate() {
            if let BasicMetadataValueEnum::IntValue(iv) = a {
                if iv.get_type().get_bit_width() == 1 {
                    cs.add_attribute(AttributeLoc::Param(i as u32), zext);
                }
            }
        }
        let r = cs.try_as_basic_value().basic();
        if let Some(BasicValueEnum::IntValue(iv)) = r {
            if iv.get_type().get_bit_width() == 1 {
                cs.add_attribute(AttributeLoc::Return, zext);
            }
        }
        r
    }

    /// Calls runtime function `name`.
    pub(crate) fn call_rt(&mut self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> Option<BasicValueEnum<'ctx>> {
        if let Some(f) = if self.f.as_ref().is_some_and(|f| f.cold > 0) { None } else { self.rc_inline_fn(name) } {
            self.b.build_call(f, args, "").unwrap();
            return None;
        }
        let f = self.rt(name);
        self.call_c(f, args)
    }

    /// Calls a runtime function that returns a value.
    pub(crate) fn call_rt_v(&mut self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> BasicValueEnum<'ctx> {
        self.call_rt(name, args).unwrap_or_else(|| panic!("codegen: {name} returns void"))
    }

    /// Calls a `noreturn` runtime function and continues in a fresh
    /// (unreachable) block.
    pub(crate) fn call_noreturn(&mut self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) {
        self.call_rt(name, args);
        self.b.build_unreachable().unwrap();
        self.dead_block();
    }

    /// Address of an external runtime constant (`zell_type_string`,
    /// `zell_ops_ref`, ...).
    pub(crate) fn rt_global(&mut self, name: &str) -> inkwell::values::PointerValue<'ctx> {
        if let Some(g) = self.module.get_global(name) {
            return g.as_pointer_value();
        }
        let ty = self.ctx.struct_type(&[self.ptr_ty().into(), self.i64_ty().into(), self.ptr_ty().into()], false);
        let g = self.module.add_global(ty, None, name);
        g.set_linkage(Linkage::External);
        g.set_constant(true);
        g.as_pointer_value()
    }
}
