//! Node-owned computed values (CSS Cascade 5 #computed / #inheriting).
//!
//! Property identity is a small dense index; values are sparse. A compact
//! property-to-slot directory provides constant-time reads without a hash
//! entry for every (node, property). Expiring a node drops only its populated
//! values, not PROPS.len() failed hash removals. Missing and cached None are
//! distinct, and stable node IDs survive native-arena compaction.

use super::style_records::{BoxContext, BoxRecord, Budget, DisplayRecord, InlineRecord};
use super::{NodeId, PROPS, arena::DenseIdMap};
use crate::layout2::{BoxStyle, InlineStyle};
use std::{cell::RefCell, rc::Rc};

const MISSING: u16 = u16::MAX;

#[derive(Default)]
pub(super) struct Row {
    slots: Vec<u16>,
    values: Vec<Option<String>>,
    boxes: [Option<Box<BoxRecord>>; 2],
    retention: Option<Box<super::style_sharing::RowCharge>>,
}

pub(super) type SharedRow = Rc<RefCell<Row>>;

#[derive(Default)]
struct NodeRows {
    common: Option<SharedRow>,
    contextual: Row,
    inline: Option<Box<InlineRecord>>,
    display: Option<DisplayRecord>,
}

fn contextual(property: usize) -> bool {
    matches!(
        PROPS[property].name,
        "line-height" | "font-weight" | "-webkit-text-stroke-width"
    )
}

impl Row {
    fn get(&self, property: usize) -> Option<Option<String>> {
        let slot = *self.slots.get(property)?;
        (slot != MISSING).then(|| self.values[usize::from(slot)].clone())
    }

    fn insert(&mut self, property: usize, value: Option<String>) {
        let old_slots = self.slots.capacity();
        let old_values = self.values.capacity();
        let added = value.as_ref().map_or(0, String::capacity);
        let mut removed = 0;
        if self.slots.len() <= property {
            self.slots.resize(property + 1, MISSING);
        }
        let slot = &mut self.slots[property];
        if *slot == MISSING {
            *slot = self.values.len() as u16;
            self.values.push(value);
        } else {
            removed = self.values[usize::from(*slot)]
                .as_ref()
                .map_or(0, String::capacity);
            self.values[usize::from(*slot)] = value;
        }
        if let Some(charge) = self.retention.as_mut() {
            charge.resize(
                added
                    + (self.slots.capacity() - old_slots) * std::mem::size_of::<u16>()
                    + (self.values.capacity() - old_values) * std::mem::size_of::<Option<String>>(),
                removed,
            );
        }
    }

    pub(super) fn retain_in_graph(&mut self, budget: &super::style_sharing::RowBudget) -> bool {
        // A row can outlive an evicted graph through a node cache. Transfer
        // its charge when a later graph acquires ownership of that same row.
        self.retention = None;
        let bytes = self.allocation_bytes()
            - self
                .boxes
                .iter()
                .flatten()
                .map(|record| record.bytes())
                .sum::<usize>()
            + std::mem::size_of::<super::style_sharing::RowCharge>();
        self.retention = budget.reserve(bytes).map(Box::new);
        self.retention.is_some()
    }

    pub(super) fn allocation_bytes(&self) -> usize {
        std::mem::size_of::<RefCell<Self>>()
            + 2 * std::mem::size_of::<usize>()
            + self.retained_bytes()
    }

    fn retained_bytes(&self) -> usize {
        self.retention.as_ref().map_or(0, |_| {
            std::mem::size_of::<super::style_sharing::RowCharge>()
        }) + self.slots.capacity() * std::mem::size_of::<u16>()
            + self.values.capacity() * std::mem::size_of::<Option<String>>()
            + self
                .values
                .iter()
                .flatten()
                .map(String::capacity)
                .sum::<usize>()
            + self
                .boxes
                .iter()
                .flatten()
                .map(|record| record.bytes())
                .sum::<usize>()
    }
}

#[derive(Default)]
pub(super) struct Values {
    rows: DenseIdMap<NodeRows>,
    record_budget: Budget,
    pub(super) record_computing: bool,
    #[cfg(test)]
    pub(super) box_hits: std::cell::Cell<usize>,
    #[cfg(test)]
    pub(super) inline_hits: std::cell::Cell<usize>,
    #[cfg(test)]
    pub(super) display_hits: std::cell::Cell<usize>,
}

impl Values {
    pub(super) fn box_record(&self, node: NodeId, context: &BoxContext) -> Option<BoxStyle> {
        let row = self.rows.get(node)?.common.as_ref()?.borrow();
        let record = row
            .boxes
            .iter()
            .flatten()
            .find(|record| record.context == *context)?;
        #[cfg(test)]
        self.box_hits.set(self.box_hits.get() + 1);
        Some(record.value.clone())
    }

    pub(super) fn put_box_record(&mut self, node: NodeId, context: BoxContext, value: BoxStyle) {
        let bytes = std::mem::size_of::<BoxRecord>()
            + context.display.as_ref().map_or(0, String::capacity)
            + crate::layout2::box_style_bytes(&value);
        let Some(common) = self.row(node) else { return };
        let mut row = common.borrow_mut();
        // A style shared by many nodes must not accumulate unbounded viewport,
        // font, or UA-context variants. Each entry remains immutable to users.
        if row.boxes[1].is_some() {
            row.boxes[0] = row.boxes[1].take();
        }
        if let Some(lease) = self.record_budget.reserve(bytes) {
            let slot = usize::from(row.boxes[0].is_some());
            row.boxes[slot] = Some(Box::new(BoxRecord {
                context,
                value,
                lease,
            }));
        }
    }

    pub(super) fn inline_record(
        &self,
        node: NodeId,
        epoch: u64,
        clickable: bool,
        parent: &InlineStyle,
        base: &url::Url,
    ) -> Option<InlineStyle> {
        let record = self.rows.get(node)?.inline.as_ref()?;
        if record.epoch != epoch
            || record.clickable != clickable
            || record.parent != *parent
            || record.base != *base
        {
            return None;
        }
        #[cfg(test)]
        self.inline_hits.set(self.inline_hits.get() + 1);
        Some(record.value.clone())
    }

    pub(super) fn put_inline_record(
        &mut self,
        node: NodeId,
        epoch: u64,
        clickable: bool,
        parent: InlineStyle,
        base: url::Url,
        value: InlineStyle,
    ) {
        let Some(row) = self.rows.get_mut(node) else {
            return;
        };
        row.inline = None;
        let bytes = std::mem::size_of::<InlineRecord>()
            + base.as_str().len()
            + parent.retained_text_bytes()
            + value.retained_text_bytes();
        if let Some(lease) = self.record_budget.reserve(bytes) {
            row.inline = Some(Box::new(InlineRecord {
                epoch,
                clickable,
                parent,
                base,
                value,
                lease,
            }));
        }
    }

    pub(super) fn display_record(&self, node: NodeId) -> Option<Option<String>> {
        let record = self.rows.get(node)?.display.as_ref()?;
        #[cfg(test)]
        self.display_hits.set(self.display_hits.get() + 1);
        Some(record.value.clone())
    }

    pub(super) fn put_display_record(&mut self, node: NodeId, value: Option<String>) {
        let Some(row) = self.rows.get_mut(node) else {
            return;
        };
        row.display = None;
        let bytes =
            std::mem::size_of::<DisplayRecord>() + value.as_ref().map_or(0, String::capacity);
        if let Some(lease) = self.record_budget.reserve(bytes) {
            row.display = Some(DisplayRecord { value, lease });
        }
    }

    pub(super) fn get(&self, &(node, property): &(NodeId, usize)) -> Option<Option<String>> {
        let row = self.rows.get(node)?;
        if contextual(property) {
            row.contextual.get(property)
        } else {
            row.common.as_ref()?.borrow().get(property)
        }
    }

    pub(super) fn row(&self, node: NodeId) -> Option<SharedRow> {
        self.rows.get(node).and_then(|row| row.common.clone())
    }

    pub(super) fn has_row(&self, node: NodeId) -> bool {
        self.rows.get(node).is_some_and(|row| row.common.is_some())
    }

    pub(super) fn ensure_row(&mut self, node: NodeId) -> SharedRow {
        if self.rows.get(node).is_none() {
            self.rows
                .insert_dense(node, NodeRows::default(), NodeRows::default);
        }
        self.rows
            .get_mut(node)
            .unwrap()
            .common
            .get_or_insert_with(Default::default)
            .clone()
    }

    pub(super) fn share_row(&mut self, node: NodeId, shared: SharedRow) {
        self.rows.get_mut(node).expect("prepared node row").common = Some(shared);
    }

    pub(super) fn insert(&mut self, (node, property): (NodeId, usize), value: Option<String>) {
        assert!(property < PROPS.len() && PROPS.len() < usize::from(MISSING));
        self.ensure_row(node);
        let row = self.rows.get_mut(node).unwrap();
        if contextual(property) {
            row.contextual.insert(property, value);
        } else {
            row.common
                .as_ref()
                .unwrap()
                .borrow_mut()
                .insert(property, value);
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
        let mut seen = std::collections::HashSet::new();
        self.rows.storage_bytes()
            + self
                .rows
                .iter()
                .map(|row| {
                    row.contextual.retained_bytes()
                        + row.inline.as_ref().map_or(0, |record| record.bytes())
                        + row.display.as_ref().map_or(0, |record| {
                            record.bytes() - std::mem::size_of::<DisplayRecord>()
                        })
                        + row.common.as_ref().map_or(0, |common| {
                            if seen.insert(Rc::as_ptr(common)) {
                                std::mem::size_of::<RefCell<Row>>()
                                    + 2 * std::mem::size_of::<usize>()
                                    + common.borrow().retained_bytes()
                            } else {
                                0
                            }
                        })
                })
                .sum::<usize>()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.rows.iter().all(|row| {
            row.contextual.values.is_empty()
                && row
                    .common
                    .as_ref()
                    .is_none_or(|row| row.borrow().values.is_empty())
        })
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
        assert_eq!(values.get(&(1, PROPS.len() - 1)), Some(None));
        values.insert((1, 0), Some("replacement".into()));
        assert_eq!(values.get(&(1, 0)), Some(Some("replacement".into())));
        values.insert((1_000_000, 0), Some("sparse identity".into()));
        assert!(values.retained_bytes() < 16_384);
        values.remove_node(1);
        assert_eq!(values.get(&(1, 0)), None);
        assert_eq!(
            values.get(&(1_000_000, 0)),
            Some(Some("sparse identity".into()))
        );
        values.retain_nodes(|_| false);
        assert!(values.is_empty());
    }
}
