//! Compact storage with stable, non-recycled DOM identities.
//!
//! DOM #concept-node-tree (a2331a45) and Web IDL #idl-objects/#SameObject (8f182624)
//! do not equate platform-object identity with a
//! physical array offset. Old async messages and snapshots cannot designate a new node after
//! collection. Slots compact and capacities shrink independently of the monotonic identity.

use super::NodeId;
use rustc_hash::FxHashMap;
use std::ops::{Index, IndexMut};

#[derive(Clone, Debug)]
pub(crate) struct DenseIdMap<T> {
    slots: Vec<(NodeId, T)>,
    relocated: FxHashMap<NodeId, usize>,
    next_id: NodeId,
    generation: Option<Box<Generation>>,
    dense_gaps: bool,
}

// Presentation snapshots compare logical ID→value mappings, not physical slot order. Retiring
// an unrelated nursery node may swap a surviving slot without changing any painted metadata.
impl<T: PartialEq> PartialEq for DenseIdMap<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .entries()
                .all(|(id, value)| other.get(id) == Some(value))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Generation {
    first_young_id: NodeId,
    first_young_slot: usize,
    remembered: rustc_hash::FxHashSet<NodeId>,
}

impl<T> Default for DenseIdMap<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            relocated: FxHashMap::default(),
            next_id: 0,
            generation: None,
            dense_gaps: true,
        }
    }
}

impl<T> DenseIdMap<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            ..Self::default()
        }
    }

    #[inline]
    fn slot(&self, id: NodeId) -> Option<usize> {
        // Ordinary parsed documents retain the original dense layout with no hash lookup.
        if self
            .slots
            .get(id)
            .is_some_and(|(identity, _)| *identity == id)
        {
            Some(id)
        } else {
            self.relocated.get(&id).copied()
        }
    }

    #[inline]
    pub(crate) fn get(&self, id: NodeId) -> Option<&T> {
        self.slot(id).map(|slot| &self.slots[slot].1)
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, id: NodeId) -> Option<&mut T> {
        let slot = self.slot(id)?;
        self.remember(id);
        Some(&mut self.slots[slot].1)
    }

    /// Central native write barrier. Enabling it only for Dom.nodes also covers direct parser
    /// and batch mutation through IndexMut; memo tables and snapshots stay barrier-free.
    pub(crate) fn enable_generations(&mut self) {
        self.generation = Some(Box::new(Generation {
            first_young_id: self.next_id,
            first_young_slot: self.slots.len(),
            remembered: Default::default(),
        }));
    }

    pub(crate) fn remember(&mut self, id: NodeId) {
        if let Some(generation) = &mut self.generation
            && id < generation.first_young_id
        {
            generation.remembered.insert(id);
        }
    }

    pub(crate) fn first_young_id(&self) -> NodeId {
        self.generation.as_ref().map_or(0, |g| g.first_young_id)
    }

    pub(crate) fn is_young(&self, id: NodeId) -> bool {
        id >= self.first_young_id()
    }

    pub(crate) fn young_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        let start = self.generation.as_ref().map_or(0, |g| g.first_young_slot);
        self.slots[start..].iter().map(|(id, _)| *id)
    }

    pub(crate) fn remembered_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.generation
            .iter()
            .flat_map(|g| g.remembered.iter().copied())
    }

    pub(crate) fn finish_generation(&mut self) {
        if let Some(generation) = &mut self.generation {
            generation.first_young_id = self.next_id;
            generation.first_young_slot = self.slots.len();
            generation.remembered.clear();
            if generation.remembered.capacity() > 64 {
                generation.remembered = Default::default();
            }
        }
    }

    /// Minor removal is restricted to the nursery suffix. Swap removal cannot move an old
    /// native node or require scanning/reindexing the old generation. Caches have no generation.
    pub(crate) fn remove(&mut self, id: NodeId) -> Option<T> {
        // Once an identity has retired, filling numeric gaps could recreate cache slots for
        // historical dead IDs. Disable even for a missing key: this cache may have been empty.
        self.dense_gaps = false;
        let slot = self.slot(id)?;
        debug_assert!(
            self.generation
                .as_ref()
                .is_none_or(|g| slot >= g.first_young_slot)
        );
        self.relocated.remove(&id);
        let (_, value) = self.slots.swap_remove(slot);
        if let Some((moved, _)) = self.slots.get(slot) {
            if *moved == slot {
                self.relocated.remove(moved);
            } else {
                self.relocated.insert(*moved, slot);
            }
        }
        Some(value)
    }

    pub(crate) fn shrink_spare(&mut self) {
        if self.slots.capacity() > self.slots.len().saturating_mul(4).max(64) {
            self.slots.shrink_to(self.slots.len().saturating_mul(2));
        }
        if self.relocated.capacity() > self.relocated.len().saturating_mul(4).max(64) {
            self.relocated
                .shrink_to(self.relocated.len().saturating_mul(2));
        }
    }

    pub(crate) fn insert(&mut self, id: NodeId, value: T) -> Option<T> {
        if let Some(slot) = self.slot(id) {
            return Some(std::mem::replace(&mut self.slots[slot].1, value));
        }
        let slot = self.slots.len();
        self.slots.push((id, value));
        if id != slot {
            self.relocated.insert(id, slot);
        }
        None
    }

    /// Memo tables commonly omit document/text nodes. Keep short untouched gaps as default
    /// slots so the original parsed-document lookup remains an array access. Never extend by
    /// an unbounded identity gap, or reintroduce holes after compaction has relocated entries.
    pub(crate) fn insert_dense(
        &mut self,
        id: NodeId,
        value: T,
        mut default: impl FnMut() -> T,
    ) -> Option<T> {
        if self.dense_gaps
            && self.relocated.is_empty()
            && id >= self.slots.len()
            && id - self.slots.len() <= 64
        {
            while self.slots.len() < id {
                self.slots.push((self.slots.len(), default()));
            }
        }
        self.insert(id, value)
    }

    pub(crate) fn allocate(&mut self, value: T) -> NodeId {
        let id = self.next_id;
        // Allocation exhaustion must never become identity aliasing or Number rounding.
        assert!(
            (id as u128) < (1u128 << 53) - 1,
            "DOM identity space exhausted"
        );
        self.next_id = id.checked_add(1).expect("DOM identity space exhausted");
        self.insert(id, value);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        // Clearing memo storage must not reset an arena's identity sequence.
        self.slots = Vec::new();
        self.relocated = FxHashMap::default();
        self.finish_generation();
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().map(|(_, value)| value)
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.slots.iter().map(|(id, _)| *id)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (NodeId, &T)> {
        self.slots.iter().map(|(id, value)| (*id, value))
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId, &mut T) -> bool) {
        let old_len = self.slots.len();
        self.slots.retain_mut(|(id, value)| keep(*id, value));
        if self.slots.len() == old_len {
            return;
        }
        self.dense_gaps = false;
        self.relocated.clear();
        for (slot, (id, _)) in self.slots.iter().enumerate() {
            if *id != slot {
                self.relocated.insert(*id, slot);
            }
        }
        self.shrink_spare();
    }

    pub(crate) fn storage_bytes(&self) -> usize {
        let generation = self.generation.as_ref().map_or(0, |g| {
            std::mem::size_of::<Generation>()
                + g.remembered.capacity() * std::mem::size_of::<NodeId>()
        });
        generation.saturating_add(
            self.slots
                .capacity()
                .saturating_mul(std::mem::size_of::<(NodeId, T)>())
                .saturating_add(
                    self.relocated
                        .capacity()
                        .saturating_mul(std::mem::size_of::<(NodeId, usize)>()),
                ),
        )
    }

    pub(crate) fn has_sparse_storage(&self) -> bool {
        self.relocated.capacity() != 0
            || self
                .generation
                .as_ref()
                .is_some_and(|g| g.remembered.capacity() != 0)
    }
}

impl<T> Index<NodeId> for DenseIdMap<T> {
    type Output = T;
    #[inline]
    fn index(&self, id: NodeId) -> &T {
        self.get(id).expect("live DOM identity")
    }
}

impl<T> IndexMut<NodeId> for DenseIdMap<T> {
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut T {
        self.get_mut(id).expect("live DOM identity")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_compaction_reuses_storage_without_reusing_identity() {
        let mut arena = DenseIdMap::default();
        let permanent = arena.allocate(42);
        let mut retired = Vec::new();
        for _ in 0..100 {
            for value in 0..1000 {
                retired.push(arena.allocate(value));
            }
            arena.retain(|id, _| id == permanent);
            assert_eq!(arena[permanent], 42);
            assert_eq!(arena.len(), 1);
            assert!(arena.slots.capacity() <= 64);
        }
        let newest = arena.allocate(99);
        assert_eq!(arena[newest], 99);
        assert!(retired.iter().all(|&id| arena.get(id).is_none()));
        assert!(newest > *retired.last().unwrap());
    }

    #[test]
    fn arena_relocation_preserves_values_and_identity_stamped_cache_slots() {
        let mut arena = DenseIdMap::default();
        for value in 0..1000 {
            assert_eq!(arena.allocate(value), value);
        }
        arena.retain(|id, _| id % 3 == 0);
        for id in 0..1000 {
            assert_eq!(arena.get(id).copied(), (id % 3 == 0).then_some(id));
        }
        let ids: Vec<_> = arena.ids().collect();
        for id in ids {
            arena[id] += 1;
        }
        let next = arena.allocate(7);
        assert_eq!(next, 1000);
        assert_eq!(arena[next], 7);
        assert_eq!(arena[999], 1000);
    }

    #[test]
    fn arena_dense_cache_gaps_are_bounded_after_sparse_identity_access() {
        let mut cache = DenseIdMap::default();
        cache.insert_dense(3, Some(3), || None);
        cache.insert_dense(7, Some(7), || None);
        assert_eq!(cache.len(), 8);
        assert!(cache.relocated.is_empty());
        assert_eq!(cache.get(2), Some(&None));
        cache.insert_dense(1_000_000, Some(42), || None);
        assert_eq!(cache.len(), 9);
        cache.retain(|id, _| id == 7);
        cache.insert_dense(1_000_001, Some(43), || None);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(7), Some(&Some(7)));
        assert_eq!(cache.get(1_000_001), Some(&Some(43)));
    }

    #[test]
    fn arena_native_nursery_barrier_and_swap_removal_leave_old_prefix_untouched() {
        let mut arena = DenseIdMap::default();
        for i in 0..1000 {
            arena.allocate(i);
        }
        arena.enable_generations();
        // Reserve before saving an address across allocation; old values need not be movable
        // during a minor, but ordinary Vec growth is allowed to relocate backing storage.
        arena.slots.reserve(4);
        let old_address = arena.get(4).unwrap() as *const _;
        let a = arena.allocate(1000);
        let b = arena.allocate(1001);
        arena[4] = 44;
        arena[4] = 45;
        arena[b] = 7;
        assert_eq!(arena.remembered_ids().collect::<Vec<_>>(), [4]);
        assert_eq!(arena.young_ids().collect::<Vec<_>>(), [a, b]);
        assert_eq!(arena.remove(a), Some(1000));
        assert_eq!(arena.get(4).unwrap() as *const _, old_address);
        assert_eq!(arena[b], 7);
        assert_eq!(arena.young_ids().collect::<Vec<_>>(), [b]);
        arena.finish_generation();
        assert_eq!(arena.first_young_id(), 1002);
        assert_eq!(arena.young_ids().count(), 0);
        assert_eq!(arena.remembered_ids().count(), 0);
        let c = arena.allocate(9);
        assert!(c > b);
        assert!(arena.get(a).is_none());
    }
}
