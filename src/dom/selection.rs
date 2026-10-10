//! Selection API §2 (local w3c/selection-api b475d4b) for the frontends that
//! paint a page's selection: what a Document's selected range selects.
//!
//! The range itself lives in the page's realm (the prelude's `Selection`).
//! The frontend needs it as layout's boundary points: each Text node's
//! selected data, in UTF-16 code units, and the replaced elements selected
//! whole. CSS UI 4 #content-selection excludes `user-select: none` content
//! from a selection that extends across it ("must exclude this element from
//! such a selection"), so it is left out here, where painting and copying
//! both read it.

use super::*;
use crate::render::SelectionSegments;

/// At most this many nodes are walked for one range; a selection of a
/// larger document is painted to there.
const WALK_LIMIT: usize = 1 << 20;

/// HTML elements whose content is replaced, which a selection selects whole
/// (CSS Pseudo 4 #highlight-replaced).
fn replaced_element(name: &str) -> bool {
    matches!(
        name,
        "img" | "svg" | "canvas" | "video" | "iframe" | "embed" | "object" | "picture" | "audio"
    )
}

impl Dom {
    fn is_character_data(&self, id: NodeId) -> bool {
        matches!(
            self.nodes[id].data,
            NodeData::Text(_)
                | NodeData::CData(_)
                | NodeData::Comment(_)
                | NodeData::ProcessingInstruction { .. }
        )
    }

    /// The child of `id` at index `offset` (DOM #concept-tree-child).
    fn child_at(&self, id: NodeId, offset: u32) -> Option<NodeId> {
        self.child_iter(id).nth(offset as usize)
    }

    /// The node following `id` in a walk that visits a shadow host's shadow
    /// tree before its light children, skipping `id`'s own subtree.
    fn next_skipping_subtree(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            if let Some(next) = self.nodes[id].next_sibling {
                return Some(next);
            }
            let parent = self.nodes[id].parent?;
            // A shadow tree's end continues with its host's light children.
            if let Some(&host) = self.shadow_hosts.get(&parent) {
                if let Some(first) = self.nodes[host].first_child {
                    return Some(first);
                }
                id = host;
            } else {
                id = parent;
            }
        }
    }

    /// The node after `id` in that walk.
    fn next_selected(&self, id: NodeId) -> Option<NodeId> {
        if let Some(&shadow) = self.shadow_roots.get(&id)
            && let Some(first) = self.nodes[shadow].first_child
        {
            return Some(first);
        }
        self.nodes[id]
            .first_child
            .or_else(|| self.next_skipping_subtree(id))
    }

    /// Whether `ancestor` is a shadow-including inclusive ancestor of `id`.
    fn shadow_including_contains(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            match self.nodes[id]
                .parent
                .or_else(|| self.shadow_hosts.get(&id).copied())
            {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }

    /// Add what the range from boundary point `start` to `end` selects
    /// (DOM #concept-range: `start` is not after `end`) to `out`.
    pub(crate) fn add_selection_segments(
        &self,
        start: (NodeId, u32),
        end: (NodeId, u32),
        out: &mut SelectionSegments,
    ) {
        let ((start_node, start_offset), (end_node, end_offset)) = (start, end);
        if !self.is_valid(start_node) || !self.is_valid(end_node) {
            return;
        }
        let mut current = if self.is_character_data(start_node) {
            Some(start_node)
        } else {
            self.child_at(start_node, start_offset)
                .or_else(|| self.next_skipping_subtree(start_node))
        };
        // An end in character data is inclusive of that node; otherwise the
        // walk stops at the node after the boundary point.
        let end_in_data = self.is_character_data(end_node);
        let stop = (!end_in_data).then(|| {
            self.child_at(end_node, end_offset)
                .or_else(|| self.next_skipping_subtree(end_node))
        });
        let mut memo = FxHashMap::default();
        let mut walked = 0;
        while let Some(node) = current {
            if stop == Some(Some(node)) || walked >= WALK_LIMIT {
                break;
            }
            walked += 1;
            match &self.nodes[node].data {
                NodeData::Text(text) | NodeData::CData(text) => {
                    let length = crate::render::selection::utf16_len(text);
                    let from = if node == start_node {
                        start_offset.min(length)
                    } else {
                        0
                    };
                    let to = if node == end_node && end_in_data {
                        end_offset.min(length)
                    } else {
                        length
                    };
                    let selectable = self.parent_flat(node).is_none_or(|parent| {
                        self.user_select_memo(parent, &mut memo) != UserSelect::None
                    });
                    if from < to && selectable {
                        out.text.entry(node).or_default().push((from, to));
                    }
                }
                NodeData::Element { .. }
                    if self.tag_name(node).is_some_and(replaced_element)
                        && !self.shadow_including_contains(node, end_node)
                        && self.user_select_memo(node, &mut memo) != UserSelect::None =>
                {
                    out.replaced.insert(node);
                }
                _ => {}
            }
            if node == end_node && end_in_data {
                break;
            }
            current = self.next_selected(node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(dom: &Dom, id: &str, index: usize) -> NodeId {
        let element = dom.get_by_id(id).unwrap();
        dom.children(element)
            .into_iter()
            .filter(|&child| dom.tag_name(child).is_none())
            .nth(index)
            .unwrap()
    }

    #[test]
    fn a_range_selects_the_text_between_its_boundary_points() {
        let dom = Dom::parse_document(
            "<p id=a>first</p><p id=b>second <img id=i src=x> third</p><p id=c>fourth</p>",
        );
        let (first, second, third, fourth) = (
            text_of(&dom, "a", 0),
            text_of(&dom, "b", 0),
            text_of(&dom, "b", 1),
            text_of(&dom, "c", 0),
        );
        let mut out = SelectionSegments::default();
        dom.add_selection_segments((first, 2), (fourth, 3), &mut out);
        assert_eq!(out.text[&first], vec![(2, 5)]);
        assert_eq!(out.text[&second], vec![(0, 7)]);
        assert_eq!(out.text[&third], vec![(0, 6)]);
        assert_eq!(out.text[&fourth], vec![(0, 3)]);
        assert!(out.replaced.contains(&dom.get_by_id("i").unwrap()));
    }

    #[test]
    fn element_boundary_points_select_whole_children() {
        let dom =
            Dom::parse_document("<div id=d><p id=a>one</p><p id=b>two</p><p id=c>three</p></div>");
        let div = dom.get_by_id("d").unwrap();
        let mut out = SelectionSegments::default();
        dom.add_selection_segments((div, 1), (div, 2), &mut out);
        assert_eq!(out.text.len(), 1);
        assert_eq!(out.text[&text_of(&dom, "b", 0)], vec![(0, 3)]);
    }

    #[test]
    fn user_select_none_is_left_out_and_partial_replaced_content_is_not_whole() {
        let dom = Dom::parse_document(
            "<p id=a>one</p><p id=n style='user-select:none'>skip <span id=s \
             style='user-select:text'>keep</span></p><p id=c>two</p>",
        );
        let (one, skip, keep, two) = (
            text_of(&dom, "a", 0),
            text_of(&dom, "n", 0),
            text_of(&dom, "s", 0),
            text_of(&dom, "c", 0),
        );
        let mut out = SelectionSegments::default();
        dom.add_selection_segments((one, 0), (two, 3), &mut out);
        assert!(out.text.contains_key(&one));
        assert!(!out.text.contains_key(&skip));
        assert!(out.text.contains_key(&keep));
        assert!(out.text.contains_key(&two));
    }
}
