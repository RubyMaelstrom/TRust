//! JS geometry from fragments (LAYOUT_OVERHAUL_PLAN.md, P7).
//!
//! `getBoundingClientRect`, `offset*`/`client*`, `scrollHeight`, and the
//! IntersectionObserver/ResizeObserver machinery all read one map:
//! `NodeId → PxRect` (border box in CSS px). The old engine reconstructed it
//! from *painted cells* plus a stack of heuristics (`element_tops` for empty
//! sentinels, `declared_boxes` floors, `clip_heights` caps). layout2 has REAL
//! stored geometry, so the map falls out of the fragment tree directly — the
//! plan's promise that "JS geometry reads the fragment tree, *more* accurate
//! than today".
//!
//! The single accommodation to CSSOM View: `scrollHeight`/`scrollWidth` read
//! the element's `__dom_rect` height/width (there is no separate stored value —
//! see `Dom::scroll_metric`), so a scroll container and the root element report
//! their CONTENT extent, while every ordinary block reports its own border box
//! (spec `getBoundingClientRect`). A composed-tree ancestor union supplies the
//! content extent and aggregates inline ancestors, empty containers, and shadow
//! hosts, exactly as the old engine's cell union did — but keyed off honest
//! fragment boxes.

use std::collections::{HashMap, HashSet};

use crate::dom::{DOCUMENT, Dom, NodeId};
use crate::layout::{NO_NODE, PxRect};

use super::flow::{Frag, FragKind};

/// A cell-quantized rectangle (inclusive-exclusive), the same integer grid the
/// paint pass stamps into, so a measured box round-trips to the cells that
/// rendered. `i64` tolerates content that overflows column 0 to the left.
#[derive(Copy, Clone)]
struct Cells {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

impl Cells {
    fn union(a: Cells, b: Cells) -> Cells {
        Cells {
            x0: a.x0.min(b.x0),
            y0: a.y0.min(b.y0),
            x1: a.x1.max(b.x1),
            y1: a.y1.max(b.y1),
        }
    }
}

/// Fold a rectangle into a `NodeId → Cells` map (union on collision — an
/// element that generates several fragments reports their bounding box).
fn add(map: &mut HashMap<NodeId, Cells>, node: NodeId, r: Cells) {
    map.entry(node)
        .and_modify(|c| *c = Cells::union(*c, r))
        .or_insert(r);
}

/// One box's own boxes: `own` = every directly-attributed box (block/replaced
/// border boxes + inline pieces, the union base); `block` = only the border
/// box a BLOCK-level fragment generates (the spec `getBoundingClientRect` for
/// a non-scroll block, used to re-cap the composed union). `nodes` collects
/// every element id touched (fixed-subtree membership).
#[derive(Default)]
struct Own {
    own: HashMap<NodeId, Cells>,
    block: HashMap<NodeId, Cells>,
    nodes: HashSet<NodeId>,
}

/// Walk a fragment (sub)tree, attributing each fragment's border box to its
/// generating element and each line piece to its item's element. `cw`/`ch` are
/// the cell size in px (the same the layout used, so edges snap consistently).
fn walk(f: &Frag<'_>, cw: f32, ch: f32, o: &mut Own) {
    if f.node != NO_NODE {
        o.nodes.insert(f.node);
        if matches!(f.kind, FragKind::Block) {
            let r = Cells {
                x0: (f.x / cw).round() as i64,
                y0: (f.y / ch).round() as i64,
                x1: ((f.x + f.w) / cw).round() as i64,
                y1: ((f.y + f.h) / ch).round() as i64,
            };
            add(&mut o.block, f.node, r);
            add(&mut o.own, f.node, r);
        }
    }
    if let FragKind::Line(pieces) = &f.kind {
        let base_col = (f.x / cw).round() as i64;
        let top_row = (f.y / ch).round() as i64;
        for p in pieces {
            if p.item.node == NO_NODE {
                continue;
            }
            let c0 = base_col + p.col as i64;
            let r0 = top_row + i64::from(p.row_off);
            let r = Cells {
                x0: c0,
                y0: r0,
                x1: c0 + i64::from(p.item.width),
                y1: r0 + i64::from(p.rows.max(1)),
            };
            o.nodes.insert(p.item.node);
            add(&mut o.own, p.item.node, r);
        }
    }
    for c in &f.children {
        walk(c, cw, ch, o);
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
    base: &HashMap<NodeId, Cells>,
    keep: impl Fn(NodeId) -> bool,
) -> HashMap<NodeId, Cells> {
    let mut content: HashMap<NodeId, Cells> = base
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
                acc = Some(acc.map_or(cr, |a| Cells::union(a, cr)));
            }
        }
        if let Some(acc) = acc {
            content.insert(id, acc);
        }
    }
    content
}

/// Select each node's reported box from the composed union `content` and the
/// own block boxes: an ordinary block reports its OWN border box (spec
/// `getBoundingClientRect`); a scroll container or the root element (`html`/
/// `body`) reports the content-tall union (so `scrollHeight`/`scrollWidth`,
/// which read this rect, are the scrollable content extent — CSSOM View).
fn select_into(
    dom: &Dom,
    content: &HashMap<NodeId, Cells>,
    block: &HashMap<NodeId, Cells>,
    cpw: f64,
    cph: f64,
    out: &mut HashMap<NodeId, PxRect>,
) {
    for (&node, &cbox) in content {
        let own_box = block.get(&node);
        let extend = own_box.is_none()
            || dom.is_scroll_container(node)
            || dom.is_hscroll_container(node)
            || matches!(dom.tag_name(node), Some("html" | "body"));
        let c = if extend { cbox } else { *own_box.unwrap() };
        out.insert(
            node,
            PxRect {
                left: c.x0 as f64 * cpw,
                top: c.y0 as f64 * cph,
                width: (c.x1 - c.x0) as f64 * cpw,
                height: (c.y1 - c.y0) as f64 * cph,
            },
        );
    }
}

/// Build the `NodeId → PxRect` geometry map from the laid fragment tree (the
/// in-flow root + the pinned fixed layer). `cpw`/`cph` are the px cell size the
/// caller wants the output in (the session cell metrics — the same used to lay
/// the tree, so the round-trip is exact).
pub(super) fn boxes(
    dom: &Dom,
    root: &Frag<'_>,
    fixed: &[Frag<'_>],
    cw: f32,
    ch: f32,
    cpw: f64,
    cph: f64,
) -> HashMap<NodeId, PxRect> {
    // In-flow tree: its own boxes never include the fixed layer.
    let mut flow = Own::default();
    walk(root, cw, ch, &mut flow);

    // The pinned fixed layer: measured separately so a fixed header never
    // inflates the document's scrollable height (a fixed box is viewport-
    // relative, contributing no scroll overflow — CSS Overflow L3).
    let mut fx = Own::default();
    for f in fixed {
        walk(f, cw, ch, &mut fx);
    }

    let mut out: HashMap<NodeId, PxRect> = HashMap::new();
    let flow_content = composed_union(dom, &flow.own, |_| true);
    select_into(dom, &flow_content, &flow.block, cpw, cph, &mut out);
    if !fx.own.is_empty() {
        let fixed_nodes = fx.nodes;
        let fx_content = composed_union(dom, &fx.own, |id| fixed_nodes.contains(&id));
        select_into(dom, &fx_content, &fx.block, cpw, cph, &mut out);
    }
    out
}
