//! The native caret model of an editing host (HTML #editing-host).
//!
//! Native editing (Input Events 2 §6, HTML #contenteditable) projects an
//! editing host onto a string so that the frontends can place a caret and
//! report the text: UTF-16 positions include authored line and paragraph
//! breaks and keep the text of `contenteditable=false` islands without
//! making it editable. The DOM and its live Range remain authoritative; the
//! projection only maps between DOM boundary points (DOM #concept-range-bp)
//! and string offsets.
//!
//! Each projection walks the host subtree once. Boundary points are ordered
//! by DOM #concept-range-bp-position, computed from one tree-order numbering
//! of the host subtree (each node's entry and exit index), so mapping a
//! point to an offset compares integers instead of re-deriving every node's
//! tree index (DOM #concept-tree-index) from its siblings. The cost of an
//! edit is therefore linear in the host with a small constant, where the
//! script projection had been quadratic in the number of siblings.

use super::*;

/// A segment kind: an editable Text node's data, a `contenteditable=false`
/// island, a line break, or the paragraph separator between blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text = 0,
    Protected = 1,
    Break = 2,
    Paragraph = 3,
}

/// A DOM boundary point (node, offset).
type Point = (NodeId, usize);

struct Segment {
    start: usize,
    end: usize,
    kind: Kind,
    node: Option<NodeId>,
    before: Point,
    after: Point,
}

/// The projection of an editing host: its text (engine text), its segments
/// as `[start, end, kind, node (-1 for none), before node, before offset,
/// after node, after offset]` rows, its trailing placeholder `br` elements
/// with their offsets, and the offsets of the requested boundary points.
pub(crate) struct EditableModel {
    pub(crate) text: String,
    pub(crate) segments: Vec<f64>,
    pub(crate) placeholders: Vec<(NodeId, usize)>,
    pub(crate) points: Vec<usize>,
}

/// UTF-16 length of engine text: a smuggled lone surrogate (U+10F800 to
/// U+10FFFF, see `lumen::embed::text_to_engine`) is one code unit, any other
/// supplementary character two.
fn units(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            0x10F800.. => 1,
            0x10000.. => 2,
            _ => 1,
        })
        .sum()
}

/// A boundary point's position in tree order, or its side of the host when
/// it lies outside the host's subtree.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Before,
    At(u32, usize),
    After,
}

impl Dom {
    /// The DOM children of `id`: its arena children other than a child
    /// navigable's Document.
    fn dom_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.child_iter(id)
            .filter(|&child| !matches!(self.nodes[child].data, NodeData::Document))
    }

    fn dom_next_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut next = self.nodes[id].next_sibling;
        while let Some(sibling) = next {
            if !matches!(self.nodes[sibling].data, NodeData::Document) {
                return Some(sibling);
            }
            next = self.nodes[sibling].next_sibling;
        }
        None
    }

    fn element_local_name(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    /// `contenteditable` in the false state (an ASCII case-insensitive
    /// "false"), which makes an element and its subtree non-editable.
    fn editing_disabled(&self, id: NodeId) -> bool {
        self.element_local_name(id).is_some()
            && self
                .attr(id, "contenteditable")
                .is_some_and(|value| value.eq_ignore_ascii_case("false"))
    }

    /// Whether `id` lays out as a block for native editing: its resolved
    /// `display` (from `display`), or for an element without one (not
    /// rendered), its HTML block element name.
    fn editing_block(&self, id: NodeId, display: &dyn Fn(NodeId) -> Option<String>) -> bool {
        let value = display(id).unwrap_or_default();
        if !value.is_empty() {
            return ["block", "flow-root", "list-item", "flex", "grid", "table"]
                .iter()
                .any(|prefix| value.starts_with(prefix));
        }
        matches!(
            self.element_local_name(id),
            Some(
                "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "pre"
            )
        )
    }

    /// Project `host` (see the module documentation) and map `points` to
    /// offsets. `display` resolves an element's `display` value.
    pub(crate) fn editable_model(
        &self,
        host: NodeId,
        points: &[Point],
        display: &dyn Fn(NodeId) -> Option<String>,
    ) -> EditableModel {
        // Tree-order entry and exit indices of every node of the host's
        // subtree (DOM #concept-tree-order).
        let mut order: FxHashMap<NodeId, (u32, u32)> = FxHashMap::default();
        let mut counter = 0u32;
        let mut stack = vec![(host, false)];
        while let Some((id, exit)) = stack.pop() {
            if exit {
                if let Some(entry) = order.get_mut(&id) {
                    entry.1 = counter;
                }
                counter += 1;
                continue;
            }
            order.insert(id, (counter, counter));
            counter += 1;
            stack.push((id, true));
            let start = stack.len();
            stack.extend(self.dom_children(id).map(|child| (child, false)));
            stack[start..].reverse();
        }

        let mut text = String::new();
        let mut length = 0usize;
        let mut segments: Vec<Segment> = Vec::new();
        let mut placeholders = Vec::new();
        let mut pending: Option<Point> = None;
        let append = |text: &mut String,
                      length: &mut usize,
                      segments: &mut Vec<Segment>,
                      value: &str,
                      before: Point,
                      after: Point,
                      node: Option<NodeId>,
                      kind: Kind| {
            let units = units(value);
            if units == 0 {
                return;
            }
            segments.push(Segment {
                start: *length,
                end: *length + units,
                kind,
                node,
                before,
                after,
            });
            text.push_str(value);
            *length += units;
        };
        // (node, its parent and index, or None for the host; exit marker,
        // whether it is a block).
        enum Step {
            Visit(NodeId, Option<Point>),
            Leave(NodeId),
        }
        let mut work = vec![Step::Visit(host, None)];
        while let Some(step) = work.pop() {
            let (node, position) = match step {
                Step::Leave(block) => {
                    pending = Some((block, self.dom_children(block).count()));
                    continue;
                }
                Step::Visit(node, position) => (node, position),
            };
            match &self.nodes[node].data {
                NodeData::Text(data) => {
                    if !data.is_empty() {
                        if let Some(before) = pending.take() {
                            append(
                                &mut text,
                                &mut length,
                                &mut segments,
                                "\n",
                                before,
                                (node, 0),
                                None,
                                Kind::Paragraph,
                            );
                        }
                        let data_units = units(data);
                        append(
                            &mut text,
                            &mut length,
                            &mut segments,
                            data,
                            (node, 0),
                            (node, data_units),
                            Some(node),
                            Kind::Text,
                        );
                    }
                    continue;
                }
                NodeData::Element { .. } => {}
                _ => continue,
            }
            // The host's own parent and index are only read for an island or
            // a line break, which the host is not.
            let (parent, index) = position.unwrap_or((node, 0));
            let flush = |text: &mut String,
                         length: &mut usize,
                         segments: &mut Vec<Segment>,
                         pending: &mut Option<Point>,
                         point: Point| {
                if let Some(before) = pending.take() {
                    append(
                        text,
                        length,
                        segments,
                        "\n",
                        before,
                        point,
                        None,
                        Kind::Paragraph,
                    );
                }
            };
            if node != host && self.editing_disabled(node) {
                flush(
                    &mut text,
                    &mut length,
                    &mut segments,
                    &mut pending,
                    (parent, index),
                );
                let content = self.text_content(node);
                let content = if content.is_empty() {
                    "\u{FFFC}"
                } else {
                    content.as_str()
                };
                append(
                    &mut text,
                    &mut length,
                    &mut segments,
                    content,
                    (parent, index),
                    (parent, index + 1),
                    Some(node),
                    Kind::Protected,
                );
                continue;
            }
            if self.element_local_name(node) == Some("br") {
                // A sole or trailing repeated BR keeps an empty editing line
                // paintable; it does not add another character to that line.
                if self.dom_next_sibling(node).is_none()
                    && (text.is_empty() || text.ends_with('\n'))
                {
                    placeholders.push(node);
                    continue;
                }
                flush(
                    &mut text,
                    &mut length,
                    &mut segments,
                    &mut pending,
                    (parent, index),
                );
                append(
                    &mut text,
                    &mut length,
                    &mut segments,
                    "\n",
                    (parent, index),
                    (parent, index + 1),
                    Some(node),
                    Kind::Break,
                );
                continue;
            }
            let block = node != host && self.editing_block(node, display);
            if block {
                if pending.is_none() && !text.is_empty() && !text.ends_with('\n') {
                    pending = segments.last().map(|segment| segment.after);
                }
                flush(
                    &mut text,
                    &mut length,
                    &mut segments,
                    &mut pending,
                    (node, 0),
                );
                work.push(Step::Leave(node));
            }
            let start = work.len();
            work.extend(
                self.dom_children(node)
                    .enumerate()
                    .map(|(index, child)| Step::Visit(child, Some((node, index)))),
            );
            work[start..].reverse();
        }

        // DOM #concept-range-bp-position over the numbering: a point in a
        // CharacterData node sorts by offset within the node; a point
        // between children sorts before the child at its offset (or at the
        // container's exit).
        let character_data = |id: NodeId| {
            matches!(
                self.nodes[id].data,
                NodeData::Text(_)
                    | NodeData::CData(_)
                    | NodeData::Comment(_)
                    | NodeData::ProcessingInstruction { .. }
            )
        };
        let key = |(node, offset): Point| -> Key {
            let Some(&(entry, exit)) = order.get(&node) else {
                // Outside the host: before it, or in another tree, the
                // comparison a script projection made returns the first
                // segment; after it, the end.
                return if self.is_valid(node)
                    && self.light_root(node) == self.light_root(host)
                    && self.follows(node, host)
                {
                    Key::After
                } else {
                    Key::Before
                };
            };
            if character_data(node) {
                return Key::At(entry, offset + 1);
            }
            match self.dom_children(node).nth(offset) {
                Some(child) => Key::At(order[&child].0, 0),
                None => Key::At(exit, 0),
            }
        };
        let keyed: Vec<(Key, Key)> = segments
            .iter()
            .map(|segment| (key(segment.before), key(segment.after)))
            .collect();
        let offset_of = |point: Point| -> usize {
            let point_key = key(point);
            for (segment, &(before, after)) in segments.iter().zip(&keyed) {
                if segment.kind == Kind::Text && segment.node == Some(point.0) {
                    return segment.start + point.1.min(segment.end - segment.start);
                }
                if point_key <= before || point_key < after {
                    return segment.start;
                }
            }
            length
        };
        let points = points.iter().map(|&point| offset_of(point)).collect();
        let placeholders = placeholders
            .into_iter()
            .map(|br| {
                let parent = self.nodes[br].parent.unwrap_or(br);
                let index = self
                    .dom_children(parent)
                    .position(|child| child == br)
                    .unwrap_or(0);
                (br, offset_of((parent, index)))
            })
            .collect();
        let mut rows = Vec::with_capacity(segments.len() * 8);
        for segment in &segments {
            rows.extend([
                segment.start as f64,
                segment.end as f64,
                segment.kind as u8 as f64,
                segment.node.map_or(-1.0, |node| node as f64),
                segment.before.0 as f64,
                segment.before.1 as f64,
                segment.after.0 as f64,
                segment.after.1 as f64,
            ]);
        }
        EditableModel {
            text,
            segments: rows,
            placeholders,
            points,
        }
    }

    /// DOM #concept-tree-root, not crossing shadow boundaries.
    fn light_root(&self, mut id: NodeId) -> NodeId {
        // A child navigable's Document is an arena child of its container
        // but the root of its own tree.
        while !matches!(self.nodes[id].data, NodeData::Document)
            && let Some(parent) = self.nodes[id].parent
        {
            id = parent;
        }
        id
    }

    /// Whether `node` follows `host` in tree order (both in one tree, and
    /// `node` outside `host`'s subtree).
    fn follows(&self, node: NodeId, host: NodeId) -> bool {
        let chain = |mut id: NodeId| {
            let mut chain = vec![id];
            while let Some(parent) = self.nodes[id].parent {
                chain.push(parent);
                id = parent;
            }
            chain.reverse();
            chain
        };
        let (a, b) = (chain(node), chain(host));
        let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        if common == 0 || common >= a.len() || common >= b.len() {
            // One contains the other: an ancestor of the host precedes it.
            return false;
        }
        let parent = a[common - 1];
        let (x, y) = (a[common], b[common]);
        self.dom_children(parent)
            .find(|&child| child == x || child == y)
            .is_some_and(|first| first == y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(html: &str, points: &[(&str, usize)]) -> (String, Vec<f64>, Vec<usize>, Dom) {
        let dom = Dom::parse_document(html);
        let host = dom.get_by_id("host").unwrap();
        let points: Vec<Point> = points
            .iter()
            .map(|(id, offset)| (dom.get_by_id(id).unwrap(), *offset))
            .collect();
        let model = dom.editable_model(host, &points, &|_| None);
        (model.text, model.segments, model.points, dom)
    }

    #[test]
    fn editable_projection_includes_breaks_paragraphs_and_islands() {
        // HTML #editing-host: block children become paragraphs, a br a line
        // break, a contenteditable=false island keeps its text.
        let (text, segments, points, _) = model(
            "<div id=host>ab<p id=p>cd<br>e</p><span contenteditable=false id=x>ro</span>\
             <div id=d><b id=b></b></div></div>",
            &[("host", 0), ("p", 1), ("d", 1), ("host", 4)],
        );
        assert_eq!(text, "ab\ncd\ne\nro\n");
        // text, paragraph, text, break, text, paragraph, island, paragraph
        // (the empty final block still starts a paragraph).
        let kinds: Vec<f64> = segments.chunks(8).map(|row| row[2]).collect();
        assert_eq!(kinds, [0.0, 3.0, 0.0, 2.0, 0.0, 3.0, 1.0, 3.0]);
        // (host, 0) is the start; (p, 1) is the break; (d, 1) and the end of
        // the host follow every segment.
        assert_eq!(points, [0, 5, 11, 11]);
    }

    #[test]
    fn editable_projection_counts_utf16_and_finds_trailing_placeholders() {
        let dom = Dom::parse_document("<div id=host>a\u{1F600}<br id=br></div>");
        let host = dom.get_by_id("host").unwrap();
        let text = dom.nodes[host].first_child.unwrap();
        let model = dom.editable_model(host, &[(text, 3)], &|_| None);
        assert_eq!(model.text, "a\u{1F600}\n");
        assert_eq!(model.points, [3]);
        // A trailing br after text that does not end a line is a break.
        assert!(model.placeholders.is_empty());
        let dom = Dom::parse_document("<div id=host><p>x</p><p id=empty><br id=br></p></div>");
        let host = dom.get_by_id("host").unwrap();
        let br = dom.get_by_id("br").unwrap();
        let model = dom.editable_model(host, &[], &|_| None);
        assert_eq!(model.text, "x\n");
        assert_eq!(model.placeholders, [(br, 2)]);
    }
}
