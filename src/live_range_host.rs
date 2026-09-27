//! Non-owning, per-Agent discovery of live DOM Ranges.
//!
//! DOM #concept-live-range requires mutation adjustment for every surviving
//! Range, including one created in a different or destroyed Window. It does
//! not make the registry an owner of otherwise unreachable Ranges or Realms.
//! A shared JavaScript Set of WeakRefs is insufficient for non-owning discovery:
//! each WeakRef's prototype retains its creating Realm. Native weak handles
//! have neither a prototype nor an associated Realm.

use lumen::embed::{HostRetainedMemoryVisitor, RetainedManagedAllocation, Value, WeakValue};
use std::cell::Cell;

const COMPACT_FLOOR: usize = 256;

pub(super) struct Registry {
    old: Vec<WeakValue>,
    young: Vec<WeakValue>,
    next_compaction: usize,
    registrations_since_old_compaction: usize,
    next_old_compaction: usize,
    collecting_young: Cell<bool>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            old: Vec::new(),
            young: Vec::new(),
            next_compaction: COMPACT_FLOOR,
            registrations_since_old_compaction: 0,
            next_old_compaction: COMPACT_FLOOR,
            collecting_young: Cell::new(false),
        }
    }
}

impl Registry {
    pub(super) fn register(&mut self, range: WeakValue) {
        // Acyclic churn can drop objects without a tracing collection. Compact
        // geometrically, not on every allocation: bounded dead Rc headers and
        // amortized linear work even if every registered Range stays alive.
        if self.young.len() >= self.next_compaction {
            self.young.retain(|range| range.upgrade().is_some());
            shrink(&mut self.young);
            self.next_compaction = self.young.len().saturating_mul(2).max(COMPACT_FLOOR);
        }
        // A Range promoted by a minor GC can subsequently die by reference
        // counting. Do not let repeated minor-only churn retain its header
        // forever, and do not rescan a large stable old census on every minor
        // GC or creation. The interval scales with the preceding census size.
        self.registrations_since_old_compaction += 1;
        if self.registrations_since_old_compaction >= self.next_old_compaction {
            self.old.retain(|range| range.upgrade().is_some());
            shrink(&mut self.old);
            self.reset_old_compaction();
        }
        self.young.push(range);
    }

    pub(super) fn snapshot(&mut self) -> Vec<Value> {
        // All upgraded handles are owned before JS resumes. In particular a
        // mutation's reentrant collection cannot invalidate its in-flight
        // range traversal, and no RefCell host borrow crosses a callback.
        let mut values = Vec::new();
        for entries in [&mut self.old, &mut self.young] {
            entries.retain(|range| {
                if let Some(value) = range.upgrade() {
                    values.push(value);
                    true
                } else {
                    false
                }
            });
            shrink(entries);
        }
        self.next_compaction = self.young.len().saturating_mul(2).max(COMPACT_FLOOR);
        self.reset_old_compaction();
        values
    }

    pub(super) fn begin_collection(&self, minor: bool) {
        self.collecting_young.set(minor);
    }

    pub(super) fn sweep(&mut self, is_live: &dyn Fn(&Value) -> bool) {
        let keep = |range: &WeakValue| range.upgrade().is_some_and(|value| is_live(&value));
        // Like the engine's nursery, a minor sweep need not visit stable old
        // entries. The predicate treats old objects as live; registering an
        // already-promoted Range is consequently conservative and safe too.
        let minor = self.collecting_young.get();
        if !minor {
            self.old.retain(keep);
        }
        self.young.retain(keep);
        self.old.append(&mut self.young);
        shrink(&mut self.old);
        shrink(&mut self.young);
        self.next_compaction = COMPACT_FLOOR;
        if !minor {
            self.reset_old_compaction();
        }
    }

    fn reset_old_compaction(&mut self) {
        self.registrations_since_old_compaction = 0;
        self.next_old_compaction = self.old.len().max(COMPACT_FLOOR);
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        for (name, entries) in [
            ("trust.live-ranges.old", &self.old),
            ("trust.live-ranges.young", &self.young),
        ] {
            if entries.capacity() != 0 {
                visitor.allocation(RetainedManagedAllocation::new(
                    name,
                    entries.as_ptr() as usize,
                    entries.capacity() * std::mem::size_of::<WeakValue>(),
                ));
            }
        }
        // Weak Rc control-block allocation layout is not part of the public
        // engine API. Never report these weak targets as retained JS roots.
        if !self.old.is_empty() || !self.young.is_empty() {
            visitor.opaque_storage();
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.old.len() + self.young.len()
    }
}

fn shrink(entries: &mut Vec<WeakValue>) {
    if entries.capacity() > entries.len().saturating_mul(4).max(COMPACT_FLOOR) {
        entries.shrink_to(entries.len().saturating_mul(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_registry_snapshots_own_handles_only_until_the_snapshot_is_dropped() {
        let mut engine = lumen::Engine::new();
        let value = Value::Obj(engine.ctx().new_object());
        let weak = engine.ctx().downgrade_object_value(&value).unwrap();
        let mut registry = Registry::default();
        registry.register(weak.clone());
        let snapshot = registry.snapshot();
        drop(value);
        assert!(weak.upgrade().is_some());
        assert_eq!(snapshot.len(), 1);
        drop(snapshot);
        assert!(weak.upgrade().is_none());
        assert!(registry.snapshot().is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn range_registry_minor_only_and_acyclic_churn_keep_metadata_bounded() {
        let mut engine = lumen::Engine::new();
        let mut registry = Registry::default();
        for minor in [false, true] {
            for _ in 0..4096 {
                let value = Value::Obj(engine.ctx().new_object());
                registry.register(engine.ctx().downgrade_object_value(&value).unwrap());
                if minor {
                    registry.begin_collection(true);
                    registry.sweep(&|_| true);
                }
            }
            assert!(registry.len() <= COMPACT_FLOOR + 1);
            assert!(registry.snapshot().is_empty());
        }
    }

    #[test]
    fn range_registry_minor_sweep_does_not_rescan_stable_old_ranges() {
        let mut engine = lumen::Engine::new();
        let mut registry = Registry::default();
        let value = Value::Obj(engine.ctx().new_object());
        registry.register(engine.ctx().downgrade_object_value(&value).unwrap());
        registry.begin_collection(false);
        registry.sweep(&|_| true);
        registry.begin_collection(true);
        registry.sweep(&|_| panic!("minor GC scanned a stable old Range"));
        assert_eq!(registry.len(), 1);
        drop(value);
        registry.begin_collection(false);
        registry.sweep(&|_| false);
        assert_eq!(registry.len(), 0);
    }
}
