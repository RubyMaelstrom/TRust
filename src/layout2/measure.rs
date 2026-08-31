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

use super::flow::{Frag, FragKind, TopFrag};

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
/// a non-scroll block, used to re-cap the composed union). `nodes` collects
/// every element id touched (fixed-subtree membership).
#[derive(Default)]
struct Own {
    own: HashMap<NodeId, Rect>,
    block: HashMap<NodeId, Rect>,
    css_size: HashMap<NodeId, [f32; 2]>,
    nodes: HashSet<NodeId>,
}

/// Walk a fragment tree, attributing border boxes and inline piece boxes.
fn walk(f: &Frag<'_>, o: &mut Own) {
    if f.node != NO_NODE {
        o.nodes.insert(f.node);
        if matches!(f.kind, FragKind::Block) {
            let r = Rect {
                x0: f.x,
                y0: f.y,
                x1: f.x + f.w,
                y1: f.y + f.h,
            };
            add(&mut o.block, f.node, r);
            add(&mut o.own, f.node, r);
            if let Some(size) = f.css_size {
                o.css_size.insert(f.node, size);
            }
        }
    }
    if let FragKind::Line(line) = &f.kind {
        for p in &line.pieces {
            if p.item.node == NO_NODE {
                continue;
            }
            let r = Rect {
                x0: f.x + p.x,
                y0: f.y + p.y,
                x1: f.x + p.x + p.box_width,
                y1: f.y + p.y + p.box_height,
            };
            o.nodes.insert(p.item.node);
            add(&mut o.own, p.item.node, r);
        }
    }
    for c in &f.children {
        walk(c, o);
    }
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
    block: &HashMap<NodeId, Rect>,
    css_size: &HashMap<NodeId, [f32; 2]>,
    out: &mut HashMap<NodeId, PxRect>,
    scroll: &mut HashMap<NodeId, PxRect>,
) {
    for (&node, &cbox) in content {
        // Non-block elements (inline wrappers and shadow hosts without their
        // own generated block) use the union of their generated pieces.
        let c = block.get(&node).copied().unwrap_or(cbox);
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
pub(super) fn boxes(
    dom: &Dom,
    root: &Frag<'_>,
    fixed: &[Frag<'_>],
    top_layer: &[TopFrag<'_>],
) -> (HashMap<NodeId, PxRect>, HashMap<NodeId, PxRect>) {
    // In-flow tree: its own boxes never include the fixed layer.
    let mut flow = Own::default();
    walk(root, &mut flow);

    // The pinned fixed layer: measured separately so a fixed header never
    // inflates the document's scrollable height (a fixed box is viewport-
    // relative, contributing no scroll overflow — CSS Overflow L3).
    let mut fx = Own::default();
    for f in fixed {
        walk(f, &mut fx);
    }
    // Top-layer boxes do not inflate the document scroll area, but CSSOM View
    // still reports their actual ICB-relative border boxes.
    for top in top_layer {
        walk(&top.fragment, &mut fx);
    }

    let mut out: HashMap<NodeId, PxRect> = HashMap::new();
    let mut scroll: HashMap<NodeId, PxRect> = HashMap::new();
    let flow_content = composed_union(dom, &flow.own, |_| true);
    select_into(
        dom,
        &flow_content,
        &flow.block,
        &flow.css_size,
        &mut out,
        &mut scroll,
    );
    if !fx.own.is_empty() {
        let fixed_nodes = fx.nodes;
        let fx_content = composed_union(dom, &fx.own, |id| fixed_nodes.contains(&id));
        select_into(
            dom,
            &fx_content,
            &fx.block,
            &fx.css_size,
            &mut out,
            &mut scroll,
        );
    }
    (out, scroll)
}
