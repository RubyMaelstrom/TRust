//! The DOM "move" primitive behind `ParentNode.moveBefore()`.
//!
//! WHATWG DOM #concept-node-move and #dom-parentnode-movebefore, HTML
//! #html-element-moving-steps (local snapshots DOM a2331a45, HTML e5071a20,
//! 2026-09-06). A move is neither a removal nor an insertion: it never runs
//! the removing or insertion steps. Focus, form-control state, a child
//! navigable and a script's "already started" flag therefore survive, and the
//! node document cannot change (both positions share a shadow-including
//! root). The arena performs the structural relocation and its rendering
//! bookkeeping; the platform prelude runs the observable steps that follow
//! (live ranges, mutation records, slot assignment, custom element
//! reactions).

use super::*;

impl Dom {
    /// DOM #concept-shadow-including-root.
    pub(crate) fn shadow_including_root(&self, id: NodeId) -> NodeId {
        let mut root = self.tree_scope(id);
        while let Some(&host) = self.shadow_hosts.get(&root) {
            root = self.tree_scope(host);
        }
        root
    }

    /// DOM #concept-node-move steps 1–6 for moving `node` into `new_parent`
    /// before `child`. `foreign_child` reports a reference child that has no
    /// arena node (an Attr), which is never a child of anything. The steps
    /// run in normative order, so the first applicable error wins.
    pub(crate) fn ensure_move_validity(
        &self,
        node: NodeId,
        new_parent: NodeId,
        child: Option<NodeId>,
        foreign_child: bool,
    ) -> Result<(), PreInsertError> {
        use PreInsertError::{HierarchyRequest, NotFound};
        // ParentNode is implemented by Document, DocumentFragment and Element.
        if !matches!(
            self.nodes[new_parent].data,
            NodeData::Document | NodeData::Fragment | NodeData::Element { .. }
        ) {
            return Err(HierarchyRequest);
        }
        // Step 1, which also makes the move preserve connectedness.
        if self.shadow_including_root(new_parent) != self.shadow_including_root(node) {
            return Err(HierarchyRequest);
        }
        // Step 2.
        if self.is_host_including_inclusive_ancestor(node, new_parent) {
            return Err(HierarchyRequest);
        }
        // Step 3.
        if foreign_child || child.is_some_and(|child| self.nodes[child].parent != Some(new_parent))
        {
            return Err(NotFound);
        }
        let new_parent_is_document = matches!(self.nodes[new_parent].data, NodeData::Document);
        match self.nodes[node].data {
            // Step 5: a Text node (including CDATASection) cannot be a
            // document child.
            NodeData::Text(_) | NodeData::CData(_) if new_parent_is_document => {
                Err(HierarchyRequest)
            }
            NodeData::Text(_)
            | NodeData::CData(_)
            | NodeData::Comment(_)
            | NodeData::ProcessingInstruction { .. } => Ok(()),
            // Step 6.
            NodeData::Element { .. } if new_parent_is_document => {
                let is_doctype = |id: NodeId| matches!(self.nodes[id].data, NodeData::Doctype(_));
                let has_element_child = self
                    .child_iter(new_parent)
                    .any(|c| matches!(self.nodes[c].data, NodeData::Element { .. }));
                let doctype_follows = child.is_some_and(|child| {
                    std::iter::successors(self.nodes[child].next_sibling, |&s| {
                        self.nodes[s].next_sibling
                    })
                    .any(is_doctype)
                });
                if has_element_child || child.is_some_and(is_doctype) || doctype_follows {
                    Err(HierarchyRequest)
                } else {
                    Ok(())
                }
            }
            NodeData::Element { .. } => Ok(()),
            // Step 4: only an Element or CharacterData node moves.
            _ => Err(HierarchyRequest),
        }
    }

    /// DOM #concept-node-move for a move already validated by
    /// [`Self::ensure_move_validity`]: steps 7–21's tree surgery and the
    /// arena-side HTML moving steps, without any removing or insertion steps.
    /// Returns the old parent.
    pub(crate) fn move_before(
        &mut self,
        node: NodeId,
        new_parent: NodeId,
        child: Option<NodeId>,
    ) -> Result<NodeId, PreInsertError> {
        self.ensure_move_validity(node, new_parent, child, false)?;
        // Steps 7–8.
        let old_parent = self.nodes[node]
            .parent
            .expect("a validated move has an old parent");
        debug_assert_eq!(
            self.nodes[node].owner_document,
            self.nodes[new_parent].owner_document
        );
        // HTML option moving steps ("update an option's nearest ancestor
        // select"): the select the options leave reruns its selectedness
        // setting algorithm over the states they had, as removal does. The
        // platform updates each option's cached nearest select afterwards.
        let old_select = self.select_losing_options(old_parent, node);
        if let Some(select) = old_select {
            self.materialize_select_states(select);
        }
        // Step 13: remove the node from its old parent's children. Unlike
        // `detach`, no removing steps run: focus stays (HTML's focus fixup is
        // part of the removing steps) and nothing becomes disconnected.
        self.note_moved_style(old_parent, node);
        self.invalidate_style_subtree(node, true);
        self.touch_content(Some(old_parent), &[node]);
        let (prev, next) = (self.nodes[node].prev_sibling, self.nodes[node].next_sibling);
        if let Some(prev) = prev {
            self.nodes[prev].next_sibling = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev_sibling = prev;
        }
        if self.nodes[old_parent].first_child == Some(node) {
            self.nodes[old_parent].first_child = next;
        }
        if self.nodes[old_parent].last_child == Some(node) {
            self.nodes[old_parent].last_child = prev;
        }
        self.nodes[node].parent = None;
        self.nodes[node].prev_sibling = None;
        self.nodes[node].next_sibling = None;
        self.child_collection_changed(old_parent, node, false);
        // HTML img and source moving steps: leaving a picture is a relevant
        // mutation.
        self.note_picture_child_change(old_parent, node);
        // A track element's parent changes in a move too (HTML
        // #sourcing-out-of-band-text-tracks is not limited to insertion and
        // removal).
        self.note_track_parent_change(old_parent, node, false);
        // Steps 18–19: append, or insert before the reference child.
        let previous = match child {
            Some(child) => self.nodes[child].prev_sibling,
            None => self.nodes[new_parent].last_child,
        };
        self.nodes[node].parent = Some(new_parent);
        self.nodes[node].prev_sibling = previous;
        self.nodes[node].next_sibling = child;
        match previous {
            Some(previous) => self.nodes[previous].next_sibling = Some(node),
            None => self.nodes[new_parent].first_child = Some(node),
        }
        match child {
            Some(child) => self.nodes[child].prev_sibling = Some(node),
            None => self.nodes[new_parent].last_child = Some(node),
        }
        self.note_moved_style(new_parent, node);
        self.invalidate_style_subtree(node, true);
        self.touch_content(Some(new_parent), &[node]);
        self.child_collection_changed(new_parent, node, true);
        self.note_picture_child_change(new_parent, node);
        self.note_track_parent_change(new_parent, node, true);
        if let Some(select) = old_select {
            self.run_selectedness_setting_algorithm(select);
        }
        // The focused area stays focused, but `:focus-within` changes on the
        // ancestors it left and joined (Selectors 4 #the-focus-within-pseudo).
        let document = self.nodes[node].owner_document;
        if let Some(focused) = self.focused_area(document)
            && self.is_host_including_inclusive_ancestor(node, focused)
        {
            self.mark();
        }
        Ok(old_parent)
    }

    /// The sheet-set bookkeeping of a move. A moved `style` or `link` keeps
    /// its CSS style sheet: HTML's style block and link processing run on
    /// connection, disconnection and child changes, and a move is none of
    /// those. Its position in the tree-ordered sheet list (and its tree
    /// scope) can change, so the style index is rebuilt. A Text node moving
    /// into or out of a `style` element changes that element's text.
    fn note_moved_style(&mut self, parent: NodeId, child: NodeId) {
        if self.tag_name(parent) == Some("style")
            && matches!(self.nodes[child].data, NodeData::Text(_))
        {
            self.reset_cssom_sheet(parent);
            self.touch_style_at(parent);
        } else if self.tree_mutation_changes_style(parent, child) {
            self.touch_style_at(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(dom: &Dom, root: NodeId) -> Vec<String> {
        dom.children(root)
            .into_iter()
            .map(|c| dom.attr(c, "id").unwrap_or("#").to_string())
            .collect()
    }

    #[test]
    fn move_validity_follows_the_normative_order() {
        // DOM #concept-node-move steps 1–6.
        use PreInsertError::{HierarchyRequest, NotFound};
        let mut dom = Dom::parse_document(
            "<!doctype html><div id=a><span id=b></span><i id=c></i></div><p id=d></p>",
        );
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|id| dom.get_by_id(id).unwrap());
        let detached = dom.create_element("em");
        // Different shadow-including roots (a parentless node included).
        assert_eq!(
            dom.ensure_move_validity(detached, a, None, false),
            Err(HierarchyRequest)
        );
        assert_eq!(
            dom.ensure_move_validity(b, detached, None, false),
            Err(HierarchyRequest)
        );
        // Host-including inclusive ancestor.
        assert_eq!(
            dom.ensure_move_validity(a, b, None, false),
            Err(HierarchyRequest)
        );
        assert_eq!(
            dom.ensure_move_validity(a, a, None, false),
            Err(HierarchyRequest)
        );
        // A reference child of another parent, or an Attr reference.
        assert_eq!(
            dom.ensure_move_validity(b, d, Some(c), false),
            Err(NotFound)
        );
        assert_eq!(dom.ensure_move_validity(b, d, None, true), Err(NotFound));
        // Document children: a second element, a Text node, before a doctype.
        assert_eq!(
            dom.ensure_move_validity(b, DOCUMENT, None, false),
            Err(HierarchyRequest)
        );
        let text = dom.create_text("t");
        dom.append(a, text);
        assert_eq!(
            dom.ensure_move_validity(text, DOCUMENT, None, false),
            Err(HierarchyRequest)
        );
        let comment = dom.create_comment("c");
        dom.append(a, comment);
        let doctype = dom.children(DOCUMENT)[0];
        assert_eq!(
            dom.ensure_move_validity(comment, DOCUMENT, Some(doctype), false),
            Ok(())
        );
        // A shadow root is not an Element or CharacterData node, and a
        // shadow tree shares its host's shadow-including root.
        let root = dom.attach_shadow(a);
        assert_eq!(dom.ensure_move_validity(c, root, None, false), Ok(()));
        assert_eq!(
            dom.ensure_move_validity(root, d, None, false),
            Err(HierarchyRequest)
        );
        assert_eq!(dom.ensure_move_validity(d, root, None, false), Ok(()));
        // A host cannot move into its own shadow tree.
        assert_eq!(
            dom.ensure_move_validity(a, root, None, false),
            Err(HierarchyRequest)
        );
        let _ = &mut dom;
    }

    #[test]
    fn moving_keeps_focus_and_relinks_siblings() {
        let mut dom = Dom::parse_document(
            "<div id=a><input id=x><b id=y></b></div><div id=b><i id=z></i></div>",
        );
        let [a, b, x, y, z] = ["a", "b", "x", "y", "z"].map(|id| dom.get_by_id(id).unwrap());
        dom.set_focused_area(DOCUMENT, Some(x));
        assert_eq!(dom.move_before(x, b, Some(z)), Ok(a));
        assert_eq!(ids(&dom, a), ["y"]);
        assert_eq!(ids(&dom, b), ["x", "z"]);
        assert_eq!(dom.focused_area(DOCUMENT), Some(x), "focus survives a move");
        assert_eq!(dom.move_before(x, b, None), Ok(b));
        assert_eq!(ids(&dom, b), ["z", "x"]);
        assert_eq!(dom.move_before(y, b, Some(z)), Ok(a));
        assert_eq!(ids(&dom, b), ["y", "z", "x"]);
        assert!(dom.children(a).is_empty());
        // Removal clears it, by contrast.
        dom.detach(x);
        assert_eq!(dom.focused_area(DOCUMENT), None);
    }
}
