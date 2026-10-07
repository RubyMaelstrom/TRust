//! TRust: execution of the legacy WebAssembly exception-handling instructions.
//!
//! Exception handling proposal, `document/legacy/exceptions/core/exec.rst` (local snapshot
//! af287a73): a `try` installs a handler for its body; a raised exception unwinds to the
//! innermost handler, whose clauses are tested in order (`exec-throw_ref` step 16), and a
//! catch clause makes the exception available to `rethrow` (`exec-rethrow`).

use super::super::{
    Args,
    dispatch::Done,
    state::{DoneReason, Freg32, Freg64, Inst, Ip, Ireg, Mem0Len, Mem0Ptr, Sp},
};
use crate::{Error, store::PrunedStore};

/// Dispatches the clauses of the handler selected by a raise, or ends the execution with an
/// uncaught exception.
///
/// # Note
///
/// Handlers dispatch with a sibling call, which requires that no address of a handler local
/// escapes: the exception state therefore lives in the store and only scalars cross the
/// out-of-line helpers.
macro_rules! dispatch_raised {
    ($store:expr, $args:expr, $raised:expr $(,)?) => {{
        let target = match $raised {
            true => $store.stack_mut().take_unwind_target(),
            false => None,
        };
        let Some((handler, sp, instance)) = target else {
            done!(
                $store,
                DoneReason::error(Error::new("uncaught WebAssembly exception"))
            )
        };
        $args.set_ip(handler);
        $args.sp = sp;
        if $args.instance != instance {
            $args.instance = instance;
            $args.reload_mem0();
        }
        dispatch!($store, $args)
    }};
}

execution_handler! {
    fn exception_try(
        store: &mut PrunedStore,
        ip: Ip,
        sp: Sp,
        mem0: Mem0Ptr,
        mem0_len: Mem0Len,
        instance: Inst,
        ireg: Ireg,
        freg32: Freg32,
        freg64: Freg64,
    ) -> Done = {
        let mut args = Args::from_parts(ip, sp, mem0, mem0_len, instance, ireg, freg32, freg64);
        let crate::ir::decode::ExceptionTry { handler, try_id } = unsafe { args.decode_op() };
        // SAFETY: the translator resolved `handler` to the clause dispatch of this function.
        let end = unsafe { ip.offset(i32::from(handler) as isize) };
        store.stack_mut().install_exception_handler(try_id, ip, end);
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_catch(
        store: &mut PrunedStore,
        ip: Ip,
        sp: Sp,
        mem0: Mem0Ptr,
        mem0_len: Mem0Len,
        instance: Inst,
        ireg: Ireg,
        freg32: Freg32,
        freg64: Freg64,
    ) -> Done = {
        let mut args = Args::from_parts(ip, sp, mem0, mem0_len, instance, ireg, freg32, freg64);
        let crate::ir::decode::ExceptionCatch { results, tag, try_id, next } =
            unsafe { args.decode_op() };
        let caught = store
            .stack_mut()
            .catch_pending(tag, args.instance, try_id, args.sp, results);
        if !caught {
            args.set_ip(ip);
            args.offset_ip(next);
        }
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_catch_all(
        store: &mut PrunedStore,
        ip: Ip,
        sp: Sp,
        mem0: Mem0Ptr,
        mem0_len: Mem0Len,
        instance: Inst,
        ireg: Ireg,
        freg32: Freg32,
        freg64: Freg64,
    ) -> Done = {
        let mut args = Args::from_parts(ip, sp, mem0, mem0_len, instance, ireg, freg32, freg64);
        let crate::ir::decode::ExceptionCatchAll { try_id } = unsafe { args.decode_op() };
        store.stack_mut().catch_pending_all(try_id, args.instance);
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_throw(
        store: &mut PrunedStore,
        ip: Ip,
        sp: Sp,
        mem0: Mem0Ptr,
        mem0_len: Mem0Len,
        instance: Inst,
        ireg: Ireg,
        freg32: Freg32,
        freg64: Freg64,
    ) -> Done = {
        let mut args = Args::from_parts(ip, sp, mem0, mem0_len, instance, ireg, freg32, freg64);
        let crate::ir::decode::ExceptionThrow { values, tag } = unsafe { args.decode_op() };
        let raised = store
            .stack_mut()
            .throw_exception(ip, args.sp, args.instance, values, tag);
        dispatch_raised!(store, args, raised)
    }
}

execution_handler! {
    fn exception_rethrow(
        store: &mut PrunedStore,
        ip: Ip,
        sp: Sp,
        mem0: Mem0Ptr,
        mem0_len: Mem0Len,
        instance: Inst,
        ireg: Ireg,
        freg32: Freg32,
        freg64: Freg64,
    ) -> Done = {
        let mut args = Args::from_parts(ip, sp, mem0, mem0_len, instance, ireg, freg32, freg64);
        let crate::ir::decode::ExceptionRethrow { try_id } = unsafe { args.decode_op() };
        let Some(raised) = store.stack_mut().rethrow(ip, try_id) else {
            done!(
                store,
                DoneReason::error(Error::new("legacy rethrow without a caught exception"))
            )
        };
        dispatch_raised!(store, args, raised)
    }
}
