//! Paint: the fragment tree → `Doc.rows` in CSS 2.1 **Appendix E** order,
//! composited at CELL granularity — overlaps are allowed and correct.
//!
//! Two stages:
//!
//! 1. **Display list** (`build_sc`): the fragment tree walks as a stacking-
//!    context tree in the Appendix E painting order — the SC's background,
//!    negative-z child SCs, in-flow block backgrounds (tree order), in-flow
//!    inline content (tree order), the merged z:auto/z:0 positioned list
//!    (z:auto boxes as pseudo-stacking-contexts whose positioned descendants
//!    elevate — E step 8), positive-z child SCs. Backgrounds are OPAQUE
//!    FILLS: the terminal paints no color, but a declared background erases
//!    what's beneath it in paint order (the modal/card-stack semantics).
//!
//! 2. **Compositor** (`Cells`): ops stamp per-row cell spans in list order —
//!    the later op owns the cell (the painter's algorithm). At emission a
//!    text run keeps only its surviving cells (split into segments); a
//!    DECODED image is atomic — fully covered it drops, partially covered it
//!    survives whole and its box stays opaque (text over a surviving image's
//!    pixels is dropped: the renderer's image pass paints pixels over those
//!    cells regardless; pixel-true compositing is the P8 polish).
//!    Paint-suppressed (invisible) items are GHOSTS: they claim only
//!    otherwise-free cells and never erase visible content.
//!
//! The single px→cell quantizer also lives here: EDGES snap to the cell grid
//! (`round(edge / cell)`), never sizes. The ONE structural terminal
//! constraint applies at this boundary: the document cannot scroll
//! horizontally, so content crossing the viewport's right edge is clipped
//! (and symmetrically at column 0 for negative overhang).
//!
//! `position:fixed` fragments paint through the same pipeline into
//! `FixedItem` row buffers (the pinned layer the renderer composites over
//! the scrolling document), ordered by stack level.

use std::collections::HashMap;

use crate::dom::{Dom, NodeId};
use crate::layout2::{
    Carousel, CompositeLayer, Emphasis, FixedItem, Item, ItemKind, NO_NODE, Region, Row,
    display_width, truncate_to_width,
};

/// The overlap-composite side-table produced by a paint pass (P8): a synthetic
/// `x-trust-composite:` URL → the layers the app alpha-blends into that box.
pub(crate) type Composites = HashMap<String, Vec<CompositeLayer>>;

use super::flow::{Clip, Frag, FragKind};
use super::style::{BOTTOM, LEFT, RIGHT, TOP};

pub(crate) struct PaintOut {
    pub rows: Vec<Row>,
    /// Fragment scroll targets: element `id`/`<a name>` → first row.
    pub anchor_rows: HashMap<String, usize>,
    /// The pinned `position:fixed` layer, in stack-level order (the renderer
    /// draws the vec in order, so higher z paints later = on top).
    pub fixed: Vec<FixedItem>,
    /// Vertical inner-scroll viewports (`overflow-y: auto|scroll` on a
    /// definite-height box whose content overflows). Each holds its scrollable
    /// content in a separate `buffer` the renderer windows over a reserved band
    /// of blank doc rows — the document stays flat (CSS Overflow L3 §2/§3).
    pub regions: Vec<Region>,
    /// `(live actor node, clientHeight rows, scrollport width cells)` for each
    /// scroll region — the app pushes clientHeight to the page's `element`
    /// scroll geometry (CSSOM View).
    pub scroll_clips: Vec<(usize, u16, u16)>,
    /// Horizontal scroll strips (`overflow-x: auto|scroll` whose content
    /// overflows). Items stay in the doc rows at their strip columns; the
    /// renderer shifts/clips them to the band via `visible_col`.
    pub carousels: Vec<Carousel>,
    /// Alpha-composited image overlap groups (P8): synthetic `x-trust-composite:`
    /// URL → ordered layers. A composite `Item` in `rows` (or a region/carousel
    /// buffer) carries the synthetic URL; the app encodes it from these layers.
    pub composites: Composites,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    dom: &Dom,
    root: &mut Frag<'_>,
    fixed: &[Frag<'_>],
    flow_bottom: f32,
    anchors: &[(NodeId, f32)],
    viewport: (usize, usize),
    cell_w: f32,
    cell_h: f32,
    // `alpha` = URL→`has_alpha` from the app's decoded cache; the overlap
    // compositor groups only overlaps where an upper image is transparent.
    alpha: &HashMap<String, bool>,
) -> PaintOut {
    let cols = viewport.0;
    // The overlap-composite side-table, filled by every `composite` call below
    // (main pass, scroll-region buffers, carousel strips, the fixed layer).
    let mut composites: Composites = HashMap::new();
    // Extract scroll containers FIRST. A vertical REGION paints its content
    // into a separate buffer and empties its frag (the main pass leaves a blank
    // band the renderer windows over the buffer). A horizontal CAROUSEL paints
    // its strip at the strip width (items keep their full columns) into a splice
    // spliced into the main rows after compositing; the renderer shifts/clips it
    // to the band via `visible_col`. CSS Overflow L3 §2/§3.
    let mut regions: Vec<Region> = Vec::new();
    let mut carousels: Vec<Carousel> = Vec::new();
    let mut scroll_clips: Vec<(usize, u16, u16)> = Vec::new();
    let mut splices: Vec<(usize, Vec<Row>)> = Vec::new();
    extract_scrollers(
        dom,
        root,
        cell_w,
        cell_h,
        0.0,
        0.0,
        &mut regions,
        &mut carousels,
        &mut scroll_clips,
        &mut splices,
        alpha,
        &mut composites,
    );
    // The document height, computed AFTER extraction so an emptied region
    // contributes only its reserved band height (not its scrolled-away content).
    let doc_h_px = root.max_bottom().max(flow_bottom).max(0.0);
    let mut ops = Vec::new();
    build_sc(root, &mut ops, cell_w, cell_h, 0.0, 0.0);
    let mut rows = composite(ops, cols, alpha, &mut composites);
    // Splice each carousel's strip rows over its (now-blank) band — the strip
    // items keep their full strip columns (possibly past the viewport), which
    // the renderer windows to the band. We inject NO scroll chrome: the page's
    // own controls (if any) render where the page put them, and the UA scroll
    // affordance (wheel/keys over the strip) is behavioural, like a scrollbar —
    // never content we synthesise. The page defines itself.
    for (start, strip) in splices {
        for (i, srow) in strip.into_iter().enumerate() {
            let r = start + i;
            while rows.len() <= r {
                rows.push(Row::default());
            }
            rows[r] = srow;
        }
    }
    // Fragment geometry → anchor targets: the topmost row each element's box
    // reaches (paint-independent — covered boxes stay scroll targets), plus
    // the IFC entry marks for boxes that emitted no cells.
    let mut node_rows: HashMap<NodeId, usize> = HashMap::new();
    collect_node_rows(root, cell_h, &mut node_rows);
    for &(node, y) in anchors {
        let row = ((y / cell_h).round() as i64).max(0) as usize;
        node_rows
            .entry(node)
            .and_modify(|r| *r = (*r).min(row))
            .or_insert(row);
    }
    // The document's quantized height (trailing margins/padding included).
    let total = (doc_h_px / cell_h).round().max(0.0) as usize;
    while rows.len() < total {
        rows.push(Row::default());
    }
    let mut anchor_rows: HashMap<String, usize> = HashMap::new();
    let mut note = |name: &str, row: usize| {
        anchor_rows
            .entry(name.to_string())
            .and_modify(|r| *r = (*r).min(row))
            .or_insert(row);
    };
    for (&node, &row) in &node_rows {
        if let Some(id) = dom.attr(node, "id").filter(|v| !v.is_empty()) {
            note(id, row);
        }
        if dom.tag_name(node) == Some("a")
            && let Some(name) = dom.attr(node, "name").filter(|v| !v.is_empty())
        {
            note(name, row);
        }
    }
    // The pinned layer: each fixed box paints through the same pipeline into
    // its own position-independent row buffer, at its viewport position.
    // Stable stack-level sort: the renderer draws the vec in order.
    let mut order: Vec<usize> = (0..fixed.len()).collect();
    order.sort_by_key(|&i| fixed[i].paint.z.unwrap_or(0));
    let vp_rows = viewport.1;
    let fixed_items = order
        .into_iter()
        .filter_map(|i| {
            let f = &fixed[i];
            let col = ((f.x / cell_w).round() as i64).max(0) as usize;
            let mut row = ((f.y / cell_h).round() as i64).max(0) as usize;
            if vp_rows > 0 {
                row = row.min(vp_rows.saturating_sub(1));
            }
            let mut ops = Vec::new();
            build_sc(f, &mut ops, cell_w, cell_h, f.x, f.y);
            let brows = composite(ops, cols.saturating_sub(col).max(1), alpha, &mut composites);
            if brows.iter().all(|r| r.items.is_empty()) {
                return None; // nothing visible: no pinned surface
            }
            Some(FixedItem {
                col: col.min(u16::MAX as usize) as u16,
                row: row.min(u16::MAX as usize) as u16,
                rows: brows,
                z: f.paint.z.unwrap_or(0),
            })
        })
        .collect();
    PaintOut {
        rows,
        anchor_rows,
        fixed: fixed_items,
        regions,
        scroll_clips,
        carousels,
        composites,
    }
}

// ---------------------------------------------------------------------------
// Scroll-region extraction (CSS Overflow L3 §2/§3, CSSOM View scrolling).
// ---------------------------------------------------------------------------

/// Walk the fragment tree extracting scroll containers. A vertical REGION
/// paints its subtree into a buffer, records a `Region` (+ `scroll_clips`), and
/// EMPTIES the frag so the main pass leaves a blank band the renderer windows.
/// A horizontal CAROUSEL paints its strip (records a `Carousel` + a splice of
/// strip rows) and empties the frag too. A scroller nested inside an extracted
/// one's buffer/strip is NOT re-extracted here (it shows within the parent's
/// scrollable content — the documented single-level limitation); everything
/// else recurses.
/// `ox`/`oy` are the current coordinate FRAME origin in px (0,0 at the document
/// level; a parent region's padding-box top-left when extracting scrollers
/// nested inside that region's buffer) — so a nested scroller's band/splice
/// come out relative to the buffer it lives in.
#[allow(clippy::too_many_arguments)]
fn extract_scrollers(
    dom: &Dom,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    regions: &mut Vec<Region>,
    carousels: &mut Vec<Carousel>,
    scroll_clips: &mut Vec<(usize, u16, u16)>,
    splices: &mut Vec<(usize, Vec<Row>)>,
    alpha: &HashMap<String, bool>,
    composites: &mut Composites,
) {
    let mut i = 0;
    while i < f.children.len() {
        // A vertical region wins over a horizontal one on a 2D scroller (rare):
        // its buffer clips the horizontal overflow to the scrollport.
        if is_scroll_region(dom, &f.children[i]) {
            regions.push(paint_region(
                dom,
                &mut f.children[i],
                cw,
                ch,
                ox,
                oy,
                scroll_clips,
                alpha,
                composites,
            ));
        } else if is_carousel(dom, &f.children[i], cw) {
            paint_carousel(
                dom,
                &mut f.children[i],
                cw,
                ch,
                ox,
                oy,
                carousels,
                splices,
                alpha,
                composites,
            );
        } else {
            extract_scrollers(
                dom,
                &mut f.children[i],
                cw,
                ch,
                ox,
                oy,
                regions,
                carousels,
                scroll_clips,
                splices,
                alpha,
                composites,
            );
        }
        i += 1;
    }
}

/// Whether `f` is a horizontal scroll strip: an `overflow-x: auto|scroll`
/// element (CSS Overflow L3 §2) whose content overflows its padding box to the
/// right (there is scrollable overflow to window). Not the document root.
fn is_carousel(dom: &Dom, f: &Frag<'_>, cw: f32) -> bool {
    if f.node == NO_NODE || matches!(dom.tag_name(f.node), Some("html" | "body")) {
        return false;
    }
    if !dom.is_hscroll_container(f.node) {
        return false;
    }
    let pad_right = f.x + f.w - f.border[RIGHT];
    content_right_px(f, cw) > pad_right + 0.5
}

/// The right edge (absolute px) of a box's in-flow content — the max over its
/// direct children of each child's own right edge. A LINE box carries its
/// extent in its `Piece`s (columns relative to `x`), NOT in its `w` (which is
/// 0), so a `white-space:pre` block's long line is measured from the rightmost
/// piece; a block/replaced child uses its border-box right edge. This is what
/// makes an `overflow-x:auto` element with overflowing INLINE content — a
/// `<pre><code>` code block, `white-space:pre` text — a horizontal scroll strip
/// (CSS Overflow L3 §2 scrollable overflow), not only a row of overflowing
/// child boxes.
fn content_right_px(f: &Frag<'_>, cw: f32) -> f32 {
    f.children
        .iter()
        .map(|c| match &c.kind {
            FragKind::Line(pieces) => {
                let cells = pieces
                    .iter()
                    .map(|p| p.col + usize::from(p.item.width))
                    .max()
                    .unwrap_or(0);
                c.x + cells as f32 * cw
            }
            _ => c.x + c.w,
        })
        .fold(f32::MIN, f32::max)
}

/// Paint one horizontal scroll strip: composite its content at the strip width
/// (items keep their full strip columns), record a `Carousel` and a splice of
/// the strip rows, then EMPTY the frag. Snapping is honored per CSS Scroll Snap
/// 1: the strip only card-SNAPS when the container declares an inline
/// `scroll-snap-type`, and each snap stop is a card's `scroll-snap-align`
/// position (start/center/end) — a card with `none`/unset contributes none.
/// Otherwise it scrolls freely. No guessed card sizing.
#[allow(clippy::too_many_arguments)]
fn paint_carousel(
    dom: &Dom,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    carousels: &mut Vec<Carousel>,
    splices: &mut Vec<(usize, Vec<Row>)>,
    alpha: &HashMap<String, bool>,
    composites: &mut Composites,
) {
    let pad_x = f.x + f.border[LEFT];
    let pad_w = (f.w - f.border[LEFT] - f.border[RIGHT]).max(0.0);
    // Band geometry in the CURRENT frame (0,0 at the document level; a parent
    // region's origin when this strip is a shelf nested inside that region).
    let start_row = ((f.y - oy) / ch).round().max(0.0) as usize;
    let band_left = ((pad_x - ox) / cw).round().max(0.0) as usize;
    let scrollport = (pad_w / cw).round().max(1.0) as usize; // the visible band
    // The strip's scrollable extent (widest child right edge — a LINE box
    // contributes its inline pieces' extent, so a `white-space:pre` code block
    // scrolls its long lines, not only wide child boxes), strip-relative.
    let content_right = content_right_px(f, cw);
    let strip_w = (((content_right - pad_x) / cw).round().max(1.0)) as usize;
    // Snapping: only when the container declares an inline-axis scroll-snap-type
    // (x / inline / both). Its snap positions come from the cards' own
    // scroll-snap-align.
    let inline_snaps = dom
        .computed_value(f.node, "scroll-snap-type")
        .is_some_and(|v| {
            matches!(
                v.split_whitespace()
                    .next()
                    .unwrap_or("none")
                    .to_ascii_lowercase()
                    .as_str(),
                "x" | "inline" | "both"
            )
        });
    let sp = scrollport as f32;
    let mut stops: Vec<u16> = Vec::new();
    for c in f.children.iter().filter(|c| c.w > 0.0 && c.node != NO_NODE) {
        let left = (c.x - pad_x) / cw;
        let right = (c.x + c.w - pad_x) / cw;
        let align = dom
            .computed_value(c.node, "scroll-snap-align")
            .and_then(|s| s.split_whitespace().last().map(str::to_ascii_lowercase));
        let stop = match align.as_deref() {
            Some("start") => left,
            Some("center") => (left + right) / 2.0 - sp / 2.0,
            Some("end") => right - sp,
            _ => continue, // none / unset: this card is not a snap position
        };
        stops.push(stop.round().max(0.0) as u16);
    }
    stops.sort_unstable();
    stops.dedup();
    let snap = inline_snaps && !stops.is_empty();
    // Composite the strip at the current-FRAME columns (`ox`), rows relative to
    // the band top (`oy`), wide enough to keep every card's full columns — so
    // the strip items land at frame columns `band_left + strip_x` for the
    // renderer's `visible_col` to window, and the splice lands in the frame's
    // rows (the document, or the parent region's buffer).
    let strip_cols = ((content_right - ox) / cw).round().max(1.0) as usize;
    let mut ops = Vec::new();
    build_sc(f, &mut ops, cw, ch, ox, f.y);
    let strip = composite(ops, strip_cols, alpha, composites);
    f.children.clear();
    let end = start_row + strip.len();
    splices.push((start_row, strip));
    carousels.push(Carousel {
        start: start_row,
        end,
        left: band_left.min(u16::MAX as usize) as u16,
        right: (band_left + scrollport).min(u16::MAX as usize) as u16,
        width: strip_w.min(u16::MAX as usize) as u16,
        stops,
        offset: 0,
        frame_right: None,
        snap,
    });
}

/// A re-laid scroll region's `(buffer rows, nested carousels, nested
/// scroll-clip clientHeights)` — the incremental region-patch payload.
pub(crate) type RegionBuffer = (Vec<Row>, Vec<Carousel>, Vec<(usize, u16, u16)>);

/// Lay one scroll region's subtree into its scrollable buffer for an
/// incremental region PATCH (INCREMENTAL_LAYOUT_PLAN.md). `root` is the region
/// node laid AS a fragment root (`lay_region_fragment`); this composites its
/// content exactly as the full-render extraction does (`paint_region` — same
/// scrollport origin, nested-scroller extraction, snap stops), so the patched
/// buffer is byte-consistent with a full relayout of the same content. Returns
/// `(buffer rows, nested carousels, nested scroll-clip clientHeights)`; nested
/// vertical regions inside a patched region are dropped in v1 (they reappear on
/// the next full render — the old engine's region patch does the same).
pub(crate) fn region_buffer(dom: &Dom, root: &mut Frag<'_>, cw: f32, ch: f32) -> RegionBuffer {
    let mut scroll_clips = Vec::new();
    // v1 region-patch cut: the incremental region re-lay does NOT alpha-composite
    // transparent image overlaps (empty alpha ⇒ no grouping) — such overlaps in a
    // patched region render separately until the next full render. Always
    // correct, matching the other P7 region-patch v1 cuts.
    let no_alpha: HashMap<String, bool> = HashMap::new();
    let mut composites: Composites = HashMap::new();
    let rg = paint_region(
        dom,
        root,
        cw,
        ch,
        0.0,
        0.0,
        &mut scroll_clips,
        &no_alpha,
        &mut composites,
    );
    // `paint_region` pushed this region's OWN clientHeight into `scroll_clips`
    // (its `live_node`); the app already knows this region's geometry, so drop
    // the self entry and keep only the NESTED scrollers' clips.
    let self_node: Option<usize> = dom
        .attr(root.node, "data-trust-node")
        .and_then(|s| s.parse().ok());
    scroll_clips.retain(|&(n, _, _)| Some(n) != self_node);
    (rg.buffer, rg.carousels, scroll_clips)
}

/// Whether `f` is a vertical scroll region: an `overflow-y: auto|scroll`
/// element (CSS Overflow L3 §2) whose content overflows its padding box (so
/// there is scrollable overflow), other than the document root itself (the
/// viewport is never a nested region — CSS Overflow L3 §3.3 governs ITS
/// overflow separately, from `html`'s own value alone, never a descendant's).
/// Establishing a scroll container is unconditional on any other element —
/// there is no spec concept of a distinguished "principal" scroller that
/// escapes this and flows into the document instead. A page that locks its
/// own viewport (`html{overflow:hidden}`) gets exactly what that says: the
/// viewport doesn't scroll, and every genuine `overflow:auto|scroll`
/// descendant — however many, however deep — is its own bounded region,
/// scrolled independently (hover + wheel), same as a real browser's own
/// nested scrollports.
fn is_scroll_region(dom: &Dom, f: &Frag<'_>) -> bool {
    if f.node == NO_NODE || matches!(dom.tag_name(f.node), Some("html" | "body")) {
        return false;
    }
    if !dom.is_scroll_container(f.node) {
        return false;
    }
    // Scrollable overflow: a descendant border box reaches past the padding
    // box's bottom edge (CSSOM View: the scrolling area's bottom is the
    // bottom-most of the padding edge and the descendants' margin edges).
    let pad_bottom = f.y + f.h - f.border[BOTTOM];
    let content_bottom = f
        .children
        .iter()
        .map(Frag::max_bottom)
        .fold(f32::MIN, f32::max);
    content_bottom > pad_bottom + 0.5
}

/// Paint one scroll region's subtree into its buffer at the scrollport width,
/// record its geometry, and EMPTY the frag (leaving an `h`-tall blank band).
/// The scrollport is the padding box and the scroll origin is its top-left
/// padding edge (CSS Overflow L3 §2, CSSOM View). Scrollers NESTED inside this
/// region are extracted recursively into the returned `Region` (buffer-relative
/// coords) so each is independently scrollable within this region's window.
/// `ox`/`oy` = the frame this region lives in (0,0 at the document level).
#[allow(clippy::too_many_arguments)]
fn paint_region(
    dom: &Dom,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    scroll_clips: &mut Vec<(usize, u16, u16)>,
    alpha: &HashMap<String, bool>,
    composites: &mut Composites,
) -> Region {
    let pad_x = f.x + f.border[LEFT];
    let pad_y = f.y + f.border[TOP];
    let pad_w = (f.w - f.border[LEFT] - f.border[RIGHT]).max(0.0);
    let pad_h = (f.h - f.border[TOP] - f.border[BOTTOM]).max(0.0);
    // Band geometry in the CURRENT frame (the parent buffer, or the document).
    let start_row = ((pad_y - oy) / ch).round().max(0.0) as usize;
    let left = ((pad_x - ox) / cw).round().max(0.0) as usize;
    let width = (pad_w / cw).round().max(1.0) as usize;
    let height = (pad_h / ch).round().max(0.0) as usize; // clientHeight
    // Extract scrollers nested inside this region FIRST, in the BUFFER frame
    // (origin = this region's padding-box top-left = its scroll origin), so
    // they empty their frags before we composite and come out buffer-relative.
    let mut n_regions: Vec<Region> = Vec::new();
    let mut n_carousels: Vec<Carousel> = Vec::new();
    let mut n_splices: Vec<(usize, Vec<Row>)> = Vec::new();
    extract_scrollers(
        dom,
        f,
        cw,
        ch,
        pad_x,
        pad_y,
        &mut n_regions,
        &mut n_carousels,
        scroll_clips, // nested clientHeights bubble up to the doc's scroll_clips
        &mut n_splices,
        alpha,
        composites,
    );
    // Paint the region's content into its buffer, origin at the padding-box
    // top-left (the scroll origin), clipped to the scrollport WIDTH — the
    // scroll axis (height) is unbounded so the buffer holds the full content.
    let mut ops = Vec::new();
    build_sc(f, &mut ops, cw, ch, pad_x, pad_y);
    let mut buffer = composite(ops, width, alpha, composites);
    // Splice nested carousel strips over their (blank) bands in this buffer.
    for (s, strip) in n_splices {
        for (i, srow) in strip.into_iter().enumerate() {
            let r = s + i;
            while buffer.len() <= r {
                buffer.push(Row::default());
            }
            buffer[r] = srow;
        }
    }
    let content_h = buffer.len(); // scrollHeight
    // Empty the frag: the main pass now leaves `height` blank rows for the band.
    f.children.clear();
    // The page's own scrollTop signal (baked `data-trust-scroll-top`, in rows)
    // seeds the offset, clamped to [0, scrollHeight − clientHeight] (CSSOM
    // View); its `data-trust-node` correlates the region with the live actor
    // element for the geometry round-trip + wheel write-back.
    let live_node: Option<usize> = dom
        .attr(f.node, "data-trust-node")
        .and_then(|s| s.parse().ok());
    let max_voffset = content_h.saturating_sub(height);
    let signal = dom
        .attr(f.node, "data-trust-scroll-top")
        .and_then(|s| s.parse::<usize>().ok());
    let voffset = signal.map_or(0, |r| r.min(max_voffset));
    if let Some(node) = live_node {
        scroll_clips.push((node, height as u16, width as u16));
    }
    Region {
        node: f.node,
        start_row,
        left: left.min(u16::MAX as usize) as u16,
        width: width.min(u16::MAX as usize) as u16,
        height: height.min(u16::MAX as usize) as u16,
        buffer,
        voffset,
        live_node,
        voffset_from_page: signal.is_some(),
        // The one scroll region a locked viewport delegates the page scroll to
        // (SPA app shell). A nested region can never be principal — the walk
        // finds its scroll-container ancestor and returns false — so setting it
        // here for every extracted region is correct at any depth.
        principal: dom.is_principal_scroller(f.node),
        carousels: n_carousels,
        regions: n_regions,
        // Region image-reflow routing (P7 incremental layout) is not populated
        // yet — regions render + scroll without it.
        image_urls: Vec::new(),
    }
}

/// Topmost row each element's fragment reaches, over the whole tree.
fn collect_node_rows(f: &Frag<'_>, cell_h: f32, out: &mut HashMap<NodeId, usize>) {
    if f.node != NO_NODE {
        let row = ((f.y / cell_h).round() as i64).max(0) as usize;
        out.entry(f.node)
            .and_modify(|r| *r = (*r).min(row))
            .or_insert(row);
    }
    if let FragKind::Line(pieces) = &f.kind {
        for p in pieces {
            if p.item.node != NO_NODE {
                let row = (((f.y / cell_h).round() as i64
                    + i64::from(p.row_off)
                    + i64::from(p.box_off_rows))
                .max(0)) as usize;
                out.entry(p.item.node)
                    .and_modify(|r| *r = (*r).min(row))
                    .or_insert(row);
            }
        }
    }
    for c in &f.children {
        collect_node_rows(c, cell_h, out);
    }
}

// ---------------------------------------------------------------------------
// Stage 1: the Appendix E display list.
// ---------------------------------------------------------------------------

/// A clip rectangle in CELL coordinates (half-open rows/cols) — the px clip a
/// fragment carries, quantized once at the paint boundary. An unclipped axis
/// is `i64::MIN..i64::MAX` (Rust saturates `±∞ as i64` to those bounds), so a
/// `None` clip is the whole plane and intersection is a plain `max`/`min`.
#[derive(Copy, Clone)]
struct ClipCells {
    r0: i64,
    r1: i64,
    c0: i64,
    c1: i64,
}

const FULL_CLIP: ClipCells = ClipCells {
    r0: i64::MIN,
    r1: i64::MAX,
    c0: i64::MIN,
    c1: i64::MAX,
};

/// Quantize a fragment's px clip to cell bounds (edges snap, like every other
/// px→cell conversion here). `ox`/`oy` shift for the box-relative pinned layer.
fn clip_cells(clip: Option<Clip>, ox: f32, oy: f32, cw: f32, ch: f32) -> ClipCells {
    match clip {
        None => FULL_CLIP,
        Some(c) => ClipCells {
            r0: ((c.y0 - oy) / ch).round() as i64,
            r1: ((c.y1 - oy) / ch).round() as i64,
            c0: ((c.x0 - ox) / cw).round() as i64,
            c1: ((c.x1 - ox) / cw).round() as i64,
        },
    }
}

/// One display-list entry, in painting order. Each carries the effective clip
/// (the fragment's containing-block clip chain) the compositor intersects with
/// the viewport before stamping.
enum Op {
    /// An opaque background fill over a cell rect (half-open rows/cols).
    Fill {
        row0: i64,
        row1: i64,
        col0: i64,
        col1: i64,
        clip: ClipCells,
    },
    /// One placed inline item at absolute (row, col).
    Item {
        row: i64,
        col: i64,
        item: Item,
        clip: ClipCells,
    },
}

/// Paint one STACKING CONTEXT per Appendix E (the root element always forms
/// one). `ox`/`oy` shift the coordinate origin (the pinned layer paints in
/// box-relative coordinates).
fn build_sc(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    // E.2 step 1/2: the element's own background.
    fill_op(f, ops, cw, ch, ox, oy);
    // Gather this SC's positioned/SC descendants (piercing pseudo-stacking
    // contexts — their positioned descendants belong to THIS context).
    let mut neg: Vec<&Frag<'_>> = Vec::new();
    let mut zero: Vec<(&Frag<'_>, bool)> = Vec::new(); // (frag, is_real_sc)
    let mut pos: Vec<&Frag<'_>> = Vec::new();
    collect_positioned(f, &mut neg, &mut zero, &mut pos);
    neg.sort_by_key(|c| c.paint.z.unwrap_or(0)); // stable: tree order within z
    pos.sort_by_key(|c| c.paint.z.unwrap_or(0));
    // E.2 step 3: negative-z stacking contexts, most negative first.
    for c in neg {
        build_sc(c, ops, cw, ch, ox, oy);
    }
    // E.2 step 4: in-flow, non-positioned block-level backgrounds, tree order.
    inflow_bgs(f, ops, cw, ch, ox, oy);
    // E.2 step 5: non-positioned floats, tree order (§9.5) — each as its own
    // pseudo stacking context.
    build_floats(f, ops, cw, ch, ox, oy);
    // E.2 step 7: in-flow, non-positioned inline content, tree order.
    inflow_content(f, ops, cw, ch, ox, oy);
    // E.2 step 8: z:auto positioned (pseudo) and z:0 SCs, one merged
    // tree-order list.
    for (c, is_sc) in zero {
        if is_sc {
            build_sc(c, ops, cw, ch, ox, oy);
        } else {
            build_pseudo(c, ops, cw, ch, ox, oy);
        }
    }
    // E.2 step 9: positive-z stacking contexts, smallest first.
    for c in pos {
        build_sc(c, ops, cw, ch, ox, oy);
    }
}

/// A positioned z:auto box: painted atomically for its own background and
/// in-flow content, but its positioned descendants and child SCs were lifted
/// into the enclosing real stacking context (E.2 step 8).
fn build_pseudo(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    fill_op(f, ops, cw, ch, ox, oy);
    inflow_bgs(f, ops, cw, ch, ox, oy);
    build_floats(f, ops, cw, ch, ox, oy);
    inflow_content(f, ops, cw, ch, ox, oy);
}

/// Appendix E step 5: paint every non-positioned float in `f`'s subtree, in
/// tree order, each as its own pseudo stacking context — its background, its
/// in-flow content, and its own nested floats (its positioned/SC descendants
/// belong to the enclosing real stacking context, collected there, §9.5). The
/// walk descends through plain in-flow boxes but treats a float atomically; a
/// float that is itself positioned or forms a stacking context is left to the
/// normal positioned/SC path (its `sc`/`positioned` flag wins).
fn build_floats(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    for c in &f.children {
        if c.paint.sc || c.paint.positioned {
            continue;
        }
        if c.paint.float {
            fill_op(c, ops, cw, ch, ox, oy);
            inflow_bgs(c, ops, cw, ch, ox, oy);
            build_floats(c, ops, cw, ch, ox, oy);
            inflow_content(c, ops, cw, ch, ox, oy);
        } else {
            build_floats(c, ops, cw, ch, ox, oy);
        }
    }
}

/// Bucket the positioned/SC descendants of `f` by stack level, descending
/// through in-flow boxes AND pseudo-stacking-contexts (whose positioned
/// descendants participate here), never into real SCs (atomic).
fn collect_positioned<'f, 't>(
    f: &'f Frag<'t>,
    neg: &mut Vec<&'f Frag<'t>>,
    zero: &mut Vec<(&'f Frag<'t>, bool)>,
    pos: &mut Vec<&'f Frag<'t>>,
) {
    for c in &f.children {
        if c.paint.sc {
            match c.paint.z.unwrap_or(0) {
                z if z < 0 => neg.push(c),
                0 => zero.push((c, true)),
                _ => pos.push(c),
            }
            continue; // atomic — its own build_sc paints its subtree
        }
        if c.paint.positioned {
            zero.push((c, false));
            // Pierce: its positioned/SC descendants belong to this SC.
            collect_positioned(c, neg, zero, pos);
            continue;
        }
        collect_positioned(c, neg, zero, pos);
    }
}

/// In-flow, non-positioned block-level backgrounds, tree order (E.2 step 4).
/// Floats paint as a unit in step 5 (`build_floats`), so they're skipped here.
fn inflow_bgs(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    for c in &f.children {
        if c.paint.sc || c.paint.positioned || c.paint.float {
            continue;
        }
        if matches!(c.kind, FragKind::Block) {
            fill_op(c, ops, cw, ch, ox, oy);
        }
        inflow_bgs(c, ops, cw, ch, ox, oy);
    }
}

/// In-flow, non-positioned inline content, tree order (E.2 step 7). Floats
/// paint as a unit in step 5 (`build_floats`), so they're skipped here.
fn inflow_content(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    for c in &f.children {
        if c.paint.sc || c.paint.positioned || c.paint.float {
            continue;
        }
        if let FragKind::Line(pieces) = &c.kind {
            let base_col = ((c.x - ox) / cw).round() as i64;
            let top_row = ((c.y - oy) / ch).round() as i64;
            let clip = clip_cells(c.clip, ox, oy, cw, ch);
            for p in pieces {
                ops.push(Op::Item {
                    row: top_row + i64::from(p.row_off) + i64::from(p.box_off_rows),
                    col: base_col + p.col as i64,
                    item: p.item.clone(),
                    clip,
                });
            }
        }
        inflow_content(c, ops, cw, ch, ox, oy);
    }
}

/// The opaque background fill of a fragment's border box, when it has one.
fn fill_op(f: &Frag<'_>, ops: &mut Vec<Op>, cw: f32, ch: f32, ox: f32, oy: f32) {
    if !f.paint.bg {
        return;
    }
    let row0 = ((f.y - oy) / ch).round() as i64;
    let row1 = ((f.y - oy + f.h) / ch).round() as i64;
    let col0 = ((f.x - ox) / cw).round() as i64;
    let col1 = ((f.x - ox + f.w) / cw).round() as i64;
    if row1 > row0 && col1 > col0 {
        ops.push(Op::Fill {
            row0,
            row1,
            col0,
            col1,
            clip: clip_cells(f.clip, ox, oy, cw, ch),
        });
    }
}

// ---------------------------------------------------------------------------
// Stage 2: the cell compositor.
// ---------------------------------------------------------------------------

/// Per-row painted spans: sorted, non-overlapping `(start, end, op)` cell
/// intervals. `usize::MAX` op = a Fill (owns cells, emits nothing).
#[derive(Default)]
struct RowSpans {
    spans: Vec<(u32, u32, usize)>,
}

impl RowSpans {
    /// Stamp `[c0, c1)` for `op`, overwriting whatever is beneath (the
    /// painter's algorithm at cell granularity).
    fn stamp(&mut self, c0: u32, c1: u32, op: usize) {
        let mut out = Vec::with_capacity(self.spans.len() + 2);
        let mut placed = false;
        for &(s, e, o) in &self.spans {
            if e <= c0 || s >= c1 {
                if !placed && s >= c1 {
                    out.push((c0, c1, op));
                    placed = true;
                }
                out.push((s, e, o));
                continue;
            }
            // Overlap: keep the uncovered flanks.
            if s < c0 {
                out.push((s, c0, o));
            }
            if !placed {
                out.push((c0, c1, op));
                placed = true;
            }
            if e > c1 {
                out.push((c1, e, o));
            }
        }
        if !placed {
            out.push((c0, c1, op));
        }
        self.spans = out;
    }

    /// Stamp `[c0, c1)` for a GHOST op: only into cells nobody owns.
    fn stamp_ghost(&mut self, c0: u32, c1: u32, op: usize) {
        let mut cur = c0;
        let mut add: Vec<(u32, u32, usize)> = Vec::new();
        for &(s, e, _) in &self.spans {
            if s >= c1 {
                break;
            }
            if e <= cur {
                continue;
            }
            if s > cur {
                add.push((cur, s.min(c1), op));
            }
            cur = cur.max(e);
        }
        if cur < c1 {
            add.push((cur, c1, op));
        }
        for (s, e, o) in add {
            if e > s {
                self.stamp(s, e, o);
            }
        }
    }

    /// The intervals `op` still owns.
    fn owned(&self, op: usize) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        for &(s, e, o) in &self.spans {
            if o != op {
                continue;
            }
            if let Some(last) = out.last_mut()
                && last.1 == s
            {
                last.1 = e;
                continue;
            }
            out.push((s, e));
        }
        out
    }
}

/// Composite the display list into non-overlapping `Doc` rows. `alpha`
/// (URL→`has_alpha`) and `composites` drive the P8 overlap grouping: image
/// fragments that overlap and where an upper image is transparent are folded
/// into ONE synthetic `x-trust-composite:` emission (registered in `composites`)
/// so the app can alpha-blend them; opaque overlaps stay separate.
fn composite(
    ops: Vec<Op>,
    cols: usize,
    alpha: &HashMap<String, bool>,
    composites: &mut Composites,
) -> Vec<Row> {
    let cols_u = cols as u32;
    let mut grid: Vec<RowSpans> = Vec::new();
    let ensure = |grid: &mut Vec<RowSpans>, row: usize| {
        while grid.len() <= row {
            grid.push(RowSpans::default());
        }
    };
    // Clip an op's placement to the viewport band, mirroring the P0 painter:
    // left overhang cuts leading cells, the right edge truncates.
    let mut placed: Vec<Option<Placed>> = Vec::with_capacity(ops.len());
    // ---- stamping pass (paint order) ----
    for (i, op) in ops.into_iter().enumerate() {
        match op {
            Op::Fill {
                row0,
                row1,
                col0,
                col1,
                clip,
            } => {
                // Intersect the fill with its clip and the viewport.
                let c0 = col0.max(clip.c0).clamp(0, cols_u as i64) as u32;
                let c1 = col1.min(clip.c1).clamp(c0 as i64, cols_u as i64) as u32;
                let rr0 = row0.max(clip.r0).max(0);
                let rr1 = row1.min(clip.r1).max(0);
                if c1 > c0 {
                    for r in rr0..rr1 {
                        ensure(&mut grid, r as usize);
                        grid[r as usize].stamp(c0, c1, usize::MAX);
                    }
                }
                placed.push(None);
            }
            Op::Item {
                row,
                col,
                mut item,
                clip,
            } => {
                // Horizontal clip band = the op's clip ∩ the viewport
                // [0, cols); vertical band = [max(0, clip.r0), clip.r1). For a
                // FULL_CLIP op these collapse to the plain viewport bounds, so
                // unclipped content is composited exactly as before.
                let lo = clip.c0.clamp(0, cols_u as i64);
                let hi = clip.c1.clamp(lo, cols_u as i64);
                let band0 = clip.r0.max(0);
                let band1 = clip.r1;
                let mut col = col;
                // Left-edge clip to the band's left edge.
                if col < lo {
                    let cut = (lo - col) as usize;
                    if item.image.is_some() || item.text.is_empty() {
                        if (item.width as usize) <= cut {
                            placed.push(None);
                            continue;
                        }
                        item.width -= cut as u16;
                    } else {
                        let keep = drop_cells(&item.text, cut);
                        if keep.is_empty() {
                            placed.push(None);
                            continue;
                        }
                        item.width = display_width(&keep) as u16;
                        item.text = keep;
                    }
                    col = lo;
                }
                let top = row.max(0);
                // Off the band on either axis → nothing shows. The item is
                // emitted anchored at its top row, so the band test is on it
                // (a box straddling the band top is dropped, not sliced — the
                // sub-row slice of a replaced box has no cell analogue).
                if col >= hi
                    || top < band0
                    || top >= band1
                    || row + i64::from(item.height.max(1)) <= band0
                {
                    placed.push(None);
                    continue;
                }
                // Right-edge clip to the band's right edge.
                let colu = col as usize;
                let avail = (hi - col) as usize;
                if item.width as usize > avail {
                    if item.text.is_empty() {
                        item.width = avail as u16;
                    } else {
                        item.text = truncate_to_width(&item.text, avail);
                        item.width = display_width(&item.text) as u16;
                        if item.width == 0 {
                            placed.push(None);
                            continue;
                        }
                    }
                }
                let top = top as usize;
                let c0 = colu as u32;
                let c1 = c0 + u32::from(item.width);
                // Stamp only rows within the vertical band: a box taller than
                // the clip claims no cells below it (following content shows
                // through), while emission stays anchored at the top row.
                let end = (top as i64 + i64::from(item.height.max(1)))
                    .min(band1)
                    .max(top as i64) as usize;
                for r in top..end {
                    ensure(&mut grid, r);
                    if item.invisible {
                        grid[r].stamp_ghost(c0, c1, i);
                    } else {
                        grid[r].stamp(c0, c1, i);
                    }
                }
                placed.push(Some(Placed {
                    row: top,
                    col: c0,
                    item,
                }));
            }
        }
    }
    // ---- P8: alpha-composite transparent image overlaps ----
    // Group image fragments that overlap where an UPPER image is transparent
    // into one synthetic `x-trust-composite:` emission (the app alpha-blends the
    // layers so lower images show through upper holes). Opaque overlaps stay
    // separate. `consumed[i]` = a placed image folded into a group.
    let mut consumed = vec![false; placed.len()];
    let mut groups: Vec<CompositeGroup> = Vec::new();
    if !alpha.is_empty() {
        group_transparent_overlaps(&placed, alpha, &mut consumed, &mut groups);
    }
    // ---- emission pass 1: atomic images (and their opaque pixel rects) ----
    let mut rows: Vec<Row> = Vec::new();
    let ensure_rows = |rows: &mut Vec<Row>, need: usize| {
        while rows.len() < need {
            rows.push(Row::default());
        }
    };
    // Opaque pixel rects per row: text landing inside them is dropped (the
    // image pass paints pixels over those cells regardless — a composite renders
    // its whole box as one widget too, so text under it drops the same way).
    let mut pixels: Vec<Vec<(u32, u32)>> = Vec::new();
    for (i, p) in placed.iter().enumerate() {
        let Some(p) = p else { continue };
        if p.item.image.is_none() || p.item.invisible || consumed[i] {
            continue;
        }
        let survives = (p.row..p.row + p.item.height.max(1) as usize)
            .any(|r| grid.get(r).is_some_and(|g| !g.owned(i).is_empty()));
        if !survives {
            continue;
        }
        let (c0, c1) = (p.col, p.col + u32::from(p.item.width));
        for r in p.row..p.row + p.item.height.max(1) as usize {
            while pixels.len() <= r {
                pixels.push(Vec::new());
            }
            pixels[r].push((c0, c1));
        }
        ensure_rows(&mut rows, p.row + p.item.height.max(1) as usize);
        let mut item = p.item.clone();
        item.col = p.col.min(u16::MAX as u32) as u16;
        rows[p.row].items.push(item);
    }
    // Emit each composite group as ONE image item over its union box, keyed by a
    // synthetic `x-trust-composite:` URL the app resolves to `composites`.
    for g in &groups {
        // Survives if any member still owns any cell (not fully covered by later
        // external content painted over the whole union).
        let survives = g.members.iter().any(|&m| {
            let p = placed[m].as_ref().unwrap();
            (p.row..p.row + p.item.height.max(1) as usize)
                .any(|r| grid.get(r).is_some_and(|gr| !gr.owned(m).is_empty()))
        });
        if !survives {
            continue;
        }
        let layers: Vec<CompositeLayer> = g
            .members
            .iter()
            .map(|&m| {
                let p = placed[m].as_ref().unwrap();
                CompositeLayer {
                    url: p.item.image.clone().unwrap(),
                    dcol: (p.col - g.col).min(u32::from(u16::MAX)) as u16,
                    drow: (p.row - g.row).min(usize::from(u16::MAX)) as u16,
                    w: p.item.width,
                    h: p.item.height.max(1),
                    crop: p.item.crop,
                    pixelated: p.item.pixelated,
                }
            })
            .collect();
        let key = composite_key(&layers);
        let (c0, c1) = (g.col, g.col + g.w);
        for r in g.row..g.row + g.h {
            while pixels.len() <= r {
                pixels.push(Vec::new());
            }
            pixels[r].push((c0, c1));
        }
        ensure_rows(&mut rows, g.row + g.h);
        // Hover / selection map the union to the BASE (bottom) image's node/link.
        let base = placed[g.members[0]].as_ref().unwrap();
        rows[g.row].items.push(Item {
            col: g.col.min(u32::from(u16::MAX)) as u16,
            width: g.w.min(u32::from(u16::MAX)) as u16,
            height: g.h.min(usize::from(u16::MAX)) as u16,
            text: String::new(),
            kind: ItemKind::Image,
            image: Some(key.clone()),
            emph: Emphasis::default(),
            node: base.item.node,
            link: base.item.link.clone(),
            crop: false,
            pixelated: false,
            invisible: false,
        });
        composites.insert(key, layers);
    }
    // ---- emission pass 2: sliceable items (text, widgets, blank boxes) ----
    for (i, p) in placed.iter().enumerate() {
        let Some(p) = p else { continue };
        if p.item.image.is_some() && !p.item.invisible {
            continue; // emitted above
        }
        let Some(g) = grid.get(p.row) else { continue };
        let mut segs = g.owned(i);
        if let Some(px) = pixels.get(p.row) {
            segs = subtract(segs, px);
        }
        for (s, e) in segs {
            let mut item = p.item.clone();
            if !item.text.is_empty() {
                let skip = (s - p.col) as usize;
                let take = (e - s) as usize;
                item.text = slice_cells(&item.text, skip, take);
                if item.text.is_empty() {
                    continue;
                }
                item.width = display_width(&item.text) as u16;
            } else {
                item.width = (e - s) as u16;
            }
            item.col = s.min(u32::from(u16::MAX)) as u16;
            ensure_rows(&mut rows, p.row + item.height.max(1) as usize);
            rows[p.row].items.push(item);
        }
    }
    // Consumers walk a row's items left-to-right; compositing guarantees
    // non-overlap, so column order is total. Stable for determinism.
    for row in &mut rows {
        row.items.sort_by_key(|it| it.col);
    }
    rows
}

/// One placed (viewport-clipped) inline item, ready to emit: its top row/col in
/// cells and the (possibly clipped) `Item`. Built in `composite`'s stamping pass
/// and read back in the emission passes and the P8 overlap grouping.
struct Placed {
    row: usize,
    col: u32,
    item: Item,
}

/// One alpha-composite overlap group: the `placed` indices of its member images
/// (ascending == paint order, bottom first) and the union box in cells.
struct CompositeGroup {
    members: Vec<usize>,
    col: u32,
    row: usize,
    w: u32,
    h: usize,
}

/// A placed item's cell box `(col0, row0, col1, row1)` (half-open).
fn placed_box(placed: &[Option<Placed>], i: usize) -> (u32, usize, u32, usize) {
    let p = placed[i].as_ref().unwrap();
    let h = p.item.height.max(1) as usize;
    (p.col, p.row, p.col + u32::from(p.item.width), p.row + h)
}

fn boxes_overlap(a: (u32, usize, u32, usize), b: (u32, usize, u32, usize)) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// Find connected components of overlapping visible image items and, for any
/// component where an UPPER (later-painted) image is transparent and overlaps a
/// lower one, record it as a composite group (marking its members `consumed`).
/// Opaque overlaps are left alone — they stay separate, cheap image items.
fn group_transparent_overlaps(
    placed: &[Option<Placed>],
    alpha: &HashMap<String, bool>,
    consumed: &mut [bool],
    groups: &mut Vec<CompositeGroup>,
) {
    let has_alpha = |i: usize| -> bool {
        placed[i]
            .as_ref()
            .and_then(|p| p.item.image.as_deref())
            .and_then(|u| alpha.get(u))
            .copied()
            .unwrap_or(false)
    };
    // Visible placed images, in paint order (index order == paint order).
    let imgs: Vec<usize> = placed
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.as_ref()
                .is_some_and(|p| p.item.image.is_some() && !p.item.invisible)
        })
        .map(|(i, _)| i)
        .collect();
    // No composite is possible without ≥2 images AND at least one transparent
    // one — so skip the O(images²) overlap scan on the (common) all-opaque page,
    // even when the alpha map is fully populated with opaque entries.
    if imgs.len() < 2 || !imgs.iter().any(|&i| has_alpha(i)) {
        return;
    }
    // Union-find (path-halving) over `imgs` positions, joined on box overlap.
    let n = imgs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for a in 0..n {
        for b in (a + 1)..n {
            if boxes_overlap(placed_box(placed, imgs[a]), placed_box(placed, imgs[b])) {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                if ra != rb {
                    parent[ra] = rb;
                }
            }
        }
    }
    let mut comp: HashMap<usize, Vec<usize>> = HashMap::new();
    for (a, &img) in imgs.iter().enumerate() {
        let r = find(&mut parent, a);
        comp.entry(r).or_default().push(img);
    }
    for members in comp.into_values() {
        if members.len() < 2 {
            continue;
        }
        let mut members = members;
        members.sort_unstable(); // placed index == paint order (bottom first)
        // Composite only if some upper member is transparent AND overlaps an
        // earlier-painted member (so its holes actually reveal a lower image).
        let need = members.iter().enumerate().skip(1).any(|(pos, &m)| {
            has_alpha(m)
                && members[..pos]
                    .iter()
                    .any(|&l| boxes_overlap(placed_box(placed, l), placed_box(placed, m)))
        });
        if !need {
            continue;
        }
        let (mut col0, mut row0, mut col1, mut row1) = (u32::MAX, usize::MAX, 0u32, 0usize);
        for &m in &members {
            let (a0, a1, a2, a3) = placed_box(placed, m);
            col0 = col0.min(a0);
            row0 = row0.min(a1);
            col1 = col1.max(a2);
            row1 = row1.max(a3);
            consumed[m] = true;
        }
        groups.push(CompositeGroup {
            members,
            col: col0,
            row: row0,
            w: col1 - col0,
            h: row1 - row0,
        });
    }
}

/// A deterministic, cache-stable synthetic URL for a composite group: hashing
/// the ordered layers means an identical overlap re-keys identically (encode
/// cache hit) and a changed member re-keys (re-encode), which is exactly the
/// invalidation the app's `EncKey`/`image_protocols` cache wants.
fn composite_key(layers: &[CompositeLayer]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for l in layers {
        l.hash(&mut h);
    }
    format!("x-trust-composite:{:016x}", h.finish())
}

/// Subtract the `cover` intervals from `segs` (output keeps order).
fn subtract(segs: Vec<(u32, u32)>, cover: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(segs.len());
    for (s, e) in segs {
        let mut covers: Vec<(u32, u32)> = cover
            .iter()
            .copied()
            .filter(|&(cs, ce)| ce > s && cs < e)
            .collect();
        covers.sort_unstable();
        let mut cur = s;
        for (cs, ce) in covers {
            if cs > cur {
                out.push((cur, cs.min(e)));
            }
            cur = cur.max(ce);
            if cur >= e {
                break;
            }
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

/// Drop the leading `cut` display cells of `s` (left-edge clipping). A wide
/// glyph straddling the cut is dropped whole.
fn drop_cells(s: &str, cut: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
        if w >= cut {
            out.push(c);
        }
        w += cw;
    }
    out
}

/// The substring of `s` covering display cells `[skip, skip+take)`. Wide
/// glyphs straddling either boundary are dropped whole.
fn slice_cells(s: &str, skip: usize, take: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
        if w >= skip && w + cw <= skip + take {
            out.push(c);
        }
        w += cw;
        if w >= skip + take {
            break;
        }
    }
    out
}
