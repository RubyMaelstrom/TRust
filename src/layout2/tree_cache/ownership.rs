//! Cache-local ownership graph for immutable formatting allocations.
//!
//! Several entries can retain the same subtree. Charge each live allocation
//! once, and visit its outgoing edges only on the first retain / final release.
//! This avoids both depth-multiplied storage estimates and a recursive subtree
//! recount on every ancestor insertion. The graph holds identities, not Arcs;
//! cache entries keep the actual allocations alive until release completes.

use super::{BoxNode, Built, Content, Inline, atom_bytes};
use crate::layout2::{memo::box_style_bytes, style::BoxStyle, tree::SharedBox};
use rustc_hash::FxHashMap;
use std::{mem::size_of, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Box(usize),
    Inlines(usize),
    Style(usize),
}

struct Allocation {
    references: usize,
    bytes: usize,
    children: Vec<Key>,
}

pub(super) struct Payload {
    pub(super) bytes: usize,
    roots: Vec<Key>,
}

#[derive(Default)]
pub(super) struct Ownership {
    allocations: FxHashMap<Key, Allocation>,
    bytes: usize,
    #[cfg(test)]
    pub(super) first_retains: usize,
}

impl Ownership {
    pub(super) fn retained_bytes(&self) -> usize {
        self.bytes + self.allocations.capacity() * size_of::<(Key, Allocation)>()
    }

    pub(super) fn clear(&mut self) {
        self.allocations.clear();
        self.bytes = 0;
    }

    pub(super) fn compact(&mut self) {
        self.allocations.shrink_to_fit();
    }

    pub(super) fn retain(&mut self, built: &Built) -> Payload {
        let mut roots = Vec::new();
        let bytes = self.built(built, &mut roots) + roots.capacity() * size_of::<Key>();
        Payload { bytes, roots }
    }

    pub(super) fn release(&mut self, payload: Payload) {
        let mut pending = payload.roots;
        while let Some(key) = pending.pop() {
            let allocation = self.allocations.get_mut(&key).expect("retained allocation");
            allocation.references -= 1;
            if allocation.references == 0 {
                let allocation = self.allocations.remove(&key).unwrap();
                self.bytes -= allocation.bytes;
                pending.extend(allocation.children);
            }
        }
        // Rejected oversized additions must not leave oversized metadata.
        // The four-to-two hysteresis makes shrinking amortized, not per-node.
        if self.allocations.capacity() > self.allocations.len().saturating_mul(4).max(64) {
            self.allocations
                .shrink_to(self.allocations.len().saturating_mul(2));
        }
    }

    fn reused(&mut self, key: Key, edges: &mut Vec<Key>) -> bool {
        edges.push(key);
        if let Some(allocation) = self.allocations.get_mut(&key) {
            allocation.references += 1;
            true
        } else {
            false
        }
    }

    fn allocate(&mut self, key: Key, bytes: usize, children: Vec<Key>) {
        let bytes = bytes + children.capacity() * size_of::<Key>();
        self.bytes += bytes;
        debug_assert!(!self.allocations.contains_key(&key));
        self.allocations.insert(
            key,
            Allocation {
                references: 1,
                bytes,
                children,
            },
        );
        #[cfg(test)]
        {
            self.first_retains += 1;
        }
    }

    fn shared_box(&mut self, b: &SharedBox, edges: &mut Vec<Key>) {
        let key = Key::Box(Arc::as_ptr(b) as usize);
        if self.reused(key, edges) {
            return;
        }
        let mut children = Vec::new();
        let bytes = size_of::<BoxNode>()
            + 2 * size_of::<usize>()
            + box_style_bytes(&b.style)
            + b.marker.as_ref().map_or(0, String::capacity)
            + b.marker_image.as_ref().map_or(0, String::capacity)
            + b.oof.capacity() * size_of::<(usize, SharedBox)>()
            + match &b.content {
                Content::Blocks(boxes) | Content::Flex(boxes) | Content::Grid(boxes) => {
                    self.boxes(boxes, &mut children)
                }
                Content::Inlines(inlines) => {
                    inlines.capacity() * size_of::<Inline>()
                        + inlines
                            .iter()
                            .map(|i| self.inline(i, &mut children))
                            .sum::<usize>()
                }
                Content::Atomic(atom) => atom_bytes(atom),
                Content::Table(table) => {
                    for cell in &table.cells {
                        self.shared_box(&cell.b, &mut children);
                    }
                    size_of::<crate::layout2::tree::TableBox>()
                        + self.boxes(&table.top_captions, &mut children)
                        + self.boxes(&table.bottom_captions, &mut children)
                        + table.col_specs.capacity()
                            * size_of::<Option<crate::layout2::tree::ColSpec>>()
                        + table.cells.capacity() * size_of::<crate::layout2::tree::TableCell>()
                }
            };
        for (_, child) in &b.oof {
            self.shared_box(child, &mut children);
        }
        self.allocate(key, bytes, children);
    }

    fn boxes(&mut self, boxes: &Vec<SharedBox>, children: &mut Vec<Key>) -> usize {
        for b in boxes {
            self.shared_box(b, children);
        }
        boxes.capacity() * size_of::<SharedBox>()
    }

    fn inline(&mut self, inline: &Inline, edges: &mut Vec<Key>) -> usize {
        match inline {
            Inline::Text(text) => text.capacity(),
            Inline::Atom(atom) => atom_bytes(atom),
            Inline::Br => 0,
            Inline::OutOfFlow(b) | Inline::Float(b) | Inline::AtomBox(b) => {
                self.shared_box(b, edges);
                0
            }
            Inline::Box { style, kids, .. } => {
                let key = Key::Style(Arc::as_ptr(style) as usize);
                if !self.reused(key, edges) {
                    self.allocate(
                        key,
                        size_of::<BoxStyle>() + 2 * size_of::<usize>() + box_style_bytes(style),
                        vec![],
                    );
                }
                let key = Key::Inlines(Arc::as_ptr(kids) as *const Inline as usize);
                if !self.reused(key, edges) {
                    let mut children = Vec::new();
                    let bytes = 2 * size_of::<usize>()
                        + kids.len() * size_of::<Inline>()
                        + kids
                            .iter()
                            .map(|i| self.inline(i, &mut children))
                            .sum::<usize>();
                    self.allocate(key, bytes, children);
                }
                0
            }
        }
    }

    fn built(&mut self, built: &Built, roots: &mut Vec<Key>) -> usize {
        match built {
            Built::Block(b) => {
                self.shared_box(b, roots);
                0
            }
            Built::Inline(inline) => self.inline(inline, roots),
            Built::Hoist(children) => {
                children.capacity() * size_of::<Built>()
                    + children.iter().map(|b| self.built(b, roots)).sum::<usize>()
            }
            Built::Skip => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_with(node: usize, content: Content) -> SharedBox {
        Arc::new(BoxNode {
            node,
            style: BoxStyle::anonymous(),
            content,
            marker: None,
            marker_image: None,
            marker_inside: false,
            oof: vec![],
        })
    }

    #[test]
    fn nested_entries_charge_and_visit_each_shared_allocation_once() {
        let mut graph = Ownership::default();
        let mut b = box_with(0, Content::Inlines(vec![Inline::Text("x".repeat(65536))]));
        let mut retained = vec![];
        for depth in 0..256 {
            let built = Built::Block(b.clone());
            let payload = graph.retain(&built);
            retained.push((built, payload));
            if depth < 255 {
                b = box_with(depth + 1, Content::Blocks(vec![b]));
            }
        }
        assert_eq!(graph.first_retains, 256);
        assert_eq!(graph.allocations.len(), 256);
        // Releasing leaf entries does not erase allocations held by ancestors.
        let root = retained.pop().unwrap();
        for (built, payload) in retained {
            graph.release(payload);
            drop(built);
        }
        assert_eq!(graph.allocations.len(), 256);
        let root_only = graph.bytes;
        let duplicate = graph.retain(&root.0);
        assert_eq!(graph.bytes, root_only);
        assert_eq!(graph.first_retains, 256);
        graph.release(root.1);
        assert_eq!(graph.bytes, root_only);
        graph.release(duplicate);
        assert_eq!(graph.bytes, 0);
        assert!(graph.allocations.is_empty());
    }

    #[test]
    fn inline_hoists_share_style_and_children_but_charge_owned_text_separately() {
        let inline = Inline::Box {
            node: 1,
            style: Arc::new(BoxStyle::anonymous()),
            kids: vec![Inline::Text("payload".repeat(1000))].into(),
        };
        let first = Built::Hoist(vec![
            Built::Inline(inline.clone()),
            Built::Inline(Inline::Text("owned".repeat(10))),
        ]);
        let second = first.clone();
        let mut graph = Ownership::default();
        let a = graph.retain(&first);
        let bytes = graph.bytes;
        let b = graph.retain(&second);
        assert_eq!(a.bytes, b.bytes);
        assert!(a.bytes > 50);
        assert_eq!(graph.bytes, bytes);
        assert_eq!(graph.first_retains, 2);
        graph.release(a);
        assert_eq!(graph.bytes, bytes);
        graph.release(b);
        assert_eq!(graph.bytes, 0);
    }
}
