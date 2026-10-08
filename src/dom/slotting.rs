//! Slot assignment: named and manual slotting, and slot-change signals.
//!
//! WHATWG DOM #finding-slots-and-slotables (#find-a-slot, #find-slotables),
//! #assigning-slotables-and-slots (#assign-slotables,
//! #assign-slotables-for-a-tree), #signaling-slot-change and the slottable's
//! #slottable-manual-slot-assignment; HTML #dom-slot-assign and the slot's
//! #manually-assigned-nodes. Consulted local WHATWG snapshots: DOM a2331a45,
//! HTML e5071a20 (2026-09-06).
//!
//! The arena evaluates assignment lazily from the tree (`find_slottables`),
//! so a slot's assigned nodes always reflect the current tree, as the user
//! agents that implement the standard do. DOM's stored "assigned nodes" list
//! survives only as the assignment each slot last signalled: "assign
//! slottables" compares against it to decide whether to signal a slot change.
//! Both standards note that manual slot assignment can use weak references
//! because script cannot reach it; the collector drops retired identities
//! from these maps instead of tracing them. Slots waiting in the signal list
//! are roots until their `slotchange` event is fired.

use super::*;

/// HTML #manually-assigned-nodes and DOM #slottable-manual-slot-assignment.
#[derive(Default)]
pub(crate) struct ManualSlots {
    /// Each slot's manually assigned nodes, an ordered set.
    pub(super) assigned: FxHashMap<NodeId, Vec<NodeId>>,
    /// Each slottable's manual slot assignment.
    pub(super) slot_of: FxHashMap<NodeId, NodeId>,
}

impl ManualSlots {
    fn retained_bytes(&self) -> usize {
        self.assigned.capacity() * std::mem::size_of::<(NodeId, Vec<NodeId>)>()
            + self
                .assigned
                .values()
                .map(|nodes| nodes.capacity() * std::mem::size_of::<NodeId>())
                .sum::<usize>()
            + self.slot_of.capacity() * std::mem::size_of::<(NodeId, NodeId)>()
    }
}

/// DOM #signal-slot-list and the assigned nodes each slot last signalled.
#[derive(Default)]
pub(crate) struct SlotSignals {
    /// Slot → the assigned nodes it last had; empty lists are not stored.
    known: FxHashMap<NodeId, Vec<NodeId>>,
    /// The signal slots, an ordered set awaiting the mutation observer
    /// microtask (DOM #notify-mutation-observers).
    pending: Vec<NodeId>,
    /// Shadow roots a parser created whose assignments are not yet known.
    parsed_roots: Vec<NodeId>,
}

impl SlotSignals {
    fn retained_bytes(&self) -> usize {
        self.known.capacity() * std::mem::size_of::<(NodeId, Vec<NodeId>)>()
            + self
                .known
                .values()
                .map(|nodes| nodes.capacity() * std::mem::size_of::<NodeId>())
                .sum::<usize>()
            + (self.pending.capacity() + self.parsed_roots.capacity())
                * std::mem::size_of::<NodeId>()
    }
}

impl StyleView<'_> {
    /// DOM #concept-slot: a slot is an HTML `slot` element.
    #[inline]
    pub(super) fn is_slot(&self, id: NodeId) -> bool {
        self.nodes.tag_name(id) == Some("slot")
            && self.nodes.namespace_uri(id) == Some("http://www.w3.org/1999/xhtml")
    }

    /// The first slot in tree order among `shadow`'s descendants whose name
    /// is `name` (DOM #find-a-slot step 6). A child navigable's Document is
    /// an arena child of its container but not part of this tree.
    pub(super) fn first_slot_named(&self, shadow: NodeId, name: &str) -> Option<NodeId> {
        let mut found = None;
        self.visit_tree_descendants(shadow, |id| {
            if self.is_slot(id) && self.attr(id, "name").unwrap_or("") == name {
                found = Some(id);
                return false;
            }
            true
        });
        found
    }

    /// Visit `root`'s descendants in tree order, stopping when `visit`
    /// returns false. Child navigables' Documents are skipped.
    pub(super) fn visit_tree_descendants(
        &self,
        root: NodeId,
        mut visit: impl FnMut(NodeId) -> bool,
    ) {
        let mut stack = Vec::new();
        let mut child = self.nodes.first_child_of(root);
        while let Some(id) = child {
            stack.push(id);
            child = self.nodes.next_sibling(id);
        }
        stack.reverse();
        while let Some(id) = stack.pop() {
            if self.nodes.is_document(id) {
                continue;
            }
            if !visit(id) {
                return;
            }
            let start = stack.len();
            let mut child = self.nodes.first_child(id);
            while let Some(next) = child {
                stack.push(next);
                child = self.nodes.next_sibling(next);
            }
            stack[start..].reverse();
        }
    }

    fn manual_slot_assignment(&self, shadow: NodeId) -> bool {
        self.state
            .shadow_data
            .get(&shadow)
            .is_some_and(|data| data.manual_slot_assignment)
    }

    /// DOM #find-a-slot with `open` false.
    pub(super) fn find_a_slot(&self, slottable: NodeId) -> Option<NodeId> {
        // Steps 1–3.
        let host = self.nodes.parent_of(slottable)?;
        let shadow = self.shadow_root(host)?;
        if !self.nodes.is_slottable(slottable) {
            return None;
        }
        // Step 5: the slot among the shadow root's descendants whose manually
        // assigned nodes contain the slottable. A slottable is in at most one
        // such set (HTML #dom-slot-assign step 3.1); its manual slot
        // assignment names that slot.
        if self.manual_slot_assignment(shadow) {
            let slot = *self.state.manual_slots.slot_of.get(&slottable)?;
            return (self.tree_scope(slot) == shadow).then_some(slot);
        }
        // Step 6: names compare exactly.
        self.first_slot_named(shadow, self.attr(slottable, "slot").unwrap_or(""))
    }

    /// DOM #find-slotables.
    pub(super) fn find_slottables(&self, slot: NodeId) -> Vec<NodeId> {
        // Steps 2–4: the slot's root must be a shadow root.
        let root = self.tree_scope(slot);
        let Some(&host) = self.state.shadow_hosts.get(&root) else {
            return Vec::new();
        };
        if !self.is_slot(slot) {
            return Vec::new();
        }
        // Step 5: the manually assigned nodes that are children of the host.
        if self.manual_slot_assignment(root) {
            return self
                .state
                .manual_slots
                .assigned
                .get(&slot)
                .map(|nodes| {
                    nodes
                        .iter()
                        .copied()
                        .filter(|&node| self.nodes.parent_of(node) == Some(host))
                        .collect()
                })
                .unwrap_or_default();
        }
        // Step 6: each slottable child of the host whose found slot is this
        // one. Every child with this slot's name finds the first slot in tree
        // order with that name, so a later namesake receives nothing.
        let name = self.attr(slot, "name").unwrap_or("");
        if self.first_slot_named(root, name) != Some(slot) {
            return Vec::new();
        }
        self.child_iter(host)
            .filter(|&child| {
                self.nodes.is_slottable(child) && self.attr(child, "slot").unwrap_or("") == name
            })
            .collect()
    }
}

impl Dom {
    /// HTML #dom-slot-assign for `slot` and the converted `nodes` argument
    /// (each an Element or Text). Returns whether a slot change was signalled.
    pub(crate) fn assign_slot_manually(&mut self, slot: NodeId, nodes: &[NodeId]) -> bool {
        if !self.is_valid(slot) {
            return false;
        }
        let mut affected = vec![slot];
        let old = self
            .manual_slots
            .assigned
            .get(&slot)
            .cloned()
            .unwrap_or_default();
        // Step 1: each node of this slot's manually assigned nodes loses its
        // manual slot assignment.
        {
            for node in &old {
                if self.manual_slots.slot_of.get(node) == Some(&slot) {
                    self.manual_slots.slot_of.remove(node);
                }
            }
        }
        // Steps 2–3.
        let mut set: Vec<NodeId> = Vec::with_capacity(nodes.len());
        let mut seen = FxHashSet::default();
        for &node in nodes {
            if !self.is_valid(node) {
                continue;
            }
            // Step 3.1: leave the slot it was manually assigned to (this
            // slot's own set is replaced in step 4).
            if let Some(&other) = self.manual_slots.slot_of.get(&node)
                && other != slot
            {
                if let Some(list) = self.manual_slots.assigned.get_mut(&other) {
                    list.retain(|&assigned| assigned != node);
                    if list.is_empty() {
                        self.manual_slots.assigned.remove(&other);
                    }
                }
                if !affected.contains(&other) {
                    affected.push(other);
                }
            }
            // Steps 3.2–3.3.
            self.manual_slots.slot_of.insert(node, slot);
            if seen.insert(node) {
                set.push(node);
            }
        }
        // Reassigning the same nodes changes nothing (step 1 cleared and
        // step 3 restored the same manual slot assignments).
        if affected.len() == 1 && set == old {
            return false;
        }
        // Step 4.
        if set.is_empty() {
            self.manual_slots.assigned.remove(&slot);
        } else {
            self.manual_slots.assigned.insert(slot, set);
        }
        self.invalidate_slot_distribution(&affected);
        // Step 5 runs "assign slottables for a tree" for this slot's root.
        // Assignment here is evaluated from the tree, so a slot in another
        // tree that lost a node has a new assignment as well; run its tree's
        // steps too, keeping slotchange in step with assignedNodes().
        let before = self.slot_signals.pending.len();
        let mut roots = Vec::new();
        for &slot in &affected {
            let root = self.tree_scope(slot);
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        for root in roots {
            self.assign_slottables_for_root(root);
        }
        self.slot_signals.pending.len() > before
    }

    /// Manual assignment changes the flattened tree without a child-list or
    /// attribute mutation: `::slotted()` matches, flat-tree inheritance and
    /// box construction all follow it (CSS Scoping 1 #flattening).
    fn invalidate_slot_distribution(&mut self, slots: &[NodeId]) {
        let rendered = slots.iter().any(|&slot| self.is_connected(slot));
        self.mark();
        if rendered {
            self.dirty_attributed = false;
            self.unattributed_geometry_change();
        }
    }

    /// DOM #assign-slotables: compare the slot's assignment with the one it
    /// last had and signal a slot change when they differ.
    fn assign_slottables(&mut self, slot: NodeId) {
        let slottables = self.style_view().find_slottables(slot);
        let known = self
            .slot_signals
            .known
            .get(&slot)
            .map_or(&[][..], Vec::as_slice);
        if slottables != known {
            self.signal_slot_change(slot);
        }
        if slottables.is_empty() {
            self.slot_signals.known.remove(&slot);
        } else {
            self.slot_signals.known.insert(slot, slottables);
        }
    }

    /// DOM #assign-slotables-for-a-tree for the tree whose root is `root`.
    /// Only a shadow tree's slots can have slottables. Any other root is a
    /// subtree that has just left a shadow tree, or a slot that did: its
    /// slots that last had slottables are re-assigned (to nothing).
    fn assign_slottables_for_root(&mut self, root: NodeId) {
        let slots: Vec<NodeId> = if self.shadow_hosts.contains_key(&root) {
            let view = self.style_view();
            let mut slots = Vec::new();
            view.visit_tree_descendants(root, |id| {
                if view.is_slot(id) {
                    slots.push(id);
                }
                true
            });
            slots
        } else {
            if self.slot_signals.known.is_empty() {
                return;
            }
            let mut slots: Vec<NodeId> = self
                .slot_signals
                .known
                .keys()
                .copied()
                .filter(|&slot| self.tree_scope(slot) == root)
                .collect();
            // Tree order, which the signal list preserves.
            slots.sort_by(|&a, &b| self.tree_order(a, b));
            slots
        };
        for slot in slots {
            self.assign_slottables(slot);
        }
    }

    /// Document order of two nodes in one tree.
    fn tree_order(&self, a: NodeId, b: NodeId) -> std::cmp::Ordering {
        if a == b {
            return std::cmp::Ordering::Equal;
        }
        let chain = |mut id: NodeId| {
            let mut out = vec![id];
            while let Some(parent) = self.nodes[id].parent {
                out.push(parent);
                id = parent;
            }
            out.reverse();
            out
        };
        let (ca, cb) = (chain(a), chain(b));
        let common = ca.iter().zip(&cb).take_while(|(x, y)| x == y).count();
        match (ca.get(common), cb.get(common)) {
            (None, _) => std::cmp::Ordering::Less,
            (_, None) => std::cmp::Ordering::Greater,
            (Some(&x), Some(&y)) => {
                let mut sibling = self.nodes[x].next_sibling;
                while let Some(next) = sibling {
                    if next == y {
                        return std::cmp::Ordering::Less;
                    }
                    sibling = self.nodes[next].next_sibling;
                }
                std::cmp::Ordering::Greater
            }
        }
    }

    /// DOM #signal-a-slot-change step 1; the caller queues the mutation
    /// observer microtask (step 2) when this returns new signals.
    fn signal_slot_change(&mut self, slot: NodeId) {
        if !self.slot_signals.pending.contains(&slot) {
            self.slot_signals.pending.push(slot);
        }
    }

    /// The slot-assignment steps of DOM's insert, remove, move, replace-all
    /// and attribute-change algorithms, run once the tree mutation at `node`
    /// is complete. `node` is the parent whose children changed (with
    /// `children_changed`), the element whose `slot` or slot `name` changed,
    /// or the root of a subtree that has just been removed. Returns whether a
    /// slot change was signalled; the caller then queues a mutation observer
    /// microtask.
    pub(crate) fn slot_mutation_check(&mut self, node: NodeId, children_changed: bool) -> bool {
        if !self.is_valid(node) {
            return false;
        }
        let before = self.slot_signals.pending.len();
        let root = self.tree_scope(node);
        // DOM #concept-node-insert step 7.4 / #concept-node-remove step 8:
        // a child of a shadow host is (or was) assigned to its shadow tree's
        // slots.
        if let Some(shadow) = self.shadow_root(node)
            && shadow != root
        {
            self.assign_slottables_for_root(shadow);
        }
        // Insert step 7.5 / remove step 9: a slot's fallback content changed
        // while it has no assigned nodes.
        if children_changed
            && self.shadow_hosts.contains_key(&root)
            && self.style_view().is_slot(node)
            && self.style_view().find_slottables(node).is_empty()
        {
            self.signal_slot_change(node);
        }
        // Insert step 7.6 / remove step 10: the tree's slots, including those
        // of an inserted subtree; a removed subtree's own former slots when
        // `node` is its root.
        if self.shadow_hosts.contains_key(&root)
            || (root == node && matches!(self.nodes[node].data, NodeData::Element { .. }))
        {
            self.assign_slottables_for_root(root);
        }
        self.slot_signals.pending.len() > before
    }

    /// The slots waiting for their `slotchange` event, in signal order.
    pub(crate) fn signal_slots(&self) -> &[NodeId] {
        &self.slot_signals.pending
    }

    /// DOM #notify-mutation-observers steps 4–5: take the signal slots.
    #[cfg(test)]
    pub(crate) fn take_signal_slots(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.slot_signals.pending)
    }

    /// Take these slots out of the signal slots.
    pub(crate) fn remove_signal_slots(&mut self, slots: &[NodeId]) {
        if slots.len() == self.slot_signals.pending.len() {
            self.slot_signals.pending.clear();
        } else {
            self.slot_signals
                .pending
                .retain(|slot| !slots.contains(slot));
        }
    }

    /// Record the assignments of shadow trees a parser created. The initial
    /// parse of a navigated Document records them silently, as its slots
    /// had them before any script could listen; markup parsed for script
    /// (`setHTMLUnsafe()`, `parseHTMLUnsafe()`) signals each assignment as
    /// the parser's insertions do (DOM #concept-node-insert step 7.4).
    pub(crate) fn settle_parsed_slot_assignments(&mut self, signal: bool) -> bool {
        let roots = std::mem::take(&mut self.slot_signals.parsed_roots);
        let before = self.slot_signals.pending.len();
        for root in roots {
            if !self.shadow_hosts.contains_key(&root) {
                continue;
            }
            if signal {
                self.assign_slottables_for_root(root);
            } else {
                let view = self.style_view();
                let mut slots = Vec::new();
                view.visit_tree_descendants(root, |id| {
                    if view.is_slot(id) {
                        slots.push(id);
                    }
                    true
                });
                for slot in slots {
                    let nodes = self.style_view().find_slottables(slot);
                    if !nodes.is_empty() {
                        self.slot_signals.known.insert(slot, nodes);
                    }
                }
            }
        }
        self.slot_signals.pending.len() > before
    }

    pub(super) fn parsed_shadow_roots(&self) -> &[NodeId] {
        &self.slot_signals.parsed_roots
    }

    pub(super) fn note_parsed_shadow_root(&mut self, root: NodeId) {
        self.slot_signals.parsed_roots.push(root);
    }

    pub(super) fn slotting_retained_bytes(&self) -> usize {
        self.manual_slots.retained_bytes() + self.slot_signals.retained_bytes()
    }

    /// Slots awaiting `slotchange` stay alive until it is fired.
    pub(super) fn visit_slot_signal_roots(&self, visit: &mut impl FnMut(NodeId)) {
        for &slot in &self.slot_signals.pending {
            visit(slot);
        }
        for &root in &self.slot_signals.parsed_roots {
            visit(root);
        }
    }

    /// Drop weak slotting relationships of retired identities.
    pub(super) fn retain_slotting(&mut self, valid: &dyn Fn(NodeId) -> bool) {
        retain(&mut self.manual_slots, &mut self.slot_signals, valid);
    }
}

/// See `Dom::retain_slotting`; borrows only the slotting state, so a sweep
/// can pass a predicate that borrows the node arena.
pub(super) fn retain(
    manual: &mut ManualSlots,
    signals: &mut SlotSignals,
    valid: &dyn Fn(NodeId) -> bool,
) {
    manual
        .slot_of
        .retain(|&node, slot| valid(node) && valid(*slot));
    manual.assigned.retain(|&slot, nodes| {
        nodes.retain(|&node| valid(node));
        valid(slot) && !nodes.is_empty()
    });
    signals.known.retain(|&slot, nodes| {
        nodes.retain(|&node| valid(node));
        valid(slot) && !nodes.is_empty()
    });
    signals.pending.retain(|&slot| valid(slot));
    signals.parsed_roots.retain(|&root| valid(root));
    macro_rules! shrink {
        ($($map:expr),+) => {$(
            if $map.capacity() > $map.len().saturating_mul(4).max(64) {
                $map.shrink_to($map.len().saturating_mul(2));
            }
        )+};
    }
    shrink!(manual.slot_of, manual.assigned, signals.known);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::SelectorList;

    fn query(dom: &Dom, root: NodeId, selector: &str) -> NodeId {
        dom.query(root, &SelectorList::parse(selector).unwrap(), true)[0]
    }

    fn manual_host(dom: &mut Dom) -> (NodeId, NodeId) {
        let body = dom.get_by_id("body").unwrap();
        let host = dom.create_element("div");
        dom.append(body, host);
        let root = dom
            .attach_shadow_with_options(
                host,
                shadow::ShadowRootData {
                    manual_slot_assignment: true,
                    ..Default::default()
                },
            )
            .unwrap();
        (host, root)
    }

    #[test]
    fn manual_assignment_follows_find_a_slot_and_find_slottables() {
        // DOM #find-a-slot step 5 and #find-slotables step 5; HTML
        // #dom-slot-assign.
        let mut dom = Dom::parse_document("<body id=body></body>");
        let (host, root) = manual_host(&mut dom);
        let first = dom.create_element("slot");
        let second = dom.create_element("slot");
        dom.append(root, first);
        dom.append(root, second);
        let a = dom.create_element("span");
        let b = dom.create_text("b");
        let outside = dom.create_element("i");
        dom.append(host, a);
        dom.append(host, b);
        // Named attributes do not participate in manual mode.
        dom.set_attr(a, "slot", "x");
        assert_eq!(dom.assigned_slot(a), None);
        assert!(dom.slot_assigned_nodes(first).is_empty());
        dom.assign_slot_manually(first, &[b, outside, a, b]);
        // Order and the duplicate follow the argument list; a node that is
        // not a host child stays assigned without being a slottable.
        assert_eq!(dom.slot_assigned_nodes(first), vec![b, a]);
        assert_eq!(dom.assigned_slot(a), Some(first));
        assert_eq!(dom.assigned_slot(outside), None);
        // Inserting a manually assigned node into the host assigns it.
        dom.append(host, outside);
        assert_eq!(dom.slot_assigned_nodes(first), vec![b, outside, a]);
        // Assigning to another slot removes it from the first.
        dom.assign_slot_manually(second, &[a]);
        assert_eq!(dom.slot_assigned_nodes(first), vec![b, outside]);
        assert_eq!(dom.slot_assigned_nodes(second), vec![a]);
        assert_eq!(dom.assigned_slot(a), Some(second));
        // Reassigning the first slot clears its old nodes' assignment.
        dom.assign_slot_manually(first, &[]);
        assert_eq!(dom.assigned_slot(b), None);
        assert!(dom.slot_assigned_nodes(first).is_empty());
        // A slot moved out of the shadow tree has no slottables, and nodes
        // assigned to it find no slot.
        dom.assign_slot_manually(first, &[b]);
        dom.detach(first);
        assert_eq!(dom.assigned_slot(b), None);
        assert!(dom.slot_assigned_nodes(first).is_empty());
        dom.append(root, first);
        assert_eq!(dom.assigned_slot(b), Some(first));
        // The flat tree projects the manual assignment, in the shadow tree's
        // order of slots (`first` was re-appended after `second`).
        assert_eq!(dom.flat_children(host), vec![a, b]);
    }

    #[test]
    fn named_slots_ignore_other_namespaces_and_child_documents() {
        let mut dom = Dom::parse_document(
            r#"<div id=host><template shadowrootmode=open><svg><slot name=x></slot></svg>
               <slot id=real name=x></slot></template><b slot=x id=b>b</b></div>"#,
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.shadow_root(host).unwrap();
        let real = query(&dom, root, "#real");
        let b = dom.get_by_id("b").unwrap();
        assert_eq!(dom.assigned_slot(b), Some(real));
        assert_eq!(dom.slot_assigned_nodes(real), vec![b]);
        let _ = &mut dom;
    }

    #[test]
    fn slot_changes_are_signalled_once_per_differing_assignment() {
        // DOM #assign-slotables step 2 and #signal-a-slot-change.
        let mut dom = Dom::parse_document("<body id=body></body>");
        let body = dom.get_by_id("body").unwrap();
        let host = dom.create_element("div");
        dom.append(body, host);
        let root = dom
            .attach_shadow_with_options(host, shadow::ShadowRootData::default())
            .unwrap();
        let slot = dom.create_element("slot");
        dom.append(root, slot);
        assert!(!dom.slot_mutation_check(root, true));
        let child = dom.create_element("span");
        dom.append(host, child);
        assert!(dom.slot_mutation_check(host, true));
        assert!(!dom.slot_mutation_check(host, true), "no change, no signal");
        // Changing the assignment and changing it back signals once per step;
        // the signal list is a set.
        dom.set_attr(child, "slot", "other");
        assert!(!dom.slot_mutation_check(host, false), "already pending");
        dom.remove_attr(child, "slot");
        assert!(!dom.slot_mutation_check(host, false));
        assert_eq!(dom.take_signal_slots(), vec![slot]);
        // Fallback content of a slot without assigned nodes.
        let empty = dom.create_element("slot");
        dom.set_attr(empty, "name", "none");
        dom.append(root, empty);
        dom.slot_mutation_check(root, true);
        assert!(dom.take_signal_slots().is_empty());
        let fallback = dom.create_text("fallback");
        dom.append(empty, fallback);
        assert!(dom.slot_mutation_check(empty, true));
        assert_eq!(dom.take_signal_slots(), vec![empty]);
        // A removed subtree's slot loses its slottables.
        dom.detach(slot);
        dom.slot_mutation_check(root, true);
        assert!(dom.slot_mutation_check(slot, false));
        assert_eq!(dom.take_signal_slots(), vec![slot]);
        // Manual assignment signals the slot through HTML #dom-slot-assign.
        let (manual, manual_root) = manual_host(&mut dom);
        let manual_slot = dom.create_element("slot");
        dom.append(manual_root, manual_slot);
        let node = dom.create_element("p");
        dom.append(manual, node);
        assert!(!dom.slot_mutation_check(manual, true));
        assert!(dom.assign_slot_manually(manual_slot, &[node]));
        assert!(!dom.assign_slot_manually(manual_slot, &[node]));
        assert_eq!(dom.take_signal_slots(), vec![manual_slot]);
    }

    #[test]
    fn slotted_selectors_follow_manual_assignment() {
        // CSS Scoping 1 #slotted-pseudo matches the slot's assigned nodes.
        let mut dom = Dom::parse_document("<body id=body></body>");
        let (host, root) = manual_host(&mut dom);
        let style = dom.create_element("style");
        dom.append_text(style, "::slotted(b) { color: red }");
        dom.append(root, style);
        let slot = dom.create_element("slot");
        dom.append(root, slot);
        let b = dom.create_element("b");
        dom.append(host, b);
        assert_ne!(dom.computed_value(b, "color").as_deref(), Some("red"));
        dom.assign_slot_manually(slot, &[b]);
        assert_eq!(dom.computed_value(b, "color").as_deref(), Some("red"));
        dom.assign_slot_manually(slot, &[]);
        assert_ne!(dom.computed_value(b, "color").as_deref(), Some("red"));
    }

    #[test]
    fn manual_slotting_is_weak_for_the_collector() {
        let mut dom = Dom::parse_document("<body id=body></body>");
        let (host, root) = manual_host(&mut dom);
        let slot = dom.create_element("slot");
        dom.append(root, slot);
        let kept = dom.create_element("b");
        let dropped = dom.create_element("i");
        dom.append(host, kept);
        dom.assign_slot_manually(slot, &[kept, dropped]);
        dom.retain_slotting(&|id| id != dropped);
        assert_eq!(dom.manual_slots.assigned[&slot], vec![kept]);
        assert!(!dom.manual_slots.slot_of.contains_key(&dropped));
        dom.retain_slotting(&|id| id != slot);
        assert!(dom.manual_slots.assigned.is_empty());
        assert!(dom.manual_slots.slot_of.is_empty());
    }
}
