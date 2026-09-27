//! Node-owned computed values (CSS Cascade 5 #computed / #inheriting).
//!
//! Property identity is a small dense index; values are sparse. A compact
//! property-to-slot directory provides constant-time reads without a hash
//! entry for every (node, property). Expiring a node drops only its populated
//! values, not PROPS.len() failed hash removals. Missing and cached None are
//! distinct, and stable node IDs survive native-arena compaction.

use super::{NodeId, PROPS, arena::DenseIdMap};

const MISSING: u16 = u16::MAX;

#[derive(Default)]
struct Row {
    slots: Vec<u16>,
    values: Vec<Option<String>>,
}

#[derive(Default)]
pub(super) struct Values {
    rows: DenseIdMap<Row>,
}

impl Values {
    pub(super) fn get(&self, &(node, property): &(NodeId, usize)) -> Option<&Option<String>> {
        let row = self.rows.get(node)?;
        let slot = *row.slots.get(property)?;
        (slot != MISSING).then(|| &row.values[usize::from(slot)])
    }

    pub(super) fn insert(&mut self, (node, property): (NodeId, usize), value: Option<String>) {
        assert!(property < PROPS.len() && PROPS.len() < usize::from(MISSING));
        if self.rows.get(node).is_none() {
            // Preserve direct node-index access in ordinary parsed documents.
            // The arena only fills short gaps and stops doing so after
            // retirement/compaction; sparse historical IDs cannot grow it.
            self.rows.insert_dense(node, Row::default(), Row::default);
        }
        let row = self.rows.get_mut(node).unwrap();
        if row.slots.len() <= property {
            row.slots.resize(property + 1, MISSING);
        }
        let slot = &mut row.slots[property];
        if *slot == MISSING {
            *slot = row.values.len() as u16;
            row.values.push(value);
        } else {
            row.values[usize::from(*slot)] = value;
        }
    }

    pub(super) fn remove_node(&mut self, node: NodeId) {
        self.rows.remove(node);
    }

    pub(super) fn clear(&mut self) {
        self.rows.clear();
    }

    pub(super) fn retain_nodes(&mut self, valid: impl Fn(NodeId) -> bool) {
        self.rows.retain(|node, _| valid(node));
    }

    pub(super) fn shrink_spare(&mut self) {
        self.rows.shrink_spare();
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.rows.storage_bytes()
            + self
                .rows
                .iter()
                .map(|row| {
                    row.slots.capacity() * std::mem::size_of::<u16>()
                        + row.values.capacity() * std::mem::size_of::<Option<String>>()
                        + row
                            .values
                            .iter()
                            .flatten()
                            .map(String::capacity)
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.rows.iter().all(|row| row.values.is_empty())
    }

    #[cfg(test)]
    pub(super) fn contains_node(&self, node: NodeId) -> bool {
        self.rows.get(node).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_properties_distinguish_missing_none_and_replaced_values() {
        let mut values = Values::default();
        values.insert((1, PROPS.len() - 1), None);
        values.insert((1, 0), Some("first".into()));
        assert_eq!(values.get(&(1, 1)), None);
        assert_eq!(values.get(&(1, PROPS.len() - 1)), Some(&None));
        values.insert((1, 0), Some("replacement".into()));
        assert_eq!(values.get(&(1, 0)), Some(&Some("replacement".into())));
        values.insert((1_000_000, 0), Some("sparse identity".into()));
        assert!(values.retained_bytes() < 16_384);
        values.remove_node(1);
        assert_eq!(values.get(&(1, 0)), None);
        assert_eq!(
            values.get(&(1_000_000, 0)),
            Some(&Some("sparse identity".into()))
        );
        values.retain_nodes(|_| false);
        assert!(values.is_empty());
    }
}
