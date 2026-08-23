//! Incremental-layout boundary emission (INCREMENTAL_LAYOUT_PLAN.md, P7).
//!
//! A relayout boundary is an element whose outer box is content-independent, so
//! a mutation confined to its subtree can be re-laid alone and spliced back
//! (`§13a`). v1 scope (matching the old engine's inline-boundary cut): a
//! BLOCK-FILLING independent-formatting-context container — `display:flex`/
//! `grid`/`flow-root`, in normal flow, NOT a flex/grid item, NOT a scroll/clip
//! box (scroll REGIONS are the Tier-1 region-patch domain). Such a box fills its
//! containing block's band (width-stable by construction), so only its HEIGHT
//! can change ⇒ Tier 2 (splice + shift).
//!
//! Geometry is read straight off the fragment tree — the boundary's fragment IS
//! its used border box. The coordinate convention (the patch re-lay in
//! `lay_subtree_fragment` mirrors it): `content_width`/`origin_col` are the
//! boundary's BORDER box (the re-lay suppresses the boundary's own margins and
//! lays at the border-box width, so the fragment's column 0 = the border-box
//! left the splice shifts to, and its content wraps at the same width).

use crate::dom::{Dom, NodeId};
use crate::layout2::BoundaryBox;

use super::flow::{Frag, FragKind};

/// Whether `node` is a v1 inline relayout boundary: a baked block-filling IFC
/// container. The cheap `data-trust-node` gate runs first (only boundaries carry
/// it), so the cascade queries below touch the sparse baked set, not every box.
pub(super) fn boundary_actor(dom: &Dom, node: NodeId) -> Option<usize> {
    let id = if dom.render_live() {
        node
    } else {
        dom.attr(node, "data-trust-node")
            .and_then(|s| s.parse::<usize>().ok())?
    };
    // A block-filling IFC: flex/grid/flow-root, NOT a scroll/clip viewport (the
    // region path owns those), in normal flow, and NOT a flex/grid ITEM (its
    // width is content-dependent — the parent sizes it).
    let disp = dom.effective_display(node)?;
    if !matches!(disp.trim(), "flex" | "grid" | "flow-root") {
        return None;
    }
    if dom.is_scroll_container(node) || dom.is_hscroll_container(node) {
        return None;
    }
    if super::style::Pos::of(dom, node).out_of_flow() {
        return None;
    }
    if dom
        .parent_composed(node)
        .and_then(|p| dom.computed_display(p))
        .is_some_and(|d| matches!(d.as_str(), "flex" | "inline-flex" | "grid" | "inline-grid"))
    {
        return None; // a flex/grid item — content-dependent width
    }
    Some(id)
}

/// Walk the fragment tree, emitting a `BoundaryBox` for every v1 inline boundary.
/// Called BEFORE paint extracts scroll regions (so the geometry is intact);
/// `mod.rs` then drops any boundary whose rows overlap a region/carousel band.
pub(super) fn collect(
    dom: &super::terminal::TerminalPaintModel,
    root: &Frag<'_>,
    cw: f32,
    ch: f32,
) -> Vec<BoundaryBox> {
    let mut out = Vec::new();
    let columns = (root.w / cw).round().max(1.0) as usize;
    let line_rows = super::terminal::line_row_map(dom, root, 0.0, 0.0, cw, ch, columns);
    walk(dom, root, cw, ch, &line_rows, &mut out);
    out
}

fn walk(
    dom: &super::terminal::TerminalPaintModel,
    f: &Frag<'_>,
    cw: f32,
    ch: f32,
    line_rows: &std::collections::HashMap<usize, super::terminal::TerminalLinePlacement>,
    out: &mut Vec<BoundaryBox>,
) {
    if f.node != crate::layout2::NO_NODE
        && matches!(f.kind, FragKind::Block)
        && let Some(node) = dom.boundary_actor(f.node)
    {
        let mut row0 = (f.y / ch).round().max(0.0) as usize;
        let mut row1 = ((f.y + f.h) / ch).round().max(0.0) as usize;
        let mut rows = Vec::new();
        descendant_line_rows(f, line_rows, &mut rows);
        if let Some(first) = rows.iter().min().copied() {
            row0 = row0.min(first.max(0) as usize);
        }
        if let Some(last) = rows.iter().max().copied() {
            row1 = row1.max(last.saturating_add(1).max(0) as usize);
        }
        let width = (f.w / cw).round().max(0.0) as u16;
        out.push(BoundaryBox {
            node,
            row_range: row0..row1.max(row0),
            origin_col: (f.x / cw).round().max(0.0) as u16,
            quantization_phase: (f.x.rem_euclid(cw), f.y.rem_euclid(ch)),
            content_width: width,
            width,
            sub_box: false,
        });
    }
    for c in &f.children {
        walk(dom, c, cw, ch, line_rows, out);
    }
}

fn descendant_line_rows(
    fragment: &Frag<'_>,
    line_rows: &std::collections::HashMap<usize, super::terminal::TerminalLinePlacement>,
    output: &mut Vec<i64>,
) {
    if matches!(fragment.kind, FragKind::Line(_))
        && let Some(row) = line_rows.get(&(std::ptr::from_ref(fragment) as usize))
    {
        output.push(row.row);
    }
    for child in &fragment.children {
        descendant_line_rows(child, line_rows, output);
    }
}
