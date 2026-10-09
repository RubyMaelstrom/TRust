//! The arena side of HTML's autofocus processing (local whatwg/html
//! e5071a20, 2026-09-04, #the-autofocus-attribute): "when an element with
//! the autofocus attribute specified is inserted into a document" (#insert-
//! an-element-into-a-document: the insertion steps run for it and it is now
//! in a document tree), the top-level Document may append it to its
//! autofocus candidates, which "flush autofocus candidates" focuses at the
//! next rendering update.
//!
//! Insertion steps run for every inclusive descendant of an inserted node,
//! and several tree operations insert (appending, inserting before, replace
//! all, so also the fragment parsing behind `innerHTML`). Parser-created
//! elements have no wrapper until script touches them, which is why this is
//! native: every public insertion into a document tree records the
//! autofocus elements it inserted, in tree order, while the platform asks
//! for them, which is until the top-level Document's autofocus processed
//! flag is set. The platform prelude takes the record at the end of each
//! task and runs the remaining insertion steps (the fully active, sandbox
//! and allow focus checks) for the Realm that owns each element.

use super::*;

#[derive(Default)]
pub(crate) struct State {
    /// Whether insertions are recorded: the top-level Document's autofocus
    /// processed flag is false and a page Realm takes the record.
    recording: bool,
    /// Inserted elements with an `autofocus` attribute, in insertion order.
    insertions: Vec<NodeId>,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.insertions.capacity() * std::mem::size_of::<NodeId>()
    }

    /// An unreachable element cannot become a focusable area: the record
    /// holds its elements weakly.
    pub(super) fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        self.insertions.retain(|&id| keep(id));
    }
}

impl Dom {
    /// HTML #attr-fe-autofocus is a global attribute of HTML elements, and
    /// of SVG and MathML elements through the HTMLOrSVGElement mixin.
    fn has_autofocus_attribute(&self, id: NodeId) -> bool {
        let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
            return false;
        };
        (name.ns == ns!(html) || name.ns == ns!(svg) || name.ns == ns!(mathml))
            && attrs
                .iter()
                .any(|attr| attr.name.ns == ns!() && attr.name.local == local_name!("autofocus"))
    }

    /// The autofocus elements among `root`'s inclusive descendants (not its
    /// shadow-including descendants: a shadow tree is not a document tree),
    /// in tree order.
    fn autofocus_elements_in(&self, root: NodeId, out: &mut Vec<NodeId>) {
        if self.has_autofocus_attribute(root) {
            out.push(root);
        }
        for id in self.descendants(root) {
            if id != root && self.has_autofocus_attribute(id) {
                out.push(id);
            }
        }
    }

    /// DOM #in-a-document-tree: the node's root is a Document.
    fn in_document_tree(&self, id: NodeId) -> bool {
        let mut current = id;
        while let Some(parent) = self.nodes[current].parent {
            current = parent;
        }
        matches!(self.nodes[current].data, NodeData::Document)
    }

    /// `node` was inserted; record the autofocus elements this inserted into
    /// a document.
    pub(super) fn note_autofocus_insertion(&mut self, node: NodeId) {
        if !self.autofocus.recording {
            return;
        }
        let mut found = Vec::new();
        self.autofocus_elements_in(node, &mut found);
        if !found.is_empty() && self.in_document_tree(node) {
            self.autofocus.insertions.extend(found);
        }
    }

    /// Start or stop recording insertions; stopping forgets the record.
    pub(crate) fn set_autofocus_recording(&mut self, recording: bool) {
        self.autofocus.recording = recording;
        if !recording {
            self.autofocus.insertions = Vec::new();
        }
    }

    pub(crate) fn has_autofocus_insertions(&self) -> bool {
        !self.autofocus.insertions.is_empty()
    }

    /// Take the recorded insertions, in order.
    pub(crate) fn take_autofocus_insertions(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.autofocus.insertions)
    }

    /// Return insertions another Window Realm of this arena takes, ahead of
    /// any recorded since they were taken.
    pub(crate) fn restore_autofocus_insertions(&mut self, mut insertions: Vec<NodeId>) {
        insertions.append(&mut self.autofocus.insertions);
        self.autofocus.insertions = insertions;
    }

    /// The autofocus elements a Document's parser inserted (all of them in
    /// its document tree, in tree order): the native parse completes before
    /// the Document's Realm runs.
    pub(crate) fn document_autofocus_elements(&self, document: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if self.is_valid(document) && matches!(self.nodes[document].data, NodeData::Document) {
            self.autofocus_elements_in(document, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertions_into_document_trees_are_recorded_in_tree_order() {
        let mut dom = Dom::parse_document(
            "<!doctype html><body id=body><input id=parsed autofocus><div id=host></div></body>",
        );
        let body = dom.get_by_id("body").unwrap();
        let parsed = dom.get_by_id("parsed").unwrap();
        assert_eq!(dom.document_autofocus_elements(DOCUMENT), vec![parsed]);
        // Nothing is recorded until the platform asks.
        let quiet = dom.create_element("input");
        dom.set_attr(quiet, "autofocus", "");
        dom.append(body, quiet);
        assert!(!dom.has_autofocus_insertions());
        dom.set_autofocus_recording(true);
        // A detached subtree is not in a document tree; inserting it records
        // its autofocus descendants in tree order.
        let wrapper = dom.create_element("div");
        let first = dom.create_element("button");
        dom.set_attr(first, "autofocus", "");
        let second = dom.create_element("textarea");
        dom.set_attr(second, "autofocus", "");
        let plain = dom.create_element("input");
        dom.append(wrapper, first);
        dom.append(wrapper, plain);
        dom.append(wrapper, second);
        assert!(!dom.has_autofocus_insertions());
        dom.insert_before(body, wrapper, Some(parsed));
        assert_eq!(dom.take_autofocus_insertions(), vec![first, second]);
        // replace all (innerHTML's fragment insertion) inserts too.
        let fragment_child = dom.create_element("select");
        dom.set_attr(fragment_child, "autofocus", "");
        let host = dom.get_by_id("host").unwrap();
        dom.replace_all_children(host, vec![fragment_child]);
        assert_eq!(dom.take_autofocus_insertions(), vec![fragment_child]);
        // An element inserted into a detached tree is not inserted into a
        // document; stopping forgets the record.
        let detached = dom.create_element("div");
        let inner = dom.create_element("input");
        dom.set_attr(inner, "autofocus", "");
        dom.append(detached, inner);
        assert!(!dom.has_autofocus_insertions());
        dom.append(body, detached);
        dom.set_autofocus_recording(false);
        assert!(!dom.has_autofocus_insertions());
    }
}
