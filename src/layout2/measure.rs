//! JS geometry from fragments (layout2 architecture, P7).
//!
//! `getBoundingClientRect`, `offset*`/`client*`, and the observer machinery
//! read a `NodeId → PxRect` border-box map in CSS px. `scrollHeight` and
//! `scrollWidth` read a separate scrolling-area map, because CSSOM View gives
//! those APIs different geometry. The old engine reconstructed both
//! from *painted cells* plus a stack of heuristics (`element_tops` for empty
//! sentinels, `declared_boxes` floors, `clip_heights` caps). layout2 has REAL
//! stored geometry, so the map falls out of the fragment tree directly — the
//! design goal that "JS geometry reads the fragment tree, *more* accurate
//! than today".
//!
//! A composed-tree ancestor union supplies each scrolling area's content extent
//! and aggregates inline ancestors, empty containers, and shadow hosts, exactly
//! as the old engine's cell union did—but without replacing the border box that
//! `getBoundingClientRect()` must report.

use std::collections::{HashMap, HashSet};

use crate::dom::{DOCUMENT, Dom, NodeId};
use crate::layout2::{NO_NODE, PxRect};

use super::flow::{Frag, FragKind, LineFrag, TopFrag};
use crate::render::{Affine2d, CssRect};

fn border_rect(frag: &Frag, transform: Affine2d) -> PxRect {
    let rect = super::transform::bounds(transform, CssRect::new(frag.x, frag.y, frag.w, frag.h));
    PxRect {
        left: f64::from(rect.x),
        top: f64::from(rect.y),
        // Keep the same edge arithmetic as the complete measurement path.
        width: f64::from((rect.x + rect.width) - rect.x),
        height: f64::from((rect.y + rect.height) - rect.y),
        css_width: frag.css_size.map(|size| f64::from(size[0])),
        css_height: frag.css_size.map(|size| f64::from(size[1])),
    }
}

/// Rectangle-only projection for an immutable layout result. CSSOM View
/// #dom-element-getboundingclientrect permits the single-border-box case to
/// avoid a composed-tree/scrolling-area projection, but not to substitute one
/// fragment for several. `None` marks a node needing complete measurement.
///
/// Build once on the second distinct rectangle query, not once per element:
/// N geometry reads after one layout must not perform N fragment-tree walks.
/// The owner is the completed layout, never the mutable DOM or a global epoch.
#[derive(Debug, Default)]
pub(super) struct SingleBoxIndex {
    boxes: rustc_hash::FxHashMap<NodeId, Option<PxRect>>,
    /// Most pages have few transformed boxes. Only those boxes need a second
    /// rectangle; don't double retained geometry for every ordinary element.
    untransformed: rustc_hash::FxHashMap<NodeId, PxRect>,
    #[cfg(test)]
    pub(super) visited: usize,
}

impl SingleBoxIndex {
    pub(super) fn new(root: &Frag, fixed: &[Frag], top_layer: &[TopFrag]) -> Self {
        let mut index = Self::default();
        for root in std::iter::once(root)
            .chain(fixed)
            .chain(top_layer.iter().map(|top| &top.fragment))
        {
            // Iterators bound the temporary traversal stack by depth, not by
            // sibling count. No fragment or inline shaping payload is copied.
            let mut pending = vec![(std::slice::from_ref(root).iter(), Affine2d::IDENTITY)];
            while let Some((children, parent)) = pending.last_mut() {
                let Some(frag) = children.next() else {
                    pending.pop();
                    continue;
                };
                let transform = parent.then(super::transform::matrix(frag));
                #[cfg(test)]
                {
                    index.visited += 1;
                }
                if frag.node != NO_NODE
                    && matches!(frag.kind, FragKind::Block | FragKind::TableCell(_))
                {
                    index
                        .boxes
                        .entry(frag.node)
                        .and_modify(|rect| *rect = None)
                        .or_insert_with(|| {
                            matches!(frag.kind, FragKind::Block)
                                .then(|| border_rect(frag, transform))
                        });
                    if !transform.is_identity() {
                        index
                            .untransformed
                            .insert(frag.node, border_rect(frag, Affine2d::IDENTITY));
                    }
                }
                if !frag.children.is_empty() {
                    pending.push((
                        frag.children.iter(),
                        if frag.paint.child_viewport {
                            Affine2d::IDENTITY
                        } else {
                            transform
                        },
                    ));
                }
            }
        }
        index
    }

    pub(super) fn get(&self, node: NodeId) -> Option<PxRect> {
        self.boxes.get(&node).copied().flatten()
    }

    pub(super) fn get_layout(&self, node: NodeId) -> Option<PxRect> {
        let visual = self.get(node)?;
        Some(self.untransformed.get(&node).copied().unwrap_or(visual))
    }

    pub(super) fn retained_bytes(&self) -> usize {
        // Requested-storage lower bound, matching the host inventory contract.
        self.boxes.capacity() * std::mem::size_of::<(NodeId, Option<PxRect>)>()
            + self.untransformed.capacity() * std::mem::size_of::<(NodeId, PxRect)>()
    }
}

/// CSSOM View #dom-element-getboundingclientrect: a single generated border
/// box already is its element's bounding box. This projection needs neither
/// composed ancestor unions nor unrelated scrolling areas. Multiple boxes,
/// inline content, and table cells keep the complete measurement path.
pub(super) fn single_border_box(
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
    node: NodeId,
) -> Option<PxRect> {
    fn visit(
        frag: &Frag,
        node: NodeId,
        found: &mut Option<PxRect>,
        parent: Affine2d,
    ) -> Option<()> {
        let transform = parent.then(super::transform::matrix(frag));
        if frag.node == node && matches!(frag.kind, FragKind::Block | FragKind::TableCell(_)) {
            if found.is_some() || !matches!(frag.kind, FragKind::Block) {
                return None;
            }
            *found = Some(border_rect(frag, transform));
        }
        for child in &frag.children {
            visit(
                child,
                node,
                found,
                if frag.paint.child_viewport {
                    Affine2d::IDENTITY
                } else {
                    transform
                },
            )?;
        }
        Some(())
    }
    let mut found = None;
    visit(root, node, &mut found, Affine2d::IDENTITY)?;
    for frag in fixed {
        visit(frag, node, &mut found, Affine2d::IDENTITY)?;
    }
    for top in top_layer {
        visit(&top.fragment, node, &mut found, Affine2d::IDENTITY)?;
    }
    found
}

/// Canonical fragment rectangle in CSS pixels. CSSOM View geometry is read
/// before any terminal adaptation or device-pixel presentation.
#[derive(Copy, Clone)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn transformed(self, transform: Affine2d) -> Self {
        let r = super::transform::bounds(
            transform,
            CssRect::new(self.x0, self.y0, self.x1 - self.x0, self.y1 - self.y0),
        );
        Self {
            x0: r.x,
            y0: r.y,
            x1: r.x + r.width,
            y1: r.y + r.height,
        }
    }
    fn union(a: Rect, b: Rect) -> Rect {
        Rect {
            x0: a.x0.min(b.x0),
            y0: a.y0.min(b.y0),
            x1: a.x1.max(b.x1),
            y1: a.y1.max(b.y1),
        }
    }
}

/// Fold a rectangle into a `NodeId → Rect` map (union on collision — an
/// element that generates several fragments reports their bounding box).
fn add(map: &mut HashMap<NodeId, Rect>, node: NodeId, r: Rect) {
    map.entry(node)
        .and_modify(|c| *c = Rect::union(*c, r))
        .or_insert(r);
}

/// One box's own boxes: `own` = every directly-attributed box (block/replaced
/// border boxes + inline pieces, the union base); `block` = only the border
/// box a BLOCK-level fragment generates (the spec `getBoundingClientRect` for
/// a non-scroll block, used to re-cap the composed union); `inline` = the
/// bounding box of a non-replaced inline box's line fragments. `nodes`
/// collects every element id touched (fixed-subtree membership).
#[derive(Default)]
struct Own {
    own: HashMap<NodeId, Rect>,
    block: HashMap<NodeId, Rect>,
    inline: HashMap<NodeId, Rect>,
    css_size: HashMap<NodeId, [f32; 2]>,
    nodes: HashSet<NodeId>,
    frame_viewports: HashMap<NodeId, crate::render::CssRect>,
}

/// Walk a fragment tree, attributing border boxes and inline piece boxes.
fn walk(dom: &Dom, f: &Frag, o: &mut Own, parent: Affine2d, visual: bool) {
    let transform = if visual {
        parent.then(super::transform::matrix(f))
    } else {
        Affine2d::IDENTITY
    };
    if f.node != NO_NODE {
        o.nodes.insert(f.node);
        if matches!(f.kind, FragKind::Block | FragKind::TableCell(_)) {
            let r = Rect {
                x0: f.x,
                y0: f.y,
                x1: f.x + f.w,
                y1: f.y + f.h,
            };
            add(&mut o.block, f.node, r.transformed(transform));
            add(&mut o.own, f.node, r.transformed(transform));
            if let Some(size) = f.css_size {
                o.css_size.insert(f.node, size);
            }
            if matches!(dom.tag_name(f.node), Some("iframe" | "frame")) {
                // HTML Rendering #the-page: each child Document fits the
                // embedding element's content box, not its border/padding box.
                o.frame_viewports.insert(f.node, f.content_box());
            }
        }
    }
    if let FragKind::Line(line) = &f.kind {
        for p in &line.pieces {
            if p.item.node == NO_NODE {
                continue;
            }
            let r = if line.sideways {
                Rect {
                    x0: f.x + f.w - p.y - p.box_height,
                    y0: f.y + p.x,
                    x1: f.x + f.w - p.y,
                    y1: f.y + p.x + p.box_width,
                }
            } else {
                Rect {
                    x0: f.x + p.x,
                    y0: f.y + p.y,
                    x1: f.x + p.x + p.box_width,
                    y1: f.y + p.y + p.box_height,
                }
            };
            o.nodes.insert(p.item.node);
            add(&mut o.own, p.item.node, r.transformed(transform));
        }
        if !line.sideways {
            for (node, r) in inline_box_fragments(f, line) {
                let r = r.transformed(transform);
                o.nodes.insert(node);
                add(&mut o.inline, node, r);
                add(&mut o.own, node, r);
            }
        }
    }
    for c in &f.children {
        // Each nested Document has its own viewport. It does not inherit the
        // embedding element's transforms in its CSSOM coordinate space.
        let parent = if f.paint.child_viewport {
            Affine2d::IDENTITY
        } else {
            transform
        };
        walk(dom, c, o, parent, visual);
    }
}

/// CSSOM View #dom-element-getclientrects: each line fragment of an element's
/// non-replaced inline box is its border area. That is the box's content area
/// (CSS 2 #inline-non-replaced: from the font's ascent to its descent, never
/// the line height) with its vertical padding and border, spanning the box's
/// pieces on the line plus its start and end edges where the box begins and
/// ends (CSS Backgrounds 3 #box-decoration-break `slice`). Decorated boxes
/// paint the same geometry.
fn inline_box_fragments(f: &Frag, line: &LineFrag) -> Vec<(NodeId, Rect)> {
    struct Run {
        node: NodeId,
        rect: Rect,
        edges: [f32; 2],
    }
    let mut runs: Vec<Run> = Vec::new();
    for piece in line.pieces.iter().chain(&line.atom_boxes) {
        let Some(boxes) = &piece.boxes else {
            continue;
        };
        let start = f.x + piece.x;
        let end = start + piece.box_width;
        let (top, bottom) =
            piece
                .shaped
                .as_ref()
                .map_or((f32::INFINITY, f32::NEG_INFINITY), |text| {
                    let baseline = f.y + piece.y + text.baseline;
                    (baseline - text.ascent, baseline + text.descent)
                });
        for entry in boxes.chain.iter().filter(|entry| entry.key.1.is_none()) {
            let mut piece_rect = Rect {
                x0: start,
                y0: top,
                x1: end,
                y1: bottom,
            };
            if let Some(&(_, distance)) = boxes.opens.iter().find(|(open, _)| *open == entry.key) {
                piece_rect.x0 = start - distance;
            }
            if let Some(&(_, distance)) = boxes.closes.iter().find(|(close, _)| *close == entry.key)
            {
                piece_rect.x1 = end + distance;
            }
            match runs.iter_mut().find(|run| run.node == entry.key.0) {
                Some(run) => run.rect = Rect::union(run.rect, piece_rect),
                None => runs.push(Run {
                    node: entry.key.0,
                    rect: piece_rect,
                    edges: entry.vertical_edges,
                }),
            }
        }
    }
    runs.into_iter()
        .map(|Run { node, rect, edges }| {
            // A box holding no text takes the line's own font metrics.
            let (top, bottom) = if rect.y0.is_finite() {
                (rect.y0, rect.y1)
            } else {
                let baseline = f.y + line.baseline;
                (baseline - line.ascent, baseline + line.descent)
            };
            let rect = Rect {
                x0: rect.x0,
                y0: top - edges[0],
                x1: rect.x1.max(rect.x0),
                y1: bottom + edges[1],
            };
            (node, rect)
        })
        .collect()
}

/// CSS 2 #anonymous-block-level: an in-flow block-level box inside an inline
/// box splits it, and CSSOM View engines report the inline's geometry around
/// that block too. Union each such block into its inline ancestors (up to the
/// enclosing block container), never an atomic inline, float or positioned
/// box, whose geometry stays outside the inline box.
fn blocks_in_inlines(dom: &Dom, own: &Own) -> HashMap<NodeId, Rect> {
    let mut out = HashMap::new();
    for (&node, &rect) in &own.block {
        let Some(parent) = dom
            .parent_composed(node)
            .filter(|p| own.inline.contains_key(p))
        else {
            continue;
        };
        let in_flow_block = !dom
            .effective_display(node)
            .is_some_and(|display| display.starts_with("inline"))
            && !dom
                .computed_value(node, "position")
                .is_some_and(|position| matches!(position.trim(), "absolute" | "fixed"))
            && dom
                .computed_value(node, "float")
                .is_none_or(|float| float.trim() == "none");
        if !in_flow_block {
            continue;
        }
        let mut current = Some(parent);
        while let Some(inline) = current.filter(|p| own.inline.contains_key(p)) {
            add(&mut out, inline, rect);
            current = dom.parent_composed(inline);
        }
    }
    out
}

/// Bottom-up composed-tree union of `base`, restricted to nodes passing `keep`.
/// Each node's result is its own box unioned with its composed children's
/// results (visiting `composed_descendants` in reverse reaches every child
/// before its parent). This gives a scroll container / the root element their
/// CONTENT extent and aggregates inline ancestors, empty containers, and shadow
/// hosts. Filtering by `keep` keeps the pinned fixed layer from inflating the
/// scrollable document (fixed boxes do not contribute to scroll overflow).
fn composed_union(
    dom: &Dom,
    base: &HashMap<NodeId, Rect>,
    keep: impl Fn(NodeId) -> bool,
) -> HashMap<NodeId, Rect> {
    let mut content: HashMap<NodeId, Rect> = base
        .iter()
        .filter(|&(&k, _)| keep(k))
        .map(|(&k, &v)| (k, v))
        .collect();
    for &id in dom.composed_descendants(DOCUMENT).iter().rev() {
        if !keep(id) {
            continue;
        }
        let mut acc = content.get(&id).copied();
        for child in dom.composed_children(id) {
            if !keep(child) {
                continue;
            }
            if let Some(&cr) = content.get(&child) {
                acc = Some(acc.map_or(cr, |a| Rect::union(a, cr)));
            }
        }
        if let Some(acc) = acc {
            content.insert(id, acc);
        }
    }
    content
}

/// Select each node's border box and, independently, any scrolling-area extent.
/// CSSOM View §6 requires `getBoundingClientRect()` to keep the element's own
/// box while `scrollWidth`/`scrollHeight` return the size of its scrolling area.
fn select_into(
    dom: &Dom,
    content: &HashMap<NodeId, Rect>,
    own: &Own,
    out: &mut HashMap<NodeId, PxRect>,
    scroll: &mut HashMap<NodeId, PxRect>,
) {
    let block = &own.block;
    let css_size = &own.css_size;
    let split = blocks_in_inlines(dom, own);
    for (&node, &cbox) in content {
        // CSS Display 3 #box-tree and CSSOM View #dom-element-getclientrects:
        // a composed descendant extent does not establish an associated box.
        // Keep that extent in `content` so a box-generating ancestor includes
        // the children, but never expose it as the box of display:contents.
        // Actual block fragments need no style lookup; only the composed
        // inline fallback needs this box-generation check.
        if !block.contains_key(&node) && !dom.generates_principal_box(node) {
            continue;
        }
        // A non-replaced inline box reports its own line fragments (with any
        // block it was split around); other non-block elements (shadow hosts
        // without their own generated block, display:contents) use the union
        // of their generated pieces.
        let inline = own.inline.get(&node).map(|&rect| {
            split
                .get(&node)
                .map_or(rect, |&blocks| Rect::union(rect, blocks))
        });
        let c = block.get(&node).copied().or(inline).unwrap_or(cbox);
        out.insert(
            node,
            PxRect {
                left: c.x0 as f64,
                top: c.y0 as f64,
                width: (c.x1 - c.x0) as f64,
                height: (c.y1 - c.y0) as f64,
                css_width: css_size.get(&node).map(|size| f64::from(size[0])),
                css_height: css_size.get(&node).map(|size| f64::from(size[1])),
            },
        );
        if dom.is_scroll_container(node)
            || dom.is_hscroll_container(node)
            || matches!(dom.tag_name(node), Some("html" | "body"))
        {
            scroll.insert(
                node,
                PxRect {
                    left: cbox.x0 as f64,
                    top: cbox.y0 as f64,
                    width: (cbox.x1 - cbox.x0) as f64,
                    height: (cbox.y1 - cbox.y0) as f64,
                    css_width: None,
                    css_height: None,
                },
            );
        }
    }
}

/// Build the `NodeId → PxRect` geometry map from the laid fragment tree (the
/// in-flow root + the pinned fixed layer), directly in CSS pixels.
#[allow(clippy::type_complexity)]
pub(super) fn boxes(
    dom: &Dom,
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
) -> (
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, crate::render::CssRect>,
) {
    project(dom, root, fixed, top_layer, true)
}

pub(super) fn layout_boxes(
    dom: &Dom,
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
) -> HashMap<NodeId, PxRect> {
    project(dom, root, fixed, top_layer, false).0
}

/// CSSOM View client* consumes unscaled padding/border edges, not a visual
/// bounding rectangle or frontend-reported, quantized scroll-region size.
/// Non-replaced inline boxes deliberately have no entry.
pub(super) fn client_metrics(
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
) -> rustc_hash::FxHashMap<NodeId, [f32; 4]> {
    let mut result = rustc_hash::FxHashMap::default();
    for root in std::iter::once(root)
        .chain(fixed)
        .chain(top_layer.iter().map(|top| &top.fragment))
    {
        let mut pending = vec![std::slice::from_ref(root).iter()];
        while let Some(children) = pending.last_mut() {
            let Some(frag) = children.next() else {
                pending.pop();
                continue;
            };
            if frag.node != NO_NODE && matches!(frag.kind, FragKind::Block | FragKind::TableCell(_))
            {
                let [top, right, bottom, left] = frag.border;
                result.entry(frag.node).or_insert([
                    left,
                    top,
                    (frag.w - left - right).max(0.),
                    (frag.h - top - bottom).max(0.),
                ]);
            }
            if !frag.children.is_empty() {
                pending.push(frag.children.iter());
            }
        }
    }
    result
}

#[allow(clippy::type_complexity)]
fn project(
    dom: &Dom,
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
    visual: bool,
) -> (
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, crate::render::CssRect>,
) {
    // In-flow tree: its own boxes never include the fixed layer.
    let mut flow = Own::default();
    walk(dom, root, &mut flow, Affine2d::IDENTITY, visual);

    // The pinned fixed layer: measured separately so a fixed header never
    // inflates the document's scrollable height (a fixed box is viewport-
    // relative, contributing no scroll overflow — CSS Overflow L3).
    let mut fx = Own::default();
    for f in fixed {
        walk(dom, f, &mut fx, Affine2d::IDENTITY, visual);
    }
    // Top-layer boxes do not inflate the document scroll area, but CSSOM View
    // still reports their actual ICB-relative border boxes.
    for top in top_layer {
        walk(dom, &top.fragment, &mut fx, Affine2d::IDENTITY, visual);
    }

    let mut out: HashMap<NodeId, PxRect> = HashMap::new();
    let mut scroll: HashMap<NodeId, PxRect> = HashMap::new();
    let flow_content = composed_union(dom, &flow.own, |_| true);
    select_into(dom, &flow_content, &flow, &mut out, &mut scroll);
    if !fx.own.is_empty() {
        let fixed_nodes = std::mem::take(&mut fx.nodes);
        let fx_content = composed_union(dom, &fx.own, |id| fixed_nodes.contains(&id));
        select_into(dom, &fx_content, &fx, &mut out, &mut scroll);
    }
    // Scroll overflow follows containing blocks and local overflow clipping;
    // the composed union above remains solely the inline border-box fallback.
    if !visual {
        return (out, HashMap::new(), flow.frame_viewports);
    }
    let mut areas = super::overflow::ScrollAreas::new(dom, root);
    for f in fixed {
        areas.extend(dom, f);
    }
    for top in top_layer {
        areas.extend(dom, &top.fragment);
    }
    for (node, rect) in areas.nodes {
        scroll.insert(node, rect);
    }
    flow.frame_viewports.extend(fx.frame_viewports);
    (out, scroll, flow.frame_viewports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(node: NodeId, kind: FragKind, children: Vec<Frag>) -> Frag {
        Frag {
            flow: Default::default(),
            node,
            x: 13.375,
            y: -2.25,
            w: 0.125,
            h: 0.0,
            border: [0.0; 4],
            css_size: Some([0.125, 0.0]),
            content_size: Some([0.125, 0.0]),
            content_offset: [0.0; 2],
            paint: Default::default(),
            clip: None,
            kind,
            children,
        }
    }

    #[test]
    fn single_box_index_preserves_fragment_eligibility_and_edge_arithmetic() {
        let root = fragment(
            NO_NODE,
            FragKind::Block,
            vec![
                fragment(
                    1,
                    FragKind::Block,
                    vec![fragment(2, FragKind::Block, vec![])],
                ),
                fragment(3, FragKind::Block, vec![]),
                fragment(3, FragKind::Block, vec![]),
                fragment(4, FragKind::TableCell(Box::new([])), vec![]),
                fragment(5, FragKind::Block, vec![]),
                fragment(6, FragKind::Fixed(0), vec![]),
                fragment(7, FragKind::Block, vec![]),
                fragment(8, FragKind::Block, vec![]),
                fragment(8, FragKind::TableCell(Box::new([])), vec![]),
            ],
        );
        let fixed = vec![
            fragment(5, FragKind::Block, vec![]),
            fragment(6, FragKind::Block, vec![]),
        ];
        let top_layer = vec![
            TopFrag {
                fragment: fragment(7, FragKind::Block, vec![]),
                fixed: true,
                order: 0,
            },
            TopFrag {
                fragment: fragment(9, FragKind::Block, vec![]),
                fixed: false,
                order: 1,
            },
        ];
        let index = SingleBoxIndex::new(&root, &fixed, &top_layer);
        for node in 0..=12 {
            assert_eq!(
                index.get(node),
                single_border_box(&root, &fixed, &top_layer, node),
                "node {node}"
            );
        }
        assert_eq!(index.visited, 15);
        assert!(
            index.get(NO_NODE).is_none(),
            "anonymous/generated boxes are not DOM elements"
        );
        for node in [3, 4, 5, 7, 8] {
            assert!(index.get(node).is_none(), "ambiguous/table node {node}");
        }
        let rect = index.get(2).unwrap();
        assert_eq!(rect.width, 0.125);
        assert_eq!(rect.height, 0.0, "zero-area generated boxes must survive");
        assert_eq!(rect.top, -2.25);
        assert_eq!(rect.css_width, Some(0.125));
        assert_eq!(
            index.get(6),
            Some(rect),
            "fixed markers must not duplicate the fixed fragment"
        );
        assert_eq!(index.get(9), Some(rect));
    }
}
