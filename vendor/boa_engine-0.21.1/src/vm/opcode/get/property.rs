use crate::{
    Context, JsResult, JsString,
    object::{internal_methods::InternalMethodPropertyContext, shape::slot::SlotAttributes},
    property::PropertyKey,
    vm::opcode::{Operation, VaryingOperand},
};

thread_local! {
    /// Diagnostic: the name of the most recent property whose GET resolved to
    /// `undefined`. A method call `obj.foo()` compiles to `GetPropertyByName
    /// "foo"` immediately before `Call`, so when the call then fails with
    /// "undefined is not a callable function" the call opcode can name `foo` —
    /// turning a missing platform method from a bare type error into a legible
    /// one (the single biggest lever for finding unimplemented APIs). Recorded
    /// only on the slow path (an absent property always misses the inline
    /// cache), so normal property access pays nothing. Best-effort: an
    /// `undefined` argument evaluated after the callee can overwrite it, and
    /// it's cleared when read.
    static LAST_UNDEFINED_GET: std::cell::Cell<Option<JsString>> = const { std::cell::Cell::new(None) };
}

/// Record a property name whose GET resolved to `undefined` (see
/// [`LAST_UNDEFINED_GET`]).
pub(crate) fn record_undefined_get(name: &JsString) {
    LAST_UNDEFINED_GET.with(|c| c.set(Some(name.clone())));
}

/// Take (and clear) the last property name recorded by [`record_undefined_get`].
pub(crate) fn take_undefined_get() -> Option<JsString> {
    LAST_UNDEFINED_GET.with(std::cell::Cell::take)
}

/// `GetPropertyByName` implements the Opcode Operation for `Opcode::GetPropertyByName`
///
/// Operation:
///  - Get a property by name from an object an push it on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByName;

impl GetPropertyByName {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, receiver, value, index): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let receiver = context.vm.get_register(receiver.into()).clone();
        let object = context.vm.get_register(value.into()).clone();
        let object = object.to_object(context)?;

        let ic = &context.vm.frame().code_block().ic[usize::from(index)];
        let object_borrowed = object.borrow();
        if let Some((shape, slot)) = ic.match_or_reset(object_borrowed.shape()) {
            let mut result = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let prototype = shape.prototype().expect("prototype should have value");
                let prototype = prototype.borrow();
                prototype.properties().storage[slot.index as usize].clone()
            } else {
                object_borrowed.properties().storage[slot.index as usize].clone()
            };

            drop(object_borrowed);
            if slot.attributes.has_get() && result.is_object() {
                result = result.as_object().expect("should contain getter").call(
                    &receiver,
                    &[],
                    context,
                )?;
            }
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }

        drop(object_borrowed);

        let name = ic.name.clone();
        let key: PropertyKey = name.clone().into();

        let context = &mut InternalMethodPropertyContext::new(context);
        let result = object.__get__(&key, receiver.clone(), context)?;

        // Diagnostic: name a missing property so a later "not a callable
        // function" can report it (see `LAST_UNDEFINED_GET`).
        if result.is_undefined() {
            record_undefined_get(&name);
        }

        // Cache the property.
        let slot = *context.slot();
        if slot.is_cachable() {
            let ic = &context.vm.frame().code_block.ic[usize::from(index)];
            let object_borrowed = object.borrow();
            let shape = object_borrowed.shape();
            ic.set(shape, slot);
        }

        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByName {
    const NAME: &'static str = "GetPropertyByName";
    const INSTRUCTION: &'static str = "INST - GetPropertyByName";
    const COST: u8 = 4;
}

/// `GetPropertyByValue` implements the Opcode Operation for `Opcode::GetPropertyByValue`
///
/// Operation:
///  - Get a property by value from an object an push it on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValue;

impl GetPropertyByValue {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, key, receiver, object): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let key = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let object = object.to_object(context)?;
        let key = key.to_property_key(context)?;

        // Fast Path
        if object.is_array()
            && let PropertyKey::Index(index) = &key
        {
            let object_borrowed = object.borrow();
            if let Some(element) = object_borrowed.properties().get_dense_property(index.get()) {
                context.vm.set_register(dst.into(), element);
                return Ok(());
            }
        }

        let receiver = context.vm.get_register(receiver.into());

        // Slow path:
        let result = object.__get__(
            &key,
            receiver.clone(),
            &mut InternalMethodPropertyContext::new(context),
        )?;

        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByValue {
    const NAME: &'static str = "GetPropertyByValue";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValue";
    const COST: u8 = 4;
}

/// `GetPropertyByValuePush` implements the Opcode Operation for `Opcode::GetPropertyByValuePush`
///
/// Operation:
///  - Get a property by value from an object an push the key and value on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValuePush;

impl GetPropertyByValuePush {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, key, receiver, object): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let key_value = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let object = object.to_object(context)?;
        let key_value = key_value.to_property_key(context)?;

        // Fast Path
        if object.is_array()
            && let PropertyKey::Index(index) = &key_value
        {
            let object_borrowed = object.borrow();
            if let Some(element) = object_borrowed.properties().get_dense_property(index.get()) {
                context.vm.set_register(key.into(), key_value.into());
                context.vm.set_register(dst.into(), element);
                return Ok(());
            }
        }

        let receiver = context.vm.get_register(receiver.into());

        // Slow path:
        let result = object.__get__(
            &key_value,
            receiver.clone(),
            &mut InternalMethodPropertyContext::new(context),
        )?;

        context.vm.set_register(key.into(), key_value.into());
        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByValuePush {
    const NAME: &'static str = "GetPropertyByValuePush";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValuePush";
    const COST: u8 = 4;
}
