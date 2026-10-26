//! Non-owning, per-Agent discovery of live DOM Ranges.
//!
//! DOM #concept-live-range requires mutation adjustment for every surviving
//! Range, including one created in a different or destroyed Window. It does
//! not make the registry an owner of otherwise unreachable Ranges or Realms.
//! A shared JavaScript Set of WeakRefs is insufficient for non-owning discovery:
//! each WeakRef's prototype retains its creating Realm. Native weak handles
//! have neither a prototype nor an associated Realm.

use std::cell::Cell;
use trust_js::embed::{
    Ctx, HostRetainedMemoryVisitor, RetainedManagedAllocation, Value, WeakHandle,
};

const COMPACT_FLOOR: usize = 256;

/// Every Range the Agent created, named weakly, in the engine's two generations: those a
/// collection has seen, and those registered since. A collection's sweep forgets those it found
/// dead. A minor collection frees only objects allocated since the last collection, so its sweep
/// visits the young Ranges alone.
#[derive(Default)]
pub(super) struct Registry {
    old: Vec<WeakHandle>,
    young: Vec<WeakHandle>,
    /// Whether the collection in progress is minor (`Registry::begin_collection`).
    collecting_young: Cell<bool>,
}

impl Registry {
    pub(super) fn register(&mut self, range: WeakHandle) {
        self.young.push(range);
    }

    /// Whether any Range may still be live. A dead handle not yet pruned
    /// answers true; the caller then takes the snapshot, which prunes it.
    pub(super) fn may_have_ranges(&self) -> bool {
        !self.old.is_empty() || !self.young.is_empty()
    }

    /// The live Ranges, read into `ctx`'s scope. The registry is taken out of the host state
    /// while it is read (`Registry::take`), so no host borrow is held.
    pub(super) fn snapshot<'s>(&mut self, ctx: &Ctx<'s>) -> Vec<Value<'s>> {
        let mut values = Vec::new();
        for ranges in [&mut self.old, &mut self.young] {
            ranges.retain(|range| match range.upgrade(ctx) {
                Ok(Some(value)) => {
                    values.push(value);
                    true
                }
                _ => false,
            });
            shrink(ranges);
        }
        values
    }

    /// Note whether the collection whose sweep comes next is minor (`HostState::trace_gc`).
    pub(super) fn begin_collection(&self, minor: bool) {
        self.collecting_young.set(minor);
    }

    /// Forget the Ranges a collection found dead, after it cleared their handles: after a minor
    /// collection, young ones alone. Every Range left is then old.
    pub(super) fn sweep(&mut self) {
        if !self.collecting_young.get() {
            self.old.retain(|range| !range.is_cleared());
        }
        self.young.retain(|range| !range.is_cleared());
        self.old.append(&mut self.young);
        shrink(&mut self.old);
        shrink(&mut self.young);
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        for (name, ranges) in [
            ("trust.live-ranges.old", &self.old),
            ("trust.live-ranges.young", &self.young),
        ] {
            if ranges.capacity() != 0 {
                visitor.allocation(RetainedManagedAllocation::new(
                    name,
                    ranges.as_ptr() as usize,
                    ranges.capacity() * std::mem::size_of::<WeakHandle>(),
                ));
                // A weak handle's table entry belongs to the engine.
                visitor.opaque_storage();
            }
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.old.len() + self.young.len()
    }
}

fn shrink(entries: &mut Vec<WeakHandle>) {
    if entries.capacity() > entries.len().saturating_mul(4).max(COMPACT_FLOOR) {
        entries.shrink_to(entries.len().saturating_mul(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register `count` new objects, reachable only through the registry's weak handles.
    #[inline(never)]
    fn register_unreachable(engine: &mut trust_js::Engine, registry: &mut Registry, count: usize) {
        engine.scope(|cx| {
            for _ in 0..count {
                let value = Value::Obj(cx.new_object());
                registry.register(WeakHandle::new(cx, &value).unwrap());
            }
        });
    }

    #[test]
    fn range_registry_snapshots_live_ranges_without_keeping_them() {
        let mut engine = trust_js::Engine::new();
        let mut registry = Registry::default();
        engine.eval("globalThis.kept = {}", false).unwrap();
        engine.scope(|cx| {
            let global = cx.global_this();
            let kept = cx.member_get(&global, "kept").unwrap();
            registry.register(WeakHandle::new(cx, &kept).unwrap());
            let snapshot = registry.snapshot(cx);
            assert_eq!(snapshot.len(), 1);
            assert!(cx.values_strict_equal(&snapshot[0], &kept));
        });
        register_unreachable(&mut engine, &mut registry, 1000);
        engine.collect_garbage_at_idle();
        registry.sweep();
        // The conservative stack scan may keep a few of the dropped objects; the kept one stays.
        assert!(registry.len() < 100, "{} ranges survived", registry.len());
        assert!(engine.scope(|cx| registry.snapshot(cx).len()) >= 1);
    }

    #[test]
    fn range_registry_churn_keeps_metadata_bounded() {
        let mut engine = trust_js::Engine::new();
        let mut registry = Registry::default();
        for _ in 0..8 {
            register_unreachable(&mut engine, &mut registry, 4096);
            engine.collect_garbage_at_idle();
            registry.sweep();
            assert!(registry.len() < 4096, "{} ranges survived", registry.len());
        }
        assert!(registry.old.capacity() <= 4 * COMPACT_FLOOR.max(4096));
        assert!(registry.young.capacity() <= 4 * COMPACT_FLOOR.max(4096));
    }

    /// A minor collection frees young Ranges alone: its sweep forgets the dead young ones and
    /// counts the rest old, and a Range that was old when it became unreachable waits for a
    /// major collection, whose sweep visits every Range.
    #[test]
    fn range_registry_minor_sweep_forgets_dead_young_ranges_and_keeps_old_ones() {
        let mut engine = trust_js::Engine::new();
        let mut registry = Registry::default();
        engine.eval("globalThis.kept = {}", false).unwrap();
        engine.scope(|cx| {
            let global = cx.global_this();
            let kept = cx.member_get(&global, "kept").unwrap();
            registry.register(WeakHandle::new(cx, &kept).unwrap());
        });
        registry.begin_collection(false);
        engine.collect_garbage_at_idle();
        registry.sweep();
        assert_eq!((registry.old.len(), registry.young.len()), (1, 0));
        engine.eval("globalThis.kept = null", false).unwrap();
        register_unreachable(&mut engine, &mut registry, 1000);
        registry.begin_collection(true);
        engine.scope(|cx| cx.unstable_collect_young_for_host_tests());
        registry.sweep();
        // The conservative stack scan may keep a few of the dropped objects.
        assert!(registry.young.is_empty());
        assert!(
            registry.old.len() < 100,
            "{} ranges survived",
            registry.old.len()
        );
        assert!(
            !registry.old[0].is_cleared(),
            "a minor collection does not free an old Range"
        );
        registry.begin_collection(false);
        engine.collect_garbage_at_idle();
        registry.sweep();
        assert!(
            registry.old.len() < 100,
            "{} ranges survived",
            registry.old.len()
        );
    }
}
