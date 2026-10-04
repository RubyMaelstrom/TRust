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
    relocated: Relocations,
    next_id: NodeId,
    generation: Option<Box<Generation>>,
    dense_gaps: bool,
}

/// Slots of identities that are not stored at their own index: script-created
/// nodes after any collection, and every entry once compaction has moved it.
///
/// Identities are allocated in increasing order, so most relocated ones form
/// a dense run. A direct table over that run answers lookups with one compact
/// array read. Identities that would make the table sparser than
/// `SPAN_PER_ENTRY` entries per relocation (old survivors, out-of-order cache
/// fills) go to a hash map instead, so storage stays proportional to the
/// relocated entries rather than to the identity space.
#[derive(Clone, Debug, Default)]
struct Relocations {
    /// `table[id - base]` is `slot + 1` for a relocated identity, else 0.
    base: NodeId,
    table: Vec<u32>,
    /// Nonzero `table` entries.
    live: usize,
    /// Relocations outside `base..base + table.len()`; no identity is ever
    /// in both places.
    sparse: FxHashMap<NodeId, usize>,
    /// Bounds of every identity inserted into `sparse` since it was last
    /// empty. The table only grows over ranges outside them.
    sparse_low: NodeId,
    sparse_high: NodeId,
    /// `sparse.len()` after the last rebuild, so trimming does not repeat a
    /// rebuild that could not move entries into the table.
    rebuilt_sparse: usize,
}

impl Relocations {
    const SPAN_PER_ENTRY: usize = 8;
    const SPAN_SLACK: usize = 1024;

    fn dense_enough(span: usize, entries: usize) -> bool {
        span <= entries
            .saturating_mul(Self::SPAN_PER_ENTRY)
            .saturating_add(Self::SPAN_SLACK)
    }

    #[inline]
    fn get(&self, id: NodeId) -> Option<usize> {
        if let Some(&entry) = self.table.get(id.wrapping_sub(self.base)) {
            return (entry != 0).then(|| entry as usize - 1);
        }
        if self.sparse.is_empty() {
            None
        } else {
            self.sparse.get(&id).copied()
        }
    }

    fn is_empty(&self) -> bool {
        self.live == 0 && self.sparse.is_empty()
    }

    /// Whether `low..=high` holds no identity of `sparse`: exactly for a
    /// short range, else by the recorded bounds.
    fn clear_of_sparse(&self, low: NodeId, high: NodeId) -> bool {
        self.sparse.is_empty()
            || high < self.sparse_low
            || low > self.sparse_high
            || (high - low < 16 && (low..=high).all(|id| !self.sparse.contains_key(&id)))
    }

    fn insert_sparse_entry(&mut self, id: NodeId, slot: usize) {
        if self.sparse.is_empty() {
            (self.sparse_low, self.sparse_high) = (id, id);
        } else {
            self.sparse_low = self.sparse_low.min(id);
            self.sparse_high = self.sparse_high.max(id);
        }
        self.sparse.insert(id, slot);
    }

    fn insert_sparse(&mut self, id: NodeId, slot: usize) {
        self.insert_sparse_entry(id, slot);
        // Out-of-order fills can strand a dense run in the hash map; place
        // it again whenever the map has doubled since the last placement.
        if self.wants_rebuild() {
            let mut entries: Vec<_> = self.sparse.iter().map(|(&id, &slot)| (id, slot)).collect();
            entries.extend(
                self.table
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| **entry != 0)
                    .map(|(offset, &entry)| (self.base + offset, entry as usize - 1)),
            );
            self.rebuild(entries.into_iter());
        }
    }

    fn insert(&mut self, id: NodeId, slot: usize) {
        let Ok(entry) = u32::try_from(slot + 1) else {
            self.spill();
            self.insert_sparse_entry(id, slot);
            return;
        };
        let offset = id.wrapping_sub(self.base);
        if let Some(cell) = self.table.get_mut(offset) {
            self.live += usize::from(*cell == 0);
            *cell = entry;
            return;
        }
        if let Some(existing) = self.sparse.get_mut(&id) {
            *existing = slot;
            return;
        }
        if self.table.is_empty() {
            if self.clear_of_sparse(id, id) {
                self.base = id;
                self.table.push(entry);
                self.live = 1;
                return;
            }
        } else if id > self.base {
            let end = self.base + self.table.len();
            if Self::dense_enough(offset + 1, self.live + 1) && self.clear_of_sparse(end, id) {
                self.table.resize(offset + 1, 0);
                self.table[offset] = entry;
                self.live += 1;
                return;
            }
        } else {
            let grow = self.base - id;
            if Self::dense_enough(self.table.len() + grow, self.live + 1)
                && self.clear_of_sparse(id, self.base - 1)
            {
                let mut table = vec![0; grow];
                table.extend_from_slice(&self.table);
                table[0] = entry;
                self.table = table;
                self.base = id;
                self.live += 1;
                return;
            }
        }
        self.insert_sparse(id, slot);
    }

    fn remove(&mut self, id: NodeId) {
        let offset = id.wrapping_sub(self.base);
        match self.table.get_mut(offset) {
            Some(cell) if *cell != 0 => {
                *cell = 0;
                self.live -= 1;
                if self.live == 0 {
                    self.table = Vec::new();
                    self.base = 0;
                    return;
                }
                while self.table.last() == Some(&0) {
                    self.table.pop();
                }
                if offset == 0 {
                    let leading = self.table.iter().take_while(|&&cell| cell == 0).count();
                    if leading >= Self::SPAN_SLACK.min(self.table.len() / 2).max(1) {
                        self.table.drain(..leading);
                        self.base += leading;
                    }
                }
            }
            Some(_) => {}
            None => {
                self.sparse.remove(&id);
            }
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Move every table entry into the hash map.
    fn spill(&mut self) {
        let (base, table) = (self.base, std::mem::take(&mut self.table));
        for (offset, &entry) in table.iter().enumerate() {
            if entry != 0 {
                self.insert_sparse_entry(base + offset, entry as usize - 1);
            }
        }
        self.base = 0;
        self.live = 0;
    }

    /// Replace every relocation: the table covers the longest dense run of
    /// the highest identities (where allocation continues), the rest hash.
    fn rebuild(&mut self, entries: impl Iterator<Item = (NodeId, usize)>) {
        self.clear();
        let mut entries: Vec<_> = entries.collect();
        if entries.is_empty() {
            return;
        }
        entries.sort_unstable_by_key(|&(id, _)| id);
        let high = entries[entries.len() - 1].0;
        // The lowest start of a suffix that is dense enough for the table.
        let start = (0..entries.len())
            .find(|&start| Self::dense_enough(high - entries[start].0 + 1, entries.len() - start))
            .unwrap_or(entries.len() - 1);
        let (sparse, dense) = entries.split_at(start);
        if dense
            .iter()
            .all(|&(_, slot)| u32::try_from(slot + 1).is_ok())
        {
            self.base = dense[0].0;
            self.table = vec![0; high - self.base + 1];
            for &(id, slot) in dense {
                self.table[id - self.base] = slot as u32 + 1;
            }
            self.live = dense.len();
        } else {
            for &(id, slot) in dense {
                self.insert_sparse_entry(id, slot);
            }
        }
        for &(id, slot) in sparse {
            self.insert_sparse_entry(id, slot);
        }
        self.rebuilt_sparse = self.sparse.len();
    }

    /// Whether `rebuild` could release table storage or move entries out of
    /// the hash map.
    fn wants_rebuild(&self) -> bool {
        !Self::dense_enough(self.table.len(), self.live)
            || self.sparse.len()
                > self
                    .rebuilt_sparse
                    .saturating_mul(2)
                    .max(Self::SPAN_SLACK / 16)
    }

    fn shrink_spare(&mut self) {
        if self.table.capacity() > self.table.len().saturating_mul(2).max(64) {
            self.table.shrink_to(self.table.len());
        }
        if self.sparse.capacity() > self.sparse.len().saturating_mul(4).max(64) {
            self.sparse.shrink_to(self.sparse.len().saturating_mul(2));
        }
    }

    fn storage_bytes(&self) -> usize {
        self.table
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>())
            .saturating_add(
                self.sparse
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(NodeId, usize)>()),
            )
    }
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
            relocated: Relocations::default(),
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
            self.relocated.get(id)
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
        self.relocated.remove(id);
        let (_, value) = self.slots.swap_remove(slot);
        if let Some(&(moved, _)) = self.slots.get(slot) {
            if moved == slot {
                self.relocated.remove(moved);
            } else {
                self.relocated.insert(moved, slot);
            }
        }
        Some(value)
    }

    pub(crate) fn shrink_spare(&mut self) {
        if self.slots.capacity() > self.slots.len().saturating_mul(4).max(64) {
            self.slots.shrink_to(self.slots.len().saturating_mul(2));
        }
        if self.relocated.wants_rebuild() {
            self.rebuild_relocations();
        }
        self.relocated.shrink_spare();
    }

    fn shrink_slots(&mut self) {
        if self.slots.capacity() > self.slots.len().saturating_mul(4).max(64) {
            self.slots.shrink_to(self.slots.len().saturating_mul(2));
        }
        self.relocated.shrink_spare();
    }

    fn rebuild_relocations(&mut self) {
        let relocated = self
            .slots
            .iter()
            .enumerate()
            .filter(|(slot, (id, _))| id != slot)
            .map(|(slot, (id, _))| (*id, slot));
        self.relocated.rebuild(relocated);
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
        self.relocated.clear();
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
        self.rebuild_relocations();
        self.shrink_slots();
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
                .saturating_add(self.relocated.storage_bytes()),
        )
    }

    pub(crate) fn has_sparse_storage(&self) -> bool {
        self.relocated.sparse.capacity() != 0
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
    fn relocation_table_and_sparse_fallback_match_a_map_through_churn() {
        let mut arena = DenseIdMap::default();
        let mut model = std::collections::BTreeMap::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for round in 0..400 {
            for _ in 0..random(40) {
                let value = random(1_000_000);
                let id = arena.allocate(value);
                model.insert(id, value);
            }
            // Mostly young deaths, with occasional long-lived survivors.
            let live: Vec<_> = model.keys().copied().collect();
            for _ in 0..random(40).min(live.len()) {
                let id = if random(8) == 0 {
                    live[random(live.len())]
                } else {
                    live[live.len() - 1 - random(live.len().min(48))]
                };
                assert_eq!(arena.remove(id), model.remove(&id));
            }
            match round % 50 {
                17 => arena.shrink_spare(),
                33 => {
                    let keep = random(3);
                    arena.retain(|id, _| id % 3 != keep);
                    model.retain(|id, _| id % 3 != keep);
                }
                _ => {}
            }
            assert_eq!(arena.len(), model.len());
            for id in 0..arena.next_id {
                assert_eq!(arena.get(id), model.get(&id), "round {round} id {id}");
            }
        }
        // Caches fill identities out of order; outliers on both sides of the
        // dense run must not lose or alias entries.
        let mut cache = DenseIdMap::default();
        let mut model = std::collections::BTreeMap::new();
        for round in 0..300 {
            for _ in 0..random(30) {
                let id = match random(4) {
                    0 => random(50_000),
                    _ => 20_000 + round * 20 + random(40),
                };
                let value = random(1_000);
                assert_eq!(
                    cache.insert_dense(id, value, || 0).is_some(),
                    model.contains_key(&id)
                );
                if !model.contains_key(&id) && cache.len() > model.len() + 1 {
                    // Gap filling created default entries; mirror them.
                    for gap in 0..cache.next_id.max(id) {
                        if let Some(&value) = cache.get(gap) {
                            model.entry(gap).or_insert(value);
                        }
                    }
                }
                model.insert(id, value);
            }
            let live: Vec<_> = model.keys().copied().collect();
            for _ in 0..random(20).min(live.len()) {
                let id = live[random(live.len())];
                assert_eq!(cache.remove(id), model.remove(&id));
            }
            if round % 40 == 13 {
                cache.shrink_spare();
            }
            assert_eq!(cache.len(), model.len());
            for (&id, value) in &model {
                assert_eq!(cache.get(id), Some(value), "round {round} id {id}");
            }
            for id in (0..50_000).step_by(97) {
                assert_eq!(cache.get(id), model.get(&id));
            }
        }
        // Survivors spread over a large identity span fall back to hashing,
        // so storage follows the entries rather than the identity space.
        let mut sparse = DenseIdMap::default();
        for _ in 0..64 {
            sparse.allocate(0);
        }
        for _ in 0..50 {
            for _ in 0..10_000 {
                sparse.allocate(1);
            }
            sparse.retain(|id, value| *value == 0 || id % 10_000 == 0);
        }
        assert!(sparse.storage_bytes() < 64 * 1024);
        for id in 0..sparse.next_id {
            let kept = id < 64 || id % 10_000 == 0;
            assert_eq!(sparse.get(id).is_some(), kept);
        }
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
