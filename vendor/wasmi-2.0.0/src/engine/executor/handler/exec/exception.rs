//! TRust: execution of the WebAssembly exception-handling instructions.
//!
//! Legacy instructions, exception handling proposal `document/legacy/exceptions/core/exec.rst`
//! (local snapshot af287a73): a `try` installs a handler for its body; a raised exception unwinds
//! to the innermost handler, whose clauses are tested in order (`exec-throw_ref` step 16), and a
//! catch clause makes the exception available to `rethrow` (`exec-rethrow`). A `delegate`
//! re-raises the exception at the `try` its label targets (`exec-try-delegate`).
//!
//! Standardized instructions, WebAssembly Core 3.0 `exec/instructions.rst` (`exec-try_table`,
//! `exec-throw`, `exec-throw_ref`; local snapshots of the spec repository 37d6b059 and the
//! exception-handling repository af287a73): a `try_table` installs a handler the same way. Its
//! catch clauses consume the pending exception, push its fields and, for `catch_ref` and
//! `catch_all_ref`, its exception reference, and branch to their label (step 15). `throw_ref`
//! raises the referenced exception again, keeping its address in the store, or traps on null.
//! Both kinds of handlers select the same pending exception, so one exception can pass through
//! legacy and standardized handlers alike.

use super::super::{
    Args,
    dispatch::Done,
    state::{DoneReason, Freg32, Freg64, Inst, Ip, Ireg, Mem0Len, Mem0Ptr, Sp, WasmException},
};
use crate::{
    Error,
    TrapCode,
    core::RawRef,
    engine::executor::CellsWriter,
    ir::BoundedSlotSpan,
    store::{PrunedStore, StoreInner},
};

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

execution_handler! {
    fn exception_delegate(
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
        let crate::ir::decode::ExceptionDelegate { target } = unsafe { args.decode_op() };
        let Some(raised) = store.stack_mut().delegate(ip, target) else {
            done!(
                store,
                DoneReason::error(Error::new("legacy delegate without a pending exception"))
            )
        };
        dispatch_raised!(store, args, raised)
    }
}

/// TRust: catches the pending exception for a `catch_ref` (`tag` is `Some`) or `catch_all_ref`
/// clause of a `try_table` (`exec-throw_ref` steps 15d and 15f): writes its fields, if `tag` is
/// `Some`, followed by its exception reference to the cells `results` of the frame at `sp`.
///
/// Returns `Some(false)` if the pending exception is not of `tag` defined by `instance`, and
/// `None` if the store cannot allocate another exception instance.
///
/// # Note
///
/// Takes and returns only scalars so that the calling handler keeps its sibling call.
#[inline(never)]
fn catch_pending_ref(
    store: &mut StoreInner,
    tag: Option<u32>,
    instance: Inst,
    sp: Sp,
    results: BoundedSlotSpan,
) -> Option<bool> {
    match store.exec_mut().stack_mut().pending_exn(tag, instance) {
        None => return Some(false),
        // Collect while the pending exception is still a root of the store's collector: its
        // fields may reference other exceptions.
        Some(None) => store.collect_exns_if_due(),
        Some(Some(_)) => {}
    }
    let Some(exception) = store
        .exec_mut()
        .stack_mut()
        .take_pending_exception_if(tag, instance)
    else {
        return Some(false);
    };
    let mut cells = sp.offset(results.span().head());
    if tag.is_some() {
        debug_assert_eq!(exception.fields().len() + 1, usize::from(results.len()));
        for &field in exception.fields() {
            let _ = CellsWriter::next(&mut cells, field);
        }
    }
    let exn = match exception.exn() {
        // The exception was thrown by reference: keep its address (`exec-throw_ref` step 15d).
        Some(exn) => exn,
        None => RawRef::from(store.exns_mut().alloc(exception.into_entity())?.get()),
    };
    let _ = CellsWriter::next(&mut cells, exn.into());
    Some(true)
}

/// TRust: the outcome of [`throw_ref`].
enum ThrowRef {
    /// The exception was raised; `true` if a handler was selected.
    Raised(bool),
    /// The exception reference was null (`exec-throw_ref` step 3).
    Null,
    /// The exception reference addresses no exception instance of the store.
    Dangling,
}

/// TRust: raises the exception referenced by `exn` at `ip` (`exec-throw_ref`).
///
/// # Note
///
/// Takes and returns only scalars so that the calling handler keeps its sibling call.
#[inline(never)]
fn throw_ref(store: &mut StoreInner, ip: Ip, exn: u32) -> ThrowRef {
    let exn = RawRef::from(exn);
    if exn.is_null() {
        return ThrowRef::Null;
    }
    let Some(entity) = store.exns().get(exn) else {
        return ThrowRef::Dangling;
    };
    let exception = WasmException::from_entity(entity, exn);
    ThrowRef::Raised(store.exec_mut().stack_mut().raise(ip, exception))
}

execution_handler! {
    fn exception_table_catch(
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
        let crate::ir::decode::ExceptionTableCatch { results, tag, next } =
            unsafe { args.decode_op() };
        let caught = store
            .stack_mut()
            .catch_pending_values(tag, args.instance, args.sp, results);
        if !caught {
            args.set_ip(ip);
            args.offset_ip(next);
        }
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_table_catch_ref(
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
        let crate::ir::decode::ExceptionTableCatchRef { results, tag, next } =
            unsafe { args.decode_op() };
        let Some(caught) =
            catch_pending_ref(store.inner_mut(), Some(tag), args.instance, args.sp, results)
        else {
            done!(store, DoneReason::error(Error::from(TrapCode::OutOfSystemMemory)))
        };
        if !caught {
            args.set_ip(ip);
            args.offset_ip(next);
        }
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_table_catch_all(
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
        // `ExceptionTableCatchAll` has no fields.
        let [] = unsafe { args.decode_op::<[u8; 0]>() };
        store.stack_mut().drop_pending();
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_table_catch_all_ref(
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
        let crate::ir::decode::ExceptionTableCatchAllRef { result } = unsafe { args.decode_op() };
        let results = BoundedSlotSpan::new(crate::ir::SlotSpan::new(result), 1);
        if catch_pending_ref(store.inner_mut(), None, args.instance, args.sp, results).is_none() {
            done!(store, DoneReason::error(Error::from(TrapCode::OutOfSystemMemory)))
        }
        dispatch!(store, args)
    }
}

execution_handler! {
    fn exception_throw_ref(
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
        let crate::ir::decode::ExceptionThrowRef { value } = unsafe { args.decode_op() };
        let exn: u32 = args.get(value);
        let raised = match throw_ref(store.inner_mut(), ip, exn) {
            ThrowRef::Raised(raised) => raised,
            ThrowRef::Null => trap!(TrapCode::NullExceptionReference),
            ThrowRef::Dangling => done!(
                store,
                DoneReason::error(Error::new("dangling WebAssembly exception reference"))
            ),
        };
        dispatch_raised!(store, args, raised)
    }
}
