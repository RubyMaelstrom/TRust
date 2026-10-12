//! Moving a subtree between the arenas of a browsing context group's
//! top-level traversables. Each traversable's Documents live in an arena of
//! their own, while DOM #concept-node-adopt (inserting a node into, or
//! adopting it by, another Document) may cross traversables: a page writing
//! nodes it created into its popup's Document. Node identities are unique
//! across a group's arenas (`Dom::share_identities`), so a transplanted
//! subtree keeps them, and every script wrapper, listener and registration
//! keyed by them stays valid.
//!
//! Afterwards no registry, record or cache of the source arena names a moved
//! node, as if the collector had retired it there (`Dom::forget_nodes`):
//! state that is the nodes' own moves with them, records of work still owed
//! to them are handed over, and the rest is forgotten. Two kinds of record
//! stay, because their steps belong to the source Document: the cancel
//! events of the CSS transitions and animations that ran there, and track
//! element changes of a media element that stays. Their targets live in the
//! other arena, which the host's collector traces them in.
use super::*;

impl Dom {
    /// DOM #concept-node-adopt of `root`, a node of this arena, into
    /// `document`, a Document of `into`, another arena of the group: the
    /// node is removed from its parent here (this arena's removing steps),
    /// its shadow-including inclusive descendants and their template
    /// contents move to `into` with their own state (form control values
    /// and states, canvas bitmaps, shadow roots, style sheets and inline
    /// style declarations, scroll positions, custom element registries, the
    /// "already started" flag), and their node document becomes `document`
    /// (the template contents': its appropriate template contents owner
    /// document), with the adopting steps' registry and image updates. The
    /// insertion that may follow is `into`'s, of a node already adopted.
    ///
    /// Removal destroys the child navigables of the subtree's navigable
    /// containers (HTML #the-iframe-element), so their Documents stay here,
    /// detached and retained, as the platform's destruction would leave
    /// them. Running CSS transitions and animations of the moved elements
    /// are canceled: they are no longer in the document that ran them.
    ///
    /// Returns whether `root` moved. A Document, a shadow root and template
    /// contents (a DocumentFragment whose host is non-null) never move:
    /// adoption throws or does nothing for them.
    pub(crate) fn transplant_subtree(
        &mut self,
        root: NodeId,
        into: &mut Dom,
        document: NodeId,
    ) -> bool {
        if !self.is_valid(root)
            || into.is_valid(root)
            || !into.is_valid(document)
            || !matches!(into.nodes[document].data, NodeData::Document)
            || matches!(self.nodes[root].data, NodeData::Document)
            || self.fragment_host(root).is_some()
        {
            return false;
        }
        // The registry steps of DOM #concept-node-adopt compare with the
        // default registry of the conceptual document the nodes leave.
        let old_default_registry = self.departing_default_registry(root);
        self.detach(root);
        let (moved, child_documents) = self.transplanted_nodes(root);
        for child_document in child_documents {
            self.detach(child_document);
        }
        let departed: FxHashSet<NodeId> = moved.iter().copied().collect();
        self.transitions.cancel_departed(&departed);
        self.animations.cancel_departed(&departed);
        let mut slots_left = false;
        for &id in &moved {
            slots_left |= self.tag_name(id) == Some("slot");
            if let NodeData::Element { name, .. } = &self.nodes[id].data
                && name.local == local_name!("dialog")
                && name.ns == ns!(html)
            {
                into.dialog_elements = into.dialog_elements.saturating_add(1);
            }
            let Some(node) = self.nodes.remove_anywhere(id) else {
                continue;
            };
            // The node document is still this arena's: `into` adopts it below.
            into.nodes.insert_moved(id, node);
            macro_rules! carry {
                ($($table:ident),* $(,)?) => {
                    $(if let Some(value) = self.$table.remove(&id) {
                        into.$table.insert(id, value);
                    })*
                };
            }
            carry!(
                input_values,
                element_states,
                control_selections,
                shadow_roots,
                shadow_hosts,
                adopted_styles,
                external_sheets,
                cssom_sheets,
                cssom_sheet_versions,
                cssom_inline,
                scroll_state,
                marquee_pauses,
            );
            if let Some(data) = self.shadow_data.remove(&id) {
                into.note_copied_shadow_data(&data);
                into.shadow_data.insert(id, data);
            }
            if let Some(sources) = self.properties.adopted_sources.remove(&id) {
                into.properties.adopted_sources.insert(id, sources);
            }
            if let Some(canvas) = self.canvases.get_mut().remove(&id) {
                into.canvases.get_mut().insert(id, canvas);
            }
            if self.started_scripts.remove(&id) {
                into.started_scripts.insert(id);
            }
            // Fullscreen API #removing-steps run in script, which reads the
            // flag where the element now is.
            if self.fullscreen_flags.remove(&id) {
                into.fullscreen_flags.insert(id);
            }
            if into.gc_allocation_leases {
                // Until its wrapper's next trace proves it reachable.
                into.pending_allocations.insert(id);
            }
        }
        self.ce_registries
            .transplant(&mut into.ce_registries, &departed);
        self.transplant_media(into, &departed);
        slotting::transplant(
            &self.manual_slots,
            &self.slot_signals,
            &mut into.manual_slots,
            &mut into.slot_signals,
            &departed,
        );
        self.media_tracks
            .transplant(&mut into.media_tracks, &departed);
        // Work still owed to the moved nodes: an image's update of its image
        // data, a scroll container's scroll event, a top-layer element's
        // removing steps.
        let images = std::mem::take(&mut self.created_images);
        let (images_moved, images_kept): (Vec<_>, Vec<_>) = images
            .into_iter()
            .partition(|(id, _)| departed.contains(id));
        self.created_images = images_kept;
        if !images_moved.is_empty() {
            into.created_images.extend(images_moved);
            into.created_images_generation = into.created_images_generation.wrapping_add(1);
        }
        self.scroll_changes.retain(|&change| {
            let moving = departed.contains(&change.0);
            if moving {
                into.scroll_changes.push(change);
            }
            !moving
        });
        self.top_layer_removed.retain(|&id| {
            let moving = departed.contains(&id);
            if moving && !into.top_layer_removed.contains(&id) {
                into.top_layer_removed.push(id);
            }
            !moving
        });
        // A moved element is not this Document's target element any more,
        // and only this arena's elements can match `:target` here.
        self.fragment_targets
            .retain(|_, target| !departed.contains(target));
        self.pending_style_invalidations
            .retain_nodes(|id| !departed.contains(&id));
        self.forget_nodes(&moved, &departed, slots_left);
        self.svg_image_memo.get_mut().clear();
        self.touch();
        // DOM #concept-node-adopt step 3 in `into`.
        into.set_owner_document_subtree(root, document);
        if let Some(old_default) = old_default_registry {
            let new = into.registry_context(root);
            into.adopt_registries_from(root, old_default, new);
        }
        into.invalidate_style_subtree(root, true);
        into.touch();
        true
    }

    /// DOM #dom-document-adoptnode of `node`, a node of `source` (another
    /// arena of the group), by `document` of this arena: the checks and the
    /// result of [`Dom::adopt_node`] (the old node document, `source`'s), with
    /// the adoption a transplant.
    pub(crate) fn adopt_foreign_node(
        &mut self,
        source: &mut Dom,
        document: NodeId,
        node: NodeId,
    ) -> Result<NodeId, AdoptError> {
        if !self.is_valid(document) || !matches!(self.nodes[document].data, NodeData::Document) {
            return Err(AdoptError::TargetNotDocument);
        }
        if !source.is_valid(node) {
            return Err(AdoptError::InvalidNode);
        }
        if matches!(source.nodes[node].data, NodeData::Document) {
            return Err(AdoptError::Document);
        }
        if source.shadow_hosts.contains_key(&node) {
            return Err(AdoptError::ShadowRoot);
        }
        if source.fragment_host(node).is_some() {
            return Err(AdoptError::HostedFragment);
        }
        let old_document = source.nodes[node].owner_document;
        if !source.transplant_subtree(node, self, document) {
            return Err(AdoptError::InvalidNode);
        }
        Ok(old_document)
    }

    /// The nodes a transplant of `root` moves, its shadow-including
    /// inclusive descendants and the template contents among them (with
    /// theirs), and the Documents of child navigables it leaves behind,
    /// which are arena children of their navigable containers.
    fn transplanted_nodes(&self, root: NodeId) -> (Vec<NodeId>, Vec<NodeId>) {
        let mut moved = Vec::new();
        let mut child_documents = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if id != root && matches!(self.nodes[id].data, NodeData::Document) {
                child_documents.push(id);
                continue;
            }
            moved.push(id);
            let mut child = self.nodes[id].last_child;
            while let Some(c) = child {
                stack.push(c);
                child = self.nodes[c].prev_sibling;
            }
            if let Some(&shadow) = self.shadow_roots.get(&id) {
                stack.push(shadow);
            }
            if let NodeData::Element {
                template_contents: Some(contents),
                ..
            } = &self.nodes[id].data
            {
                stack.push(*contents);
            }
        }
        (moved, child_documents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arenas of an opener's and its popup's Documents, one group.
    fn group(opener: &str, popup: &str) -> (Dom, Dom) {
        let mut source = Dom::parse_document_in_group(opener, None);
        let ids = source.share_identities();
        let into = Dom::parse_document_in_group(popup, Some(ids));
        (source, into)
    }

    /// No node structure, registry or record of `dom` names one of `gone`.
    fn assert_forgotten(dom: &mut Dom, gone: &FxHashSet<NodeId>) {
        let named = |id: &NodeId| gone.contains(id);
        for (id, node) in dom.nodes.entries() {
            assert!(!named(&id), "node {id} is still here");
            let links = [
                node.parent,
                node.first_child,
                node.last_child,
                node.prev_sibling,
                node.next_sibling,
                Some(node.owner_document),
            ];
            assert!(
                !links.iter().flatten().any(named),
                "node {id} links a moved node"
            );
        }
        macro_rules! keys {
            ($($table:ident),+ $(,)?) => {$(
                assert!(!dom.$table.keys().any(named), stringify!($table));
            )+};
        }
        keys!(
            input_values,
            element_states,
            control_selections,
            shadow_roots,
            shadow_hosts,
            shadow_data,
            adopted_styles,
            external_sheets,
            cssom_sheets,
            cssom_sheet_versions,
            cssom_inline,
            scroll_state,
            marquee_pauses,
            geometry_dirty_nodes,
            focused_areas,
        );
        macro_rules! members {
            ($($set:ident),+ $(,)?) => {$(
                assert!(!dom.$set.iter().any(named), stringify!($set));
            )+};
        }
        members!(
            pending_allocations,
            hover_hosts,
            paint_patch_hosts,
            render_clickables,
            hover_chain,
            popover_open,
            modal_dialogs,
            fullscreen_flags,
            started_scripts,
            top_layer,
            top_layer_removed,
        );
        assert!(!dom.shadow_roots.values().any(named));
        assert!(!dom.focused_areas.values().any(named));
        assert!(!dom.fragment_targets.values().any(named));
        assert!(!dom.canvases.borrow().keys().any(named));
        assert!(!dom.dirty_nodes.iter().any(|(id, _)| named(id)));
        assert!(!dom.scroll_changes.iter().any(|(id, _, _)| named(id)));
        assert!(!dom.created_images.iter().any(|(id, _)| named(id)));
        dom.visit_gc_roots(|id| assert!(!named(&id), "root {id} moved"));
        let insertions = dom.take_autofocus_insertions();
        assert!(!insertions.iter().any(named));
        dom.restore_autofocus_insertions(insertions);
        let updates = dom.take_style_block_updates();
        assert!(!updates.iter().any(named));
        dom.restore_style_block_updates(updates);
    }

    #[test]
    fn a_transplant_leaves_nothing_of_its_nodes_behind() {
        // DOM #concept-node-adopt across a group's arenas: the subtree's
        // state goes with it, the work owed to its nodes is handed over,
        // and the source arena keeps no record of them, even for nodes of
        // its old generation.
        let (mut source, mut into) = group(
            "<!doctype html><body><div id=moved>\
             <style id=style>p { color: red }</style>\
             <input id=field>\
             <div id=scroller style='overflow:auto;height:10px'><p>a</p><p>b</p></div>\
             <template id=tpl><b>inert</b></template>\
             <video id=video><track id=track></video>\
             <iframe id=frame></iframe>\
             <div id=host><template shadowrootmode=open><slot></slot></template>\
             <span>slotted</span></div>\
             <dialog id=dialog></dialog>\
             </div><p id=stays>stays</p></body>",
            "<!doctype html><body id=target></body>",
        );
        let id = |dom: &Dom, name: &str| dom.get_by_id(name).unwrap();
        let (moved, style, field, scroller) = (
            id(&source, "moved"),
            id(&source, "style"),
            id(&source, "field"),
            id(&source, "scroller"),
        );
        let (tpl, video, track, frame, host) = (
            id(&source, "tpl"),
            id(&source, "video"),
            id(&source, "track"),
            id(&source, "frame"),
            id(&source, "host"),
        );
        source.install_frame_document(frame, "<!doctype html><p>child</p>", "https://a.test/c");
        let child_document = source.frame_document(frame).unwrap();
        source.enable_gc_allocation_leases();
        into.enable_gc_allocation_leases();
        source.finish_gc_generation();
        assert!(!source.nodes.is_young(moved));
        // Records that name moved nodes.
        source.set_style_load_recording(true);
        source.set_autofocus_recording(true);
        let text = source.create_text(" b { color: blue }");
        source.append(style, text);
        let autofocus = source.create_element("input");
        source.set_attr(autofocus, "autofocus", "");
        source.append(moved, autofocus);
        let image = source.create_element("img");
        source.append(moved, image);
        source.note_created_image(image);
        source.set_input_value(field, "typed", false);
        assert!(source.set_scroll_pos(scroller, 5.0, 0.0, true));
        assert!(source.has_style_block_updates() && source.has_autofocus_insertions());
        assert!(source.has_track_changes());
        let contents = match &source.nodes[tpl].data {
            NodeData::Element {
                template_contents: Some(contents),
                ..
            } => *contents,
            _ => unreachable!("a template"),
        };
        let shadow = source.shadow_root(host).unwrap();
        let mut gone = FxHashSet::default();
        source.visit_shadow_including_subtree(moved, |node| {
            gone.insert(node);
            true
        });
        gone.remove(&child_document);
        gone.extend(source.descendants(contents).chain([contents]));
        gone.retain(|&node| source.owner_document(node) != Some(child_document) || node == frame);
        let document = into.document();
        assert!(source.transplant_subtree(moved, &mut into, document));

        assert_forgotten(&mut source, &gone);
        for &node in &gone {
            assert!(into.is_valid(node), "{node} did not arrive");
        }
        // Node documents: `into`'s Document, the template contents its
        // appropriate template contents owner document.
        assert_eq!(into.owner_document(moved), Some(document));
        assert_eq!(into.owner_document(shadow), Some(document));
        let inert = into.owner_document(contents).unwrap();
        assert!(inert != document && into.is_template_contents_owner(inert));
        // State that is the nodes' own.
        assert_eq!(into.input_value(field), "typed");
        assert_eq!(into.shadow_root(host), Some(shadow));
        assert_eq!(into.dialog_elements, 1);
        // Work still owed to them.
        assert_eq!(into.take_scroll_changes(), [(scroller, 5.0, 0.0)]);
        assert!(into.created_images.iter().any(|&(node, _)| node == image));
        assert_eq!(into.take_track_changes(), [(track, video, true)]);
        assert!(!source.has_track_changes());
        // The child navigable's Document stays, detached and retained.
        assert!(source.is_valid(child_document));
        assert_eq!(source.node(child_document).parent, None);
        assert_eq!(into.frame_document(frame), None);
        // The arenas stay sound for the collector and further mutation.
        let young: Vec<_> = source.nodes.young_ids().collect();
        assert!(young.iter().all(|&node| source.nodes.is_young(node)));
        assert_eq!(
            source
                .nodes
                .entries()
                .filter(|(node, _)| source.nodes.is_young(*node))
                .count(),
            young.len()
        );
        let target = into.get_by_id("target").unwrap();
        into.append(target, moved);
        // Moved identities are this arena's old generation from its next one
        // on, by identity as by slot.
        into.finish_gc_generation();
        assert_eq!(into.nodes.young_ids().count(), 0);
        assert!(gone.iter().all(|&node| !into.nodes.is_young(node)));
        let fresh = into.create_element("p");
        assert!(into.nodes.is_young(fresh) && fresh > *gone.iter().max().unwrap());
        assert!(into.is_connected(field));
        let stays = id(&source, "stays");
        let copy = source.create_element("p");
        source.append(stays, copy);
        assert!(source.is_connected(copy));
    }

    #[test]
    fn running_transitions_of_departing_elements_are_canceled_where_they_ran() {
        // CSS Transitions 1 #starting: an element no longer in the document
        // has its running transitions canceled; the transitioncancel event
        // is the source Document's, whose timeline ran the transition.
        let (mut source, mut into) = group(
            "<!doctype html><style>#box{width:0px;transition:width 1s linear}\
             #box.on{width:100px}</style><div id=box></div>",
            "<!doctype html><body></body>",
        );
        source.set_viewport_px(640., 480.);
        let element = source.get_by_id("box").unwrap();
        source.update_css_transitions(0.);
        source.set_attr(element, "class", "on");
        source.update_css_transitions(0.5);
        assert!(source.css_transitions_active());
        let _ = source.take_css_transition_events();
        let document = into.document();
        assert!(source.transplant_subtree(element, &mut into, document));
        assert!(!source.css_transitions_active());
        let events = source.take_css_transition_events();
        assert!(
            events
                .iter()
                .any(|(target, property, kind, _, _)| *target == element
                    && *property == "width"
                    && *kind == "transitioncancel"),
            "{events:?}"
        );
        source.update_css_transitions(1.);
        assert!(source.take_css_transition_events().is_empty());
    }

    #[test]
    fn foreign_nodes_are_validated_where_they_would_be_inserted() {
        // DOM #concept-node-ensure-pre-insertion-validity reads only the
        // inserted node's type and children, which its own arena describes;
        // the move happens only once the insertion is valid.
        let (mut source, into) = group(
            "<!doctype html><body><div id=div></div></body>",
            "<!doctype html><body id=body></body>",
        );
        let div = source.get_by_id("div").unwrap();
        let fragment = source.create_fragment();
        let (a, b) = (source.create_element("a"), source.create_element("b"));
        source.append(fragment, a);
        source.append(fragment, b);
        let text = source.create_text("t");
        let check = |dom: &Dom, node: InsertedNode, parent: NodeId| {
            dom.ensure_pre_insert_validity_of(node.foreign(), parent, None, &|_| false)
        };
        let (document, body) = (into.document(), into.get_by_id("body").unwrap());
        let html = source.inserted_node(div);
        assert_eq!(
            check(&into, html, document),
            Err(PreInsertError::HierarchyRequest)
        );
        assert_eq!(check(&into, html, body), Ok(()));
        let pair = source.inserted_node(fragment);
        assert_eq!(
            check(&into, pair, document),
            Err(PreInsertError::HierarchyRequest)
        );
        assert_eq!(check(&into, pair, body), Ok(()));
        let text = source.inserted_node(text);
        assert_eq!(
            check(&into, text, document),
            Err(PreInsertError::HierarchyRequest)
        );
        assert!(source.is_valid(div) && !into.is_valid(div));
    }

    #[test]
    fn template_contents_stay_with_their_template() {
        // DOM #dom-document-adoptnode step 3 returns a DocumentFragment
        // whose host is non-null without adopting it, in one arena or across
        // arenas of a group.
        let (mut source, mut into) = group(
            "<!doctype html><template id=t><b>x</b></template>",
            "<!doctype html><body></body>",
        );
        let template = source.get_by_id("t").unwrap();
        let contents = match &source.nodes[template].data {
            NodeData::Element {
                template_contents: Some(contents),
                ..
            } => *contents,
            _ => unreachable!("a template"),
        };
        assert_eq!(source.fragment_host(contents), Some(template));
        let owner = source.owner_document(contents);
        let document = source.document();
        assert_eq!(
            source.adopt_node(document, contents),
            Err(AdoptError::HostedFragment)
        );
        let target = into.document();
        assert_eq!(
            into.adopt_foreign_node(&mut source, target, contents),
            Err(AdoptError::HostedFragment)
        );
        assert!(!source.transplant_subtree(contents, &mut into, target));
        assert_eq!(source.owner_document(contents), owner);
        assert!(source.is_valid(contents) && !into.is_valid(contents));
        // A fragment that is no template's moves.
        let fragment = source.create_fragment();
        assert_eq!(source.fragment_host(fragment), None);
        assert_eq!(
            into.adopt_foreign_node(&mut source, target, fragment),
            Ok(document)
        );
        assert_eq!(into.owner_document(fragment), Some(target));
    }
}
