use super::Executor;
use crate::{
    core::UntypedVal,
    ir::{index, Const16, Slot},
    store::StoreInner,
};

#[cfg(doc)]
use crate::ir::Op;

impl Executor<'_> {
    /// Executes an [`Op::GlobalGet`].
    pub fn execute_global_get(
        &mut self,
        store: &mut StoreInner,
        result: Slot,
        global: index::Global,
    ) {
        // The executor refreshes its caches after host calls and instance switches.
        let value = unsafe { self.cache.global_at(store, global).get() };
        self.set_stack_slot(result, value);
        self.next_instr()
    }

    /// Executes an [`Op::GlobalSet`].
    pub fn execute_global_set(
        &mut self,
        store: &mut StoreInner,
        global: index::Global,
        input: Slot,
    ) {
        let input = self.get_stack_slot(input);
        self.execute_global_set_impl(store, global, input)
    }

    /// Executes an [`Op::GlobalSetI32Imm16`].
    pub fn execute_global_set_i32imm16(
        &mut self,
        store: &mut StoreInner,
        global: index::Global,
        input: Const16<i32>,
    ) {
        let input = i32::from(input).into();
        self.execute_global_set_impl(store, global, input)
    }

    /// Executes an [`Op::GlobalSetI64Imm16`].
    pub fn execute_global_set_i64imm16(
        &mut self,
        store: &mut StoreInner,
        global: index::Global,
        input: Const16<i64>,
    ) {
        let input = i64::from(input).into();
        self.execute_global_set_impl(store, global, input)
    }

    /// Executes a generic `global.set` instruction.
    fn execute_global_set_impl(
        &mut self,
        store: &mut StoreInner,
        global: index::Global,
        new_value: UntypedVal,
    ) {
        // Validation guarantees a mutable global of the matching type. Cache
        // invalidation is identical to the existing global-zero optimization.
        unsafe { self.cache.global_at(store, global).set(new_value) };
        self.next_instr()
    }
}
