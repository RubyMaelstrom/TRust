//! Native live light-tree collection views. DOM a2331a45
//! #concept-collection-live, #dom-node-childnodes, #dom-parentnode-children.
//! Counts change with tree links, not with unrelated attribute/text mutations.
//! A single cursor makes sequential indexed traversal linear without arrays.

use super::*;

const MAX_ROOTS: usize = 4096;

#[derive(Default)]
struct Row {
    counts: [usize; 2],
    cursor: [Option<(usize, NodeId)>; 2],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_collections53_counts_and_cursor_follow_all_tree_operations() {
        let mut dom =
            Dom::parse_document("<main id=r>a<i></i><!--c--><b></b></main><aside id=p></aside>");
        let root = dom.get_by_id("r").unwrap();
        let other = dom.get_by_id("p").unwrap();
        let check = |dom: &Dom| {
            for root in [root, other] {
                for elements in [false, true] {
                    let expected: Vec<_> = dom
                        .child_iter(root)
                        .filter(|&id| included(&dom.nodes[id].data, elements))
                        .collect();
                    assert_eq!(dom.child_collection_len(root, elements), expected.len());
                    for index in (0..expected.len()).rev().chain(0..expected.len()) {
                        assert_eq!(
                            dom.child_collection_item(root, index, elements),
                            Some(expected[index])
                        );
                    }
                    assert_eq!(
                        dom.child_collection_item(root, expected.len(), elements),
                        None
                    );
                }
            }
        };
        check(&dom);
        for _ in 0..40 {
            let child = dom.create_text("x");
            dom.append(root, child);
            check(&dom);
            dom.insert_before(other, child, None);
            check(&dom);
            dom.insert_before(root, child, dom.nodes[root].first_child);
            check(&dom);
            dom.detach(child);
            check(&dom);
        }
        let scanned = dom.child_lists.borrow().scanned;
        assert_eq!(
            scanned, 4,
            "membership updates must not rescan the growing child list"
        );
        let text = dom.create_text("replacement");
        dom.replace_all_children(root, vec![text]);
        check(&dom);
        dom.replace_all_children(root, vec![]);
        check(&dom);
        let document = dom.new_node(NodeData::Document);
        dom.append(root, document);
        check(&dom);
        assert_eq!(
            dom.child_collection_len(root, false),
            0,
            "frame Documents are not DOM children"
        );
    }
}

#[derive(Default)]
pub(super) struct State {
    rows: FxHashMap<NodeId, Row>,
    #[cfg(test)]
    pub(super) scanned: usize,
}

impl State {
    pub(super) fn remove(&mut self, id: NodeId) {
        self.rows.remove(&id);
    }

    pub(super) fn retain(&mut self, valid: impl Fn(NodeId) -> bool) {
        self.rows.retain(|&id, _| valid(id));
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<(NodeId, Row)>()
    }
}

fn included(data: &NodeData, elements: bool) -> bool {
    if elements {
        matches!(data, NodeData::Element { .. })
    } else {
        // Frame Documents are arena ownership links, not light-tree children.
        !matches!(data, NodeData::Document)
    }
}

impl Dom {
    fn with_child_collection<T>(&self, root: NodeId, read: impl FnOnce(&mut Row) -> T) -> T {
        let mut state = self.child_lists.borrow_mut();
        if !state.rows.contains_key(&root) {
            if state.rows.len() >= MAX_ROOTS {
                state.rows.clear();
            }
            let mut row = Row::default();
            for child in self.child_iter(root) {
                let data = &self.nodes[child].data;
                row.counts[0] += usize::from(included(data, false));
                row.counts[1] += usize::from(included(data, true));
                #[cfg(test)]
                {
                    state.scanned += 1;
                }
            }
            state.rows.insert(root, row);
        }
        read(state.rows.get_mut(&root).unwrap())
    }

    pub(crate) fn child_collection_len(&self, root: NodeId, elements: bool) -> usize {
        self.with_child_collection(root, |row| row.counts[usize::from(elements)])
    }

    pub(crate) fn child_collection_item(
        &self,
        root: NodeId,
        index: usize,
        elements: bool,
    ) -> Option<NodeId> {
        self.with_child_collection(root, |row| {
            let kind = usize::from(elements);
            let count = row.counts[kind];
            if index >= count {
                return None;
            }
            let root_node = self.nodes.get(root)?;
            let mut forward = index <= count - 1 - index;
            let mut rank = if forward { 0 } else { count - 1 };
            let mut next = if forward {
                root_node.first_child
            } else {
                root_node.last_child
            };
            if let Some((position, node)) = row.cursor[kind]
                && index.abs_diff(position) < index.abs_diff(rank)
            {
                rank = position;
                next = Some(node);
                forward = index >= position;
            }
            while let Some(id) = next {
                let node = &self.nodes[id];
                if included(&node.data, elements) {
                    if rank == index {
                        row.cursor[kind] = Some((index, id));
                        return Some(id);
                    }
                    if forward {
                        rank += 1;
                    } else {
                        rank -= 1;
                    }
                }
                next = if forward {
                    node.next_sibling
                } else {
                    node.prev_sibling
                };
            }
            None
        })
    }

    pub(super) fn child_collection_changed(
        &mut self,
        parent: NodeId,
        child: NodeId,
        inserted: bool,
    ) {
        let Some(row) = self.child_lists.get_mut().rows.get_mut(&parent) else {
            return;
        };
        let data = &self.nodes[child].data;
        for kind in 0..2 {
            if included(data, kind == 1) {
                if inserted {
                    row.counts[kind] += 1;
                } else {
                    row.counts[kind] -= 1;
                }
            }
        }
        row.cursor = [None; 2];
    }
}
