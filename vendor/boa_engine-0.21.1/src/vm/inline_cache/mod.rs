use std::cell::Cell;

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};

use crate::{
    JsString,
    object::shape::{
        Shape, WeakShape,
        slot::{Slot, SlotAttributes},
    },
};

#[cfg(test)]
mod tests;

/// An inline cache entry for a property access.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct InlineCache {
    /// The property that is accessed.
    pub(crate) name: JsString,

    /// A pointer is kept to the shape to avoid the shape from being deallocated.
    pub(crate) shape: GcRefCell<WeakShape>,

    /// For a direct-prototype hit, the shape of the prototype object that
    /// actually holds the property (`WeakShape::None` for an own-property hit).
    ///
    /// The receiver `shape` only pins the prototype *pointer*; it does not
    /// change when the prototype's own layout does. Deleting/redefining a
    /// prototype property reorders the prototype's storage while leaving the
    /// receiver shape untouched, which would otherwise leave `slot` pointing at
    /// the wrong (or an out-of-bounds) entry. Re-checking this on a hit closes
    /// that hole.
    pub(crate) proto_shape: GcRefCell<WeakShape>,

    /// The [`Slot`] of the property.
    #[unsafe_ignore_trace]
    pub(crate) slot: Cell<Slot>,
}

impl InlineCache {
    pub(crate) const fn new(name: JsString) -> Self {
        Self {
            name,
            shape: GcRefCell::new(WeakShape::None),
            proto_shape: GcRefCell::new(WeakShape::None),
            slot: Cell::new(Slot::new()),
        }
    }

    pub(crate) fn set(&self, shape: &Shape, slot: Slot) {
        *self.shape.borrow_mut() = shape.into();
        self.slot.set(slot);
        // For a prototype hit, also pin the prototype-holder's current shape so
        // a later layout change on the prototype invalidates this entry.
        *self.proto_shape.borrow_mut() = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            shape.prototype().map_or(WeakShape::None, |proto| {
                WeakShape::from(proto.borrow().shape())
            })
        } else {
            WeakShape::None
        };
    }

    pub(crate) fn slot(&self) -> Slot {
        self.slot.get()
    }

    /// Returns true, if the [`InlineCache`]'s shape matches with the given shape.
    ///
    /// Otherwise we reset the internal weak reference to [`WeakShape::None`],
    /// so it can be deallocated by the GC.
    pub(crate) fn match_or_reset(&self, shape: &Shape) -> Option<(Shape, Slot)> {
        let mut old = self.shape.borrow_mut();

        let old_upgraded = old.upgrade();
        if old_upgraded.as_ref().map_or(0, Shape::to_addr_usize) != shape.to_addr_usize() {
            *old = WeakShape::None;
            return None;
        }

        let matched = old_upgraded.expect("addr matched, so the weak shape is live");
        let slot = self.slot();

        // For a direct-prototype hit the cached `slot` indexes the *prototype's*
        // own storage, but the receiver shape only pins the prototype pointer —
        // not the prototype's shape. A mutation on the prototype (delete /
        // redefine / reorder) shifts those slots while leaving the receiver
        // shape untouched, so the cached index goes stale: an out-of-bounds read
        // (panic) or, if it stays in range, a silently wrong property. Pointer
        // equality of the prototype's current shape against the one captured at
        // install time rejects exactly those cases; on any mismatch we drop the
        // entry and fall through to the standards-compliant slow path.
        if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            let cached_addr = self
                .proto_shape
                .borrow()
                .upgrade()
                .as_ref()
                .map_or(0, Shape::to_addr_usize);
            let current_addr = matched
                .prototype()
                .map_or(0, |proto| proto.borrow().shape().to_addr_usize());
            if cached_addr == 0 || cached_addr != current_addr {
                *old = WeakShape::None;
                *self.proto_shape.borrow_mut() = WeakShape::None;
                return None;
            }
        }

        Some((matched, slot))
    }
}
