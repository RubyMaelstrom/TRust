use super::Executor;
use crate::{
    core::UntypedVal,
    ir::{AnyConst32, BoundedSlotSpan, BranchOffset, Slot},
    store::StoreInner,
    Error,
};
use alloc::vec::Vec;

/// A legacy WebAssembly exception and its tag fields.
#[derive(Debug)]
struct WasmException {
    tag: u32,
    fields: Vec<UntypedVal>,
}

/// A handler installed by a legacy `try` instruction.
#[derive(Debug, Copy, Clone)]
struct ExceptionHandler {
    try_id: u32,
    call_depth: usize,
    catch_ip: super::InstructionPtr,
}

/// The exception selected by a catch clause and available to `rethrow`.
#[derive(Debug)]
struct CaughtException {
    try_id: u32,
    call_depth: usize,
    exception: WasmException,
}

/// Dynamic legacy exception state for one Wasm execution.
#[derive(Debug, Default)]
pub(super) struct ExceptionState {
    handlers: Vec<ExceptionHandler>,
    caught: Vec<CaughtException>,
    pending: Option<WasmException>,
}

impl ExceptionState {
    fn remove_handler(&mut self, try_id: u32) -> bool {
        let Some(index) = (0..self.handlers.len())
            .rev()
            .find(|&index| self.handlers[index].try_id == try_id)
        else {
            return false;
        };
        self.handlers.remove(index);
        true
    }

    fn remove_caught(&mut self, try_id: u32) -> Option<WasmException> {
        let index = (0..self.caught.len())
            .rev()
            .find(|&index| self.caught[index].try_id == try_id)?;
        Some(self.caught.remove(index).exception)
    }

    pub(super) fn prune(&mut self, call_depth: usize) {
        self.handlers
            .retain(|handler| handler.call_depth <= call_depth);
        self.caught
            .retain(|caught| caught.call_depth <= call_depth);
    }
}

impl Executor<'_> {
    /// Installs a handler whose target is the first legacy catch clause.
    pub(super) fn execute_exception_try(&mut self, handler: BranchOffset, try_id: AnyConst32) {
        let mut catch_ip = self.ip;
        catch_ip.offset(handler.to_i32() as isize);
        self.exceptions.handlers.push(ExceptionHandler {
            try_id: u32::from(try_id),
            call_depth: self.stack.calls.len(),
            catch_ip,
        });
        self.next_instr();
    }

    /// Selects a typed catch clause or advances to its next clause.
    pub(super) fn execute_exception_catch(
        &mut self,
        results: BoundedSlotSpan,
        tag: AnyConst32,
        next: BranchOffset,
    ) -> Result<(), Error> {
        let tag = u32::from(tag);
        let Some(exception) = self.exceptions.pending.as_ref() else {
            self.next_instr();
            return Ok(());
        };
        if exception.tag != tag {
            self.ip.offset(next.to_i32() as isize);
            return Ok(());
        }
        let exception = self
            .exceptions
            .pending
            .take()
            .expect("pending exception disappeared after matching catch");
        if exception.fields.len() != usize::from(results.len()) {
            return Err(Error::new("legacy exception tag field count mismatch"));
        }
        let try_id = self
            .exceptions
            .current_try_id()
            .expect("matching handler must identify its try");
        self.exceptions.remove_handler(try_id);
        for (slot, value) in results.into_iter().zip(exception.fields.iter().copied()) {
            self.set_stack_slot(slot, value);
        }
        self.exceptions.caught.push(CaughtException {
            try_id,
            call_depth: self.stack.calls.len(),
            exception,
        });
        self.next_instr();
        Ok(())
    }

    /// Selects a legacy `catch_all` clause.
    pub(super) fn execute_exception_catch_all(
        &mut self,
        try_id: AnyConst32,
    ) -> Result<(), Error> {
        let try_id = u32::from(try_id);
        let Some(exception) = self.exceptions.pending.take() else {
            self.next_instr();
            return Ok(());
        };
        self.exceptions.remove_handler(try_id);
        self.exceptions.caught.push(CaughtException {
            try_id,
            call_depth: self.stack.calls.len(),
            exception,
        });
        self.next_instr();
        Ok(())
    }

    /// Raises a typed legacy exception.
    pub(super) fn execute_exception_throw(
        &mut self,
        store: &mut StoreInner,
        tag: AnyConst32,
        values: BoundedSlotSpan,
    ) -> Result<(), Error> {
        let fields = values
            .into_iter()
            .map(|slot: Slot| self.get_stack_slot(slot))
            .collect();
        self.raise_exception(
            store,
            WasmException {
                tag: u32::from(tag),
                fields,
            },
        )
    }

    /// Rethrows the pending exception or the exception selected by a catch.
    pub(super) fn execute_exception_rethrow(
        &mut self,
        store: &mut StoreInner,
        try_id: AnyConst32,
    ) -> Result<(), Error> {
        let try_id = u32::from(try_id);
        let exception = match self.exceptions.pending.take() {
            Some(exception) => exception,
            None => self
                .exceptions
                .remove_caught(try_id)
                .ok_or_else(|| Error::new("legacy rethrow without a caught exception"))?,
        };
        self.exceptions.remove_handler(try_id);
        self.raise_exception(store, exception)
    }

    /// Removes the handler after normal completion of a legacy `try`.
    pub(super) fn execute_exception_end(&mut self, try_id: AnyConst32) {
        let try_id = u32::from(try_id);
        self.exceptions.remove_handler(try_id);
        self.exceptions.remove_caught(try_id);
        self.next_instr();
    }

    fn raise_exception(
        &mut self,
        store: &mut StoreInner,
        exception: WasmException,
    ) -> Result<(), Error> {
        let Some(handler_index) = (0..self.exceptions.handlers.len())
            .rev()
            .find(|&index| self.handler_is_in_current_call(self.exceptions.handlers[index]))
        else {
            return Err(Error::new("uncaught WebAssembly exception"));
        };
        let handler = self.exceptions.handlers[handler_index];
        self.exceptions.handlers.truncate(handler_index + 1);
        self.exceptions.pending = Some(exception);

        while self.stack.calls.len() > handler.call_depth {
            let (returned, popped_instance) = self
                .stack
                .calls
                .pop()
                .expect("exception handler call depth must be live");
            self.stack.values.truncate(returned.frame_offset());
            if let Some(new_instance) = popped_instance.and_then(|_| self.stack.calls.instance()) {
                self.cache.update(store, new_instance);
            }
        }
        let Some(frame) = self.stack.calls.peek() else {
            return Err(Error::new("uncaught WebAssembly exception"));
        };
        Self::init_call_frame_impl(
            &mut self.stack.values,
            &mut self.sp,
            &mut self.ip,
            frame,
        );
        self.ip = handler.catch_ip;
        self.exceptions.prune(self.stack.calls.len());
        Ok(())
    }

    fn handler_is_in_current_call(&self, handler: ExceptionHandler) -> bool {
        if handler.call_depth == 0 || handler.call_depth > self.stack.calls.len() {
            return false;
        }
        let _ = handler.catch_ip;
        true
    }
}

impl ExceptionState {
    fn current_try_id(&self) -> Option<u32> {
        self.handlers.last().map(|handler| handler.try_id)
    }

}
