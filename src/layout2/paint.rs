//! Terminal compatibility adapter: the canonical CSS-pixel fragment tree →
//! `Doc.rows` in CSS 2.1 **Appendix E** order, composited at cell granularity.
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
//! 2. **Terminal composition**: shaped-run clipping is resolved before CSS
//!    pixels become cells. Every operation stamps spans in paint order, but a
//!    text spelling is sliced only when later paint genuinely overlaps its
//!    canonical pixel bounds; adjacent boxes rounding onto the same cell cannot
//!    delete characters. Paint-suppressed items are GHOSTS: they claim only
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

use crate::doc::Link;
use crate::dom::{Dom, NodeId};
use crate::layout2::{
    Carousel, CompositeLayer, Emphasis, FixedItem, HitBox, Item, ItemKind, NO_NODE, Region, Row,
    display_width, truncate_to_width,
};

/// The overlap-composite side-table produced by a paint pass (P8): a synthetic
/// `x-trust-composite:` URL → the layers the app alpha-blends into that box.
pub(crate) type Composites = HashMap<String, Vec<CompositeLayer>>;

use super::flow::{Clip, Frag, FragKind, TopFrag};
use super::style::{BOTTOM, LEFT, Outline, OutlineStyle, RIGHT, TOP, outline_of};

type TerminalPageMedia = (Link, Option<(String, f32, f32)>);

/// Immutable paint metadata captured from the canonical DOM at the end of the
/// shared CSS-pixel layout pass. It is deliberately not a DOM mirror: no tree
/// mutation, selector matching, cascade, or HTML attributes are reconstructed.
/// The terminal adapter receives only the already-resolved facts it needs to
/// quantize the retained pixel fragments into cells.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TerminalPaintModel {
    nodes: Vec<TerminalNodePaint>,
    links: HashMap<NodeId, Link>,
    page_media: Option<TerminalPageMedia>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TerminalNodePaint {
    tag: Option<String>,
    id: Option<String>,
    anchor_name: Option<String>,
    vertical_scroll: bool,
    horizontal_scroll: bool,
    principal_scroll: bool,
    point_hit_subtree: bool,
    background_covers: bool,
    scrollbar_hidden: bool,
    /// The element is an atomic inline formatting context or an item in a
    /// flex/grid formatting context. Canonical layout keeps its contents in
    /// that independently sized horizontal box; terminal adaptation expands
    /// the row presentation instead of re-breaking the box's text.
    horizontal_item: bool,
    /// Establishes an independent horizontal adaptation band. Fixed-cell
    /// expansion inside this box must not consume a neighboring flex/grid or
    /// table cell. Floats are detected from fragment paint state.
    independent_band: bool,
    inline_snap: bool,
    snap_align: Option<String>,
    live_node: Option<usize>,
    boundary_actor: Option<usize>,
    scroll_top: Option<f32>,
    scroll_left: Option<f32>,
    outline: Outline,
}

impl TerminalPaintModel {
    pub(crate) fn from_dom(dom: &Dom, base: &url::Url) -> Self {
        let mut links = HashMap::new();
        let mut nodes = Vec::with_capacity(dom.node_count());
        for node in 0..dom.node_count() {
            // CSS Display 3 §2: descendants of a display:none/otherwise
            // suppressed flat-tree element generate no boxes. Keep their actor
            // state in the canonical DOM, but do not let newly-created hidden
            // nodes make the retained terminal adapter appear to have changed.
            if !dom.is_connected(node) || dom.omitted_from_flat_box_tree(node) {
                nodes.push(TerminalNodePaint::default());
                continue;
            }
            let tag = dom.tag_name(node).map(str::to_string);
            let point_hit = dom.point_hit_testable(node);
            if point_hit {
                if dom.render_clickable(node) {
                    links.insert(
                        node,
                        Link::JsClick {
                            node,
                            href: dom.attr(node, "href").unwrap_or("").to_string(),
                        },
                    );
                } else if tag.as_deref() == Some("a")
                    && let Some(href) = dom.attr(node, "href")
                {
                    links.insert(node, crate::http::resolve(base, href));
                } else if matches!(tag.as_deref(), Some("video" | "audio"))
                    && let Some(target) = super::inline::media_target(dom, base, node)
                {
                    links.insert(node, Link::Media(target));
                }
            }
            let inline_snap = dom
                .computed_value_resolved(node, "scroll-snap-type")
                .is_some_and(|value| {
                    matches!(
                        value
                            .split_whitespace()
                            .next()
                            .unwrap_or("none")
                            .to_ascii_lowercase()
                            .as_str(),
                        "x" | "inline" | "both"
                    )
                });
            let live_node = if dom.render_live() {
                Some(node)
            } else {
                dom.attr(node, "data-trust-node")
                    .and_then(|value| value.parse().ok())
            };
            let display = dom
                .computed_value_resolved(node, "display")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let parent_display = dom
                .parent_flat(node)
                .and_then(|parent| dom.computed_value_resolved(parent, "display"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            nodes.push(TerminalNodePaint {
                tag,
                id: dom
                    .attr(node, "id")
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                anchor_name: dom
                    .attr(node, "name")
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                vertical_scroll: dom.is_scroll_container(node),
                horizontal_scroll: dom.is_hscroll_container(node),
                principal_scroll: dom.is_principal_scroller(node),
                point_hit_subtree: dom.subtree_has_point_hit_target(node),
                background_covers: terminal_background_covers_dom(dom, node),
                scrollbar_hidden: matches!(
                    dom.computed_value_resolved(node, "scrollbar-width")
                        .as_deref(),
                    Some("none")
                ),
                horizontal_item: matches!(
                    display.as_str(),
                    "inline-block" | "inline-flex" | "inline-grid"
                ) || matches!(
                    parent_display.as_str(),
                    "flex" | "inline-flex" | "grid" | "inline-grid"
                ),
                independent_band: matches!(
                    display.as_str(),
                    "inline-block" | "flex" | "inline-flex" | "grid" | "inline-grid" | "table-cell"
                ),
                inline_snap,
                snap_align: dom
                    .computed_value_resolved(node, "scroll-snap-align")
                    .and_then(|value| value.split_whitespace().last().map(str::to_ascii_lowercase)),
                live_node,
                boundary_actor: super::boundary::boundary_actor(dom, node),
                scroll_top: dom
                    .attr(node, "data-trust-scroll-top")
                    .and_then(|value| value.parse().ok())
                    .or_else(|| {
                        dom.render_live()
                            .then(|| dom.scroll_metric(node, 0).map(|value| value as f32))
                            .flatten()
                    }),
                scroll_left: dom
                    .attr(node, "data-trust-scroll-left")
                    .and_then(|value| value.parse().ok())
                    .or_else(|| {
                        dom.render_live()
                            .then(|| dom.scroll_metric(node, 1).map(|value| value as f32))
                            .flatten()
                    }),
                outline: outline_of(dom, node, super::Units::of(dom, node)),
            });
        }
        while nodes
            .last()
            .is_some_and(|node| *node == TerminalNodePaint::default())
        {
            nodes.pop();
        }
        Self {
            nodes,
            links,
            page_media: None,
        }
    }

    fn horizontal_item(&self, node: NodeId) -> bool {
        node != NO_NODE
            && self
                .nodes
                .get(node)
                .is_some_and(|paint| paint.horizontal_item)
    }

    fn independent_band(&self, node: NodeId) -> bool {
        node != NO_NODE
            && self
                .nodes
                .get(node)
                .is_some_and(|paint| paint.independent_band)
    }

    pub(crate) fn capture_page_media(
        &mut self,
        dom: &Dom,
        base: &url::Url,
        images: &super::ImageSizes,
    ) {
        if dom
            .flat_descendants(crate::dom::DOCUMENT)
            .into_iter()
            .any(|id| matches!(dom.tag_name(id), Some("video" | "audio")))
            || !super::page_declares_video(dom)
        {
            self.page_media = None;
            return;
        }
        let target = super::unique_structured_media(dom, base, true)
            .map(|media| media.target)
            .unwrap_or_else(|| base.clone());
        let poster = super::page_preview_image(dom, base).and_then(|source| {
            crate::responsive_image::density_corrected_size(images.get(&source), 1.0)
                .map(|(width, height)| (source, width, height))
        });
        self.page_media = Some((Link::Media(target), poster));
    }

    pub(crate) fn page_media(&self) -> Option<&TerminalPageMedia> {
        self.page_media.as_ref()
    }

    fn node(&self, node: NodeId) -> Option<&TerminalNodePaint> {
        self.nodes.get(node)
    }

    fn tag_name(&self, node: NodeId) -> Option<&str> {
        self.node(node)?.tag.as_deref()
    }

    fn is_scroll_container(&self, node: NodeId) -> bool {
        self.node(node).is_some_and(|node| node.vertical_scroll)
    }

    fn is_hscroll_container(&self, node: NodeId) -> bool {
        self.node(node).is_some_and(|node| node.horizontal_scroll)
    }

    fn is_principal_scroller(&self, node: NodeId) -> bool {
        self.node(node).is_some_and(|node| node.principal_scroll)
    }

    fn live_node(&self, node: NodeId) -> Option<usize> {
        self.node(node)?.live_node
    }

    pub(super) fn boundary_actor(&self, node: NodeId) -> Option<usize> {
        self.node(node)?.boundary_actor
    }

    fn scroll_top(&self, node: NodeId) -> Option<f32> {
        self.node(node)?.scroll_top
    }

    fn scroll_left(&self, node: NodeId) -> Option<f32> {
        self.node(node)?.scroll_left
    }

    fn outline(&self, node: NodeId) -> Outline {
        self.node(node)
            .map(|node| node.outline)
            .unwrap_or(Outline::NONE)
    }
}

static BORDERS_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_borders_enabled(on: bool) {
    BORDERS_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

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
    dom: &TerminalPaintModel,
    root: &mut Frag<'_>,
    fixed: &[Frag<'_>],
    top_layer: &[TopFrag<'_>],
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
    let links = &dom.links;
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
        links,
        alpha,
        &mut composites,
    );
    // The document height, computed AFTER extraction so an emptied region
    // contributes only its reserved band height (not its scrolled-away content).
    let doc_h_px = root.max_bottom().max(flow_bottom).max(0.0);
    let mut ops = Vec::new();
    let line_rows = line_row_map(dom, root, 0.0, 0.0, cell_w, cell_h, cols);
    build_sc(
        dom, root, &mut ops, cell_w, cell_h, 0.0, 0.0, links, &line_rows,
    );
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
        if let Some(id) = dom.node(node).and_then(|node| node.id.as_deref()) {
            note(id, row);
        }
        if dom.tag_name(node) == Some("a")
            && let Some(name) = dom.node(node).and_then(|node| node.anchor_name.as_deref())
        {
            note(name, row);
        }
    }
    // The pinned layer: each fixed box paints through the same pipeline into
    // its own position-independent row buffer, at its viewport position.
    // Stable stack-level sort: the renderer draws the vec in order.
    //
    // A fixed box cannot simply be appended after `rows`: CSS Positioned
    // Layout §2.2 and CSS 2.1 Appendix E §E.2 place fixed stacking contexts
    // in the same stacking-context order as their tree-positioned siblings.
    // Keep the pinned geometry separate so it remains viewport-addressed, but
    // mark a zero/auto-level fixed box as an under-document layer when a later
    // ordinary positioned sibling paints above it. This is the terminal
    // compositor equivalent of the graphical painter's fixed marker.
    let fixed_under = fixed_under_document(root, fixed.len());
    let mut order: Vec<usize> = (0..fixed.len()).collect();
    order.sort_by_key(|&i| fixed[i].paint.z.unwrap_or(0));
    let vp_rows = viewport.1;
    let mut fixed_items: Vec<FixedItem> = order
        .into_iter()
        .filter_map(|i| {
            let f = &fixed[i];
            let col = ((f.x / cell_w).round() as i64).max(0) as usize;
            let mut row = ((f.y / cell_h).round() as i64).max(0) as usize;
            if vp_rows > 0 {
                row = row.min(vp_rows.saturating_sub(1));
            }
            let mut ops = Vec::new();
            let fixed_cols = cols.saturating_sub(col).max(1);
            let line_rows = line_row_map(dom, f, f.x, f.y, cell_w, cell_h, fixed_cols);
            build_sc(
                dom, f, &mut ops, cell_w, cell_h, f.x, f.y, links, &line_rows,
            );
            let brows = composite(ops, fixed_cols, alpha, &mut composites);
            if brows.iter().all(|r| r.items.is_empty()) {
                return None; // nothing visible: no pinned surface
            }
            Some(FixedItem {
                col: col.min(u16::MAX as usize) as u16,
                row: row.min(u16::MAX as usize) as u16,
                rows: brows,
                z: f.paint.z.unwrap_or(0),
                under_document: fixed_under.get(i).copied().unwrap_or(false),
            })
        })
        .collect();
    // Top-layer entries paint after the root stacking context and ordinary
    // fixed layer. Terminal overlays are viewport-addressed; the graphical
    // display list additionally distinguishes absolute from fixed scrolling.
    for top in top_layer {
        let f = &top.fragment;
        let col = ((f.x / cell_w).round() as i64).max(0) as usize;
        let mut row = ((f.y / cell_h).round() as i64).max(0) as usize;
        if vp_rows > 0 {
            row = row.min(vp_rows.saturating_sub(1));
        }
        let mut ops = Vec::new();
        let top_cols = cols.saturating_sub(col).max(1);
        let line_rows = line_row_map(dom, f, f.x, f.y, cell_w, cell_h, top_cols);
        build_sc(
            dom, f, &mut ops, cell_w, cell_h, f.x, f.y, links, &line_rows,
        );
        let brows = composite(ops, top_cols, alpha, &mut composites);
        if !brows.iter().all(|r| r.items.is_empty()) {
            fixed_items.push(FixedItem {
                col: col.min(u16::MAX as usize) as u16,
                row: row.min(u16::MAX as usize) as u16,
                rows: brows,
                z: i32::MAX,
                under_document: false,
            });
        }
    }
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

/// Decide which pinned fixed stacking contexts must be composited before the
/// scrolling document rows. The row renderer keeps fixed geometry in a
/// viewport-pinned buffer, but CSS still orders a fixed box at its marker's
/// position in the containing stacking context. In particular, a fixed
/// `z-index:auto/0` box before a later positioned sibling paints below that
/// sibling; it is not an always-on-top layer merely because its coordinates
/// are viewport-relative.
fn fixed_under_document(root: &Frag<'_>, fixed_len: usize) -> Vec<bool> {
    let mut under = vec![false; fixed_len];
    let mut negative = Vec::new();
    let mut zero = Vec::new();
    let mut positive = Vec::new();
    collect_positioned(root, &mut negative, &mut zero, &mut positive);

    for child in negative {
        if let FragKind::Fixed(index) = child.kind
            && let Some(slot) = under.get_mut(index)
        {
            *slot = true;
        }
    }

    let later_positive_positioned = positive
        .iter()
        .any(|child| !matches!(child.kind, FragKind::Fixed(_)));
    for (position, (child, _)) in zero.iter().enumerate() {
        let FragKind::Fixed(index) = child.kind else {
            continue;
        };
        let later_zero_positioned = zero[position + 1..]
            .iter()
            .any(|(later, _)| !matches!(later.kind, FragKind::Fixed(_)));
        if let Some(slot) = under.get_mut(index) {
            *slot = later_zero_positioned || later_positive_positioned;
        }
    }
    under
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
    dom: &TerminalPaintModel,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    regions: &mut Vec<Region>,
    carousels: &mut Vec<Carousel>,
    scroll_clips: &mut Vec<(usize, u16, u16)>,
    splices: &mut Vec<(usize, Vec<Row>)>,
    links: &HashMap<NodeId, Link>,
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
                links,
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
                links,
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
                links,
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
fn is_carousel(dom: &TerminalPaintModel, f: &Frag<'_>, cw: f32) -> bool {
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
fn content_right_px(f: &Frag<'_>, _cw: f32) -> f32 {
    f.children
        .iter()
        .map(|c| match &c.kind {
            FragKind::Line(line) => {
                let right = line
                    .pieces
                    .iter()
                    .map(|p| p.x + p.box_width)
                    .fold(0.0, f32::max);
                c.x + right
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
    dom: &TerminalPaintModel,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    carousels: &mut Vec<Carousel>,
    splices: &mut Vec<(usize, Vec<Row>)>,
    links: &HashMap<NodeId, Link>,
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
    let inline_snaps = dom.node(f.node).is_some_and(|node| node.inline_snap);
    let sp = scrollport as f32;
    let mut stops: Vec<u16> = Vec::new();
    for c in f.children.iter().filter(|c| c.w > 0.0 && c.node != NO_NODE) {
        let left = (c.x - pad_x) / cw;
        let right = (c.x + c.w - pad_x) / cw;
        let align = dom.node(c.node).and_then(|node| node.snap_align.clone());
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
    let hide_scrollbar = dom.node(f.node).is_some_and(|node| node.scrollbar_hidden);
    // Composite the strip at the current-FRAME columns (`ox`), rows relative to
    // the band top (`oy`), wide enough to keep every card's full columns — so
    // the strip items land at frame columns `band_left + strip_x` for the
    // renderer's `visible_col` to window, and the splice lands in the frame's
    // rows (the document, or the parent region's buffer).
    let strip_cols = ((content_right - ox) / cw).round().max(1.0) as usize;
    let mut ops = Vec::new();
    let line_rows = line_row_map(dom, f, ox, f.y, cw, ch, strip_cols);
    build_sc(dom, f, &mut ops, cw, ch, ox, f.y, links, &line_rows);
    let strip = composite(ops, strip_cols, alpha, composites);
    f.children.clear();
    let end = start_row + strip.len();
    splices.push((start_row, strip));
    let live_node = dom.live_node(f.node);
    let max_offset = strip_w.saturating_sub(scrollport);
    let offset = dom
        .scroll_left(f.node)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map_or(0, |px| {
            ((px / cw).round().max(0.0) as usize).min(max_offset)
        });
    carousels.push(Carousel {
        start: start_row,
        end,
        live_node,
        left: band_left.min(u16::MAX as usize) as u16,
        right: (band_left + scrollport).min(u16::MAX as usize) as u16,
        width: strip_w.min(u16::MAX as usize) as u16,
        stops,
        offset: offset.min(u16::MAX as usize) as u16,
        frame_right: None,
        snap,
        hide_scrollbar,
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
pub(crate) fn region_buffer(
    dom: &Dom,
    base: &url::Url,
    root: &mut Frag<'_>,
    cw: f32,
    ch: f32,
) -> RegionBuffer {
    let model = TerminalPaintModel::from_dom(dom, base);
    let mut scroll_clips = Vec::new();
    // v1 region-patch cut: the incremental region re-lay does NOT alpha-composite
    // transparent image overlaps (empty alpha ⇒ no grouping) — such overlaps in a
    // patched region render separately until the next full render. Always
    // correct, matching the other P7 region-patch v1 cuts.
    let no_alpha: HashMap<String, bool> = HashMap::new();
    let mut composites: Composites = HashMap::new();
    let links = &model.links;
    let rg = paint_region(
        &model,
        root,
        cw,
        ch,
        0.0,
        0.0,
        &mut scroll_clips,
        links,
        &no_alpha,
        &mut composites,
    );
    // `paint_region` pushed this region's OWN clientHeight into `scroll_clips`
    // (its `live_node`); the app already knows this region's geometry, so drop
    // the self entry and keep only the NESTED scrollers' clips.
    let self_node = model.live_node(root.node);
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
fn is_scroll_region(dom: &TerminalPaintModel, f: &Frag<'_>) -> bool {
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
    dom: &TerminalPaintModel,
    f: &mut Frag<'_>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    scroll_clips: &mut Vec<(usize, u16, u16)>,
    links: &HashMap<NodeId, Link>,
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
        links,
        alpha,
        composites,
    );
    // Paint the region's content into its buffer, origin at the padding-box
    // top-left (the scroll origin), clipped to the scrollport WIDTH — the
    // scroll axis (height) is unbounded so the buffer holds the full content.
    let mut ops = Vec::new();
    let line_rows = line_row_map(dom, f, pad_x, pad_y, cw, ch, width);
    build_sc(dom, f, &mut ops, cw, ch, pad_x, pad_y, links, &line_rows);
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
    // The page's own scrollTop signal is canonical CSS-pixel DOM state. This
    // terminal adapter converts it to rows here, then clamps it to
    // [0, scrollHeight − clientHeight] (CSSOM View). Its `data-trust-node`
    // correlates the region with the live actor for geometry round-trips.
    let live_node = dom.live_node(f.node);
    let max_voffset = content_h.saturating_sub(height);
    let signal = dom
        .scroll_top(f.node)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|px| (px / ch).round().max(0.0) as usize);
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
    if let FragKind::Line(line) = &f.kind {
        for p in &line.pieces {
            if p.item.node != NO_NODE {
                let row = (((f.y + p.y + p.paint_y) / cell_h).round() as i64).max(0) as usize;
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

/// Intersect a paint rectangle with its canonical CSS-pixel clip before any
/// edge becomes a terminal cell. Keeping this rectangle beside the quantized
/// operation lets composition distinguish real paint overlap from two
/// adjacent CSS boxes that merely round onto the same cell.
fn clipped_paint_bounds(x0: f32, y0: f32, x1: f32, y1: f32, clip: Option<Clip>) -> Option<Clip> {
    let mut bounds = Clip { x0, y0, x1, y1 };
    if let Some(clip) = clip {
        bounds.x0 = bounds.x0.max(clip.x0);
        bounds.y0 = bounds.y0.max(clip.y0);
        bounds.x1 = bounds.x1.min(clip.x1);
        bounds.y1 = bounds.y1.min(clip.y1);
    }
    (bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0).then_some(bounds)
}

fn paint_bounds_overlap(left: Clip, right: Clip) -> bool {
    left.x0 < right.x1 && right.x0 < left.x1 && left.y0 < right.y1 && right.y0 < left.y1
}

/// Quantize a fragment's px clip to cell bounds (edges snap, like every other
/// px→cell conversion here). `ox`/`oy` shift for the box-relative pinned layer.
/// A clipped terminal cell is atomic: when CSS leaves any part of a cell
/// intersecting the clip, retain that cell so the adapter does not erase the
/// final character merely because the CSS edge falls between columns. This
/// represents CSS Overflow 3 §5.1's allowance for partially rendered
/// characters at `text-overflow:clip` boundaries.
fn clip_cells(clip: Option<Clip>, ox: f32, oy: f32, cw: f32, ch: f32) -> ClipCells {
    match clip {
        None => FULL_CLIP,
        Some(c) => ClipCells {
            r0: ((c.y0 - oy) / ch).round() as i64,
            r1: ((c.y1 - oy) / ch).round() as i64,
            c0: ((c.x0 - ox) / cw).floor() as i64,
            c1: ((c.x1 - ox) / cw).ceil() as i64,
        },
    }
}

/// Quantize a clip for a one-row text item. A CSS clip can cut through a glyph
/// row between terminal-cell boundaries (for example a heading pulled 5px
/// above an `overflow:hidden` media-object body). If at least half of the row
/// remains inside the px clip, preserve that snapped cell; otherwise nearest-
/// edge quantization would sometimes discard a mostly-visible line solely
/// because the ancestor and line edges round in opposite directions. Sub-cell
/// visually-hidden boxes still suppress their text because less than half a
/// row is visible.
fn text_item_clip_cells(
    clip: Option<Clip>,
    item_y: f32,
    ox: f32,
    oy: f32,
    cw: f32,
    ch: f32,
) -> ClipCells {
    let mut cells = clip_cells(clip, ox, oy, cw, ch);
    let Some(c) = clip else {
        return cells;
    };
    let visible = (item_y + ch).min(c.y1) - item_y.max(c.y0);
    if visible >= ch / 2.0 {
        let row = ((item_y - oy) / ch).round() as i64;
        cells.r0 = cells.r0.min(row);
        cells.r1 = cells.r1.max(row + 1);
    }
    cells
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
        bounds: Clip,
    },
    /// One placed inline item at absolute (row, col).
    Item {
        row: i64,
        col: i64,
        item: Item,
        clip: ClipCells,
        /// Canonical CSS-pixel paint bounds after the fragment clip. This is
        /// retained through quantization so cell collisions can be separated
        /// from genuine CSS paint overlap.
        bounds: Option<Clip>,
        /// The run's own horizontal CSS clip was resolved from shaped glyphs;
        /// only the terminal viewport may clip its fixed-cell spelling.
        text_clip_resolved: bool,
    },
    /// A generated interactive border box. Unlike `Item`, this does not paint
    /// cells; it survives solely for standards hit testing when an anchor is
    /// transparent/empty (CSSOM View paint-order targeting).
    Hit {
        row0: i64,
        row1: i64,
        col0: i64,
        col1: i64,
        node: NodeId,
        link: Link,
        clip: ClipCells,
    },
}

/// The anchors represented by generated boxes in this layout arena. Ordinary
/// inert elements are intentionally absent: an empty `div` is not an
/// activation target merely because it has geometry. Live JS clickables have
/// already been serialized as `x-trust-js:` anchors, so the same rule covers
/// native hyperlinks and page-script activation.
/// Append a non-painting hit-test display-list entry for this element's
/// generated border box. `display:none` never reaches the fragment tree;
/// visibility, inertness and pointer-events are the remaining eligibility
/// filters. Opacity deliberately does not participate.
fn hit_op(
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
) {
    let Some(link) = links.get(&f.node) else {
        return;
    };
    if f.w <= 0.0 || f.h <= 0.0 {
        return;
    }
    ops.push(Op::Hit {
        row0: ((f.y - oy) / ch).round() as i64,
        row1: ((f.y + f.h - oy) / ch).round() as i64,
        col0: ((f.x - ox) / cw).round() as i64,
        col1: ((f.x + f.w - ox) / cw).round() as i64,
        node: f.node,
        link: link.clone(),
        clip: clip_cells(f.clip, ox, oy, cw, ch),
    });
}

/// Assign each canonical line box a terminal row without allowing two
/// vertically distinct proportional lines to collapse onto one cell row.
/// Lines sharing the same CSS y (columns/flex siblings) intentionally share a
/// row. This map exists only for compatibility painting; canonical y/height
/// and CSSOM geometry remain untouched.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TerminalLinePlacement {
    pub row: i64,
    pub end_row: i64,
    pub joins_previous: bool,
    columns: usize,
    continuation_col: i64,
    /// The canonical line fits inside its containing block, so terminal
    /// quantization must not let its display-cell approximation cross that
    /// block's right edge. `None` preserves genuine CSS overflow and
    /// independently sized horizontal items.
    clip_right: Option<i64>,
    /// A containing edge can adapt an ordinary inline formatting context
    /// without becoming a second layout boundary for flex/grid/atomic items.
    reflow_right: Option<i64>,
    /// Stable outer independent horizontal formatting-context band. This is
    /// retained on emitted items so the final terminal collision pass cannot
    /// move content into a sibling table/flex/grid/float panel.
    band: Option<(u16, u16)>,
}

pub(crate) fn line_row_map(
    dom: &TerminalPaintModel,
    root: &Frag<'_>,
    ox: f32,
    oy: f32,
    cell_w: f32,
    cell_h: f32,
    columns: usize,
) -> HashMap<usize, TerminalLinePlacement> {
    #[derive(Clone, Copy)]
    struct LineEntry<'a, 'tree> {
        line: &'a Frag<'tree>,
        positioned: bool,
        containing_right: Option<f32>,
        horizontal_item: bool,
        /// Outermost independent box: fixed-cell collision recovery and
        /// continuation rows remain local to this panel. Nested table cells
        /// and inline-blocks share their outer panel's terminal cursor.
        adapt_band: Option<(f32, f32)>,
        parent: usize,
    }

    #[derive(Clone, Copy)]
    struct CollectState {
        positioned: bool,
        containing_right: Option<f32>,
        horizontal_item: bool,
        adapt_band: Option<(f32, f32)>,
        parent: usize,
    }

    #[derive(Clone, Copy)]
    struct FlowState {
        preferred_row: i64,
        css_y: f32,
        row: i64,
        span: i64,
        end_col: i64,
        right: Option<i64>,
        forced: bool,
        reflowed: bool,
    }

    fn collect<'a, 'tree>(
        dom: &TerminalPaintModel,
        fragment: &'a Frag<'tree>,
        state: CollectState,
        lines: &mut Vec<LineEntry<'a, 'tree>>,
    ) {
        if matches!(fragment.kind, FragKind::Line(_)) {
            lines.push(LineEntry {
                line: fragment,
                positioned: state.positioned,
                containing_right: state.containing_right,
                horizontal_item: state.horizontal_item,
                adapt_band: state.adapt_band,
                parent: state.parent,
            });
        }
        let child_containing_right = Some(fragment.x + fragment.w);
        for child in &fragment.children {
            let independent_band = if child.paint.float || dom.independent_band(child.node) {
                Some((child.x, child.x + child.w))
            } else {
                None
            };
            // Nested table cells and inline-blocks refine canonical layout,
            // but they do not start unrelated terminal documents. A sponsor
            // table following a headlines table in one outer sidebar cell
            // must see the rows added while adapting those headlines. The
            // same stable outer cursor keeps a flex/inline sequence together.
            let child_adapt_band = state.adapt_band.or(independent_band);
            collect(
                dom,
                child,
                CollectState {
                    positioned: state.positioned || child.paint.positioned,
                    containing_right: child_containing_right,
                    horizontal_item: state.horizontal_item || dom.horizontal_item(child.node),
                    adapt_band: child_adapt_band,
                    parent: std::ptr::from_ref(fragment) as usize,
                },
                lines,
            );
        }
    }

    let mut lines = Vec::new();
    collect(
        dom,
        root,
        CollectState {
            positioned: false,
            containing_right: None,
            horizontal_item: dom.horizontal_item(root.node),
            adapt_band: None,
            parent: std::ptr::from_ref(root) as usize,
        },
        &mut lines,
    );
    lines.sort_by(|left, right| {
        left.line
            .y
            .total_cmp(&right.line.y)
            .then_with(|| left.line.x.total_cmp(&right.line.x))
    });
    let mut map = HashMap::with_capacity(lines.len());
    let mut previous_y: Option<f32> = None;
    let mut previous_row = i64::MIN;
    // A terminal-adapted line reserves additional rows only in its own
    // quantized line-box band. Independent inline formatting contexts can
    // have slightly different CSS y coordinates, but extra rows in a sidebar
    // must not push the main flow down.
    let mut next_row_by_band: HashMap<Option<(i64, i64)>, i64> = HashMap::new();
    // Positioned descendants paint out of flow and therefore must not reserve
    // rows in the document allocator. Their own line boxes still need a
    // private floor, however: a quantization-only continuation row from one
    // positioned line must not be overwritten by the next canonical line.
    let mut next_row_by_positioned_parent: HashMap<usize, i64> = HashMap::new();
    let mut last_band_parent: HashMap<Option<(i64, i64)>, usize> = HashMap::new();
    let mut flow_states: HashMap<usize, FlowState> = HashMap::new();
    for entry in lines {
        let line = entry.line;
        let positioned = entry.positioned;
        let containing_right = entry.containing_right;
        let preferred = ((line.y - oy) / cell_h).round() as i64;
        let line_start = ((line.x - ox) / cell_w).round().max(0.0) as i64;
        let mut clip_right = containing_right.and_then(|right| {
            let FragKind::Line(line_fragment) = &line.kind else {
                return None;
            };
            (line.x + line_fragment.width <= right + 0.01)
                .then(|| ((right - ox) / cell_w).ceil() as i64)
        });
        let quantize_band = |(left, right): (f32, f32)| {
            (
                ((left - ox) / cell_w).floor() as i64,
                ((right - ox) / cell_w).ceil() as i64,
            )
        };
        let band = entry.adapt_band.map(quantize_band);
        let item_band = band.and_then(|(left, right)| {
            let left = left.clamp(0, columns as i64);
            let right = right.clamp(left, columns as i64);
            (left < right).then_some((left as u16, right as u16))
        });
        // Terminal cell quantization may need an extra row when a proportional
        // line's text is wider than its cell approximation. Do not invent that
        // row inside a one-line clipped box, though: CSS Text's `nowrap` line
        // remains one line and CSS Overflow clips it at the box edge. The
        // horizontal compositor retains an intersected final cell instead.
        let has_vertical_reflow_room = line
            .clip
            .is_none_or(|clip| clip.y1 > line.y + line.h + 0.01);
        let contains_atomic_inline = matches!(
            &line.kind,
            FragKind::Line(line_fragment) if line_fragment.contains_atomic_inline
        );
        if entry.horizontal_item || contains_atomic_inline {
            clip_right = None;
        }
        let reflow_right = clip_right.filter(|right| {
            *right > line_start && *right <= columns as i64 && has_vertical_reflow_room
        });
        let previous_flow = flow_states.get(&entry.parent).copied();
        let can_continue = previous_flow.is_some_and(|state| {
            let canonical_step = preferred - state.preferred_row;
            !state.forced
                && state.reflowed
                && reflow_right == state.right
                && canonical_step <= 2
                && (line.y - state.css_y) <= cell_h * 1.25
                // Rejoin only when the predecessor actually needed a
                // terminal continuation row. A canonical line that already
                // fits its terminal band remains a normal line boundary;
                // this preserves the viewport's ordinary paragraph wrapping.
                && state.span > 1
                && state.end_col < state.right.unwrap_or(i64::MAX)
        });
        let carry_row = can_continue.then(|| {
            let state = previous_flow.expect("can_continue implies a flow state");
            state.row + state.span - 1
        });
        let mut band_floor = next_row_by_band.get(&band).copied().unwrap_or(i64::MIN);
        let positioned_floor = next_row_by_positioned_parent
            .get(&entry.parent)
            .copied()
            .unwrap_or(i64::MIN);
        if can_continue
            && previous_flow.is_some_and(|state| {
                state.row + state.span == band_floor
                    && last_band_parent.get(&band) == Some(&entry.parent)
            })
        {
            // This is the same flow's previous reservation. Its final row may
            // still have room for the next canonical soft-wrapped line, so do
            // not mistake that reservation for a sibling.
            band_floor = i64::MIN;
        }
        let vertically_clipped_away = line.clip.is_some_and(|clip| {
            ((clip.y0 - oy) / cell_h).round() as i64 >= ((clip.y1 - oy) / cell_h).round() as i64
        });
        if vertically_clipped_away {
            map.insert(
                std::ptr::from_ref(line) as usize,
                TerminalLinePlacement {
                    row: preferred,
                    end_row: preferred,
                    joins_previous: false,
                    columns,
                    continuation_col: line_start,
                    clip_right,
                    reflow_right: None,
                    band: item_band,
                },
            );
            continue;
        }
        let same_band = !positioned && previous_y.is_some_and(|y| (line.y - y).abs() <= 0.01);
        let row = if same_band {
            previous_row
        } else if let Some(carry_row) = carry_row {
            carry_row
        } else if positioned {
            // An out-of-flow formatting context must not advance the shared
            // document row allocator, but its own proportional lines still
            // need the same terminal-cell adaptation as in-flow text.
            preferred.max(positioned_floor)
        } else {
            preferred.max(band_floor)
        };
        let start_col = if carry_row.is_some_and(|carry| carry == row) {
            previous_flow
                .expect("carry row implies a flow state")
                .end_col
        } else {
            line_start
        };
        let joins_previous = carry_row.is_some_and(|carry| carry == row);
        let (span, end_col) = if let Some(right) = reflow_right {
            terminal_line_extent(
                line,
                ox,
                cell_w,
                cell_h,
                right,
                line_start,
                start_col,
                joins_previous,
            )
        } else if clip_right.is_some() && !has_vertical_reflow_room {
            (1, line_start)
        } else {
            (
                terminal_line_span(line, ox, cell_w, cell_h, columns, line_start, clip_right),
                line_start,
            )
        };
        map.insert(
            std::ptr::from_ref(line) as usize,
            TerminalLinePlacement {
                row,
                end_row: row + span.max(1) - 1,
                joins_previous,
                columns,
                continuation_col: line_start,
                clip_right,
                reflow_right,
                band: item_band,
            },
        );
        previous_y = Some(line.y);
        previous_row = row;
        if !positioned {
            let next_row = next_row_by_band.entry(band).or_insert(i64::MIN);
            *next_row = (*next_row).max(row + span.max(1));
            last_band_parent.insert(band, entry.parent);
        } else {
            let next_row = next_row_by_positioned_parent
                .entry(entry.parent)
                .or_insert(i64::MIN);
            *next_row = (*next_row).max(row + span.max(1));
        }
        let forced = matches!(&line.kind, FragKind::Line(line) if line.forced);
        flow_states.insert(
            entry.parent,
            FlowState {
                preferred_row: preferred,
                css_y: line.y,
                row,
                span: span.max(1),
                end_col,
                right: reflow_right,
                forced,
                reflowed: reflow_right.is_some(),
            },
        );
    }
    map
}

fn terminal_piece_width(piece: &super::inline::Piece, cell_w: f32, _cell_h: f32) -> usize {
    let text = terminal_piece_text(piece);
    if piece.item.image.is_some() || (piece.shaped.is_none() && text.is_empty()) {
        let x0 = (piece.paint_x / cell_w).round();
        let x1 = ((piece.paint_x + piece.paint_width) / cell_w).round();
        (x1 - x0).max(1.0) as usize
    } else {
        display_width(&terminal_piece_text_with_space(piece))
    }
}

/// Rows occupied below the canonical line-box origin by one terminal item.
/// An atomic inline can be several cells tall even though it participates in
/// one CSS line box. If an earlier proportional line has shifted that line
/// downward, the terminal allocator must carry the atomic item's complete
/// painted height forward before placing a following block.
fn terminal_piece_row_extent(piece: &super::inline::Piece, cell_h: f32) -> i64 {
    if piece.item.image.is_some()
        || (piece.shaped.is_none() && terminal_piece_text(piece).is_empty())
    {
        let offset = (piece.paint_y / cell_h).round() as i64;
        let bottom = ((piece.paint_y + piece.paint_height) / cell_h).round() as i64;
        let height = (bottom - offset).max(1);
        (offset + height).max(1)
    } else {
        1
    }
}

/// The terminal presentation is selected only after canonical CSS layout.
/// Graphical paint and CSSOM continue to consume `item.text`; this adapter may
/// add character-cell widget punctuation without feeding it back into either.
fn terminal_piece_text(piece: &super::inline::Piece) -> &str {
    piece
        .item
        .terminal_text
        .as_deref()
        .unwrap_or(&piece.item.text)
}

fn terminal_piece_text_with_space(piece: &super::inline::Piece) -> String {
    let mut text = terminal_piece_text(piece).to_owned();
    if piece.space_before && piece.item.image.is_none() && !text.is_empty() {
        text.insert(0, ' ');
    }
    text
}

/// Resolve horizontal CSS clipping while the canonical shaped geometry is
/// still available. CSS Overflow 3 §5.1 clips glyph painting and permits a
/// character to be partially visible. A terminal cannot paint part of a
/// character cell, so every shaped cluster intersecting the clip contributes
/// its complete character(s). The returned offset is still in CSS pixels and
/// is quantized together with the run's origin by the caller.
fn visible_terminal_piece_text(
    piece: &super::inline::Piece,
    origin_x: f32,
    clip: Option<Clip>,
) -> Option<(String, f32)> {
    let text = terminal_piece_text(piece);
    if piece.item.image.is_some() || text.is_empty() {
        return Some((text.to_owned(), 0.0));
    }
    let Some(clip) = clip else {
        return Some((text.to_owned(), 0.0));
    };
    let Some(shaped) = piece.shaped.as_ref().filter(|shaped| shaped.text == text) else {
        let right = origin_x + piece.paint_width.max(0.0);
        return (right > clip.x0 && origin_x < clip.x1).then(|| (text.to_owned(), 0.0));
    };

    let mut byte_start = usize::MAX;
    let mut byte_end = 0usize;
    let mut x_offset = f32::INFINITY;
    for cluster in &shaped.clusters {
        let left = origin_x + cluster.x.min(cluster.x + cluster.advance);
        let right = origin_x + cluster.x.max(cluster.x + cluster.advance);
        if right > clip.x0 && left < clip.x1 {
            byte_start = byte_start.min(cluster.text_range.start);
            byte_end = byte_end.max(cluster.text_range.end);
            x_offset = x_offset.min(cluster.x.min(cluster.x + cluster.advance));
        }
    }
    if byte_start >= byte_end {
        return None;
    }
    let visible = text.get(byte_start..byte_end)?.to_owned();
    Some((visible, x_offset.max(0.0)))
}

/// Split an ordinary terminal text flow at a display-cell boundary. Prefer
/// whitespace, then use an emergency character split when proportional text
/// cannot fit the containing cell band. Independently sized horizontal items
/// never enter this reflow path.
fn terminal_text_chunk(text: &str, max_cells: usize) -> (String, String) {
    if max_cells == 0 {
        return (String::new(), text.to_owned());
    }
    if display_width(text) <= max_cells {
        return (text.to_owned(), String::new());
    }
    let mut prefix = truncate_to_width(text, max_cells);
    if prefix.is_empty() {
        let Some(first) = text.chars().next() else {
            return (String::new(), String::new());
        };
        prefix.push(first);
    } else {
        let break_at = prefix
            .char_indices()
            .filter(|(index, ch)| *index > 0 && ch.is_whitespace())
            .map(|(index, ch)| index + ch.len_utf8())
            .next_back();
        if let Some(end) = break_at {
            prefix.truncate(end);
            // CSS Text's collapsible whitespace is the soft-wrap
            // opportunity, not painted content at the end of the line.
            // Consume it from the continuation while leaving no trailing
            // terminal cell behind.
            let consumed = display_width(&prefix);
            let visible = prefix.trim_end_matches(char::is_whitespace).to_owned();
            return (visible, drop_cells(text, consumed));
        } else if text
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace())
        {
            // A collapsed leading gap cannot be painted by itself at the end
            // of a terminal row. Move the word to the continuation; callers
            // use the empty prefix as the signal to drop this gap at the new
            // row's inline start.
            let leading_end = text
                .char_indices()
                .find(|(_, character)| !character.is_whitespace())
                .map_or(text.len(), |(index, _)| index);
            return (String::new(), text[leading_end..].to_owned());
        }
    }
    let used = display_width(&prefix);
    (prefix, drop_cells(text, used))
}

/// Simulate a terminal reflow inside one canonical containing-cell band.
/// `start_col` lets a soft-wrapped CSS line continue in the final terminal row
/// of its predecessor. The returned end column is used to carry the next line
/// without treating a proportional CSS line-box boundary as a forced break.
#[allow(clippy::too_many_arguments)]
fn terminal_line_extent(
    line: &Frag<'_>,
    ox: f32,
    cell_w: f32,
    cell_h: f32,
    right: i64,
    continuation_col: i64,
    start_col: i64,
    joins_previous: bool,
) -> (i64, i64) {
    let FragKind::Line(line_fragment) = &line.kind else {
        return (1, start_col);
    };
    let mut rows = 1i64;
    let mut span = 1i64;
    let mut pen = start_col;
    let mut restore_soft_gap = joins_previous;
    for piece in &line_fragment.pieces {
        let mut preferred = ((line.x + piece.x + piece.paint_x - ox) / cell_w).round() as i64;
        if piece.space_before
            && piece.item.image.is_none()
            && !terminal_piece_text(piece).is_empty()
        {
            preferred -= 1;
        }
        let mut remaining_text =
            (!piece.item.image.is_some() && !terminal_piece_text(piece).is_empty()).then(|| {
                let mut text = terminal_piece_text_with_space(piece);
                // CSS Text collapses the whitespace at a canonical soft line
                // break. When the terminal adapter rejoins that line into the
                // predecessor's final cell row, painting restores one gap. The
                // extent simulation MUST make the identical transition or it will
                // under-reserve rows and a following block can overwrite the
                // unaccounted continuation.
                if restore_soft_gap && !piece.space_before {
                    text.insert(0, ' ');
                }
                restore_soft_gap = false;
                text
            });
        let mut remaining = remaining_text.as_deref().map_or_else(
            || terminal_piece_width(piece, cell_w, cell_h),
            display_width,
        );
        let mut col = preferred.max(pen);
        while remaining > 0 {
            if col >= right {
                rows += 1;
                pen = continuation_col;
                col = continuation_col;
                continue;
            }
            let available = (right - col) as usize;
            if let Some(text) = remaining_text.as_deref() {
                let (prefix, rest) = terminal_text_chunk(text, available);
                let used = display_width(&prefix);
                if used == 0 {
                    remaining_text = (!rest.is_empty()).then_some(rest);
                    remaining = remaining_text.as_deref().map_or(0, display_width);
                    rows += 1;
                    pen = continuation_col;
                    col = continuation_col;
                    continue;
                }
                remaining_text = (!rest.is_empty()).then_some(rest);
                remaining = remaining_text.as_deref().map_or(0, display_width);
                pen = col + used as i64;
                if remaining > 0 {
                    rows += 1;
                    pen = continuation_col;
                    col = continuation_col;
                }
            } else {
                let width = remaining as i64;
                if width > available as i64 && col > continuation_col {
                    rows += 1;
                    pen = continuation_col;
                    col = continuation_col;
                } else {
                    pen = col + width;
                    remaining = 0;
                }
            }
        }
        span = span.max(rows - 1 + terminal_piece_row_extent(piece, cell_h));
    }
    (span.max(rows), pen)
}

fn terminal_line_span(
    line: &Frag<'_>,
    ox: f32,
    cell_w: f32,
    cell_h: f32,
    columns: usize,
    continuation_col: i64,
    clip_right: Option<i64>,
) -> i64 {
    let FragKind::Line(line_fragment) = &line.kind else {
        return 1;
    };

    // A line that fits its containing block but whose quantized edge remains
    // beyond the viewport is clipped by the compositor, not wrapped. This is
    // what keeps an off-screen sidebar from reserving phantom rows for the
    // main flow. A containing edge at or inside the viewport can require
    // terminal-side reflow.
    let Some(right) =
        clip_right.filter(|right| *right > continuation_col && *right <= columns as i64)
    else {
        if clip_right.is_some() {
            return 1;
        }
        let mut row_offset = 0i64;
        let mut span = 1i64;
        let mut pen = continuation_col;
        for piece in &line_fragment.pieces {
            let width = terminal_piece_width(piece, cell_w, cell_h) as i64;
            let mut preferred = ((line.x + piece.x + piece.paint_x - ox) / cell_w).round() as i64;
            if piece.space_before
                && piece.item.image.is_none()
                && !terminal_piece_text(piece).is_empty()
            {
                preferred -= 1;
            }
            let col = preferred.max(pen);
            if columns > 0 && col > continuation_col && col + width > columns as i64 {
                row_offset += 1;
                pen = continuation_col;
            } else {
                pen = col + width;
            }
            span = span.max(row_offset + terminal_piece_row_extent(piece, cell_h));
        }
        return span.max(row_offset + 1);
    };

    let mut row_offset = 0i64;
    let mut span = 1i64;
    let mut pen = continuation_col;
    for piece in &line_fragment.pieces {
        let mut preferred = ((line.x + piece.x + piece.paint_x - ox) / cell_w).round() as i64;
        if piece.space_before
            && piece.item.image.is_none()
            && !terminal_piece_text(piece).is_empty()
        {
            preferred -= 1;
        }
        let mut remaining_text = (!piece.item.image.is_some()
            && !terminal_piece_text(piece).is_empty())
        .then(|| terminal_piece_text_with_space(piece));
        let mut remaining = remaining_text.as_deref().map_or_else(
            || terminal_piece_width(piece, cell_w, cell_h),
            display_width,
        );
        let mut col = if row_offset > 0 {
            continuation_col
        } else {
            preferred.max(pen)
        };
        while remaining > 0 {
            if col >= right {
                row_offset += 1;
                pen = continuation_col;
                col = continuation_col;
                continue;
            }
            let available = (right - col) as usize;
            if available == 0 {
                row_offset += 1;
                pen = continuation_col;
                col = continuation_col;
                continue;
            }
            if let Some(text) = remaining_text.as_deref() {
                let (prefix, rest) = terminal_text_chunk(text, available);
                let used = display_width(&prefix);
                if used == 0 {
                    remaining_text = (!rest.is_empty()).then_some(rest);
                    remaining = remaining_text.as_deref().map_or(0, display_width);
                    row_offset += 1;
                    pen = continuation_col;
                    col = continuation_col;
                    continue;
                }
                remaining_text = (!rest.is_empty()).then_some(rest);
                remaining = remaining_text.as_deref().map_or(0, display_width);
                pen = col + used as i64;
                if remaining > 0 {
                    row_offset += 1;
                    pen = continuation_col;
                    col = continuation_col;
                }
            } else {
                // Atomic inline boxes remain unbreakable. A box that starts
                // inside the cell may move to the continuation row; one that
                // starts at the cell edge retains the CSS overflow behavior
                // and is clipped by the containing edge.
                let width = remaining as i64;
                if width > available as i64 && col > continuation_col {
                    row_offset += 1;
                    pen = continuation_col;
                    col = continuation_col;
                } else {
                    pen = col + width;
                    remaining = 0;
                }
            }
        }
        span = span.max(row_offset + terminal_piece_row_extent(piece, cell_h));
    }
    span.max(row_offset + 1)
}

/// Paint one STACKING CONTEXT per Appendix E (the root element always forms
/// one). `ox`/`oy` shift the coordinate origin (the pinned layer paints in
/// box-relative coordinates).
#[allow(clippy::too_many_arguments)]
fn build_sc(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
    line_rows: &HashMap<usize, TerminalLinePlacement>,
) {
    // E.2 step 1/2: the element's own background.
    hit_op(f, ops, cw, ch, ox, oy, links);
    fill_op(dom, f, ops, cw, ch, ox, oy);
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
        build_sc(dom, c, ops, cw, ch, ox, oy, links, line_rows);
    }
    // E.2 step 4: in-flow, non-positioned block-level backgrounds, tree order.
    inflow_bgs(dom, f, ops, cw, ch, ox, oy, links, line_rows);
    // E.2 step 5: non-positioned floats, tree order (§9.5) — each as its own
    // pseudo stacking context.
    build_floats(dom, f, ops, cw, ch, ox, oy, links, line_rows);
    // E.2 step 7: in-flow, non-positioned inline content, tree order.
    inflow_content(dom, f, ops, cw, ch, ox, oy, links, line_rows);
    // E.2 step 8: z:auto positioned (pseudo) and z:0 SCs, one merged
    // tree-order list.
    for (c, is_sc) in zero {
        if is_sc {
            build_sc(dom, c, ops, cw, ch, ox, oy, links, line_rows);
        } else {
            build_pseudo(dom, c, ops, cw, ch, ox, oy, links, line_rows);
        }
    }
    // E.2 step 9: positive-z stacking contexts, smallest first.
    for c in pos {
        build_sc(dom, c, ops, cw, ch, ox, oy, links, line_rows);
    }
}

/// A positioned z:auto box: painted atomically for its own background and
/// in-flow content, but its positioned descendants and child SCs were lifted
/// into the enclosing real stacking context (E.2 step 8).
#[allow(clippy::too_many_arguments)]
fn build_pseudo(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
    line_rows: &HashMap<usize, TerminalLinePlacement>,
) {
    hit_op(f, ops, cw, ch, ox, oy, links);
    fill_op(dom, f, ops, cw, ch, ox, oy);
    inflow_bgs(dom, f, ops, cw, ch, ox, oy, links, line_rows);
    build_floats(dom, f, ops, cw, ch, ox, oy, links, line_rows);
    inflow_content(dom, f, ops, cw, ch, ox, oy, links, line_rows);
}

/// Appendix E step 5: paint every non-positioned float in `f`'s subtree, in
/// tree order, each as its own pseudo stacking context — its background, its
/// in-flow content, and its own nested floats (its positioned/SC descendants
/// belong to the enclosing real stacking context, collected there, §9.5). The
/// walk descends through plain in-flow boxes but treats a float atomically; a
/// float that is itself positioned or forms a stacking context is left to the
/// normal positioned/SC path (its `sc`/`positioned` flag wins).
#[allow(clippy::too_many_arguments)]
fn build_floats(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
    line_rows: &HashMap<usize, TerminalLinePlacement>,
) {
    for c in &f.children {
        if c.paint.sc || c.paint.positioned {
            continue;
        }
        if c.paint.float {
            hit_op(c, ops, cw, ch, ox, oy, links);
            fill_op(dom, c, ops, cw, ch, ox, oy);
            inflow_bgs(dom, c, ops, cw, ch, ox, oy, links, line_rows);
            build_floats(dom, c, ops, cw, ch, ox, oy, links, line_rows);
            inflow_content(dom, c, ops, cw, ch, ox, oy, links, line_rows);
        } else {
            build_floats(dom, c, ops, cw, ch, ox, oy, links, line_rows);
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
#[allow(clippy::too_many_arguments)]
fn inflow_bgs(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
    _line_rows: &HashMap<usize, TerminalLinePlacement>,
) {
    for c in &f.children {
        if c.paint.sc || c.paint.positioned || c.paint.float {
            continue;
        }
        if matches!(c.kind, FragKind::Block) {
            hit_op(c, ops, cw, ch, ox, oy, links);
            fill_op(dom, c, ops, cw, ch, ox, oy);
        }
        inflow_bgs(dom, c, ops, cw, ch, ox, oy, links, _line_rows);
    }
}

/// In-flow, non-positioned inline content, tree order (E.2 step 7). Floats
/// paint as a unit in step 5 (`build_floats`), so they're skipped here.
#[allow(clippy::too_many_arguments)]
fn inflow_content(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
    links: &HashMap<NodeId, Link>,
    line_rows: &HashMap<usize, TerminalLinePlacement>,
) {
    // Keep the terminal pen across adjacent canonical line boxes in one
    // block's inline formatting context. CSS Text's soft-wrap line boxes are
    // not forced breaks, so a line that was split for proportional CSS
    // metrics may continue into the same terminal row after quantization.
    let mut row_pens: HashMap<i64, i64> = HashMap::new();
    let mut previous_line: Option<(i64, bool)> = None;
    for c in &f.children {
        if c.paint.sc || c.paint.positioned || c.paint.float {
            previous_line = None;
            continue;
        }
        if !matches!(c.kind, FragKind::Block) {
            hit_op(c, ops, cw, ch, ox, oy, links);
        }
        if let FragKind::Line(line) = &c.kind {
            // Proportional CSS advances do not map monotonically to terminal
            // display-cell widths (for example `Multi:` may shape narrower
            // than six 8px cells). Preserve the inline sequence at this final
            // compatibility boundary: snap each piece's preferred CSS-pixel
            // origin, then advance it past the preceding adapted piece on the
            // same terminal row. This never feeds back into line breaking or
            // fragment geometry.
            let placement = line_rows
                .get(&(std::ptr::from_ref(c) as usize))
                .copied()
                .unwrap_or(TerminalLinePlacement {
                    row: ((c.y - oy) / ch).round() as i64,
                    end_row: ((c.y - oy) / ch).round() as i64,
                    joins_previous: false,
                    columns: usize::MAX,
                    continuation_col: ((c.x - ox) / cw).round() as i64,
                    clip_right: None,
                    reflow_right: None,
                    band: None,
                });
            let joins_soft_line_on_same_row = placement.joins_previous
                && previous_line
                    .is_some_and(|(end_row, forced)| !forced && end_row == placement.row);
            let mut first_text_item = true;
            if !placement.joins_previous {
                row_pens.clear();
            }
            for p in &line.pieces {
                let canonical_text_origin = c.x + p.x;
                let text_clip_resolved = p.item.image.is_none()
                    && p.item.terminal_text.is_none()
                    && p.shaped
                        .as_ref()
                        .is_some_and(|shaped| shaped.text == p.item.text);
                let (visible_text, text_x_offset) = if text_clip_resolved {
                    let Some(visible) =
                        visible_terminal_piece_text(p, canonical_text_origin, c.clip)
                    else {
                        continue;
                    };
                    visible
                } else {
                    (terminal_piece_text(p).to_owned(), 0.0)
                };
                let mut item_x = c.x + p.x + p.paint_x + text_x_offset;
                let canonical_paint_right = c.x + p.x + p.paint_x + p.paint_width;
                let bounds = clipped_paint_bounds(
                    item_x,
                    c.y + p.y + p.paint_y,
                    canonical_paint_right,
                    c.y + p.y + p.paint_y + p.paint_height,
                    c.clip,
                );
                // A terminal row has no typographic baseline. Anchor adapted
                // items to the canonical line-box top (plus object-fit inset),
                // not to the glyph/image baseline offset: a short inline image
                // and the following line may otherwise round onto the same row
                // and overwrite each other in the cell compositor.
                let item_y = c.y + p.paint_y;
                let mut item = Item {
                    col: 0,
                    width: 0,
                    height: 1,
                    text: visible_text,
                    kind: p.item.kind,
                    image: p.item.image.clone(),
                    emph: p.item.emph,
                    node: p.item.node,
                    link: p.item.link.clone(),
                    crop: p.item.crop,
                    pixelated: p.item.pixelated,
                    invisible: p.item.invisible,
                    terminal_band: placement.band,
                };
                quantize_item(&mut item, p, cw, ch);
                if p.space_before
                    && text_x_offset <= 0.01
                    && item.image.is_none()
                    && !item.text.is_empty()
                {
                    // The terminal compatibility model deliberately represents
                    // a collapsed proportional-space gap as one cell.
                    item.text.insert(0, ' ');
                    item.width = item.width.saturating_add(1);
                    item_x -= cw;
                }
                if joins_soft_line_on_same_row
                    && first_text_item
                    && item.image.is_none()
                    && !item.text.is_empty()
                    && !p.space_before
                {
                    // CSS Text drops the collapsible whitespace at a soft line
                    // break. If this adapter rejoins that line on the same
                    // terminal row, restore the one collapsed gap at the new
                    // row position.
                    item.text.insert(0, ' ');
                    item.width = item.width.saturating_add(1);
                }
                if item.image.is_none() && !item.text.is_empty() {
                    first_text_item = false;
                }
                let mut clip = if item.image.is_none() && !item.text.is_empty() && item.height <= 1
                {
                    text_item_clip_cells(c.clip, item_y, ox, oy, cw, ch)
                } else {
                    clip_cells(c.clip, ox, oy, cw, ch)
                };
                let baseline_offset = if item.image.is_some() {
                    0
                } else {
                    (p.y / ch).round() as i64
                };
                let mut row = placement.row + baseline_offset + (p.paint_y / ch).round() as i64;
                let mut preferred_col = ((item_x - ox) / cw).round() as i64;
                if item.image.is_none()
                    && !item.text.is_empty()
                    && placement.reflow_right.is_none()
                    && let Some(right) = placement.clip_right
                    && item.width as i64 > right - preferred_col
                {
                    // CSS Overflow 3 clips the canonical glyphs, not a second
                    // fixed-cell spelling of the run. Horizontal text clipping
                    // is resolved from the shaped clusters before this item is
                    // emitted; retain the adapted spelling whole here.
                    let retained_right = preferred_col + i64::from(item.width);
                    clip.c1 = clip.c1.max(retained_right);
                }
                let reflow_right = placement.reflow_right.filter(|right| {
                    *right > placement.continuation_col && *right <= placement.columns as i64
                });
                let text_reflow = reflow_right.is_some()
                    && item.image.is_none()
                    && !item.text.is_empty()
                    && item.height <= 1;
                loop {
                    let pen = *row_pens.entry(row).or_insert(preferred_col);
                    let col = preferred_col.max(pen);

                    if let Some(right) = reflow_right {
                        if col >= right {
                            row += 1;
                            preferred_col = placement.continuation_col;
                            continue;
                        }
                        let available = (right - col) as usize;
                        if text_reflow && item.width as usize > available {
                            let (prefix, rest) = terminal_text_chunk(&item.text, available);
                            let used = display_width(&prefix);
                            if used == 0 {
                                let next_row = row + 1;
                                let gap_has_following_text = row_pens
                                    .get(&next_row)
                                    .is_some_and(|pen| *pen > placement.continuation_col);
                                if !gap_has_following_text {
                                    item.text = rest;
                                    item.width =
                                        display_width(&item.text).min(u16::MAX as usize) as u16;
                                }
                                row += 1;
                                preferred_col = placement.continuation_col;
                                continue;
                            }
                            let mut chunk = item.clone();
                            chunk.text = prefix;
                            chunk.width = used.min(u16::MAX as usize) as u16;
                            let mut chunk_clip = clip;
                            chunk_clip.c1 = chunk_clip.c1.min(right);
                            row_pens.insert(row, col + used as i64);
                            ops.push(Op::Item {
                                row,
                                col,
                                item: chunk,
                                clip: chunk_clip,
                                bounds,
                                text_clip_resolved,
                            });
                            if rest.is_empty() {
                                break;
                            }
                            item.text = rest;
                            item.width = display_width(&item.text).min(u16::MAX as usize) as u16;
                            row += 1;
                            preferred_col = placement.continuation_col;
                            continue;
                        }
                        if !text_reflow
                            && col > placement.continuation_col
                            && col + i64::from(item.width) > right
                        {
                            row += 1;
                            preferred_col = placement.continuation_col;
                            continue;
                        }
                    } else if placement.clip_right.is_none()
                        && placement.columns != usize::MAX
                        && col > placement.continuation_col
                        && col + i64::from(item.width) > placement.columns as i64
                    {
                        row += 1;
                        preferred_col = placement.continuation_col;
                        continue;
                    }

                    let pen = row_pens.entry(row).or_insert(col);
                    *pen = col + i64::from(item.width);
                    let mut item_clip = clip;
                    if placement.reflow_right.is_some()
                        && let Some(right) = placement.clip_right
                    {
                        item_clip.c1 = item_clip.c1.min(right);
                    }
                    ops.push(Op::Item {
                        row,
                        col,
                        item,
                        clip: item_clip,
                        bounds,
                        text_clip_resolved,
                    });
                    if p.item.kind == ItemKind::Form && p.item.style_node != NO_NODE {
                        push_outline_cells(
                            ops,
                            dom.outline(p.item.style_node),
                            row,
                            col,
                            ((p.box_width / cw).round() as i64).max(1),
                            ((p.box_height / ch).round() as i64).max(1),
                            cw,
                            ch,
                            item_clip,
                            p.item.node,
                        );
                    }
                    break;
                }
            }
            previous_line = Some((placement.end_row, line.forced));
        } else {
            previous_line = None;
        }
        inflow_content(dom, c, ops, cw, ch, ox, oy, links, line_rows);
    }
}

/// The one canonical CSS-pixel → terminal-cell adaptation for inline items.
/// It affects only the legacy `Item` copy; fragment and shaped-run geometry
/// remains untouched for graphical paint and CSSOM consumers.
fn quantize_item(item: &mut Item, piece: &super::inline::Piece, cw: f32, ch: f32) {
    if item.image.is_some() || (piece.shaped.is_none() && item.text.is_empty()) {
        let x0 = (piece.paint_x / cw).round();
        let x1 = ((piece.paint_x + piece.paint_width) / cw).round();
        let y0 = (piece.paint_y / ch).round();
        let y1 = ((piece.paint_y + piece.paint_height) / ch).round();
        item.width = (x1 - x0).max(1.0).min(u16::MAX as f32) as u16;
        item.height = (y1 - y0).max(1.0).min(u16::MAX as f32) as u16;
    } else {
        item.width = display_width(&item.text).min(u16::MAX as usize) as u16;
        item.height = 1;
    }
}

/// Whether a canonical background should claim an opaque terminal cell span.
///
/// The graphical display list retains every non-transparent background. The
/// terminal cannot alpha-blend arbitrary CSS image/gradient layers, so its
/// historical binary cover approximation is deliberately quarantined here,
/// after layout and immediately before cell compositing.
fn terminal_background_covers_dom(dom: &Dom, node: NodeId) -> bool {
    if node == NO_NODE {
        return false;
    }
    if let Some(color) = dom.computed_value_resolved(node, "background-color") {
        let color = color.trim().to_ascii_lowercase();
        if !color.is_empty()
            && !matches!(
                color.as_str(),
                "transparent"
                    | "none"
                    | "initial"
                    | "inherit"
                    | "unset"
                    | "revert"
                    | "revert-layer"
            )
            && !zero_alpha_color(&color)
        {
            return true;
        }
    }
    let Some(image) = dom.computed_value_resolved(node, "background-image") else {
        return false;
    };
    let image = image.trim().to_ascii_lowercase();
    if image.is_empty()
        || matches!(
            image.as_str(),
            "none" | "initial" | "inherit" | "unset" | "revert" | "revert-layer"
        )
    {
        return false;
    }
    split_top_level_commas(&image).any(|layer| {
        let layer = layer.trim();
        layer.starts_with("url(") || gradient_mean_alpha(layer) > 0.5
    })
}

fn gradient_mean_alpha(gradient: &str) -> f32 {
    let Some(open) = gradient.find('(') else {
        return 1.0;
    };
    let inner = &gradient[open + 1..gradient.rfind(')').unwrap_or(gradient.len())];
    let mut sum = 0.0;
    let mut count = 0u32;
    for (index, part) in split_top_level_commas(inner).enumerate() {
        let part = part.trim();
        if index == 0 && is_gradient_direction(part) {
            continue;
        }
        if !part.is_empty() {
            sum += color_stop_alpha(part);
            count += 1;
        }
    }
    if count == 0 { 1.0 } else { sum / count as f32 }
}

fn is_gradient_direction(part: &str) -> bool {
    part.starts_with("to ")
        || part.starts_with("at ")
        || part.contains(" at ")
        || part.starts_with("circle")
        || part.starts_with("ellipse")
        || part.starts_with("closest-")
        || part.starts_with("farthest-")
        || part.starts_with("from ")
        || part.split_whitespace().next().is_some_and(|word| {
            ["deg", "rad", "grad", "turn"].iter().any(|unit| {
                word.ends_with(unit) && word.trim_end_matches(unit).parse::<f32>().is_ok()
            })
        })
}

fn color_stop_alpha(stop: &str) -> f32 {
    if stop.contains("transparent") {
        return 0.0;
    }
    for prefix in ["rgb", "hsl", "hwb"] {
        if let Some(index) = stop.find(prefix) {
            let rest = &stop[index..];
            if let Some(open) = rest.find('(') {
                let inner = &rest[open + 1..];
                let inner = &inner[..inner.find(')').unwrap_or(inner.len())];
                return color_args_alpha(inner).unwrap_or(1.0).clamp(0.0, 1.0);
            }
        }
    }
    1.0
}

fn split_top_level_commas(value: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts.into_iter()
}

fn color_args_alpha(arguments: &str) -> Option<f32> {
    let alpha = if let Some((_, alpha)) = arguments.rsplit_once('/') {
        alpha
    } else {
        let parts: Vec<&str> = arguments.split(',').collect();
        if parts.len() < 4 {
            return None;
        }
        parts[3]
    };
    let alpha = alpha.trim();
    let (number, percentage) = match alpha.strip_suffix('%') {
        Some(number) => (number.trim(), true),
        None => (alpha, false),
    };
    let value = number.parse::<f32>().ok()?;
    Some(if percentage { value / 100.0 } else { value })
}

fn zero_alpha_color(color: &str) -> bool {
    if !(color.starts_with("rgb") || color.starts_with("hsl") || color.starts_with("hwb")) {
        return false;
    }
    let Some((_, arguments)) = color.split_once('(') else {
        return false;
    };
    color_args_alpha(arguments.trim_end_matches(')')).is_some_and(|alpha| alpha == 0.0)
}

/// The opaque background fill of a fragment's border box, when it has one.
fn fill_op(
    dom: &TerminalPaintModel,
    f: &Frag<'_>,
    ops: &mut Vec<Op>,
    cw: f32,
    ch: f32,
    ox: f32,
    oy: f32,
) {
    let row0 = ((f.y - oy) / ch).round() as i64;
    let row1 = ((f.y - oy + f.h) / ch).round() as i64;
    let col0 = ((f.x - ox) / cw).round() as i64;
    let col1 = ((f.x - ox + f.w) / cw).round() as i64;
    let clip = clip_cells(f.clip, ox, oy, cw, ch);
    if f.paint.bg
        && dom.node(f.node).is_some_and(|node| node.background_covers)
        && row1 > row0
        && col1 > col0
        && let Some(bounds) = clipped_paint_bounds(f.x, f.y, f.x + f.w, f.y + f.h, f.clip)
    {
        ops.push(Op::Fill {
            row0,
            row1,
            col0,
            col1,
            clip,
            bounds,
        });
    }
    if BORDERS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
        && !f.border.iter().all(|width| *width <= 0.0)
        && row1 > row0
        && col1 > col0
    {
        let horizontal = "─".repeat((col1 - col0) as usize);
        if f.border[TOP] > 0.0 {
            ops.push(Op::Item {
                row: row0,
                col: col0,
                item: border_item(f.node, horizontal.clone()),
                clip,
                bounds: None,
                text_clip_resolved: false,
            });
        }
        if f.border[BOTTOM] > 0.0 {
            ops.push(Op::Item {
                row: row1 - 1,
                col: col0,
                item: border_item(f.node, horizontal),
                clip,
                bounds: None,
                text_clip_resolved: false,
            });
        }
        for row in row0..row1 {
            if f.border[LEFT] > 0.0 {
                ops.push(Op::Item {
                    row,
                    col: col0,
                    item: border_item(f.node, String::from("│")),
                    clip,
                    bounds: None,
                    text_clip_resolved: false,
                });
            }
            if f.border[RIGHT] > 0.0 {
                ops.push(Op::Item {
                    row,
                    col: col1 - 1,
                    item: border_item(f.node, String::from("│")),
                    clip,
                    bounds: None,
                    text_clip_resolved: false,
                });
            }
        }
    }
    // CSS UI 4 §3 outlines are always a paint decoration, even when TRust's
    // optional terminal border chrome is disabled. Quantize the outside edge
    // at the same single px→cell boundary as every other paint operation.
    push_outline_box(
        ops,
        f.paint.outline,
        f.x,
        f.y,
        f.w,
        f.h,
        ox,
        oy,
        cw,
        ch,
        clip,
        f.node,
    );
}

fn border_item(node: NodeId, text: String) -> Item {
    Item {
        col: 0,
        width: display_width(&text).min(u16::MAX as usize) as u16,
        height: 1,
        text,
        kind: ItemKind::Border,
        image: None,
        emph: Emphasis::default(),
        node,
        link: None,
        crop: false,
        pixelated: false,
        invisible: false,
        terminal_band: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_outline_box(
    ops: &mut Vec<Op>,
    outline: Outline,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    ox: f32,
    oy: f32,
    cw: f32,
    ch: f32,
    clip: ClipCells,
    node: NodeId,
) {
    if !outline.paints() || matches!(outline.style, OutlineStyle::Auto) {
        return;
    }
    let grow = outline.offset + outline.width;
    let row0 = ((y - oy - grow) / ch).round() as i64;
    let row1 = ((y - oy + h + grow) / ch).round() as i64;
    let col0 = ((x - ox - grow) / cw).round() as i64;
    let col1 = ((x - ox + w + grow) / cw).round() as i64;
    if row1 <= row0 || col1 <= col0 {
        return;
    }
    let horizontal = "─".repeat((col1 - col0) as usize);
    ops.push(Op::Item {
        row: row0,
        col: col0,
        item: border_item(node, horizontal.clone()),
        clip,
        bounds: None,
        text_clip_resolved: false,
    });
    ops.push(Op::Item {
        row: row1 - 1,
        col: col0,
        item: border_item(node, horizontal),
        clip,
        bounds: None,
        text_clip_resolved: false,
    });
    for row in row0..row1 {
        ops.push(Op::Item {
            row,
            col: col0,
            item: border_item(node, String::from("│")),
            clip,
            bounds: None,
            text_clip_resolved: false,
        });
        ops.push(Op::Item {
            row,
            col: col1 - 1,
            item: border_item(node, String::from("│")),
            clip,
            bounds: None,
            text_clip_resolved: false,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn push_outline_cells(
    ops: &mut Vec<Op>,
    outline: Outline,
    row: i64,
    col: i64,
    width: i64,
    height: i64,
    cw: f32,
    ch: f32,
    clip: ClipCells,
    node: NodeId,
) {
    if !outline.paints() || matches!(outline.style, OutlineStyle::Auto) {
        return;
    }
    let grow = outline.offset + outline.width;
    let grow_rows = ((grow / ch).round() as i64).max(1);
    let grow_cols = ((grow / cw).round() as i64).max(1);
    let row0 = row - grow_rows;
    let row1 = row + height + grow_rows;
    let col0 = col - grow_cols;
    let col1 = col + width + grow_cols;
    if row1 <= row0 || col1 <= col0 {
        return;
    }
    let horizontal = "─".repeat((col1 - col0) as usize);
    ops.push(Op::Item {
        row: row0,
        col: col0,
        item: border_item(node, horizontal.clone()),
        clip,
        bounds: None,
        text_clip_resolved: false,
    });
    ops.push(Op::Item {
        row: row1 - 1,
        col: col0,
        item: border_item(node, horizontal),
        clip,
        bounds: None,
        text_clip_resolved: false,
    });
    for row in row0..row1 {
        ops.push(Op::Item {
            row,
            col: col0,
            item: border_item(node, String::from("│")),
            clip,
            bounds: None,
            text_clip_resolved: false,
        });
        ops.push(Op::Item {
            row,
            col: col1 - 1,
            item: border_item(node, String::from("│")),
            clip,
            bounds: None,
            text_clip_resolved: false,
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

fn is_terminal_text_run(item: &Item) -> bool {
    item.image.is_none() && !item.text.is_empty() && item.kind != ItemKind::Border
}

fn op_paint_bounds(op: &Op) -> Option<Clip> {
    match op {
        Op::Fill { bounds, .. } => Some(*bounds),
        Op::Item { item, bounds, .. } if !item.invisible => *bounds,
        _ => None,
    }
}

fn op_cell_bounds(op: &Op) -> Option<(i64, i64, i64, i64)> {
    match op {
        Op::Fill {
            row0,
            row1,
            col0,
            col1,
            clip,
            ..
        } => Some((
            (*row0).max(clip.r0),
            (*row1).min(clip.r1),
            (*col0).max(clip.c0),
            (*col1).min(clip.c1),
        )),
        Op::Item {
            row,
            col,
            item,
            clip,
            ..
        } if !item.invisible => Some((
            (*row).max(clip.r0),
            (*row + i64::from(item.height.max(1))).min(clip.r1),
            (*col).max(clip.c0),
            (*col + i64::from(item.width)).min(clip.c1),
        )),
        _ => None,
    }
}

fn cell_bounds_overlap(left: (i64, i64, i64, i64), right: (i64, i64, i64, i64)) -> bool {
    left.0 < right.1 && right.0 < left.1 && left.2 < right.3 && right.2 < left.3
}

/// A terminal text spelling is emitted whole unless a LATER paint operation
/// genuinely overlaps it in canonical CSS pixels. Adjacent proportional runs
/// can round into the same terminal cell without overlapping in CSS; that is
/// an adapter collision, not permission to delete either run. Operations that
/// exist only in cell space (generated border chrome) use the cell fallback.
fn preserve_text_op(ops: &[Op], index: usize) -> bool {
    let Op::Item {
        item,
        bounds: Some(bounds),
        ..
    } = &ops[index]
    else {
        return false;
    };
    if !is_terminal_text_run(item) {
        return false;
    }
    let cells = op_cell_bounds(&ops[index]);
    !ops[index + 1..].iter().any(|later| {
        if let Some(later_bounds) = op_paint_bounds(later) {
            paint_bounds_overlap(*bounds, later_bounds)
        } else {
            cells
                .zip(op_cell_bounds(later))
                .is_some_and(|(left, right)| cell_bounds_overlap(left, right))
        }
    })
}

/// Composite the display list into `Doc` rows. Canonically non-overlapping
/// text stays whole while every operation still stamps paint-order ownership;
/// genuine later CSS paint therefore covers earlier text, but cell rounding
/// alone cannot manufacture a substring. `alpha`
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
    let preserve_text: Vec<bool> = (0..ops.len())
        .map(|index| preserve_text_op(&ops, index))
        .collect();
    let mut grid: Vec<RowSpans> = Vec::new();
    let ensure = |grid: &mut Vec<RowSpans>, row: usize| {
        while grid.len() <= row {
            grid.push(RowSpans::default());
        }
    };
    // Clip an op's placement to the viewport band, mirroring the P0 painter:
    // left overhang cuts leading cells, the right edge truncates.
    let mut placed: Vec<Option<Placed>> = Vec::with_capacity(ops.len());
    let mut hits: Vec<PlacedHit> = Vec::new();
    // ---- stamping pass (paint order) ----
    for (i, op) in ops.into_iter().enumerate() {
        match op {
            Op::Fill {
                row0,
                row1,
                col0,
                col1,
                clip,
                bounds: _,
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
                bounds: _,
                text_clip_resolved,
            } => {
                let text_run = is_terminal_text_run(&item);
                let horizontal_clip_resolved = text_clip_resolved && text_run;
                // Text's horizontal CSS clip has already been applied to its
                // shaped clusters. Only the actual terminal viewport clips the
                // resulting cell string. Atomic boxes continue to use their
                // quantized CSS clip in both axes.
                let (lo, hi) = if horizontal_clip_resolved {
                    (0, cols_u as i64)
                } else {
                    let lo = clip.c0.clamp(0, cols_u as i64);
                    (lo, clip.c1.clamp(lo, cols_u as i64))
                };
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
                    preserve_text: preserve_text[i],
                }));
            }
            Op::Hit {
                row0,
                row1,
                col0,
                col1,
                node,
                link,
                clip,
            } => {
                let c0 = col0.max(clip.c0).clamp(0, cols_u as i64);
                let c1 = col1.min(clip.c1).clamp(c0, cols_u as i64);
                let r0 = row0.max(clip.r0).max(0);
                let r1 = row1.min(clip.r1).max(r0);
                if c1 > c0 && r1 > r0 {
                    hits.push(PlacedHit {
                        row: r0 as usize,
                        col: c0 as u32,
                        width: (c1 - c0).min(i64::from(u16::MAX)) as u16,
                        height: (r1 - r0).min(i64::from(u16::MAX)) as u16,
                        node,
                        link,
                        order: i,
                    });
                }
                placed.push(None);
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
    // Opaque pixel rects per row: later sliceable text/atomic boxes do not
    // claim image cells. Canonically non-overlapping text bypasses only the
    // ownership-derived substring, not the paint-order stamping pass.
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
            terminal_band: base.item.terminal_band,
        });
        composites.insert(key, layers);
    }
    // ---- emission pass 2: preserved text runs, then sliceable items ----
    for (i, p) in placed.iter().enumerate() {
        let Some(p) = p else { continue };
        if p.item.image.is_some() && !p.item.invisible {
            continue; // emitted above
        }
        if p.preserve_text && is_terminal_text_run(&p.item) {
            let mut item = p.item.clone();
            item.col = p.col.min(u32::from(u16::MAX)) as u16;
            ensure_rows(&mut rows, p.row + usize::from(item.height.max(1)));
            rows[p.row].items.push(item);
            continue;
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
    // Consumers walk a row's items left-to-right. Text may share a preferred
    // column after proportional-to-cell quantization; `visual_columns` extends
    // the later run without mutating either item's content. Keep this stable.
    for row in &mut rows {
        row.items.sort_by_key(|it| it.col);
    }
    // A generated anchor box that already emitted a normal linked text/image
    // item keeps the established item path. The additive hit surface is only
    // needed for a box with no painted activation representation. This is the
    // compatibility boundary: ordinary links and their navigation order do not
    // change, while an empty overlay anchor finally exposes its real CSS box.
    for hit in hits {
        // An ordinary anchor needs no second hit surface once linked content
        // represents it. A media element is different: its small textual
        // caption represents the WHOLE generated video box, including the
        // custom controls that paint over it, so retain that box surface.
        let represented = !matches!(hit.link, Link::Media(_))
            && rows
                .iter()
                .enumerate()
                .flat_map(|(row, items)| items.items.iter().map(move |item| (row, item)))
                .any(|(row, item)| {
                    if item.link.as_ref() != Some(&hit.link) {
                        return false;
                    }
                    let item_right = u32::from(item.col) + u32::from(item.width);
                    let hit_right = hit.col + u32::from(hit.width);
                    let item_bottom = row + usize::from(item.height.max(1));
                    let hit_bottom = hit.row + usize::from(hit.height.max(1));
                    row < hit_bottom
                        && hit.row < item_bottom
                        && u32::from(item.col) < hit_right
                        && hit.col < item_right
                });
        if represented {
            continue;
        }
        ensure_rows(&mut rows, hit.row + usize::from(hit.height.max(1)));
        let item = rows[hit.row].items.len();
        rows[hit.row].items.push(Item {
            col: hit.col.min(u32::from(u16::MAX)) as u16,
            width: hit.width,
            height: hit.height.max(1),
            text: String::new(),
            kind: ItemKind::HitRegion,
            image: None,
            emph: Emphasis::default(),
            node: hit.node,
            link: Some(hit.link),
            crop: false,
            pixelated: false,
            invisible: true,
            terminal_band: None,
        });
        rows[hit.row].hits.push(HitBox {
            col: hit.col.min(u32::from(u16::MAX)) as u16,
            width: hit.width,
            height: hit.height.max(1),
            item,
            order: hit.order,
        });
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
    preserve_text: bool,
}

struct PlacedHit {
    row: usize,
    col: u32,
    width: u16,
    height: u16,
    node: NodeId,
    link: Link,
    order: usize,
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
