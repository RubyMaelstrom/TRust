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

/// A row's values and box records as owned data, detached from its nodes,
/// thread and budgets: what a parallel style pass hands to the page thread.
pub(super) struct RowExport {
    slots: Vec<u16>,
    values: Vec<Option<String>>,
    boxes: Vec<(BoxContext, BoxStyle)>,
}

/// A node's own state besides its (possibly shared) row: the contextual
/// values and the node-private records.
pub(super) struct NodeExport {
    contextual: RowExport,
    inline: Option<InlineExport>,
    display: Option<Option<String>>,
}

struct InlineExport {
    epoch: u64,
    clickable: bool,
    parent: InlineStyle,
    base: url::Url,
    value: InlineStyle,
}

impl RowExport {
    /// The populated `(property, value)` pairs.
    pub(super) fn entries(&self) -> impl Iterator<Item = (usize, &Option<String>)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| **slot != MISSING)
            .map(|(property, slot)| (property, &self.values[usize::from(*slot)]))
    }
}

impl NodeExport {
    pub(super) fn contextual(&self) -> &RowExport {
        &self.contextual
    }

    pub(super) fn inline(&self) -> Option<(&InlineStyle, &InlineStyle)> {
        self.inline
            .as_ref()
            .map(|record| (&record.parent, &record.value))
    }

    pub(super) fn display(&self) -> Option<&Option<String>> {
        self.display.as_ref()
    }
}

fn box_record_bytes(context: &BoxContext, value: &BoxStyle) -> usize {
    std::mem::size_of::<BoxRecord>()
        + context.display.as_ref().map_or(0, String::capacity)
        + crate::layout2::box_style_bytes(value)
}

fn inline_record_bytes(base: &url::Url, parent: &InlineStyle, value: &InlineStyle) -> usize {
    std::mem::size_of::<InlineRecord>()
        + base.as_str().len()
        + parent.retained_text_bytes()
        + value.retained_text_bytes()
}

fn display_record_bytes(value: &Option<String>) -> usize {
    std::mem::size_of::<DisplayRecord>() + value.as_ref().map_or(0, String::capacity)
}

#[derive(Default)]
struct NodeRows {
    common: Option<SharedRow>,
    contextual: Row,
    inline: Option<Box<InlineRecord>>,
    display: Option<DisplayRecord>,
}

/// Properties whose value is computed per element (unit guards), never shared
/// through a style-sharing row. Indexed by property, built at compile time so
/// a cache read does not compare property names.
const CONTEXTUAL: [bool; PROPS.len()] = {
    const fn same(a: &str, b: &str) -> bool {
        let (a, b) = (a.as_bytes(), b.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
    let mut table = [false; PROPS.len()];
    let mut i = 0;
    while i < PROPS.len() {
        let name = PROPS[i].name;
        table[i] = same(name, "line-height")
            || same(name, "font-weight")
            || same(name, "-webkit-text-stroke-width");
        i += 1;
    }
    table
};

#[inline]
fn contextual(property: usize) -> bool {
    CONTEXTUAL[property]
}

impl Row {
    fn export(&self) -> RowExport {
        RowExport {
            slots: self.slots.clone(),
            values: self.values.clone(),
            boxes: self
                .boxes
                .iter()
                .flatten()
                .map(|record| (record.context.clone(), record.value.clone()))
                .collect(),
        }
    }

    fn from_export(export: RowExport) -> Self {
        Row {
            slots: export.slots,
            values: export.values,
            ..Row::default()
        }
    }

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

    /// Drop one cached value while keeping the row's identity and its slot
    /// directory dense: the last value moves into the vacated slot, so a
    /// property evicted every animation frame never grows the row.
    fn forget(&mut self, property: usize) {
        let Some(&slot) = self.slots.get(property) else {
            return;
        };
        if slot == MISSING {
            return;
        }
        self.slots[property] = MISSING;
        let removed = self
            .values
            .swap_remove(usize::from(slot))
            .map_or(0, |value| value.capacity());
        let moved = self.values.len() as u16;
        if moved != slot
            && let Some(entry) = self.slots.iter_mut().find(|entry| **entry == moved)
        {
            *entry = slot;
        }
        if let Some(charge) = self.retention.as_mut() {
            charge.resize(0, removed);
        }
    }

    /// Drop every cached value and typed record, keeping the row's identity
    /// (and therefore the style-sharing keys of the rows below it).
    fn clear(&mut self) {
        let removed = self
            .values
            .iter()
            .flatten()
            .map(String::capacity)
            .sum::<usize>();
        self.slots.clear();
        self.values.clear();
        self.boxes = [None, None];
        if let Some(charge) = self.retention.as_mut() {
            charge.resize(0, removed);
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
        let bytes = box_record_bytes(&context, &value);
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
        let bytes = inline_record_bytes(&base, &parent, &value);
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
        let bytes = display_record_bytes(&value);
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

    /// A copy of `row` as owned data.
    pub(super) fn export_row(row: &SharedRow) -> RowExport {
        row.borrow().export()
    }

    /// A copy of `node`'s own state as owned data.
    pub(super) fn export_node(&self, node: NodeId) -> Option<NodeExport> {
        let rows = self.rows.get(node)?;
        Some(NodeExport {
            contextual: rows.contextual.export(),
            inline: rows.inline.as_ref().map(|record| InlineExport {
                epoch: record.epoch,
                clickable: record.clickable,
                parent: record.parent.clone(),
                base: record.base.clone(),
                value: record.value.clone(),
            }),
            display: rows.display.as_ref().map(|record| record.value.clone()),
        })
    }

    /// A row of this cache made from exported data. Its box records are
    /// leased from this cache's record budget like any other.
    pub(super) fn adopt_row(&self, mut export: RowExport) -> SharedRow {
        let boxes = std::mem::take(&mut export.boxes);
        let mut row = Row::from_export(export);
        for (slot, (context, value)) in boxes.into_iter().take(2).enumerate() {
            if let Some(lease) = self
                .record_budget
                .reserve(box_record_bytes(&context, &value))
            {
                row.boxes[slot] = Some(Box::new(BoxRecord {
                    context,
                    value,
                    lease,
                }));
            }
        }
        Rc::new(RefCell::new(row))
    }

    /// Install `node`'s computed state from a parallel style pass: its
    /// (possibly shared) row and its own exported state. Replaces whatever
    /// the node had.
    pub(super) fn adopt_node(
        &mut self,
        node: NodeId,
        common: Option<SharedRow>,
        export: Option<NodeExport>,
    ) {
        let mut rows = NodeRows {
            common,
            ..NodeRows::default()
        };
        if let Some(export) = export {
            rows.contextual = Row::from_export(export.contextual);
            if let Some(record) = export.inline
                && let Some(lease) = self.record_budget.reserve_ahead(inline_record_bytes(
                    &record.base,
                    &record.parent,
                    &record.value,
                ))
            {
                rows.inline = Some(Box::new(InlineRecord {
                    epoch: record.epoch,
                    clickable: record.clickable,
                    parent: record.parent,
                    base: record.base,
                    value: record.value,
                    lease,
                }));
            }
            if let Some(value) = export.display
                && let Some(lease) = self.record_budget.reserve(display_record_bytes(&value))
            {
                rows.display = Some(DisplayRecord { value, lease });
            }
        }
        self.rows.insert_dense(node, rows, NodeRows::default);
    }

    pub(super) fn remove_node(&mut self, node: NodeId) {
        self.rows.remove(node);
    }

    /// CSS Animations 1 #animations: an animation-origin value changed. Only
    /// rows below a privately computed animated element reach here, so the
    /// shared row can be updated in place for every node that uses it.
    pub(super) fn forget(&mut self, node: NodeId, property: usize) {
        let Some(row) = self.rows.get_mut(node) else {
            return;
        };
        if contextual(property) {
            row.contextual.forget(property);
        } else if let Some(common) = &row.common {
            common.borrow_mut().forget(property);
        }
    }

    /// Retire a node's values and records, clearing its (possibly shared)
    /// row in place so a retained style-sharing key cannot return them.
    pub(super) fn clear_node(&mut self, node: NodeId) {
        if let Some(common) = self.rows.get(node).and_then(|row| row.common.as_ref()) {
            common.borrow_mut().clear();
        }
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

    #[test]
    fn animated_evictions_keep_shared_rows_dense_and_their_identity() {
        let mut values = Values::default();
        for property in 0..4 {
            values.insert((1, property), Some(format!("v{property}")));
        }
        let row = values.row(1).unwrap();
        values.forget(1, 1);
        assert_eq!(values.get(&(1, 1)), None);
        assert_eq!(values.get(&(1, 3)), Some(Some("v3".into())));
        // An animated property evicted every frame never grows the row.
        for frame in 0..100 {
            values.insert((1, 1), Some(format!("frame {frame}")));
            values.forget(1, 1);
        }
        assert_eq!(row.borrow().values.len(), 3);
        assert_eq!(values.get(&(1, 0)), Some(Some("v0".into())));
        assert_eq!(values.get(&(1, 2)), Some(Some("v2".into())));
        // Clearing in place retires values for every user of the shared row.
        values.clear_node(1);
        assert!(row.borrow().values.is_empty());
        assert_eq!(values.get(&(1, 0)), None);
    }
}
