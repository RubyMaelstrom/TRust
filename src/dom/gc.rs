//! Native half of the joint DOM/JavaScript heap.
//!
//! WHATWG DOM snapshot a2331a45, #concept-node-tree, #concept-node-document,
//! #dom-node-parentnode, #dom-node-childnodes, #concept-shadow-tree. Reachable detached children
//! retain ancestors/siblings; ownerDocument is a directed edge (documents do not own detached
//! nodes). HTML e5071a20 #template-contents and DOM #concept-documentfragment-host
//! add the template/fragment relationship in both directions.
use super::*;

impl Dom {
    /// Enable only when a resident JavaScript host takes ownership. Initial parsing and static
    /// layout do not have a native collector and avoid per-allocation lease bookkeeping.
    pub(crate) fn enable_gc_allocation_leases(&mut self) {
        self.gc_allocation_leases = true;
        self.nodes.enable_generations();
    }

    pub(crate) fn first_young_gc_id(&self) -> NodeId {
        self.nodes.first_young_id()
    }

    pub(crate) fn is_young_gc_node(&self, id: NodeId) -> bool {
        self.nodes.is_young(id)
    }

    /// Advance only after successful host tracing and sweeping. Borrowed-host fallbacks must
    /// retain the journal: their JavaScript handles become conservative roots, not proof of death.
    pub(crate) fn finish_gc_generation(&mut self) {
        self.nodes.finish_generation();
    }

    /// Enumerate relationships, not conservative roots. The engine's joint mark follows these
    /// edges together with JS→native wrappers/NodeLists and native→JS platform state.
    pub(crate) fn visit_gc_edges(&self, mut visit: impl FnMut(NodeId, NodeId)) {
        for id in self.nodes.ids() {
            self.visit_node_gc_edges(id, &mut visit);
        }
    }

    pub(crate) fn visit_gc_young_edges(&self, mut visit: impl FnMut(NodeId, NodeId)) {
        for id in self.nodes.young_ids().chain(self.nodes.remembered_ids()) {
            self.visit_node_gc_edges(id, &mut visit);
        }
    }

    fn visit_node_gc_edges(&self, id: NodeId, visit: &mut impl FnMut(NodeId, NodeId)) {
        if let Some(node) = self.nodes.get(id) {
            for target in [
                node.parent,
                node.first_child,
                node.last_child,
                node.prev_sibling,
                node.next_sibling,
                Some(node.owner_document),
            ]
            .into_iter()
            .flatten()
            {
                visit(id, target);
            }
            if let NodeData::Element {
                template_contents: Some(contents),
                ..
            } = &node.data
            {
                visit(id, *contents);
                // The fragment's host participates in DOM pre-insertion validity even though
                // unlike ShadowRoot.host this relationship has no directly exposed getter.
                visit(*contents, id);
            }
        }
        if let Some(&root) = self.shadow_roots.get(&id) {
            visit(id, root);
            visit(root, id);
        }
        if let Some(&host) = self.shadow_hosts.get(&id) {
            visit(id, host);
        }
        if id == DOCUMENT
            && let Some(target) = self.fragment_target
        {
            visit(id, target);
        }
    }

    /// Native tasks may still deliver author-observable events after the last JS wrapper dies.
    /// They keep their targets until the task queue/animation state relinquishes them.
    pub(crate) fn visit_gc_roots(&self, mut visit: impl FnMut(NodeId)) {
        visit(DOCUMENT);
        self.transitions.visit_gc_roots(&mut visit);
        for &(node, _, _) in &self.scroll_changes {
            visit(node);
        }
    }

    pub(crate) fn visit_gc_pending_allocations(&self, mut visit: impl FnMut(NodeId)) {
        for &id in &self.pending_allocations {
            visit(id);
        }
    }

    pub(crate) fn release_gc_allocation(&mut self, id: NodeId) {
        self.pending_allocations.remove(&id);
    }

    pub(crate) fn clear_gc_allocation_leases(&mut self) {
        self.pending_allocations.clear();
        if self.pending_allocations.capacity() > 64 {
            self.pending_allocations = FxHashSet::default();
        }
    }

    /// Retire only nursery identities. Old physical nodes and caches are never walked here;
    /// remaining queue work is proportional to pending native mutations/events, not tree size.
    pub(crate) fn sweep_gc_young_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) -> Vec<NodeId> {
        let nodes = &self.nodes;
        self.pending_style_invalidations
            .retain_nodes(|id| !nodes.is_young(id) || live(id));
        let removed: Vec<_> = self.nodes.young_ids().filter(|&id| !live(id)).collect();
        if removed.is_empty() {
            return removed;
        }
        let dead: FxHashSet<_> = removed.iter().copied().collect();
        let mut changed_popovers = false;
        let mut retired_scope = false;
        let mut retired_slot = false;
        let mut svg_consumers = Vec::new();
        for &id in &removed {
            debug_assert_ne!(id, DOCUMENT);
            retired_slot |= self.tag_name(id) == Some("slot");
            svg_consumers.extend(self.svg_dependencies.get_mut().remove_node(id));
            self.child_lists.get_mut().remove(id);
            self.nodes.remove(id);
            macro_rules! map_remove {
                ($($field:ident),+ $(,)?) => {$(self.$field.remove(&id);)+};
            }
            map_remove!(
                document_content_types,
                document_modes,
                input_values,
                control_selections,
                shadow_roots,
                shadow_hosts,
                shadow_data,
                geometry_dirty_nodes,
                adopted_styles,
                external_sheets,
                cssom_sheets,
                cssom_sheet_versions,
                cssom_inline,
                scroll_state,
                pending_allocations,
                hover_hosts,
                paint_patch_hosts,
                render_clickables,
                hover_chain
            );
            changed_popovers |= self.popover_open.remove(&id);
            self.canvases.get_mut().remove(&id);
            self.container_sizes.get_mut().remove(&id);
            self.container_dependencies.get_mut().remove_node(id);
            self.properties.remove_node(id);
            self.transitions.remove_node(id);
            macro_rules! cache_remove {
                ($($field:ident),+ $(,)?) => {$(self.$field.get_mut().slots.remove(id);)+};
            }
            cache_remove!(
                matched_cache,
                selector_cache,
                class_cache,
                cascaded_cache,
                hidden_cache,
                font_cache,
                font_units_cache,
                decoration_cache
            );
            self.computed_cache.get_mut().1.remove_node(id);
            self.custom_prop_cache.get_mut().1.remove(&id);
            for kind in 0..3 {
                self.serialization_cache.get_mut().1.remove(&(id, kind));
            }
            if let Some((_, generated)) = self.generated_cache.get_mut() {
                generated.content.remove(&(id, 0));
                generated.content.remove(&(id, 1));
                generated.list_items.remove(&id);
            }
            if let Some((_, index)) = self.style_cache.get_mut() {
                retired_scope |= index.scopes.contains_key(&id)
                    || index.font_sets.contains_key(&id)
                    || index.counter_styles.contains_key(&id)
                    || index.properties.contains_key(&id);
                index.slot_assignments.borrow_mut().by_element.remove(&id);
            }
            self.layout_cache.get_mut().invalidate(id);
            self.box_tree_cache.get_mut().invalidate(id);
        }
        self.invalidate_layout_paths(svg_consumers);
        if retired_scope {
            *self.style_cache.get_mut() = None;
            self.matched_cache.get_mut().slots.clear();
            self.selector_cache.get_mut().slots.clear();
        } else if retired_slot {
            // Removing a slot changes flattened assignments. Its old-epoch per-element vectors
            // can contain the retired identity even when their light-DOM subjects remain old.
            if let Some((_, index)) = self.style_cache.get_mut() {
                *index.slot_assignments.borrow_mut() = SlotAssignments::default();
            }
        }
        if changed_popovers {
            self.popover_order.retain(|id| !dead.contains(id));
        }
        self.dirty_nodes.retain(|(id, _)| !dead.contains(id));
        // Scroll and transition events are roots, so a dead target here is a host-tracing bug.
        debug_assert!(
            self.scroll_changes
                .iter()
                .all(|(id, _, _)| !dead.contains(id))
        );
        debug_assert!(!self.fragment_target.is_some_and(|id| dead.contains(&id)));
        if self
            .resource_base_element_cache
            .get()
            .is_some_and(|(_, doc, node)| {
                dead.contains(&doc) || node.is_some_and(|id| dead.contains(&id))
            })
        {
            self.resource_base_element_cache.set(None);
        }
        self.nodes.shrink_spare();
        self.svg_dependencies.get_mut().compact(self.nodes.len());
        macro_rules! shrink {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        shrink!(
            document_content_types,
            document_modes,
            input_values,
            control_selections,
            shadow_roots,
            shadow_hosts,
            shadow_data,
            geometry_dirty_nodes,
            adopted_styles,
            external_sheets,
            cssom_sheets,
            cssom_sheet_versions,
            cssom_inline,
            scroll_state,
            pending_allocations,
            hover_hosts,
            paint_patch_hosts,
            render_clickables,
            hover_chain,
            popover_open,
            popover_order,
            dirty_nodes
        );
        macro_rules! shrink_cache {
            ($($field:ident),+ $(,)?) => {$(self.$field.get_mut().slots.shrink_spare();)+};
        }
        shrink_cache!(
            matched_cache,
            selector_cache,
            class_cache,
            cascaded_cache,
            hidden_cache,
            font_cache,
            font_units_cache,
            decoration_cache
        );
        macro_rules! shrink_ref_map {
            ($map:expr) => {{
                let map = $map;
                if map.capacity() > map.len().saturating_mul(4).max(64) {
                    map.shrink_to(map.len().saturating_mul(2));
                }
            }};
        }
        shrink_ref_map!(self.canvases.get_mut());
        shrink_ref_map!(self.container_sizes.get_mut());
        self.computed_cache.get_mut().1.shrink_spare();
        shrink_ref_map!(&mut self.custom_prop_cache.get_mut().1);
        shrink_ref_map!(&mut self.serialization_cache.get_mut().1);
        if let Some((_, generated)) = self.generated_cache.get_mut() {
            shrink_ref_map!(&mut generated.content);
            shrink_ref_map!(&mut generated.list_items);
        }
        if let Some((_, index)) = self.style_cache.get_mut() {
            shrink_ref_map!(&mut index.slot_assignments.borrow_mut().by_element);
        }
        removed
    }

    /// Called only after joint tracing proves these native identities dead. No JS executes
    /// during sweeping, and borrowed DOMs must be conservatively excluded by the host.
    pub(crate) fn sweep_gc_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) -> Vec<NodeId> {
        self.pending_style_invalidations.retain_nodes(live);
        let removed: Vec<_> = self.nodes.ids().filter(|&id| !live(id)).collect();
        if removed.is_empty() {
            return removed;
        }
        debug_assert!(live(DOCUMENT));
        let mut svg_consumers = Vec::new();
        for &id in &removed {
            svg_consumers.extend(self.svg_dependencies.get_mut().remove_node(id));
        }
        self.nodes.retain(|id, _| live(id));
        let nodes = &self.nodes;
        let valid = |id| nodes.get(id).is_some();
        macro_rules! map {
            ($($field:ident),+ $(,)?) => {$(
                self.$field.retain(|&id, _| valid(id));
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        macro_rules! set {
            ($($field:ident),+ $(,)?) => {$(
                self.$field.retain(|&id| valid(id));
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        map!(
            document_content_types,
            document_modes,
            input_values,
            control_selections,
            shadow_roots,
            shadow_hosts,
            shadow_data,
            geometry_dirty_nodes,
            adopted_styles,
            external_sheets,
            cssom_sheets,
            cssom_sheet_versions,
            cssom_inline,
            scroll_state,
        );
        set!(
            pending_allocations,
            hover_hosts,
            paint_patch_hosts,
            render_clickables,
            hover_chain,
            popover_open
        );
        self.popover_order.retain(|&id| valid(id));
        self.dirty_nodes.retain(|(id, _)| valid(*id));
        self.scroll_changes.retain(|(id, _, _)| valid(*id));
        macro_rules! shrink_vec {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        shrink_vec!(popover_order, dirty_nodes, scroll_changes);
        let canvases = self.canvases.get_mut();
        canvases.retain(|&id, _| valid(id));
        if canvases.capacity() > canvases.len().saturating_mul(4).max(64) {
            canvases.shrink_to(canvases.len().saturating_mul(2));
        }
        let container_sizes = self.container_sizes.get_mut();
        container_sizes.retain(|&id, _| valid(id));
        if container_sizes.capacity() > container_sizes.len().saturating_mul(4).max(64) {
            container_sizes.shrink_to(container_sizes.len().saturating_mul(2));
        }
        self.container_dependencies.get_mut().retain_nodes(&valid);
        self.properties.retain_nodes(&valid);
        self.transitions.retain_nodes(&valid);
        macro_rules! cache {
            ($($field:ident),+ $(,)?) => {$(
                self.$field.get_mut().slots.retain(|id, _| valid(id));
            )+};
        }
        cache!(
            matched_cache,
            selector_cache,
            class_cache,
            cascaded_cache,
            hidden_cache,
            font_cache,
            font_units_cache,
            decoration_cache
        );
        self.child_lists.get_mut().retain(valid);
        self.computed_cache.get_mut().1.retain_nodes(valid);
        let custom = &mut self.custom_prop_cache.get_mut().1;
        custom.retain(|&id, _| valid(id));
        if custom.capacity() > custom.len().saturating_mul(4).max(64) {
            custom.shrink_to(custom.len().saturating_mul(2));
        }
        let serialized = &mut self.serialization_cache.get_mut().1;
        serialized.retain(|(id, _), _| valid(*id));
        if serialized.capacity() > serialized.len().saturating_mul(4).max(64) {
            serialized.shrink_to(serialized.len().saturating_mul(2));
        }
        if let Some((_, generated)) = self.generated_cache.get_mut() {
            generated.content.retain(|(id, _), _| valid(*id));
            generated.list_items.retain(|&id, _| valid(id));
            if generated.content.capacity() > generated.content.len().saturating_mul(4).max(64) {
                generated
                    .content
                    .shrink_to(generated.content.len().saturating_mul(2));
            }
            if generated.list_items.capacity()
                > generated.list_items.len().saturating_mul(4).max(64)
            {
                generated
                    .list_items
                    .shrink_to(generated.list_items.len().saturating_mul(2));
            }
        }
        if self.fragment_target.is_some_and(|id| !valid(id)) {
            self.fragment_target = None;
        }
        if self
            .resource_base_element_cache
            .get()
            .is_some_and(|(_, document, node)| {
                !valid(document) || node.is_some_and(|id| !valid(id))
            })
        {
            self.resource_base_element_cache.set(None);
        }
        if let Some((_, index)) = self.style_cache.get_mut() {
            if index
                .scopes
                .keys()
                .chain(index.font_sets.keys())
                .chain(index.counter_styles.keys())
                .chain(index.properties.keys())
                .any(|&scope| !valid(scope))
            {
                // A removed document/shadow scope's parsed rules/font sets cannot be retained by
                // the current style-index cache. Existing immutable snapshots own their own copy.
                *self.style_cache.get_mut() = None;
                // Matching memos contain indices into a particular rebuilt rule array.
                self.matched_cache.get_mut().slots.clear();
                self.selector_cache.get_mut().slots.clear();
            } else {
                let mut assignments = index.slot_assignments.borrow_mut();
                assignments.by_element.retain(|&id, pairs| {
                    pairs.retain(|(scope, slot)| valid(*scope) && valid(*slot));
                    valid(id) && !pairs.is_empty()
                });
                if assignments.by_element.capacity()
                    > assignments.by_element.len().saturating_mul(4).max(64)
                {
                    let len = assignments.by_element.len();
                    assignments.by_element.shrink_to(len.saturating_mul(2));
                }
            }
        }
        // These independently byte-bounded caches are already invalidated through ancestor
        // mutation dependencies. Drop any remaining entries directly owned by retired identities.
        for &id in &removed {
            self.layout_cache.get_mut().invalidate(id);
            self.box_tree_cache.get_mut().invalidate(id);
        }
        self.invalidate_layout_paths(svg_consumers);
        self.svg_dependencies.get_mut().compact(self.nodes.len());
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(dom: &Dom, extra: &[NodeId]) -> FxHashSet<NodeId> {
        let mut edges: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
        dom.visit_gc_edges(|from, to| edges.entry(from).or_default().push(to));
        let mut pending = extra.to_vec();
        dom.visit_gc_roots(|id| pending.push(id));
        let mut live = FxHashSet::default();
        while let Some(id) = pending.pop() {
            if live.insert(id)
                && let Some(edges) = edges.get(&id)
            {
                pending.extend_from_slice(edges);
            }
        }
        live
    }

    fn minor_mark(dom: &Dom, extra: &[NodeId]) -> (FxHashSet<NodeId>, usize) {
        let mut edges: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
        let mut pending = extra.to_vec();
        let mut emitted = 0;
        dom.visit_gc_young_edges(|from, to| {
            emitted += 1;
            edges.entry(from).or_default().push(to);
            if !dom.is_young_gc_node(from) {
                pending.push(from);
            }
            if !dom.is_young_gc_node(to) {
                pending.push(to);
            }
        });
        dom.visit_gc_roots(|id| {
            if dom.is_young_gc_node(id) {
                pending.push(id);
            }
        });
        dom.visit_gc_pending_allocations(|id| pending.push(id));
        let mut live = FxHashSet::default();
        while let Some(id) = pending.pop() {
            if live.insert(id)
                && let Some(edges) = edges.get(&id)
            {
                pending.extend_from_slice(edges);
            }
        }
        (live, emitted)
    }

    fn minor(dom: &mut Dom, extra: &[NodeId]) -> (Vec<NodeId>, usize) {
        let (live, emitted) = minor_mark(dom, extra);
        let first = dom.first_young_gc_id();
        let removed = dom.sweep_gc_young_nodes(&|id| id < first || live.contains(&id));
        dom.finish_gc_generation();
        (removed, emitted)
    }

    #[test]
    fn native_gc_detached_child_keeps_parent_siblings_and_owner_document() {
        let mut dom = Dom::new();
        let parent = dom.create_element("div");
        let child = dom.create_element("span");
        let sibling = dom.create_text("peer");
        dom.append(parent, child);
        dom.append(parent, sibling);
        let doc = dom.owner_document(child).unwrap();
        let live = mark(&dom, &[child]);
        assert!(live.contains(&parent) && live.contains(&sibling) && live.contains(&doc));
        dom.sweep_gc_nodes(&|id| live.contains(&id));
        assert_eq!(dom.node(child).parent, Some(parent));
        let live = mark(&dom, &[]);
        let retired = dom.sweep_gc_nodes(&|id| live.contains(&id));
        assert_eq!(retired.len(), 3);
        assert!(!dom.is_valid(child));
        let replacement = dom.create_element("aside");
        assert!(replacement > sibling);
        assert!(!dom.is_valid(child));
    }

    #[test]
    fn native_gc_releases_per_node_cache_payloads_and_never_aliases_slots() {
        let mut dom = Dom::new();
        let mut dead = Vec::new();
        for n in 0..200 {
            let id = dom.create_element("div");
            dom.set_attr(id, "class", &format!("old-{n}"));
            dom.class_cache.borrow_mut().put(id, 0, Default::default());
            dom.font_cache.borrow_mut().put(id, 0, 999.0);
            dom.scroll_state.insert(id, ScrollBox::default());
            dead.push(id);
        }
        dom.clear_gc_allocation_leases();
        let live = mark(&dom, &[]);
        dom.sweep_gc_nodes(&|id| live.contains(&id));
        assert_eq!(dom.node_count(), 1);
        assert!(dom.class_cache.borrow().slots.len() <= dom.node_count());
        assert!(dom.font_cache.borrow().slots.len() <= dom.node_count());
        assert!(
            dom.font_cache
                .borrow()
                .slots
                .iter()
                .all(|(_, value)| value.is_none())
        );
        assert!(dom.scroll_state.is_empty());
        let new_id = dom.create_element("p");
        assert!(new_id > *dead.last().unwrap());
        assert!(dom.font_cache.borrow().get(new_id, 0).is_none());
    }

    #[test]
    fn native_gc_shadow_and_template_components_are_retained_bidirectionally() {
        let mut dom = Dom::new();
        let host = dom.create_element("div");
        let shadow = dom.attach_shadow(host);
        let child = dom.create_text("shadow child");
        dom.append(shadow, child);
        let template = dom.create_element("template");
        let contents = dom.content_target(template);
        let template_child = dom.create_comment("template child");
        dom.append(contents, template_child);
        for root in [host, shadow, child] {
            let live = mark(&dom, &[root]);
            assert!([host, shadow, child].iter().all(|id| live.contains(id)));
            assert!(!live.contains(&template));
        }
        for root in [template, contents, template_child] {
            let live = mark(&dom, &[root]);
            assert!(
                [template, contents, template_child]
                    .iter()
                    .all(|id| live.contains(id))
            );
            assert!(!live.contains(&host));
        }
        let live = mark(&dom, &[]);
        assert_eq!(dom.sweep_gc_nodes(&|id| live.contains(&id)).len(), 6);
        assert!(dom.shadow_roots.is_empty());
        assert!(dom.shadow_hosts.is_empty());
        assert!(dom.shadow_data.is_empty());
    }

    #[test]
    fn native_gc_owner_document_is_directed_and_detached_documents_are_collectable() {
        let mut dom = Dom::new();
        let document = dom.create_document("application/xml");
        let tree = dom.create_element("root");
        dom.append(document, tree);
        let detached = dom.create_comment("detached");
        dom.set_owner_document_subtree(detached, document);
        let live = mark(&dom, &[detached]);
        assert!(
            [document, tree, detached]
                .iter()
                .all(|id| live.contains(id))
        );
        let live = mark(&dom, &[document]);
        assert!(!live.contains(&detached));
        dom.sweep_gc_nodes(&|id| live.contains(&id));
        assert!(dom.is_valid(document));
        let live = mark(&dom, &[]);
        dom.sweep_gc_nodes(&|id| live.contains(&id));
        assert!(!dom.is_valid(document));
        assert!(dom.document_content_types.is_empty());
    }

    #[test]
    fn native_gc_churn_reclaims_arena_and_cache_capacity() {
        let mut dom = Dom::new();
        let permanent = dom.create_element("main");
        dom.append(DOCUMENT, permanent);
        let mut previous = None;
        for _ in 0..20 {
            for _ in 0..300 {
                let id = dom.create_element("span");
                dom.append(permanent, id);
                dom.font_cache.borrow_mut().put(id, 0, 17.0);
                dom.detach(id);
                if let Some(previous) = previous {
                    assert!(id > previous);
                }
                previous = Some(id);
            }
            dom.clear_gc_allocation_leases();
            let live = mark(&dom, &[]);
            dom.sweep_gc_nodes(&|id| live.contains(&id));
            assert_eq!(dom.node_count(), 2);
            assert!(dom.font_cache.borrow().slots.len() <= dom.node_count());
            assert!(
                dom.font_cache
                    .borrow()
                    .slots
                    .iter()
                    .all(|(_, value)| value.is_none())
            );
            assert!(dom.nodes.storage_bytes() < std::mem::size_of::<Node>() * 64);
            assert!(dom.font_cache.borrow().slots.storage_bytes() < 4096);
        }
    }

    #[test]
    fn native_gc_allocation_leases_start_at_host_attachment_and_end_at_handoff() {
        let mut dom = Dom::new();
        dom.create_element("parser-only");
        assert!(dom.pending_allocations.is_empty());
        dom.enable_gc_allocation_leases();
        let handed_off = dom.create_text("owned by wrapper");
        let temporary = dom.create_comment("awaiting a wrapper");
        dom.release_gc_allocation(handed_off);
        let mut leases = Vec::new();
        dom.visit_gc_pending_allocations(|id| leases.push(id));
        assert_eq!(leases, [temporary]);
        dom.clear_gc_allocation_leases();
        assert!(dom.pending_allocations.is_empty());
        let next_job = dom.create_fragment();
        assert!(dom.pending_allocations.contains(&next_job));
    }

    #[test]
    fn native_nursery_does_not_trace_or_sweep_unchanged_old_tree_and_caches() {
        let mut dom = Dom::new();
        for n in 0..3000 {
            let id = dom.create_element("p");
            dom.append(DOCUMENT, id);
            dom.font_cache.borrow_mut().put(id, 0, n as f32);
        }
        dom.enable_gc_allocation_leases();
        let count = dom.node_count();
        let old_font_slots = dom.font_cache.borrow().slots.len();
        let unused = dom.create_text("unused");
        dom.clear_gc_allocation_leases();
        let (dead, work) = minor(&mut dom, &[]);
        assert_eq!(dead, [unused]);
        assert_eq!(work, 1, "only nursery node's ownerDocument edge");
        assert_eq!(dom.node_count(), count);
        assert_eq!(dom.font_cache.borrow().slots.len(), old_font_slots);
        let (dead, work) = minor(&mut dom, &[]);
        assert!(dead.is_empty());
        assert_eq!(work, 0, "unchanged old DOM must emit no graph");
    }

    #[test]
    fn native_nursery_current_old_edges_keep_young_targets_but_not_deleted_links() {
        let mut dom = Dom::new();
        let parent = dom.create_element("main");
        dom.append(DOCUMENT, parent);
        let old_child = dom.create_element("p");
        dom.append(parent, old_child);
        dom.enable_gc_allocation_leases();
        let child = dom.create_element("span");
        let dead = dom.create_text("inserted then detached");
        dom.append(parent, child);
        dom.append(parent, dead);
        dom.detach(dead);
        dom.clear_gc_allocation_leases();
        let (removed, _) = minor(&mut dom, &[]);
        assert_eq!(removed, [dead]);
        assert!(dom.is_valid(child));
        let new_parent = dom.create_element("section");
        dom.append(new_parent, old_child);
        dom.clear_gc_allocation_leases();
        assert!(
            minor(&mut dom, &[]).0.is_empty(),
            "old child retains young ancestor"
        );
        assert!(dom.is_valid(new_parent));
        dom.detach(old_child);
        let full = mark(&dom, &[]);
        dom.sweep_gc_nodes(&|id| full.contains(&id));
        dom.finish_gc_generation();
        assert!(
            !dom.is_valid(new_parent),
            "major reclaims old dead components"
        );
    }

    #[test]
    fn native_nursery_shadow_attachment_adoption_and_cached_payload_retirement() {
        let mut dom = Dom::new();
        let host = dom.create_element("div");
        dom.append(DOCUMENT, host);
        dom.enable_gc_allocation_leases();
        let root = dom.attach_shadow(host);
        let child = dom.create_text("retained shadow child");
        dom.append(root, child);
        let unused = dom.create_element("span");
        dom.set_attr(unused, "style", "color:red;--test:123");
        assert_eq!(dom.computed_value(unused, "color").as_deref(), Some("red"));
        dom.font_cache.borrow_mut().put(unused, 0, 77.0);
        dom.text_content(unused);
        dom.clear_gc_allocation_leases();
        let (dead, _) = minor(&mut dom, &[]);
        assert_eq!(dead, [unused]);
        assert!(dom.is_valid(root) && dom.is_valid(child));
        assert!(!dom.computed_cache.borrow().1.contains_node(unused));
        assert!(dom.font_cache.borrow().get(unused, 0).is_none());
        assert!(
            !dom.serialization_cache
                .borrow()
                .1
                .contains_key(&(unused, 0))
        );
        let document = dom.create_document("text/html");
        dom.adopt_node(document, host).unwrap();
        dom.clear_gc_allocation_leases();
        assert!(
            minor(&mut dom, &[]).0.is_empty(),
            "old host's ownerDocument edge is remembered"
        );
        assert!(dom.is_valid(document));
    }

    #[test]
    fn native_nursery_small_repeated_churn_never_recreates_historical_cache_holes() {
        let mut dom = Dom::new();
        dom.enable_gc_allocation_leases();
        for _ in 0..1000 {
            let node = dom.create_element("span");
            dom.font_cache.borrow_mut().put(node, 0, 17.0);
            dom.computed_cache_put(node, 0, Some("payload".repeat(64)));
            dom.clear_gc_allocation_leases();
            assert_eq!(minor(&mut dom, &[]).0, [node]);
            assert_eq!(dom.node_count(), 1);
            assert!(dom.font_cache.borrow().slots.len() <= 1);
            assert!(dom.font_cache.borrow().slots.storage_bytes() < 4096);
            assert!(dom.computed_cache.borrow().1.is_empty());
        }
    }
}
