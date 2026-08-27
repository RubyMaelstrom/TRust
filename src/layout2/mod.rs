//! layout2 — TRust's standards-oriented layout engine.
//!
//! The standard architecture, replacing the former single-pass text flow:
//!
//! ```text
//! styled DOM (dom.rs cascade, KEPT)
//!   → 1. BOX TREE  (tree.rs)   display/formatting context decided once,
//!                              anonymous boxes per CSS 2.1 §9.2; out-of-flow
//!                              boxes routed to static-position marks
//!   → 2. FRAGMENTS (flow.rs)   used geometry in f32 CSS px: widths top-down
//!                              (§10.3.3), real §8.3.1 margin collapsing,
//!                              heights bottom-up; inline.rs builds line
//!                              boxes (CSS Text collapsing/wrapping/align);
//!                              the positioned post-pass lays abspos/fixed
//!                              boxes against their containing blocks'
//!                              FINAL geometry (§10.1/§10.3.7/§10.6.4)
//!   → 3a. GRAPHICS (graphics.rs) retained shaped glyphs and CSS-pixel paint
//!                              primitives for native render backends
//!   → 3b. TERMINAL (paint.rs) CSS 2.1 Appendix E painting order + the ONE
//!                              px→cell compatibility quantization → existing
//!                              `Doc.rows`/`Item` and `FixedItem` consumers
//! ```
//!
//! P0 = the skeleton: block flow, inline text, images, form-control atoms,
//! lists, real UA-stylesheet margins. Flex (P2), grid (P3), positioned/
//! stacking/paint-order/transform-translate (P4), overflow (P5), tables
//! (P6, the CSS 2.1 §17 model in `table.rs`), floats + multi-column (P8b) are
//! all real.
//! P5 splits by CSS Overflow L3 §2: `hidden`/`clip` are a pure CLIP to the
//! padding box (P5a — sr-only boxes clip to nothing, definite-height panels
//! clip their overflow); `auto`/`scroll` are SCROLL CONTAINERS whose overflow
//! rides the scroll axis into a windowed buffer (a vertical Region, P5b —
//! unconditional, every `overflow:auto|scroll` element becomes one; the
//! viewport's OWN overflow is a separate, §3.3 concern, never delegated to a
//! descendant) or an inline strip (a horizontal Carousel, P5c). This is the
//! sole layout engine (the old flow engine was deleted after the P9 soak).
//! Incremental-layout boundaries (P7) are emitted, so a live mutation confined
//! to one re-lays only its subtree and splices back; everything else takes the
//! always-correct full-relayout path.

mod boundary;
mod contract;
mod flex;
mod float;
mod flow;
mod graphics;
mod grid;
mod inline;
mod intrinsic;
mod measure;
// The legacy Row/Item output is now explicitly a terminal compatibility
// adapter. Its source filename is retained to keep this refactor reviewable.
mod replaced;
mod style;
mod table;
#[path = "paint.rs"]
mod terminal;
mod tree;
mod value;

use std::collections::HashMap;

use url::Url;

use crate::doc::Form;
use crate::dom::{Dom, NodeId};

/// Shared CSS geometry/value helpers plus the terminal adapter's
/// `Doc.rows`/`Item` output model (`Region`/`Carousel`/`FixedItem`). `Units`
/// contains only CSS font bases; cell and border settings are re-exported
/// separately from the terminal adapter.
/// Consumed by the renderer (ui.rs/app.rs) and by every engine module below,
/// so it is re-exported flat at `crate::layout2::*` (formerly `crate::layout`).
pub use contract::*;
pub use terminal::set_borders_enabled;

use flow::Flow;
use value::{Len, Vp};

/// Whether a computed CSS-pixel length fits within another after allowing for
/// the bounded f32 noise introduced by equivalent layout arithmetic. This is
/// deliberately relative and much smaller than any paintable subpixel; it
/// does not forgive a substantive overflow.
#[inline]
pub(crate) fn css_px_fits(used: f32, available: f32) -> bool {
    if used <= available {
        return true;
    }
    if !used.is_finite() || !available.is_finite() {
        return false;
    }
    let scale = used.abs().max(available.abs()).max(1.0);
    used - available <= 8.0 * f32::EPSILON * scale
}

/// Resolve HTML's `<source-size-value>` through the same CSS Values parser as
/// layout. Percentages are excluded by the HTML `sizes` grammar; `auto` and
/// non-length sizing keywords likewise do not produce a source size here.
pub(crate) fn image_source_size_px(
    dom: &Dom,
    node: NodeId,
    value: &str,
    viewport: Viewport,
) -> Option<f32> {
    if contains_css_percentage(value) || contains_non_math_css_function(value) {
        return None;
    }
    Len::parse(
        value,
        Units::of(dom, node),
        Vp {
            w: viewport.width,
            h: viewport.height,
        },
    )
    .and_then(|length| length.resolve(None))
    .filter(|value| value.is_finite() && *value >= 0.0)
}

fn contains_non_math_css_function(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() || matches!(bytes[index], b'-' | b'_') {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'-' | b'_'))
            {
                index += 1;
            }
            let mut next = index;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if bytes.get(next) == Some(&b'(') {
                let name = value[start..index].to_ascii_lowercase();
                // These are the CSS math functions the shared length engine
                // currently evaluates. Other CSS functions are invalid in a
                // source-size value; newer math functions fail closed until
                // their numeric evaluator is available here.
                if !matches!(name.as_str(), "calc" | "min" | "max" | "clamp") {
                    return true;
                }
            }
        } else {
            index += 1;
        }
    }
    false
}

fn contains_css_percentage(value: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == '%' {
            return true;
        }
    }
    false
}

/// Canonical graphical result: a CSS-pixel display list plus the same fragment
/// geometry consumed by CSSOM View. No terminal or device-pixel types occur.
#[derive(Clone, Debug)]
pub struct GraphicalLayout {
    pub paint: crate::render::PagePaint,
    pub boxes: HashMap<NodeId, PxRect>,
    pub grid_tracks: HashMap<NodeId, (Vec<f32>, Vec<f32>)>,
    /// Actor ids of every laid independent formatting context that can safely
    /// be serialized as a width-stable subtree patch. Not every such boundary
    /// is independently spliceable in paint order; `boundaries` is the strict
    /// atomic subset. The frontend can still apply a non-atomic patch to its
    /// retained document and run the ordinary full graphical layout without a
    /// whole-document actor serialization or parse.
    pub patch_boundaries: Vec<GraphicalPatchBoundary>,
    /// Paint-only hover selector subjects correlated to actor nodes. Their
    /// geometry remains valid when the actor emits a `BoundaryTier::Paint`
    /// patch, so graphical paint can be replayed without box-tree or flow work.
    pub paint_boundaries: Vec<GraphicalPaintBoundary>,
    /// Layout-contained boxes that are also atomic CSS stacking contexts.
    /// Their Appendix-E command ranges can be replaced independently when the
    /// actor emits a subtree patch and the outer border box stays unchanged.
    pub boundaries: Vec<GraphicalBoundary>,
    paint_cache: Option<GraphicalPaintCache>,
}

#[derive(Clone, Debug)]
struct GraphicalPaintCache {
    root: flow::Frag<'static>,
    fixed: Vec<flow::Frag<'static>>,
    top_layer: Vec<flow::TopFrag<'static>>,
    flow_bottom: f32,
    viewport: Viewport,
    anchors: Vec<(NodeId, f32)>,
    terminal: terminal::TerminalPaintModel,
}

/// The single shared CSS-pixel layout product. The historical name remains as
/// a compatibility alias while callers migrate: graphical frontends consume
/// `paint` directly, and the terminal frontend invokes [`adapt_terminal`] on
/// the retained CSS-pixel fragments. Neither frontend reparses HTML.
pub type PixelLayout = GraphicalLayout;

impl GraphicalLayout {
    pub(crate) fn presentation_eq(&self, other: &Self) -> bool {
        self.paint == other.paint
            && self.boxes == other.boxes
            && self.grid_tracks == other.grid_tracks
            && self.patch_boundaries == other.patch_boundaries
            && self.paint_boundaries == other.paint_boundaries
            && self.boundaries == other.boundaries
            && self.paint_cache.as_ref().map(|cache| &cache.terminal)
                == other.paint_cache.as_ref().map(|cache| &cache.terminal)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphicalBoundary {
    pub actor: usize,
    pub node: NodeId,
    pub rect: crate::render::CssRect,
    pub commands: std::ops::Range<usize>,
    pub lines: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicalPatchBoundary {
    pub actor: usize,
    pub node: NodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicalPaintBoundary {
    pub actor: usize,
    pub node: NodeId,
}

fn graphical_paint_boundaries(
    dom: &Dom,
    boxes: &HashMap<NodeId, PxRect>,
) -> Vec<GraphicalPaintBoundary> {
    let mut boundaries = dom
        .flat_descendants(crate::dom::DOCUMENT)
        .into_iter()
        .filter(|node| boxes.contains_key(node))
        .filter_map(|node| {
            if dom.render_live() && dom.paint_patch_host(node) {
                Some(node)
            } else {
                dom.attr(node, "data-trust-paint-node")
                    .and_then(|actor| actor.parse().ok())
            }
            .map(|actor| GraphicalPaintBoundary { actor, node })
        })
        .collect::<Vec<_>>();
    boundaries.sort_unstable_by_key(|boundary| (boundary.actor, boundary.node));
    boundaries.dedup_by_key(|boundary| boundary.actor);
    boundaries
}

fn graphical_paint_cache(
    root: &flow::Frag<'_>,
    fixed: &[flow::Frag<'_>],
    top_layer: &[flow::TopFrag<'_>],
    flow_bottom: f32,
    viewport: Viewport,
    anchors: &[(NodeId, f32)],
    terminal: terminal::TerminalPaintModel,
) -> Option<GraphicalPaintCache> {
    Some(GraphicalPaintCache {
        root: flow::retain_for_paint(root)?,
        fixed: fixed
            .iter()
            .map(flow::retain_for_paint)
            .collect::<Option<Vec<_>>>()?,
        top_layer: top_layer
            .iter()
            .map(|top| {
                Some(flow::TopFrag {
                    fragment: flow::retain_for_paint(&top.fragment)?,
                    fixed: top.fixed,
                    order: top.order,
                })
            })
            .collect::<Option<Vec<_>>>()?,
        flow_bottom,
        viewport,
        anchors: anchors.to_vec(),
        terminal,
    })
}

/// Rebuild only Appendix-E paint commands from retained CSS-pixel fragments.
/// This is valid for `BoundaryTier::Paint`: the actor has proven that changed
/// declarations cannot affect box construction, intrinsic sizes, flow,
/// transforms, opacity, or stacking order. The painter still queries the
/// updated canonical DOM for colors, backgrounds, borders, shadows,
/// decoration, object fit, and hit-test metadata.
pub fn repaint_graphical(
    layout: &mut GraphicalLayout,
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
) -> bool {
    let Some(cache) = &layout.paint_cache else {
        return false;
    };
    let (paint, patch_boundaries, boundaries) = graphics::paint(
        dom,
        base,
        images,
        &cache.root,
        &cache.fixed,
        &cache.top_layer,
        cache.flow_bottom,
        cache.viewport.width,
        cache.viewport.height,
    );
    layout.paint = paint;
    layout.patch_boundaries = patch_boundaries;
    layout.boundaries = boundaries;
    if let Some(cache) = &mut layout.paint_cache {
        cache.terminal = terminal::TerminalPaintModel::from_dom(dom, base);
        cache.terminal.capture_page_media(dom, base, images);
    }
    true
}

/// A structural subtree transplant changes presentation-arena node ids, so a
/// fragment cache made against the prior tree must not be replayed.
pub fn invalidate_graphical_paint_cache(layout: &mut GraphicalLayout) {
    layout.paint_cache = None;
}

/// Lay HTML out for a graphical viewport expressed in CSS pixels. OS device
/// scale is intentionally absent; Vello applies it while rendering.
pub fn lay_out_graphical(
    dom: &Dom,
    base: &Url,
    viewport: Viewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
) -> GraphicalLayout {
    let trace = std::env::var_os("TRUST_LAYOUT_TRACE").is_some();
    let started = std::time::Instant::now();
    let vp = Vp {
        w: viewport.width,
        h: viewport.height,
    };
    let Some(root) = tree::build(dom, base, controls, forms, vp) else {
        return GraphicalLayout {
            paint: crate::render::PagePaint {
                width: viewport.width,
                height: 0.0,
                background: None,
                lines: Vec::new(),
                primitives: Vec::new(),
                fixed_under_primitives: Vec::new(),
                fixed_primitives: Vec::new(),
                fixed_interleaved: false,
                top_layer: Vec::new(),
                image_requests: Vec::new(),
                scroll_containers: Vec::new(),
                sticky_constraints: Vec::new(),
            },
            boxes: HashMap::new(),
            grid_tracks: HashMap::new(),
            patch_boundaries: Vec::new(),
            paint_boundaries: Vec::new(),
            boundaries: Vec::new(),
            paint_cache: None,
        };
    };
    let tree_done = started.elapsed();
    let flow = Flow {
        dom,
        base,
        forms,
        images,
        vp,
        imemo: Default::default(),
        grid_tracks: Default::default(),
    };
    let (frag, flow_bottom, anchors, fixed, top_layer) = flow.layout(&root);
    let flow_done = started.elapsed();
    let (boxes, _scrolling_areas) = measure::boxes(dom, &frag, &fixed, &top_layer);
    let measure_done = started.elapsed();
    let (paint, patch_boundaries, boundaries) = graphics::paint(
        dom,
        base,
        images,
        &frag,
        &fixed,
        &top_layer,
        flow_bottom,
        vp.w,
        vp.h,
    );
    let paint_boundaries = graphical_paint_boundaries(dom, &boxes);
    let mut terminal_model = terminal::TerminalPaintModel::from_dom(dom, base);
    terminal_model.capture_page_media(dom, base, images);
    let paint_cache = graphical_paint_cache(
        &frag,
        &fixed,
        &top_layer,
        flow_bottom,
        viewport,
        &anchors,
        terminal_model,
    );
    if trace {
        eprintln!(
            "layout: tree={:?} flow={:?} measure={:?} paint={:?} total={:?}",
            tree_done,
            flow_done.saturating_sub(tree_done),
            measure_done.saturating_sub(flow_done),
            started.elapsed().saturating_sub(measure_done),
            started.elapsed(),
        );
    }
    GraphicalLayout {
        paint,
        boxes,
        grid_tracks: flow.grid_tracks.into_inner(),
        patch_boundaries,
        paint_boundaries,
        boundaries,
        paint_cache,
    }
}

/// Quantize one already-laid CSS-pixel page into the terminal compatibility
/// contract. This is the only terminal adapter boundary: it clones retained
/// fragments because scroll-region extraction windows/empties its working
/// copy, while the canonical pixel layout remains reusable by other adapters
/// and by repaint.
pub fn adapt_terminal(
    layout: &PixelLayout,
    viewport: TerminalViewport,
    alpha: &HashMap<String, bool>,
) -> Output {
    let Some(cache) = &layout.paint_cache else {
        return Output {
            rows: Vec::new(),
            anchor_rows: HashMap::new(),
            fixed: Vec::new(),
            regions: Vec::new(),
            carousels: Vec::new(),
            scroll_clips: Vec::new(),
            boundaries: Vec::new(),
            composites: HashMap::new(),
        };
    };
    let mut root = cache.root.clone();
    let fixed = cache.fixed.clone();
    let top_layer = cache.top_layer.clone();
    let candidates = boundary::collect(
        &cache.terminal,
        &root,
        viewport.cell_width,
        viewport.cell_height,
    );
    let out = terminal::paint(
        &cache.terminal,
        &mut root,
        &fixed,
        &top_layer,
        cache.flow_bottom,
        &cache.anchors,
        (viewport.columns, viewport.rows),
        viewport.cell_width,
        viewport.cell_height,
        alpha,
    );
    let boundaries = filter_boundaries(candidates, &out.regions, &out.carousels);
    let mut rows = out.rows;
    page_media_fallback(&cache.terminal, viewport.columns, &mut rows);
    Output {
        rows,
        anchor_rows: out.anchor_rows,
        fixed: out.fixed,
        regions: out.regions,
        carousels: out.carousels,
        scroll_clips: out.scroll_clips,
        boundaries,
        composites: out.composites,
    }
}

/// Re-lay one retained graphical patch boundary at its existing border-box
/// origin and width. The caller may splice the result only when the returned
/// boundary's complete outer rect equals the cached rect; otherwise parent flow
/// geometry changed and a full layout is required.
///
/// Like [`lay_subtree_fragment`], the boundary's own margins are suppressed:
/// they live outside its cached border box. CSS 2.2 Appendix E paint extraction
/// is the same `graphics::paint` pass as a full page, so an atomic stacking
/// context yields an equivalent replacement command segment.
#[allow(clippy::too_many_arguments)]
pub fn lay_graphical_subtree(
    dom: &Dom,
    base: &Url,
    boundary: NodeId,
    rect: crate::render::CssRect,
    viewport: Viewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
) -> Option<GraphicalLayout> {
    let vp = Vp {
        w: rect.width.max(1.0),
        h: viewport.height,
    };
    let mut root = tree::build_at(dom, base, controls, forms, vp, boundary)?;
    root.style.margin = std::array::from_fn(|_| value::Len::px(0.0));
    let flow = Flow {
        dom,
        base,
        forms,
        images,
        vp,
        imemo: Default::default(),
        grid_tracks: Default::default(),
    };
    let (mut frag, mut flow_bottom, mut anchors, mut fixed, mut top_layer) = flow.layout(&root);
    Flow::offset_frag(&mut frag, rect.x, rect.y);
    for (_, y) in &mut anchors {
        *y += rect.y;
    }
    for fragment in &mut fixed {
        Flow::offset_frag(fragment, rect.x, rect.y);
    }
    for top in &mut top_layer {
        Flow::offset_frag(&mut top.fragment, rect.x, rect.y);
    }
    flow_bottom += rect.y;
    let (boxes, _scrolling_areas) = measure::boxes(dom, &frag, &fixed, &top_layer);
    let (paint, patch_boundaries, boundaries) = graphics::paint(
        dom,
        base,
        images,
        &frag,
        &fixed,
        &top_layer,
        flow_bottom,
        viewport.width,
        viewport.height,
    );
    let paint_boundaries = graphical_paint_boundaries(dom, &boxes);
    let paint_cache = graphical_paint_cache(
        &frag,
        &fixed,
        &top_layer,
        flow_bottom,
        Viewport::new(viewport.width, viewport.height),
        &anchors,
        terminal::TerminalPaintModel::from_dom(dom, base),
    );
    Some(GraphicalLayout {
        paint,
        boxes,
        grid_tracks: flow.grid_tracks.into_inner(),
        patch_boundaries,
        paint_boundaries,
        boundaries,
        paint_cache,
    })
}

/// What the engine hands back to `http::parse_seeded`. Carousels, regions,
/// scroll clips, and boundaries arrive with their phases — consumers treat
/// the empty collections as "none on this page", and the live-page patch
/// machinery falls back to full relayout (always correct).
pub struct Output {
    pub rows: Vec<Row>,
    pub anchor_rows: HashMap<String, usize>,
    /// The pinned `position:fixed` layer (P4), in stack-level order.
    pub fixed: Vec<FixedItem>,
    /// Vertical inner-scroll viewports (P5b): each windows its own buffer over
    /// a reserved band of blank doc rows.
    pub regions: Vec<Region>,
    /// Horizontal scroll strips (P5c): items stay inline in the doc rows,
    /// column-shifted/clipped to the band at render.
    pub carousels: Vec<Carousel>,
    /// `(live actor node, clientHeight rows, scrollport width cells)` per scroll
    /// region, for the app's per-element scroll-geometry push (CSSOM View).
    pub scroll_clips: Vec<(usize, u16, u16)>,
    /// Incremental-layout boundaries (P7): block-filling IFC containers baked
    /// with `data-trust-node`, so a live mutation confined to one re-lays only
    /// its subtree (`lay_subtree_fragment`) and splices back. Empty ⇒ every live
    /// mutation takes the always-correct full-relayout path.
    pub boundaries: Vec<BoundaryBox>,
    /// Alpha-composited image overlap groups (P8): synthetic `x-trust-composite:`
    /// URL → ordered layers. A composite `Item` in `rows`/buffers carries the
    /// synthetic URL as its `image`; the app encodes it from these layers. Empty
    /// ⇒ no transparent image overlaps on the page.
    pub composites: HashMap<String, Vec<crate::layout2::CompositeLayer>>,
}

/// Lay an HTML document for a terminal content area. The fragment pass uses
/// `viewport.css_viewport()`; cell metrics are consumed only by the terminal
/// adapter after canonical layout.
pub fn lay_out_document(
    dom: &Dom,
    base: &Url,
    viewport: TerminalViewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    // `alpha` = URL→`has_alpha` from the app's decoded cache; the paint
    // compositor groups only image overlaps where an upper image is transparent.
    alpha: &HashMap<String, bool>,
) -> Output {
    let css = viewport.css_viewport();
    let pixel = lay_out_graphical(dom, base, css, forms, controls, images);
    adapt_terminal(&pixel, viewport, alpha)
}

/// Drop any candidate boundary whose row span overlaps a scroll region or
/// carousel band — those hold content in a side buffer/strip, not in `Doc.rows`,
/// so the inline `Doc.rows` splice can't apply to them (they take the region
/// path or the full fallback). Mirrors the old engine's `harvest_boundaries`.
fn filter_boundaries(
    candidates: Vec<BoundaryBox>,
    regions: &[Region],
    carousels: &[Carousel],
) -> Vec<BoundaryBox> {
    let overlaps = |a: std::ops::Range<usize>, s: usize, e: usize| a.start < e && s < a.end;
    candidates
        .into_iter()
        .filter(|b| {
            !regions.iter().any(|r| {
                overlaps(
                    b.row_range.clone(),
                    r.start_row,
                    r.start_row + r.height as usize,
                )
            }) && !carousels
                .iter()
                .any(|c| overlaps(b.row_range.clone(), c.start, c.end))
        })
        .collect()
}

/// Terminal compatibility wrapper for JS geometry. It maps the TUI's content
/// cell extent to a CSS-pixel [`Viewport`] and then delegates to
/// [`measure_boxes_css`]. Returned `PxRect`s come straight from fragments; no
/// painted rows are reconstructed.
///
/// One pass ALSO yields each grid container's used track sizes in px
/// `(columns, rows)` — the CSSOM resolved value `getComputedStyle` reports for
/// `grid-template-columns`/`-rows` (`js::sys_computed_style`). One pass backs
/// both.
#[allow(clippy::type_complexity)]
pub fn measure_boxes_terminal(
    dom: &Dom,
    base: &Url,
    viewport: (usize, usize),
    forms: &[Form],
    controls: &ControlMap,
    cell_px: (u16, u16),
    images: &ImageSizes,
) -> (
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, (Vec<f32>, Vec<f32>)>,
) {
    // Existing terminal page actors describe their viewport in cells. Convert
    // that frontend contract to a CSS-pixel viewport exactly once here; the
    // fragment geometry returned below never sees cells.
    let cell_w = f32::from(cell_px.0.max(1));
    let cell_h = f32::from(cell_px.1.max(1));
    let (boxes, tracks, _scrolling_areas) = measure_boxes_css(
        dom,
        base,
        Viewport::new(
            viewport.0.max(10) as f32 * cell_w,
            viewport.1 as f32 * cell_h,
        ),
        forms,
        controls,
        images,
    );
    (boxes, tracks)
}

/// CSSOM geometry pass for a native CSS-pixel viewport. Returned rectangles
/// are direct fragment coordinates, not reconstructed terminal cells.
#[allow(clippy::type_complexity)]
pub fn measure_boxes_css(
    dom: &Dom,
    base: &Url,
    viewport: Viewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
) -> (
    HashMap<NodeId, PxRect>,
    HashMap<NodeId, (Vec<f32>, Vec<f32>)>,
    HashMap<NodeId, PxRect>,
) {
    let vp = Vp {
        w: viewport.width,
        h: viewport.height,
    };
    let Some(root) = tree::build(dom, base, controls, forms, vp) else {
        return (HashMap::new(), HashMap::new(), HashMap::new());
    };
    let flow = Flow {
        dom,
        base,
        forms,
        images,
        vp,
        imemo: Default::default(),
        grid_tracks: Default::default(),
    };
    let (frag, _flow_bottom, _anchors, fixed, top_layer) = flow.layout(&root);
    let (boxes, scrolling_areas) = measure::boxes(dom, &frag, &fixed, &top_layer);
    (boxes, flow.grid_tracks.into_inner(), scrolling_areas)
}

/// Lay one INLINE relayout-boundary subtree (a block-filling IFC box, NOT a
/// scroll region) for the general incremental splice (incremental-layout contract
/// §14; the layout2 sibling of `layout::lay_out_subtree_fragment`). `boundary`
/// is the box in a re-parsed fragment DOM (`serialize_patch` output, inherited
/// context materialized). `content_width` is the boundary's BORDER-box width
/// (the band it fills, captured in `boundary::collect`). Its OWN margins are
/// suppressed for the lay — the boundary is spliced by its border-box
/// `origin_col`/`row_range`, so the fragment lays its border box at `(0,0)` and
/// its content wraps at the same width, byte-for-byte with the full render (the
/// §9 differential guard). `rows` are fragment-relative (cols from 0); the app
/// shifts by `origin_col` and splices. Non-empty `regions`/`carousels` mean the
/// box grew a sub-frame since capture ⇒ the app resyncs.
#[allow(clippy::too_many_arguments)]
pub fn lay_subtree_fragment(
    dom: &Dom,
    base: &Url,
    content_width: usize,
    viewport: TerminalViewport,
    controls: &ControlMap,
    images: &ImageSizes,
    boundary: NodeId,
    _sub_box: bool,
    quantization_phase: (f32, f32),
) -> crate::layout2::SubtreeFragment {
    let cols = content_width.max(1);
    let cell_w = viewport.cell_width;
    let cell_h = viewport.cell_height;
    let vp = Vp {
        w: cols as f32 * cell_w,
        h: viewport.rows as f32 * cell_h,
    };
    let empty = || crate::layout2::SubtreeFragment {
        rows: Vec::new(),
        height: 0,
        width: 0,
        carousels: Vec::new(),
        regions: Vec::new(),
        scroll_clips: Vec::new(),
    };
    let Some(mut root) = tree::build_at(dom, base, controls, &[], vp, boundary) else {
        return empty();
    };
    // Suppress the boundary's OWN margins: it is spliced at its border-box
    // origin, so the fragment must lay its border box at the top-left — margins
    // live outside the border box and belong to the splice position, not the
    // patched rows (matches `boundary::collect`'s border-box convention).
    root.style.margin = std::array::from_fn(|_| value::Len::px(0.0));
    let flow = Flow {
        dom,
        base,
        forms: &[],
        images,
        vp,
        imemo: Default::default(),
        grid_tracks: Default::default(),
    };
    let (mut frag, mut flow_bottom, mut anchors, mut fixed, mut top_layer) = flow.layout(&root);
    // A standalone patch still quantizes at the full document's original
    // sub-cell phase. Otherwise proportional line heights make the same CSS
    // fragment snap to a different row when re-laid at origin (0,0).
    let phase_x = quantization_phase.0.rem_euclid(cell_w);
    let phase_y = quantization_phase.1.rem_euclid(cell_h);
    Flow::offset_frag(&mut frag, phase_x, phase_y);
    for (_, y) in &mut anchors {
        *y += phase_y;
    }
    for fragment in &mut fixed {
        Flow::offset_frag(fragment, phase_x, phase_y);
    }
    for top in &mut top_layer {
        Flow::offset_frag(&mut top.fragment, phase_x, phase_y);
    }
    flow_bottom += phase_y;
    // v1 subtree-patch cut: an inline boundary re-lay does NOT alpha-composite
    // transparent image overlaps (empty alpha ⇒ no grouping); they reappear on
    // the next full render, matching the region-patch v1 cut.
    let terminal_model = terminal::TerminalPaintModel::from_dom(dom, base);
    let mut out = terminal::paint(
        &terminal_model,
        &mut frag,
        &fixed,
        &top_layer,
        flow_bottom,
        &anchors,
        (cols, viewport.rows),
        cell_w,
        cell_h,
        &HashMap::new(),
    );
    let origin_col = (phase_x / cell_w).round().max(0.0) as u16;
    let origin_row = (phase_y / cell_h).round().max(0.0) as usize;
    if origin_row > 0 {
        out.rows.drain(..origin_row.min(out.rows.len()));
    }
    if origin_col > 0 {
        for row in &mut out.rows {
            for item in &mut row.items {
                item.col = item.col.saturating_sub(origin_col);
                item.terminal_band = item.terminal_band.map(|(left, right)| {
                    (
                        left.saturating_sub(origin_col),
                        right.saturating_sub(origin_col),
                    )
                });
            }
            for hit in &mut row.hits {
                hit.col = hit.col.saturating_sub(origin_col);
            }
        }
    }
    let width = out
        .rows
        .iter()
        .flat_map(|r| &r.items)
        .map(|it| it.col + it.width)
        .max()
        .unwrap_or(0);
    crate::layout2::SubtreeFragment {
        height: out.rows.len(),
        width,
        rows: out.rows,
        carousels: out.carousels,
        regions: out.regions,
        scroll_clips: out.scroll_clips,
    }
}

/// Lay one scroll REGION's subtree into a fresh scrollable buffer for an
/// incremental region patch (incremental-layout contract; the layout2 sibling of
/// `layout::lay_out_region_fragment_cached`). `boundary` is the region node in a
/// re-parsed fragment DOM (`serialize_patch` output, inherited context
/// materialized); it is laid AS a fragment root at `content_width` (the existing
/// `Region.width` scrollport), then composited by the same `paint_region` the
/// full render uses — so the buffer is consistent with a full relayout. Returns
/// `(buffer rows, nested carousels, nested scroll-clip clientHeights)`; the app
/// swaps these into the live `Region`. (No row-cache memo in v1 — correctness
/// over the reuse optimization; the region is small.)
pub fn lay_region_fragment(
    dom: &Dom,
    base: &Url,
    content_width: usize,
    viewport: TerminalViewport,
    controls: &ControlMap,
    images: &ImageSizes,
    boundary: NodeId,
) -> terminal::RegionBuffer {
    let cols = content_width.max(1);
    let cell_w = viewport.cell_width;
    let cell_h = viewport.cell_height;
    let vp = Vp {
        w: cols as f32 * cell_w,
        h: viewport.rows as f32 * cell_h,
    };
    let Some(root) = tree::build_at(dom, base, controls, &[], vp, boundary) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let flow = Flow {
        dom,
        base,
        forms: &[],
        images,
        vp,
        imemo: Default::default(),
        grid_tracks: Default::default(),
    };
    let (mut frag, _flow_bottom, _anchors, _fixed, _top_layer) = flow.layout(&root);
    terminal::region_buffer(dom, base, &mut frag, cell_w, cell_h)
}

/// The page-level media affordance: a page that declares itself a video page
/// (Open Graph `og:video` — `page_declares_video`) but mounts NO
/// `<video>`/`<audio>` element still gets a "play in mpv" representation,
/// because yt-dlp can resolve the page itself (the Twitch watch-page fix —
/// the player never mounts without MSE). The page's og:image preview IS the
/// link once decoded; the text affordance stands in until then.
fn page_media_fallback(model: &terminal::TerminalPaintModel, cols: usize, rows: &mut Vec<Row>) {
    use crate::layout2::{Emphasis, Item, ItemKind, NO_NODE, display_width};
    let Some((target, poster)) = model.page_media() else {
        return;
    };
    let link = Some(target.clone());
    let item = match poster {
        Some((url, iw, ih)) => {
            let w = (*iw as usize).min(cols).max(1) as u16;
            let h = (*ih * f32::from(w) / *iw).max(1.0) as u16;
            Item {
                col: 0,
                width: w,
                height: h,
                text: String::new(),
                kind: ItemKind::Image,
                image: Some(url.clone()),
                emph: Emphasis::default(),
                node: NO_NODE,
                link,
                crop: false,
                pixelated: false,
                invisible: false,
                terminal_band: None,
            }
        }
        None => Item {
            col: 0,
            width: display_width("▶ Watch in mpv") as u16,
            height: 1,
            text: String::from("▶ Watch in mpv"),
            kind: ItemKind::Link,
            image: None,
            emph: Emphasis::default(),
            node: NO_NODE,
            link,
            crop: false,
            pixelated: false,
            invisible: false,
            terminal_band: None,
        },
    };
    let extra = usize::from(item.height.max(1)) - 1;
    rows.push(Row {
        items: vec![item],
        hits: Vec::new(),
    });
    for _ in 0..extra {
        rows.push(Row::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout2::{Item, ItemKind, display_width};

    fn lay(html: &str, cols: usize) -> Output {
        lay_images(html, cols, &HashMap::new())
    }

    fn lay_graphical(html: &str, width: f32, images: &ImageSizes) -> GraphicalLayout {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        lay_out_graphical(
            &dom,
            &base,
            Viewport::new(width, 600.0),
            &forms,
            &controls,
            images,
        )
    }

    fn graphical_text<'a>(layout: &'a GraphicalLayout, needle: &str) -> (f32, f32, &'a str) {
        layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, shaped, .. }
                    if shaped.text.contains(needle) =>
                {
                    Some((origin.x, origin.y, shaped.text.as_str()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("graphical glyph run containing {needle:?} not found"))
    }

    fn terminal_text(out: &Output) -> String {
        out.rows
            .iter()
            .flat_map(|row| row.items.iter())
            .map(|item| item.text.as_str())
            .collect()
    }

    #[test]
    fn fixed_keyframe_animation_is_retained_for_scene_sampling() {
        let layout = lay_graphical(
            "<style>
               @keyframes fall { from { top:-10% } to { top:100% } }
               @keyframes shake { 0%,100% { transform:translateX(0) }
                                  50% { transform:translateX(80px) } }
               #snow { position:fixed; top:-10%; z-index:9;
                       animation-name:fall,shake;
                       animation-duration:10s,3s;
                       animation-timing-function:linear,ease-in-out;
                       animation-iteration-count:infinite,infinite }
             </style><div id=snow>snow</div>",
            800.0,
            &ImageSizes::new(),
        );
        let scope = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::BeginCssAnimation(scope) => Some(scope),
                _ => None,
            })
            .expect("animated fixed element has a scene-sampled scope");
        assert_eq!(scope.animations.len(), 2);
        assert_eq!(scope.animations[0].position[0].value.y, 0.0);
        assert_eq!(scope.animations[0].position[1].value.y, 660.0);
        assert_eq!(scope.animations[1].transform[1].value.x, 80.0);
        assert!(layout.paint.has_css_animations());
    }

    #[test]
    fn hscroll_strip_escapes_an_overflow_hidden_ancestor_clip() {
        // A `<pre overflow-x:auto>` code line nested inside an `overflow:hidden`
        // ancestor (a locked model-card / app-shell column — HuggingFace) must
        // pull its FULL line into the carousel buffer, not the ancestor's
        // clipped slice. The ancestor's clip escape (`resolve_oof`) fired only
        // for vertical scroll containers, so a horizontal-only scroller had its
        // long lines truncated to the ancestor's right edge at composite time —
        // the tail was never in the buffer and the strip "cut off" mid-band no
        // matter how far you scrolled.
        let line = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz"; // 62
        let html = format!(
            "<div style=\"width:20ch;overflow:hidden\">\
             <pre style=\"overflow-x:auto;white-space:pre\"><code>{line}</code></pre></div>"
        );
        let out = lay(&html, 40);
        assert_eq!(out.carousels.len(), 1, "the pre is a horizontal strip");
        // The line item lives in the strip rows; its full width must survive
        // (not be truncated to the 20-cell ancestor clip).
        let widest = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| it.text.starts_with("ABC"))
            .map(|it| it.width)
            .max()
            .unwrap_or(0);
        assert_eq!(
            widest,
            line.chars().count() as u16,
            "the full code line is in the buffer, not clipped to the ancestor"
        );
        // And it scrolls to reveal the tail: at a large offset the window shows
        // the end of the line, filling the band.
        let mut cars = out.carousels.clone();
        let band = cars[0].right - cars[0].left;
        cars[0].offset = cars[0].width.saturating_sub(band); // max offset
        let long_row = out
            .rows
            .iter()
            .position(|r| r.items.iter().any(|it| it.text.starts_with("ABC")))
            .unwrap();
        let vc = crate::layout2::visual_columns(&out.rows[long_row], &cars, long_row);
        let (i, _, w, cut) = vc[0];
        let vis = crate::layout2::slice_display(&out.rows[long_row].items[i].text, cut, w as usize);
        assert!(
            vis.ends_with("xyz"),
            "scrolled to the end, the strip shows the line's tail: {vis:?}"
        );
    }

    fn lay_images(html: &str, cols: usize, images: &ImageSizes) -> Output {
        lay_full(html, cols, images, &HashMap::new())
    }

    fn lay_with_cells(html: &str, cols: usize, cell_width: f32, cell_height: f32) -> Output {
        let html = html.to_string();
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(move || {
                let dom = Dom::parse_document(&html);
                let base = Url::parse("http://e.com/").unwrap();
                lay_out_document(
                    &dom,
                    &base,
                    TerminalViewport::new(cols, 24, cell_width, cell_height),
                    &[],
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                )
            })
            .unwrap()
            .join()
            .unwrap()
    }

    /// Lay out with an explicit `has_alpha` map, to exercise the P8 overlap
    /// compositor (which groups only overlaps where an upper image is transparent).
    fn lay_full(
        html: &str,
        cols: usize,
        images: &ImageSizes,
        alpha: &HashMap<String, bool>,
    ) -> Output {
        // Run on a big stack like the app does (layout is on the 64MB `trust-js`
        // thread in production): a pathologically deep box tree (the 40-nested-
        // tables stress test) exceeds a 2MB cargo-test thread otherwise.
        let (html, images, alpha) = (html.to_string(), images.clone(), alpha.clone());
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(move || {
                let dom = Dom::parse_document(&html);
                let base = Url::parse("http://e.com/").unwrap();
                lay_out_document(
                    &dom,
                    &base,
                    TerminalViewport::new(cols, 24, 8.0, 16.0),
                    &[],
                    &HashMap::new(),
                    &images,
                    &alpha,
                )
            })
            .unwrap()
            .join()
            .unwrap()
    }

    /// A row's text with items placed at their columns (gaps = spaces).
    fn row_text(row: &Row) -> String {
        let mut out = String::new();
        let mut col = 0usize;
        for it in &row.items {
            while col < it.col as usize {
                out.push(' ');
                col += 1;
            }
            out.push_str(&it.text);
            col = it.col as usize + display_width(&it.text);
        }
        out
    }

    fn find<'a>(out: &'a Output, text: &str) -> (usize, &'a Item) {
        for (r, row) in out.rows.iter().enumerate() {
            for it in &row.items {
                if it.text.contains(text) {
                    return (r, it);
                }
            }
        }
        panic!("item containing {text:?} not found");
    }

    /// No painted item anywhere contains `text` (clipped away entirely).
    fn absent(out: &Output, text: &str) -> bool {
        !out.rows
            .iter()
            .any(|r| r.items.iter().any(|i| i.text.contains(text)))
    }

    #[test]
    fn float_run_max_content_sums_into_shrink_to_fit() {
        // §10.3.5/css-sizing-3: at max-content no soft breaks are taken, so a
        // run of block floats measures as the SUM of their margin boxes — a
        // shrink-to-fit abspos container (Steam's #global_header nav) must
        // come out wide enough to hold the whole float bar on one shelf, not
        // shrink to the widest single item and wrap every float below.
        let out = lay(
            r#"<body style="margin:0"><div style="position:absolute;left:200px;font:18px Arial,sans-serif">
                <a style="display:block;float:inline-start;padding:45px 7px 7px;position:relative">STORE</a>
                <a style="display:block;float:inline-start;padding:45px 7px 7px;position:relative">COMMUNITY</a>
                <a style="display:block;float:inline-start;padding:45px 7px 7px;position:relative">ABOUT</a>
                <a style="display:block;float:inline-start;padding:45px 7px 7px;position:relative">SUPPORT</a>
               </div></body>"#,
            120,
        );
        let (rs, s) = find(&out, "STORE");
        let (rc, _) = find(&out, "COMMUNITY");
        let (ra, _) = find(&out, "ABOUT");
        let (rp, _) = find(&out, "SUPPORT");
        assert_eq!((rs, rc, ra, rp), (rs, rs, rs, rs), "floats pack one shelf");
        assert_eq!(s.col, 26, "left:200px = col 25, + the float's 7px padding");
        // A clear starts a new shelf: the cleared float drops below and the
        // shelves compete by max.
        let out = lay(
            r#"<body style="margin:0"><div style="position:absolute;left:0">
                <a style="display:block;float:left">AAAA</a>
                <a style="display:block;float:left;clear:left">BB</a>
               </div></body>"#,
            240,
        );
        let (ra, _) = find(&out, "AAAA");
        let (rb, _) = find(&out, "BB");
        assert!(rb > ra, "the cleared float takes its own shelf");
    }

    #[test]
    fn nested_shrink_to_fit_float_keeps_spaced_label_on_one_line() {
        // CSS 2.2 §10.3.5: an auto-width float uses the shrink-to-fit
        // formula. Under its max-content constraint, the two inner floats and
        // every soft-wrappable word in them must be formatted without taking
        // soft line breaks. Repeated f32 addition/subtraction at the nested
        // intrinsic boundaries must not manufacture an infinitesimal deficit.
        let html = r#"<body id="body" style="margin:0;font-size:15px;font-family:-apple-system,system-ui,blinkmacsystemfont,'Segoe UI',roboto,oxygen,ubuntu,'Helvetica Neue',arial,sans-serif;font-weight:300;line-height:1.5">
                <nav style="height:40px">
                  <ul id="float-set" data-trust-clearfix style="float:right;margin:0;padding:0">
                    <li id="label-item" style="float:left;margin:0">
                      <a id="label" style="display:inline-block;padding:8px 12px;box-sizing:border-box">Sign In</a>
                    </li>
                    <li id="fixed-item" style="float:left;margin:0">
                      <span style="display:inline-block;min-width:42px;padding:8px 12px;box-sizing:border-box"></span>
                    </li>
                  </ul>
                </nav>
               </body>"#;
        let out = lay(html, 120);
        let (sign_row, _) = find(&out, "Sign");
        let (in_row, _) = find(&out, "In");
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        let graphical = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(960.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let label = graphical.boxes[&dom.get_by_id("label").unwrap()];
        assert_eq!(in_row, sign_row, "the spaced label must not soft-wrap");
        assert!(
            (label.height - 38.5).abs() < 0.01,
            "one 22.5px line plus 16px padding, got {label:?}"
        );
    }

    #[test]
    fn real_subpixel_inline_deficit_still_soft_wraps() {
        // CSS Text 3 §5: numerical equality must not become a blanket overflow
        // allowance. This definite content box is genuinely 0.15px narrower
        // than the shaped label, so the space remains a soft-wrap opportunity.
        let html = r#"<body style="margin:0;font-size:15px;font-family:-apple-system,system-ui,blinkmacsystemfont,'Segoe UI',roboto,oxygen,ubuntu,'Helvetica Neue',arial,sans-serif;font-weight:300;line-height:1.5">
                 <div id="label" style="width:69px;padding:8px 12px;box-sizing:border-box">Sign In</div>
               </body>"#;
        let dom = Dom::parse_document(html);
        let graphical = lay_out_graphical(
            &dom,
            &Url::parse("http://e.com/").unwrap(),
            Viewport::new(960.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let label = graphical.boxes[&dom.get_by_id("label").unwrap()];
        assert!(
            (label.height - 61.0).abs() < 0.01,
            "two 22.5px lines plus 16px padding, got {label:?}"
        );
    }

    #[test]
    fn inline_atom_margins_take_line_space() {
        // §9.4.2/§10.8: an inline-level replaced box's margin edges occupy
        // real inline space — Steam's hero `<video margin-left:50%>` label
        // sits at mid-line, not col 0 (its margin was silently dropped).
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <div style="position:absolute;width:100%;overflow:hidden;z-index:1">
                  <div style="display:block">
                    <video preload="none" style="max-height:450px;margin-left:50%">
                      <source src="v.webm" type="video/webm">
                    </video>
                  </div>
                </div>
               </div></body>"#,
            240,
        );
        let (_, v) = find(&out, "▶ Video");
        assert_eq!(v.col, 120, "margin-left:50% of 1920px = 960px = col 120");
    }

    #[test]
    fn transparent_gradient_background_does_not_erase_content() {
        // A gradient with a transparent stop is a scrim/edge fade: a browser
        // shows the content beneath it, so it must not stamp an opaque fill
        // over earlier-painted cells. An all-opaque gradient (and any url()
        // layer) keeps the cover semantics.
        let scrim = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <span>UNDERNEATH</span>
                <div style="position:absolute;left:0;top:0;width:400px;height:32px;z-index:5;background-image:linear-gradient(to right, rgba(0,0,0,0), rgba(0,0,0,1))"></div>
               </div></body>"#,
            80,
        );
        let (_, u) = find(&scrim, "UNDERNEATH");
        assert_eq!(u.col, 0, "the scrim leaves the text beneath");
        let opaque = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <span>UNDERNEATH</span>
                <div style="position:absolute;left:0;top:0;width:400px;height:32px;z-index:5;background-image:linear-gradient(to right, rgb(0,0,0), rgb(9,9,9))"></div>
               </div></body>"#,
            80,
        );
        assert!(
            absent(&opaque, "UNDERNEATH"),
            "an all-opaque gradient still covers"
        );
        // A predominantly-opaque backdrop with one glow stop (mean alpha
        // ≈ 0.71 — Steam's store-nav radial) binarizes to a cover: the
        // browser's effectively-solid bar hides what's behind it.
        let backdrop = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <span>UNDERNEATH</span>
                <div style="position:absolute;left:0;top:0;width:400px;height:32px;z-index:5;background-image:radial-gradient(107% 57% at 50% 100%, rgba(24,37,53,0.2) 0%, rgba(24,37,53,0.52) 5%, rgba(24,37,53,0.85) 20%, #182535 60%, #192330 100%)"></div>
               </div></body>"#,
            80,
        );
        assert!(
            absent(&backdrop, "UNDERNEATH"),
            "a mostly-opaque backdrop still covers"
        );
    }

    #[test]
    fn fractional_atom_box_reservation_ceils() {
        // A shrink-to-fit abspos sized from an inline-block whose max-content
        // is a fractional cell count (13 chars + 43px padding = 147px =
        // 18.375 cells at the 8px test cell): ROUNDING the atom-box probe
        // down under-reports the container's max-content, and the real pass
        // re-laying at that width re-wraps the box's own text — Steam's
        // header "Install Steam" button split into two rows at a 10px cell.
        // The reservation must CEIL: a box needing 18.375 cells fits in 19.
        let out = lay(
            r#"<body style="margin:0"><div style="position:absolute;left:0;top:0">
                <span style="display:inline-block;padding-left:35px;padding-right:8px">Install Steam</span>
               </div></body>"#,
            240,
        );
        let (r, _) = find(&out, "Install Steam");
        assert_eq!(r, 0, "the label stays on one line inside its box");
    }

    // ---- the P0 gate: plain articles render with a browser's structure ----
    // Test cells are the nominal 8×16 px, so 1em = 16px = 1 row = 2 cols and
    // the UA sheet's px values quantize predictably: body margin 8px = 1 col,
    // list gutter 40px = 5 cols.

    #[test]
    fn article_structure_matches_browser() {
        let out = lay(
            "<body><h1>Title</h1><p>One two three.</p><p>Second para.</p></body>",
            80,
        );
        // body's 8px top margin collapses with h1's 0.67em·32px = 21.44px
        // margin → h1's line at y=21.44px → row 1; its left content edge is
        // body's 8px margin → col 1.
        let (r1, h1) = find(&out, "Title");
        assert_eq!((r1, h1.col), (1, 1));
        assert_eq!(h1.kind, ItemKind::Heading(1));
        // h1 bottom 37.44 + collapsed max(21.44, 16) → p at 58.88px → row 4.
        let (r2, p1) = find(&out, "One two three.");
        assert!(
            r2 > r1,
            "paragraph follows the heading's variable-height line box"
        );
        assert_eq!(p1.col, 1);
        assert_eq!(p1.kind, ItemKind::Text);
        // p↕p: exactly one collapsed 1em margin → one blank row between.
        let (r3, _) = find(&out, "Second para.");
        assert!(r3 > r2, "the second paragraph follows the first");
    }

    #[test]
    fn paragraph_wraps_at_content_width() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">aaa bbb ccc ddd</p></body>"#,
            10,
        );
        assert_eq!(row_text(&out.rows[0]), "aaa bbb");
        assert_eq!(row_text(&out.rows[1]), "ccc ddd");
    }

    #[test]
    fn quantized_table_cell_text_does_not_cross_sibling_cell() {
        // The proportional line fits inside the 80% cell, but its Unicode
        // display-cell width is a couple of cells wider after terminal
        // adaptation. The terminal presentation must keep that quantization
        // overhang inside the cell instead of painting over the 20% sibling.
        // This is the same nested-table boundary used by CSS 2.1 §17.5.2;
        // the cell's default overflow is still visible for genuine CSS
        // overflow, covered by `overlong_word_overflows_and_clips_at_viewport`.
        let story_text = "i".repeat(100);
        let html = format!(
            r#"<body style="margin:0"><table style="width:980px;table-layout:auto"><tr>
                <td style="width:80%;padding:6px;vertical-align:top"><p style="margin:0;font-size:15px">{story_text}</p></td>
                <td style="width:20%;padding:6px;vertical-align:top"><div style="margin-top:16px">SIDE PANEL</div></td>
            </tr></table></body>"#
        );
        let out = lay(&html, 120);
        let (story_row, story) = find(&out, "iiii");
        let (side_row, side) = find(&out, "SIDE PANEL");
        assert!(
            story_row < side_row,
            "the boundary marker is below the story"
        );
        assert!(
            story.col as usize + story.width as usize <= side.col as usize,
            "main-cell item {:?} crosses into sibling at column {}",
            story,
            side.col
        );
        let story_cells: usize = out
            .rows
            .iter()
            .flat_map(|row| &row.items)
            .map(|item| item.text.chars().filter(|&ch| ch == 'i').count())
            .sum();
        assert_eq!(
            story_cells,
            story_text.len(),
            "terminal reflow must preserve text"
        );
    }

    #[test]
    fn terminal_reflow_continues_across_soft_css_line_boxes() {
        // CSS Text 3 §5 distinguishes soft-wrap opportunities from forced
        // breaks. A proportional CSS line box can end at the cell edge while
        // its terminal-cell equivalent still has room on the continuation
        // row; the next soft-wrapped line must use that room as part of the
        // same paragraph.
        let html = r#"<body style="margin:0"><table style="width:980px;table-layout:auto"><tr>
            <td style="width:80%;padding:6px;vertical-align:top"><p style="margin:0;font-size:15px">
                As I mentioned above, there are several editions of Helwan Linux, though each edition appears to be for a separate version (or generation) of the distribution. For version 5.0 I found just one edition, Developer, which ships with the Cinnamon desktop.
            </p></td>
            <td style="width:20%;padding:6px;vertical-align:top">SIDE PANEL</td>
        </tr></table></body>"#;
        let out = lay(html, 120);
        let be_row = out
            .rows
            .iter()
            .map(row_text)
            .find(|text| text.split_whitespace().any(|word| word == "be"))
            .expect("the paragraph contains be");
        assert!(
            be_row.contains("be for a separate"),
            "soft-wrapped continuation restarted a terminal row: {be_row:?}"
        );
        let edition_row = out
            .rows
            .iter()
            .map(row_text)
            .find(|text| text.contains("edition,"))
            .expect("the paragraph contains edition,");
        assert!(
            edition_row.contains("edition, Developer"),
            "the next soft-wrapped line was not packed into its available row: {edition_row:?}"
        );
    }

    #[test]
    fn positioned_text_reflows_without_reserving_document_rows() {
        // CSS Text 3 §3 permits normal text to wrap at soft opportunities after
        // the containing block has been sized. The graphical layout therefore
        // keeps this proportional line in one positioned box, but its terminal
        // cell equivalent needs a private continuation row. That row must not
        // erase the next canonical line or move unrelated in-flow content.
        let text = "systems. By using this legacy tool for fileless, low-visibility attacks, threat actors highlight the growing trend of weaponizing overlooked, living-off-the-land utilities.";
        let html = format!(
            r#"<body style="margin:0"><div style="position:absolute;left:0;top:0;width:624px;font-size:15px"><p style="margin:0">{text}</p></div><p style="margin:0;position:absolute;top:160px">after</p></body>"#
        );
        let out = lay(&html, 80);
        let (after_row, after) = find(&out, "after");
        let rendered = out
            .rows
            .iter()
            .take(after_row)
            .map(row_text)
            .collect::<Vec<_>>()
            .join(" ");
        let compact = |value: &str| value.split_whitespace().collect::<String>();
        assert_eq!(
            compact(&rendered),
            compact(text),
            "positioned text lost content during terminal-cell reflow: {rendered:?}"
        );
        assert!(
            after.col < 80,
            "the following positioned content remains visible"
        );
    }

    #[test]
    fn sidebar_viewport_clipping_does_not_add_main_flow_rows() {
        // The sidebar's containing block extends a little beyond the terminal
        // viewport. Its right-edge clipping must not be mistaken for a second
        // line in the shared row allocator; CSS 2.1 §10.8 line boxes remain
        // vertically adjacent in the main cell.
        let out = lay(
            r#"<body style="margin:0"><table style="width:980px;table-layout:auto"><tr>
                <td style="width:80%;padding:6px;vertical-align:top">
                    <p style="margin:0;line-height:16px">main line one</p><p style="margin:0;line-height:16px">main line two</p>
                </td>
                <td style="width:20%;padding:6px;vertical-align:top">
                    <p style="margin:0">PrivacyGuard Laptops</p><p style="margin:0">another sidebar line</p>
                </td>
            </tr></table></body>"#,
            120,
        );
        let (first_row, _) = find(&out, "main line one");
        let (second_row, _) = find(&out, "main line two");
        assert_eq!(second_row, first_row + 1, "sidebar clipping inserted a row");
    }

    #[test]
    fn overflow_wrap_breaks_an_overflowing_word() {
        // Under `normal` an unbreakable word overflows the line whole (the
        // output clips it at the viewport edge; nothing lands on row 1)…
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">abcdefghijklmno</p></body>"#,
            10,
        );
        let visible = row_text(&out.rows[0]);
        assert!("abcdefghijklmno".starts_with(&visible));
        assert!(absent(&out, "klmno"), "no emergency break under normal");
        // …`overflow-wrap: break-word` takes the emergency break instead
        // (CSS Text §5.5), and wrapped words still break normally.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;overflow-wrap:break-word">abcdefghijklmno xy</p></body>"#,
            10,
        );
        assert!(out.rows.iter().filter(|row| !row.items.is_empty()).count() >= 2);
        assert_eq!(terminal_text(&out).replace(' ', ""), "abcdefghijklmnoxy");
        // The legacy `word-wrap` alias parses identically.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;word-wrap:break-word">abcdefghijklmno</p></body>"#,
            10,
        );
        assert_eq!(terminal_text(&out).replace(' ', ""), "abcdefghijklmno");
    }

    #[test]
    fn overflow_wrap_prefers_the_existing_word_boundary_before_emergency_breaks() {
        // CSS Text 3 §5 and §5.4: `overflow-wrap:break-word` introduces
        // an arbitrary break only for an otherwise-unbreakable sequence. The
        // space before a word is already a normal soft-wrap opportunity, so a
        // word which fits on an empty line moves there intact instead of being
        // split merely to fill the preceding line's remaining pixels.
        let style = crate::text::TextStyle::default();
        let width = crate::text::shape("encyclopedia", &style).advance + 0.1;
        let layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0;overflow-wrap:break-word">alpha encyclopedia</p></body>"#,
            width,
            &HashMap::new(),
        );
        assert!(layout.paint.lines.len() >= 2);
        let first = &layout.paint.lines[0].rect;
        let first_line: String = layout
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, shaped, .. }
                    if origin.y >= first.y && origin.y < first.y + first.height =>
                {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(first_line.trim_end(), "alpha");
    }

    #[test]
    fn word_break_break_all_fills_lines() {
        // `word-break: break-all` breaks between any two characters, so the
        // greedy fill uses the current line before wrapping (CSS Text §5.2):
        // "bbbb" splits at the line edge instead of wrapping whole.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;word-break:break-all">aaaaaaaa bbbb cccccc</p></body>"#,
            10,
        );
        assert!(out.rows.iter().filter(|row| !row.items.is_empty()).count() >= 2);
        assert_eq!(terminal_text(&out).replace(' ', ""), "aaaaaaaabbbbcccccc");

        // Prove the greedy decision in canonical CSS pixels. Give the line
        // exactly enough measured room for one `b` from the next word; a
        // character-count model cannot satisfy this proportional boundary.
        let style = crate::text::TextStyle::default();
        let width = crate::text::shape("aaaaaaaa b", &style).advance + 0.1;
        let graphics = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0;word-break:break-all">aaaaaaaa bbbb</p></body>"#,
            width,
            &HashMap::new(),
        );
        let first_y = graphics.paint.lines[0].rect.y;
        let first_line: String = graphics
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, shaped, .. }
                    if (origin.y - first_y).abs() < graphics.paint.lines[0].rect.height =>
                {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(first_line.replace(' ', "").len() > 8, "{first_line:?}");
    }

    #[test]
    fn word_break_keep_all_suppresses_cjk_wraps() {
        // CJK ideographs are normally soft-wrap opportunities per glyph…
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">漢字漢字漢字</p></body>"#,
            10,
        );
        assert_eq!(row_text(&out.rows[0]), "漢字漢字漢");
        assert_eq!(row_text(&out.rows[1]), "字");
        // …`keep-all` keeps the run unbreakable: it overflows like a long
        // word (clipped at the viewport; nothing lands on row 1).
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;word-break:keep-all">漢字漢字漢字</p></body>"#,
            10,
        );
        assert!(
            out.rows.len() < 2 || row_text(&out.rows[1]).is_empty(),
            "keep-all leaves nothing on the second row"
        );
    }

    #[test]
    fn overflow_wrap_min_content_distinction() {
        // `break-word` adds NO min-content break opportunities: a
        // width:min-content block still sizes to the widest word…
        let out = lay(
            r#"<body style="margin:0"><div style="width:min-content;overflow-wrap:break-word">abcdef</div></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "abcdef");
        // …while `anywhere` shrinks min-content to a single glyph
        // (CSS Text §5.5 — the one difference between the two values).
        let out = lay(
            r#"<body style="margin:0"><div style="width:min-content;overflow-wrap:anywhere">abcdef</div></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a");
        assert_eq!(row_text(&out.rows[1]), "b");
    }

    #[test]
    fn block_width_intrinsic_keywords() {
        // css-sizing-3 §5: the keywords on a plain BLOCK box (previously
        // they fell to auto = fill). min-content = the widest word…
        let out = lay(
            r#"<body style="margin:0"><div style="width:min-content">aa bbbb</div></body>"#,
            20,
        );
        assert_eq!(row_text(&out.rows[0]), "aa");
        assert_eq!(row_text(&out.rows[1]), "bbbb");
        // …max-content = the unwrapped single line…
        let out = lay(
            r#"<body style="margin:0"><div style="width:max-content">aa bbbb</div></body>"#,
            20,
        );
        assert_eq!(row_text(&out.rows[0]), "aa bbbb");
        // …fit-content: at a wide viewport it hugs the content (a nested
        // full-width child would fill 20 cells under auto, 7 under
        // fit-content — probe via a right-aligned second line).
        let out = lay(
            r#"<body style="margin:0"><div style="width:fit-content;text-align:right">aa bbbb<br>c</div></body>"#,
            20,
        );
        assert_eq!(row_text(&out.rows[0]), "aa bbbb");
        let (_, c) = find(&out, "c");
        assert!(
            c.col > 0,
            "right-aligned inside the proportional fit-content box"
        );
    }

    #[test]
    fn generated_content_renders_as_first_and_last_children() {
        // CSS 2.1 §12.1: `::before`/`::after` boxes render (this regressed to
        // nothing when the old flow engine was deleted — the serializer bakes
        // the text but layout2 never consumed it). Both sources work: the
        // live cascade (this test) and the baked attribute (the second half).
        let out = lay(
            "<head><style>#a::before{content:\"B-\"} #a::after{content:\"-A\"}</style></head>\
             <body style=\"margin:0\"><p id=a style=\"margin:0\">mid</p></body>",
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "B-mid-A");
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0" data-trust-before="B-" data-trust-after="-A">mid</p></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "B-mid-A");
    }

    #[test]
    fn empty_generated_block_reserves_percentage_padding_height() {
        // CSS Pseudo 4 §4.1 + CSS Content 3 §2: content:"" generates an
        // empty, fully styleable ::before box (only `none` inhibits it). CSS
        // Box 4 §4 then resolves vertical percentage padding against the
        // containing block's width. This is the standard ratio-box idiom used
        // by lazy-loaded media thumbnails: 320 * 56.25% = 180 CSS px.
        let (dom, boxes) = measure(
            r#"<head><style>
               #thumb::before{content:"";display:block;width:100%;padding-top:56.25%}
               </style></head><body style="margin:0">
               <div id=thumb style="width:320px"></div>
               <p id=next style="margin:0">next</p></body>"#,
            80,
            24,
        );
        let thumb = rect(&dom, &boxes, "thumb");
        let next = rect(&dom, &boxes, "next");
        assert!((thumb.width - 320.0).abs() < 0.01, "thumb={thumb:?}");
        assert!((thumb.height - 180.0).abs() < 0.01, "thumb={thumb:?}");
        assert!(next.top >= 180.0, "next={next:?}, thumb={thumb:?}");
    }

    #[test]
    fn tab_size_sets_preserved_tab_stops() {
        let style = crate::text::TextStyle::default();
        let space = crate::text::shape(" ", &style).advance;
        // The default: tabs advance to eight measured space advances in
        // preserved modes. This is canonical CSS geometry, not eight cells.
        let layout = lay_graphical(
            "<body style=\"margin:0\"><pre style=\"margin:0\">a\tb</pre></body>",
            200.0,
            &HashMap::new(),
        );
        let (bx, _, _) = graphical_text(&layout, "b");
        assert!((bx - 8.0 * space).abs() < 0.25, "b at {bx}, space={space}");
        // `tab-size: 4` moves the stops (CSS Text §3).
        let layout = lay_graphical(
            "<body style=\"margin:0\"><pre style=\"margin:0;tab-size:4\">a\tb\tc</pre></body>",
            200.0,
            &HashMap::new(),
        );
        let (bx, _, _) = graphical_text(&layout, "b");
        let (cx, _, _) = graphical_text(&layout, "c");
        assert!((bx - 4.0 * space).abs() < 0.25, "b at {bx}, space={space}");
        assert!((cx - 8.0 * space).abs() < 0.25, "c at {cx}, space={space}");
        // A zero tab renders no advance.
        let layout = lay_graphical(
            "<body style=\"margin:0\"><pre style=\"margin:0;tab-size:0\">a\tb</pre></body>",
            200.0,
            &HashMap::new(),
        );
        let (x, _, text) = graphical_text(&layout, "b");
        assert_eq!(text, "ab", "zero tab leaves the adjacent runs abutting");
        assert!(x.abs() < 0.01);
        let run = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { shaped, .. } if shaped.text == "ab" => {
                    Some(shaped)
                }
                _ => None,
            })
            .unwrap();
        assert!((run.advance - crate::text::shape("ab", &style).advance).abs() < 0.01);
    }

    #[test]
    fn soft_wrap_before_punctuation_word_when_its_first_break_does_not_fit() {
        // CSS Text 3 §5: the collapsible space is a valid soft-wrap
        // opportunity. If the next word also contains a later opportunity
        // (the hyphen), that later segment must not be forced into the
        // remaining sliver and overflow an otherwise wrap-capable line.
        let layout = lay_graphical(
            r#"<body style="margin:0"><div style="width:100px;font:15px Arial">prefixword Ubuntu-based</div></body>"#,
            300.0,
            &HashMap::new(),
        );
        for primitive in &layout.paint.primitives {
            if let crate::render::Primitive::GlyphRun { origin, shaped, .. } = primitive {
                assert!(
                    origin.x + shaped.advance <= 100.01,
                    "a breakable line must fit its line box: origin={origin:?}, shaped={shaped:?}"
                );
            }
        }
    }

    #[test]
    fn sibling_margins_collapse_to_max() {
        let out = lay(
            r#"<body style="margin:0"><div style="margin-bottom:32px">a</div><div style="margin-top:16px">b</div></body>"#,
            20,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        assert_eq!(ra, 0);
        assert_eq!(rb, 3, "gap = max(32px, 16px) = 2 rows");
    }

    #[test]
    fn parent_child_top_margins_collapse_through() {
        let out = lay(
            r#"<body style="margin:0"><div style="margin-top:32px"><p style="margin-top:16px;margin-bottom:0">x</p></div></body>"#,
            20,
        );
        let (r, _) = find(&out, "x");
        assert_eq!(r, 2, "joint margin = max(32, 16) = 32px = 2 rows");
    }

    #[test]
    fn empty_block_self_collapses() {
        let out = lay(
            r#"<body style="margin:0"><div>a</div><div style="margin-top:16px;margin-bottom:16px"></div><div>b</div></body>"#,
            20,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        assert_eq!(
            rb - ra,
            2,
            "empty div's margins collapse into one 1-row gap"
        );
    }

    #[test]
    fn width_auto_margins_center_and_padding_indents() {
        // 80 cols = 640px CB; content width 50% = 320px, border box 336px
        // with the 16px padding; §10.3.3: ml = (640−336)/2 = 152px; content
        // at 152+16 = 168px = col 21.
        let out = lay(
            r#"<body style="margin:0"><div style="width:50%;margin:0 auto;padding-left:16px">x</div></body>"#,
            80,
        );
        let (_, it) = find(&out, "x");
        assert_eq!(it.col, 21);
    }

    #[test]
    fn box_sizing_border_box_shrinks_content() {
        // width:320px border-box with 16px padding each side → content 288px
        // = 36 cells; text wraps there, not at 40.
        let out = lay(
            r#"<body style="margin:0"><div style="box-sizing:border-box;width:320px;padding:0 16px;margin:0">
               <p style="margin:0">aaaa</p></div></body>"#,
            80,
        );
        let (_, it) = find(&out, "aaaa");
        assert_eq!(it.col, 2, "16px padding-left = 2 cols");
    }

    #[test]
    fn nested_lists_indent_and_change_markers() {
        let out = lay(
            r#"<body style="margin:0"><ul style="margin:0"><li>one</li><li>two<ul><li>sub</li></ul></li></ul></body>"#,
            40,
        );
        let (r1, one) = find(&out, "one");
        assert_eq!((r1, one.col), (0, 5), "40px list gutter = 5 cols");
        let marker = &out.rows[0].items[0];
        assert_eq!(marker.text, "• ");
        assert_eq!(marker.col, 3, "marker right-aligned against content");
        let (r3, sub) = find(&out, "sub");
        assert_eq!(sub.col, 10, "nested list adds another 40px gutter");
        let sub_marker = &out.rows[r3].items[0];
        assert_eq!(sub_marker.text, "◦ ", "depth-2 UA marker is circle");
    }

    #[test]
    fn ordered_list_counts_with_start_and_value() {
        let out = lay(
            r#"<body style="margin:0"><ol start="3" style="margin:0"><li>a</li><li value="10">b</li><li>c</li></ol></body>"#,
            40,
        );
        let (ra, _) = find(&out, "a");
        assert_eq!(out.rows[ra].items[0].text, "3. ");
        let (rb, _) = find(&out, "b");
        assert_eq!(out.rows[rb].items[0].text, "10. ");
        let (rc, _) = find(&out, "c");
        assert_eq!(out.rows[rc].items[0].text, "11. ");
    }

    #[test]
    fn blockquote_indents_both_sides() {
        let out = lay(
            r#"<body style="margin:0"><blockquote style="margin-top:0">quoted text</blockquote></body>"#,
            80,
        );
        let (_, it) = find(&out, "quoted text");
        assert_eq!(it.col, 5, "40px UA margin-left = 5 cols");
        assert_eq!(it.kind, ItemKind::Quote);
    }

    #[test]
    fn pre_preserves_spaces_newlines_and_tabs() {
        let out = lay(
            "<body style=\"margin:0\"><pre style=\"margin:0\">a  b\n\tc</pre></body>",
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a  b");
        assert!(
            row_text(&out.rows[1]).ends_with('c') && find(&out, "c").1.col > 0,
            "the terminal adapter quantizes the measured eight-space tab stop"
        );
        let (_, it) = find(&out, "a  b");
        assert_eq!(it.kind, ItemKind::Pre);
    }

    #[test]
    fn br_forces_breaks_and_blank_lines() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">a<br>b<br><br>c</p></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a");
        assert_eq!(row_text(&out.rows[1]), "b");
        assert!(out.rows[2].items.is_empty(), "<br><br> yields a blank line");
        assert_eq!(row_text(&out.rows[3]), "c");
    }

    #[test]
    fn links_and_emphasis_thread_into_items() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">go <a href="/x">here <b>now</b></a></p></body>"#,
            40,
        );
        let (_, here) = find(&out, "here");
        assert_eq!(here.kind, ItemKind::Link);
        assert!(matches!(&here.link, Some(crate::doc::Link::Http(u)) if u.path() == "/x"));
        let (_, now) = find(&out, "now");
        assert!(now.emph.bold);
        assert_eq!(now.kind, ItemKind::Link);
        assert!(now.link.is_some(), "emphasis inside a link keeps the link");
    }

    #[test]
    fn collapsing_spans_inline_boundaries() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">a <b> b</b></p></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a b", "one collapsed space");
    }

    #[test]
    fn decoded_image_reserves_box_and_text_sits_on_baseline() {
        let mut images = HashMap::new();
        images.insert("http://e.com/i.png".to_string(), (80u32, 48u32));
        let out = lay_images(
            r#"<body style="margin:0"><p style="margin:0"><img src="i.png" alt="pic">after</p></body>"#,
            40,
            &images,
        );
        let img = out.rows[0]
            .items
            .iter()
            .find(|it| it.kind == ItemKind::Image)
            .expect("image item");
        assert_eq!((img.col, img.width, img.height), (0, 10, 3));
        assert_eq!(img.image.as_deref(), Some("http://e.com/i.png"));
        // Baseline alignment: the replaced box's baseline is its bottom edge,
        // so the adjacent text sits on the image's LAST row.
        let (r, after) = find(&out, "after");
        assert_eq!((r, after.col), (2, 10));
    }

    #[test]
    fn graphical_inline_image_and_text_share_a_css_baseline() {
        let images = HashMap::from([("http://e.com/i.png".to_string(), (80u32, 48u32))]);
        let layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0"><img src="i.png">after</p></body>"#,
            320.0,
            &images,
        );
        let line = layout.paint.lines.first().expect("line box");
        let image = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("image paint operation");
        let (origin, shaped) = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, shaped, .. }
                    if shaped.text.contains("after") =>
                {
                    Some((*origin, shaped))
                }
                _ => None,
            })
            .expect("adjacent text");
        assert_eq!((image.width, image.height), (80.0, 48.0));
        assert!((image.y + image.height - line.baseline).abs() < 0.01);
        assert!((origin.y + shaped.baseline - line.baseline).abs() < 0.01);
        assert!(origin.x >= image.x + image.width - 0.01);
    }

    #[test]
    fn list_style_image_creates_an_image_marker() {
        let svg = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='4' height='4'%3E%3Crect width='4' height='4'/%3E%3C/svg%3E";
        let html = format!(
            r#"<body style="margin:0"><ul style="margin:0;padding:0;list-style-image:url('{svg}')"><li>heart</li></ul></body>"#
        );
        let layout = lay_graphical(&html, 320.0, &HashMap::new());
        let image = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("list-style-image should paint an image marker");
        assert!(image.width > 0.0 && image.height > 0.0);
        assert!(
            layout
                .paint
                .image_requests
                .iter()
                .any(|request| request.source == svg),
            "the marker image must enter the normal subresource request path"
        );
        assert!(
            layout.paint.primitives.iter().any(|primitive| matches!(
                primitive,
                crate::render::Primitive::GlyphRun { shaped, .. }
                    if shaped.text.contains("heart")
            )),
            "list content remains alongside the anonymous marker image"
        );
    }

    #[test]
    fn outside_image_marker_keeps_its_absolute_resource_and_the_first_text_cell() {
        // rubymaelstrom.com: the 15px marker box and body/list percentages hit
        // an adverse cell phase at 100 columns. The marker's trailing U+0020
        // used to paint after the principal text in that shared cell, turning
        // "20 February" into "0 February"; its relative URL also missed the
        // decoded cache even though the resource had been fetched.
        let out = lay(
            r#"<style>
                body { box-sizing:border-box; font-size:15px; width:65vw;
                       padding:20px; border:3px ridge;
                       margin-left:17.5vw; margin-right:17.5vw }
                ul { list-style-image:url('/images/HeartDot.png') }
               </style>
               <body><ul><li><a href="/post">20 February, 2026</a></li></ul></body>"#,
            100,
        );
        let (row, text) = find(&out, "20 February, 2026");
        assert_eq!(text.text, "20 February, 2026");
        let marker = out.rows[row]
            .items
            .iter()
            .find(|item| item.image.is_some())
            .expect("outside image marker");
        assert_eq!(
            marker.image.as_deref(),
            Some("http://e.com/images/HeartDot.png")
        );
        assert!(
            usize::from(marker.col + marker.width) <= usize::from(text.col),
            "terminal marker must end before the principal text: marker={marker:?} text={text:?}"
        );
    }

    #[test]
    fn responsive_density_corrects_decoded_natural_dimensions() {
        let images = HashMap::from([("http://e.com/two.png".to_string(), (600u32, 400u32))]);
        let layout = lay_graphical(
            r#"<body style="margin:0"><img src="giant.png"
                srcset="two.png 600w, giant.png 3840w"
                sizes="300px"></body>"#,
            800.0,
            &images,
        );
        let image = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("selected responsive image paint operation");
        assert_eq!((image.width, image.height), (300.0, 200.0));
        assert_eq!(
            layout.paint.image_requests[0].source,
            "http://e.com/two.png"
        );
    }

    #[test]
    fn mixed_font_spans_retain_real_metrics_on_one_baseline() {
        let layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0"><span style="font-size:12px">small</span><strong style="font-size:28px;font-style:italic">BIG</strong></p></body>"#,
            320.0,
            &HashMap::new(),
        );
        let line = layout.paint.lines.first().expect("line box");
        let runs: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, shaped, .. } => {
                    Some((*origin, shaped))
                }
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(runs[1].1.line_height > runs[0].1.line_height);
        for (origin, shaped) in runs {
            assert!((origin.y + shaped.baseline - line.baseline).abs() < 0.01);
        }
    }

    #[test]
    fn nested_inline_text_moves_with_its_vertically_aligned_parent() {
        // CSS 2.2 §10.8.1 aligns an inline element's whole aligned subtree.
        // Flattening the two anchors into independent runs must not leave them
        // on the baseline while the parent's literal separator moves.
        let layout = lay_graphical(
            r##"<body style="margin:0"><p style="margin:0;line-height:40px">
               <span style="vertical-align:middle"><a href="#a">SIGN UP</a>|<a href="#b">LOG IN</a></span>
               </p></body>"##,
            320.0,
            &HashMap::new(),
        );
        let origins: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::DisplayCommand::GlyphRun { origin, shaped, .. }
                    if ["SIGN", "UP", "|", "LOG", "IN"]
                        .iter()
                        .any(|part| shaped.text.contains(part)) =>
                {
                    Some(origin.y)
                }
                _ => None,
            })
            .collect();
        assert!(origins.len() >= 3, "missing toolbar runs: {origins:?}");
        assert!(
            origins.iter().all(|y| (*y - origins[0]).abs() < 0.1),
            "aligned subtree split across baselines: {origins:?}"
        );
    }

    #[test]
    fn proportional_advances_decide_graphical_line_wrapping() {
        let style = crate::text::TextStyle::default();
        let narrow = crate::text::shape("iiii iiii", &style).advance;
        let wide = crate::text::shape("WWWW WWWW", &style).advance;
        assert!(wide > narrow);
        let viewport = (narrow + wide) / 2.0;
        let narrow_layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0">iiii iiii</p></body>"#,
            viewport,
            &HashMap::new(),
        );
        let wide_layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0">WWWW WWWW</p></body>"#,
            viewport,
            &HashMap::new(),
        );
        assert_eq!(narrow_layout.paint.lines.len(), 1);
        assert!(wide_layout.paint.lines.len() >= 2);
    }

    #[test]
    fn undecoded_image_falls_back_to_alt_text() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0"><img src="i.png" alt="a kitten"></p></body>"#,
            40,
        );
        let (_, it) = find(&out, "a kitten");
        assert_eq!(it.kind, ItemKind::Image);
        assert_eq!(it.image, None);
    }

    #[test]
    fn undecoded_image_with_attr_dims_reserves_blank_box() {
        let out = lay(
            r#"<body style="margin:0"><img src="i.png" width="80" height="64" alt="x"></body>"#,
            40,
        );
        let img = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.kind == ItemKind::Image)
            .expect("reserved box");
        assert_eq!((img.width, img.height), (10, 4));
        assert_eq!(img.image, None, "no pixels yet — renderer paints blank");
    }

    #[test]
    fn width_percent_height_auto_image_scales_by_computed_width_not_attrs() {
        // erome.com /explore: a 250×250 thumbnail (`width`/`height` HTML
        // attrs) styled `width:100%;height:auto` inside a narrower card.
        // `height:auto` must resolve via the intrinsic ratio against the
        // CSS-computed width (176px → 176px, ratio 1:1), NOT the raw 250px
        // attribute value — the oversized box (250px = 16 rows) buried the
        // album title/username text painted below it (the atomic-image paint
        // rule lets a surviving image claim its whole box, even cells a
        // later sibling legitimately owns).
        let out = lay(
            r#"<body style="margin:0"><div style="width:176px">
               <img src="i.jpg" width="250" height="250" style="width:100%;height:auto" alt="x">
               </div></body>"#,
            40,
        );
        let (_, img) = first_image(&out);
        assert_eq!(
            (img.width, img.height),
            (22, 11),
            "176px computed width, height follows the 1:1 ratio — not the raw 250px attr"
        );
    }

    #[test]
    fn author_height_auto_overrides_only_the_height_presentational_hint() {
        // 9to5linux article hero shape: the HTML dimensions provide early
        // width/height and aspect-ratio hints, while the stylesheet constrains
        // width and explicitly returns height to auto. The author `height:auto`
        // outranks only the height hint; max-width then constrains the remaining
        // width hint and the auto height follows the decoded preferred ratio
        // (the attribute ratio is the pre-decode fallback).
        // Treating explicit auto as "no declaration" produced 40×50 cells:
        // width was clamped to the viewport but the raw 800px height survived.
        let images = img_sizes(&[("http://e.com/hero.webp", 80, 23)]);
        let out = lay_images(
            r#"<style>img { max-width:100%; height:auto }</style>
               <body style="margin:0">
                 <img src="hero.webp" width="1400" height="800">
               </body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(
            (img.width, img.height),
            (40, 12),
            "the constrained width re-derives auto height through the preferred ratio"
        );
    }

    #[test]
    fn ratio_only_svg_sizes_to_the_containing_block() {
        // CSS 2.1 §10.3.2 rule 3: an <img> of a viewBox-only SVG (intrinsic
        // ratio, no intrinsic width/height) sized auto/auto takes its width
        // from the containing block — NOT the decoder's 150×150 default object
        // size (the archive.org media-icon "giant icon" bug). Here the CB is a
        // 80px div (10 cells); the decoder's fabricated natural is 19×9 cells.
        let svg = "data:image/svg+xml,%3csvg%20viewBox='0%200%20300%20300'%20xmlns='http://www.w3.org/2000/svg'%3e%3c/svg%3e";
        let mut images = HashMap::new();
        images.insert(svg.to_string(), (152u32, 144u32));
        let html = format!(
            r#"<body style="margin:0"><div style="width:80px"><img src="{svg}" alt="icon"></div></body>"#
        );
        let out = lay_images(&html, 60, &images);
        let img = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.kind == ItemKind::Image)
            .expect("image item");
        // 80px CB / 8px cell = 10 cells wide, square ratio → 80px/16 = 5 rows;
        // the 19×9-cell natural would be wrong.
        assert_eq!(
            (img.width, img.height),
            (10, 5),
            "rule 3: width = containing-block width, height from ratio"
        );
    }

    #[test]
    fn external_ratio_only_svg_sizes_to_the_containing_block() {
        // An EXTERNAL ratio-only SVG (layout can't read its markup): the image
        // loader records its ratio by URL as it decodes it, and replaced sizing
        // reads that cache to apply rule 3. Unique URL — the cache is global.
        let url = "http://e.com/l2-ratio-only-icon-test.svg";
        crate::img::record_svg_intrinsic_metadata(
            url,
            br#"<svg viewBox="0 0 300 300" xmlns="http://www.w3.org/2000/svg"/>"#,
        );
        let mut images = HashMap::new();
        images.insert(url.to_string(), (152u32, 144u32));
        let out = lay_images(
            r#"<body style="margin:0"><div style="width:80px"><img src="l2-ratio-only-icon-test.svg" alt="v"></div></body>"#,
            60,
            &images,
        );
        let img = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.kind == ItemKind::Image)
            .expect("image item");
        assert_eq!(
            (img.width, img.height),
            (10, 5),
            "rule 3 via the external ratio-only cache"
        );
    }

    #[test]
    fn text_align_center_and_right() {
        let width = 160.0;
        let layout = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0;text-align:center">mid</p><p style="margin:0;text-align:right">end</p></body>"#,
            width,
            &HashMap::new(),
        );
        let (mx, _, _) = graphical_text(&layout, "mid");
        let (ex, _, _) = graphical_text(&layout, "end");
        let style = crate::text::TextStyle::default();
        let mid_w = crate::text::shape("mid", &style).advance;
        let end_w = crate::text::shape("end", &style).advance;
        assert!((mx - (width - mid_w) / 2.0).abs() < 0.25, "mid x={mx}");
        assert!((ex - (width - end_w)).abs() < 0.25, "end x={ex}");
    }

    #[test]
    fn legacy_center_aligns_overconstrained_descendant_blocks() {
        // WHATWG HTML Rendering §15.2/15.3.3: this is not merely inherited
        // text-align. A qualifying fixed-width descendant has its forced used
        // margin divided according to the nearest legacy alignment context.
        let html = r#"<body style="margin:0">
            <center><div id="centered" style="position:relative;left:-10px;width:100px;height:1px;margin:0"></div></center>
            <div style="text-align:center"><div id="css-only" style="width:100px;height:1px;margin:0"></div></div>
            <div align="right"><div id="right" style="width:100px;height:1px;margin:0"></div></div>
            <center><div id="excluded" align="left" style="width:100px;height:1px;margin:0"></div></center>
        </body>"#;
        let dom = Dom::parse_document(html);
        let layout = lay_graphical(html, 300.0, &HashMap::new());
        let x = |id: &str| layout.boxes[&dom.get_by_id(id).unwrap()].left;
        assert!((x("centered") - 90.0).abs() < 0.01);
        assert!((x("css-only") - 0.0).abs() < 0.01);
        assert!((x("right") - 200.0).abs() < 0.01);
        assert!((x("excluded") - 0.0).abs() < 0.01);
    }

    #[test]
    fn relative_positioning_visually_offsets_a_float_without_moving_its_shelf() {
        // CSS Positioned Layout 3 §2/§3.3: relative positioning moves the
        // rendered float, but does not change where later floats see its
        // original margin box. The second float is too wide to sit beside the
        // first, so its shelf remains y=40 rather than following the -10px
        // visual shift to y=30.
        let html = r#"<body style="margin:0"><div style="width:200px">
            <div id="first" style="float:left;position:relative;left:5px;top:-10px;width:100px;height:40px"></div>
            <div id="second" style="float:left;width:150px;height:10px"></div>
        </div></body>"#;
        let dom = Dom::parse_document(html);
        let layout = lay_graphical(html, 200.0, &HashMap::new());
        let rect = |id: &str| layout.boxes[&dom.get_by_id(id).unwrap()];
        assert!((rect("first").left - 5.0).abs() < 0.01);
        assert!((rect("first").top + 10.0).abs() < 0.01);
        assert!((rect("second").left - 0.0).abs() < 0.01);
        assert!((rect("second").top - 40.0).abs() < 0.01);
    }

    #[test]
    fn text_indent_and_justification_use_fractional_css_geometry() {
        let indent = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0;text-indent:20.5px">indented</p></body>"#,
            200.0,
            &HashMap::new(),
        );
        let (x, _, _) = graphical_text(&indent, "indented");
        assert!((x - 20.5).abs() < 0.01);

        let justified = lay_graphical(
            r#"<body style="margin:0"><p style="margin:0;text-align:justify">aa bb cc dd ee ff gg hh ii jj</p></body>"#,
            100.5,
            &HashMap::new(),
        );
        assert!(justified.paint.lines.len() > 1);
        for line in justified
            .paint
            .lines
            .iter()
            .take(justified.paint.lines.len() - 1)
        {
            assert!((line.rect.width - 100.5).abs() < 0.02, "{line:?}");
        }
    }

    #[test]
    fn terminal_adapter_preserves_justified_words_within_its_cell_band() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;text-align:justify">aa bb cc dd ee ff gg hh ii jj kk zz</p></body>"#,
            10,
        );
        // Canonical justification is verified above in fractional CSS pixels.
        // The terminal adapter deliberately rounds proportional stretched gaps;
        // it must preserve every word and stay inside its 10-cell band.
        let words: Vec<_> = out
            .rows
            .iter()
            .flat_map(|row| {
                row_text(row)
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            words,
            [
                "aa", "bb", "cc", "dd", "ee", "ff", "gg", "hh", "ii", "jj", "kk", "zz"
            ]
        );
        assert!(
            out.rows
                .iter()
                .all(|row| display_width(row_text(row).trim_end()) <= 10)
        );
    }

    #[test]
    fn visibility_hidden_lays_out_but_paints_blank() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;visibility:hidden">ghost</p><p style="margin:0">real</p></body>"#,
            40,
        );
        let (rg, ghost) = find(&out, "ghost");
        assert!(ghost.invisible);
        assert_eq!(rg, 0, "hidden box still occupies its row");
        let (rr, real) = find(&out, "real");
        assert!(!real.invisible);
        assert_eq!(rr, 1);
    }

    #[test]
    fn display_none_generates_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="display:none">gone</p><p style="margin:0">kept</p></body>"#,
            40,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("gone")),
            "display:none subtree renders nothing"
        );
        let (r, _) = find(&out, "kept");
        assert_eq!(r, 0);
    }

    #[test]
    fn anchor_rows_map_ids_to_first_rows() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">top</p><h2 id="sec" style="margin:16px 0">Section</h2><a name="legacy"></a></body>"#,
            40,
        );
        let (r, _) = find(&out, "Section");
        assert_eq!(out.anchor_rows.get("sec"), Some(&r));
        assert!(out.anchor_rows.contains_key("legacy"));
    }

    #[test]
    fn overlong_word_overflows_and_clips_at_viewport() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">aaaaaaaaaaaaaaaaaaaa</p></body>"#,
            10,
        );
        // 20-cell word at 10-cell viewport: clipped at the right edge (what a
        // browser shows before you scroll right), never force-broken.
        assert_eq!(row_text(&out.rows[0]), "aaaaaaaaaa");
        assert_eq!(out.rows.len(), 1);
    }

    #[test]
    fn nowrap_does_not_wrap() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;white-space:nowrap">aaa bbb ccc ddd eee</p></body>"#,
            10,
        );
        let non_empty = out.rows.iter().filter(|r| !r.items.is_empty()).count();
        assert_eq!(non_empty, 1);
    }

    #[test]
    fn cjk_wraps_between_ideographs() {
        // 10 cols (the engine's minimum content width) = 5 wide glyphs.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">日本語のテキスト</p></body>"#,
            10,
        );
        assert_eq!(row_text(&out.rows[0]), "日本語のテ");
        assert_eq!(row_text(&out.rows[1]), "キスト");
    }

    #[test]
    fn details_closed_shows_only_summary() {
        let out = lay(
            r#"<body style="margin:0"><details><summary>more</summary><p>secret</p></details></body>"#,
            40,
        );
        find(&out, "more");
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("secret")),
            "closed details hides non-summary children"
        );
    }

    #[test]
    fn definite_height_reserves_rows() {
        let out = lay(
            r#"<body style="margin:0"><div style="height:64px"></div><p style="margin:0">below</p></body>"#,
            40,
        );
        let (r, _) = find(&out, "below");
        assert_eq!(r, 4, "64px = 4 rows reserved by the empty box");
    }

    // ---- the P1 gate: replaced elements (image-heavy pages) ----

    fn lay_with_forms(html: &str, cols: usize, images: &ImageSizes) -> Output {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        lay_out_document(
            &dom,
            &base,
            TerminalViewport::new(cols, 24, 8.0, 16.0),
            &forms,
            &controls,
            images,
            &HashMap::new(),
        )
    }

    fn img_sizes(pairs: &[(&str, u16, u16)]) -> ImageSizes {
        pairs
            .iter()
            .map(|&(u, w, h)| (u.to_string(), (u32::from(w) * 8, u32::from(h) * 16)))
            .collect()
    }

    fn first_image(out: &Output) -> (usize, &Item) {
        for (r, row) in out.rows.iter().enumerate() {
            for it in &row.items {
                if it.kind == ItemKind::Image {
                    return (r, it);
                }
            }
        }
        panic!("no image item");
    }

    #[test]
    fn max_width_100pct_downscales_preserving_ratio() {
        // Decoded 100×20 cells = 800×320px, 40-col (320px) viewport,
        // `max-width:100%`: the §10.4 table scales to 320×128px = 40×8.
        let images = img_sizes(&[("http://e.com/big.png", 100, 20)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="big.png" style="max-width:100%" alt="x"></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (40, 8));
        assert_eq!(img.image.as_deref(), Some("http://e.com/big.png"));
    }

    #[test]
    fn object_fit_cover_sets_crop() {
        let images = img_sizes(&[("http://e.com/i.png", 20, 5)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="i.png" style="width:80px;height:80px;object-fit:cover"></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (10, 5));
        assert!(img.crop, "cover fills the box and crops overflow");
    }

    #[test]
    fn object_fit_contain_letterboxes_centered() {
        // Natural 10×3 cells (80×48px) in an 80×144px box: contain keeps the
        // natural rect, centered 48px (3 rows) below the box top; the BOX
        // (10 cells × 9 rows) is what the flow reserves.
        let images = img_sizes(&[("http://e.com/i.png", 10, 3)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="i.png" style="width:80px;height:144px;object-fit:contain"><p style="margin:0">after</p></body>"#,
            40,
            &images,
        );
        let (r, img) = first_image(&out);
        assert_eq!(r, 3, "paint rect centered: (9-3)/2 rows below box top");
        assert_eq!((img.width, img.height), (10, 3));
        assert!(!img.crop);
        let (after, _) = find(&out, "after");
        assert_eq!(after, 9, "the flow advanced by the full 9-row box");
    }

    #[test]
    fn pct_height_resolves_against_definite_cb() {
        // CB height 160px; img height:50% = 80px = 5 rows; natural ratio
        // 80:160 px (1:2) gives width 40px = 5 cells.
        let images = img_sizes(&[("http://e.com/i.png", 10, 10)]);
        let out = lay_images(
            r#"<body style="margin:0"><div style="height:160px"><img src="i.png" style="height:50%"></div></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (5, 5));
    }

    #[test]
    fn undecoded_image_with_aspect_ratio_reserves_box() {
        let out = lay(
            r#"<body style="margin:0"><img src="i.png" style="width:160px;aspect-ratio:2/1" alt="x"></body>"#,
            40,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (20, 5), "160×80px from the ratio");
        assert_eq!(img.image, None, "reserved, not yet decoded");
    }

    #[test]
    fn thumbnail_row_wraps_into_grid() {
        // The image-heavy gate: six 10×4 thumbnails at 40 cols pack four to
        // a line and wrap, exactly like a browser's inline image run.
        let images = img_sizes(&[("http://e.com/t.png", 10, 4)]);
        let html = r#"<body style="margin:0"><p style="margin:0"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"></p></body>"#;
        let out = lay_images(html, 40, &images);
        let row0: Vec<u16> = out.rows[0].items.iter().map(|i| i.col).collect();
        assert_eq!(row0, vec![0, 10, 20, 30]);
        let row4: Vec<u16> = out.rows[4].items.iter().map(|i| i.col).collect();
        assert_eq!(row4, vec![0, 10], "fifth and sixth wrap to the next strip");
    }

    #[test]
    fn text_input_size_attr_sets_replaced_box_not_glyph_padding() {
        let layout = lay_graphical(
            r#"<body style="margin:0"><form action="/s"><input id="q" name="q" placeholder="q" size="10"><input id="r" name="r" placeholder="r"></form></body>"#,
            640.0,
            &HashMap::new(),
        );
        let q_glyph = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { shaped, .. }
                    if shaped.text.starts_with('q') =>
                {
                    Some(shaped)
                }
                _ => None,
            })
            .expect("q widget");
        assert_eq!(q_glyph.text, "q");
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form action="/s"><input id="q" name="q" placeholder="q" size="10"><input id="r" name="r" placeholder="r"></form></body>"#,
        );
        let q = dom.get_by_id("q").unwrap();
        let r = dom.get_by_id("r").unwrap();
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(640.0, 300.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let q_box = layout.boxes.get(&q).expect("q control box");
        let r_box = layout.boxes.get(&r).expect("r control box");
        assert!(
            q_box.width > f64::from(q_glyph.advance),
            "{q_box:?} {q_glyph:?}"
        );
        assert!(r_box.width > q_box.width, "{q_box:?} {r_box:?}");
    }

    #[test]
    fn css_width_sizes_text_input() {
        let out = lay_with_forms(
            r#"<body style="margin:0"><form action="/s"><input name="q" style="width:80px"></form></body>"#,
            80,
            &HashMap::new(),
        );
        let q = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.kind == ItemKind::Form)
            .expect("widget");
        assert!(
            (10..=20).contains(&display_width(&q.text)),
            "80 CSS px is adapted to a coherent terminal label"
        );
    }

    #[test]
    fn graphical_controls_do_not_paint_terminal_widget_syntax() {
        // WHATWG HTML form rendering + implicit submission: an input paints
        // its value/placeholder inside its CSS-sized box. A form without an
        // authored submit button does not gain one merely because Enter can
        // submit it. Brackets remain a terminal-only affordance.
        let html = r#"<body style="margin:0"><form action="/results" style="display:flex;width:320px">
            <input name="search_query" placeholder="Suchen" style="width:100%">
        </form></body>"#;
        let graphics = lay_graphical(html, 400.0, &HashMap::new());
        let painted: Vec<_> = graphics
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(painted.iter().any(|text| text.starts_with("Suchen")));
        assert!(
            painted
                .iter()
                .all(|text| !text.contains('[') && !text.contains(']') && !text.contains("Submit")),
            "graphical paint leaked terminal/synthetic content: {painted:?}"
        );

        let terminal = lay_with_forms(html, 60, &HashMap::new());
        let terminal = terminal_text(&terminal);
        assert!(
            terminal.contains("[Suchen"),
            "terminal affordance retained: {terminal:?}"
        );
        assert!(
            !terminal.contains("Submit"),
            "no synthetic submit: {terminal:?}"
        );
    }

    #[test]
    fn graphical_text_control_uses_css_box_not_placeholder_and_hits_full_box() {
        // HTML Rendering §15.5.6 + CSS UI 4 §7.2: a normal one-line field
        // is a replaced inline-block. Its placeholder scrolls/clips inside the
        // used CSS box; authored background/padding paint on that whole box,
        // and activation is not limited to the glyph ink.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form action="/s" style="width:280px">
               <input id=q name=q placeholder="a placeholder much wider than the declared field and its containing form"
                 style="box-sizing:border-box;width:100%;padding:7px 10px;background:#123456;border:0">
               </form></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let q = dom.get_by_id("q").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 300.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&q).expect("input geometry");
        assert!((geometry.width - 280.0).abs() < 0.1, "{geometry:?}");
        assert!(
            geometry.height > 20.0,
            "padding contributes to height: {geometry:?}"
        );

        let shape_rect = |shape: &crate::render::PaintShape| match shape {
            crate::render::PaintShape::Rect(rect)
            | crate::render::PaintShape::RoundedRect { rect, .. } => Some(*rect),
            crate::render::PaintShape::Path(_) => None,
        };
        let painted = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::DisplayCommand::Fill {
                    shape,
                    brush:
                        crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                            0x12,
                            0x34,
                            0x56,
                            0xff,
                        )),
                } => shape_rect(shape),
                _ => None,
            });
        let painted = painted.expect("authored input background fill");
        assert!((painted.width - 280.0).abs() < 0.1, "{painted:?}");
        let hit = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::DisplayCommand::HitRegion(region)
                    if region.node == q
                        && matches!(region.link, Some(crate::doc::Link::Form { .. })) =>
                {
                    Some(region.rect)
                }
                _ => None,
            });
        let hit = hit.expect("full form activation region");
        assert!((hit.width - 280.0).abs() < 0.1, "{hit:?}");
    }

    #[test]
    fn auto_width_control_uses_its_own_computed_font() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form style="font-size:40px">
               <input id=s type=submit value="Go"
                 style="font-size:10px;padding:0;border:0">
               </form></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let submit = dom.get_by_id("s").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 300.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&submit).expect("submit geometry");
        assert!(
            geometry.width < 30.0,
            "control must not inherit the surrounding 40px label metrics: {geometry:?}"
        );
    }

    #[test]
    fn button_ua_border_box_keeps_its_authored_width() {
        // WHATWG HTML Rendering §15.5.6: button controls use
        // `box-sizing: border-box` in the UA style sheet. Padding and borders
        // therefore fit inside an authored width instead of enlarging the
        // control (as happened to Archive's compact sort-direction button).
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><button id=sort type=button
               style="width:30px;padding:7px 8px;border:1px solid">↕</button></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let sort = dom.get_by_id("sort").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 200.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&sort).expect("button geometry");
        assert!((geometry.width - 30.0).abs() < 0.1, "{geometry:?}");
    }

    #[test]
    fn checkbox_and_radio_have_one_native_appearance() {
        // WHATWG HTML Rendering §15.5.10: each checkable input is a single
        // widget. The graphical glyph is its native appearance; a generic
        // text-control fill/stroke must not be painted around it.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form>
               <input id=c type=checkbox><input id=r type=radio name=g>
               <input id=t type=text>
               </form></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 200.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let shape_rect = |shape: &crate::render::PaintShape| match shape {
            crate::render::PaintShape::Rect(rect)
            | crate::render::PaintShape::RoundedRect { rect, .. } => Some(*rect),
            crate::render::PaintShape::Path(_) => None,
        };
        let checkable_has_surface = |node| {
            let origin = layout
                .paint
                .primitives
                .iter()
                .find_map(|command| match command {
                    crate::render::DisplayCommand::GlyphRun {
                        node: glyph_node,
                        origin,
                        ..
                    } if *glyph_node == node => Some(*origin),
                    _ => None,
                });
            let origin = origin.expect("native checkable glyph");
            layout.paint.primitives.iter().any(|command| {
                let rect = match command {
                    crate::render::DisplayCommand::Fill { shape, .. }
                    | crate::render::DisplayCommand::Stroke { shape, .. } => shape_rect(shape),
                    _ => None,
                };
                rect.is_some_and(|rect| {
                    origin.x >= rect.x
                        && origin.x < rect.x + rect.width
                        && origin.y >= rect.y
                        && origin.y < rect.y + rect.height
                })
            })
        };
        assert!(!checkable_has_surface(dom.get_by_id("c").unwrap()));
        assert!(!checkable_has_surface(dom.get_by_id("r").unwrap()));
        assert!(layout.paint.primitives.iter().any(|command| matches!(
            command,
            crate::render::DisplayCommand::Fill {
                brush: crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                    255, 255, 255, 255
                )),
                ..
            }
        )));
    }

    #[test]
    fn floated_control_intrinsic_width_does_not_double_count_edges() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form>
               <input id=s type=submit value="Go"
                 style="float:right;font-size:10px;padding:5px;text-align:center;background:#e93250;border:1px solid">
               </form></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let submit = dom.get_by_id("s").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 300.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&submit).expect("submit geometry");
        let expected = f64::from(
            crate::text::shape(
                "Go",
                &crate::text::TextStyle {
                    size: 10.0,
                    ..crate::text::TextStyle::default()
                },
            )
            .advance,
        ) + 12.0;
        assert!((geometry.width - expected).abs() < 0.1, "{geometry:?}");

        let shape_rect = |shape: &crate::render::PaintShape| match shape {
            crate::render::PaintShape::Rect(rect)
            | crate::render::PaintShape::RoundedRect { rect, .. } => Some(*rect),
            crate::render::PaintShape::Path(_) => None,
        };
        let authored_surfaces: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::DisplayCommand::Fill {
                    shape,
                    brush:
                        crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                            0xe9,
                            0x32,
                            0x50,
                            0xff,
                        )),
                } => shape_rect(shape),
                _ => None,
            })
            .collect();
        assert_eq!(
            authored_surfaces.len(),
            1,
            "blockification must not paint a nested control surface: {authored_surfaces:?}"
        );
        assert!(
            (authored_surfaces[0].width - geometry.width as f32).abs() < 0.1,
            "authored background and used border box diverged: {authored_surfaces:?} {geometry:?}"
        );
        let full_hit = layout.paint.primitives.iter().any(|primitive| {
            matches!(primitive,
                crate::render::DisplayCommand::HitRegion(region)
                    if region.node == submit
                        && (region.rect.width - geometry.width as f32).abs() < 0.1)
        });
        assert!(full_hit, "the complete floated control must be activatable");
    }

    #[test]
    fn iframe_keeps_replaced_viewport_size_and_clips_nested_document() {
        // HTML Rendering §15.2/§15.4.1: a child navigable is sized to the
        // iframe content box and the iframe remains a replaced element. Its
        // document must not become an unconstrained parent-document sibling.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div style="width:400px">
               <iframe id=f style="width:50%;height:74px;border:0"></iframe><span>after</span>
               </div></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<body style="margin:0"><div style="position:relative;width:300px;height:74px">
               <span id=inside style="position:absolute;right:0;bottom:0">nested viewport content</span>
               </div></body>"#,
            "https://frame.test/widget",
        )
        .unwrap();
        let inside = dom.get_by_id("inside").unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(400.0, 300.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&frame).expect("iframe geometry");
        assert!((geometry.width - 200.0).abs() < 0.1, "{geometry:?}");
        assert!((geometry.height - 74.0).abs() < 0.1, "{geometry:?}");
        let clip = layout
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::DisplayCommand::GlyphRun {
                    node, shaped, clip, ..
                } if *node == inside && shaped.text.contains("nested") => *clip,
                _ => None,
            });
        let clip = clip.expect("nested document viewport clip");
        assert!((clip.width - 200.0).abs() < 0.1, "{clip:?}");
        assert!((clip.height - 74.0).abs() < 0.1, "{clip:?}");
    }

    #[test]
    fn serialized_frame_keeps_fixed_viewport_and_flex_body_alignment() {
        // The resident page actor serializes its live DOM before the native
        // frontend reparses and lays it out. That adapter must preserve the
        // same two-box model as `Tree::frame`: a positioned replaced viewport
        // containing the child document's BODY formatting context.
        let mut resident = Dom::parse_document(
            r#"<body style="margin:0"><iframe id=f
               style="position:fixed;inset:0;width:100%;height:100%;border:0"></iframe>
               <p id=after style="margin:0">after</p></body>"#,
        );
        let frame = resident.get_by_id("f").unwrap();
        resident
            .install_frame_document(
                frame,
                r#"<body style="display:flex;flex-direction:column;align-items:center;
                   min-height:100vh;margin:0"><main id=shell style="width:400px">
                   centered shell</main></body>"#,
                "https://frame.test/",
            )
            .unwrap();

        let html = resident.serialize_live(crate::dom::DOCUMENT, &std::collections::HashSet::new());
        let snapshot = Dom::parse_document(&html);
        let shell = snapshot.get_by_id("shell").unwrap();
        let after = snapshot.get_by_id("after").unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &snapshot,
            &base,
            Viewport::new(800.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let shell = layout.boxes.get(&shell).expect("centered child shell");
        assert!((shell.left - 200.0).abs() < 0.1, "{shell:?}\n{html}");
        let after = layout.boxes.get(&after).expect("parent flow sibling");
        assert!(
            after.top < 1.0,
            "a fixed iframe must not consume space in parent flow: {after:?}\n{html}"
        );
    }

    #[test]
    fn canonical_frame_keeps_child_body_flex_formatting_context() {
        // This exercises Tree::frame directly, before the resident DOM is
        // serialized for another presentation arena. CSS Display 3 §2 makes
        // BODY's inner display type authoritative for its children in either
        // frontend path.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><iframe id=f
               style="width:800px;height:300px;border:0"></iframe></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<body style="display:flex;flex-direction:column;align-items:center;margin:0">
               <main id=shell style="width:400px">centered shell</main></body>"#,
            "https://frame.test/",
        )
        .unwrap();
        let shell = dom.get_by_id("shell").unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(800.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let shell = layout.boxes.get(&shell).expect("centered child shell");
        assert!((shell.left - 200.0).abs() < 0.1, "{shell:?}");
    }

    #[test]
    fn iframe_content_navigable_is_an_independent_scrollport() {
        // HTML iframe content is a child navigable with its own viewport. CSS
        // Overflow 3 §2.3 makes overflowing content independently scrollable
        // within that viewport instead of extending the parent document.
        let mut dom = Dom::parse_document(
            r#"<style>iframe{width:200px;height:100px;overflow:scroll}</style>
               <body style="margin:0"><iframe id=f></iframe></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<body style="margin:0"><div style="height:400px">child</div></body>"#,
            "https://frame.test/",
        )
        .unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(400.0, 300.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let scroll = layout
            .paint
            .scroll_containers
            .iter()
            .find(|scroll| scroll.node == frame)
            .expect("iframe viewport is a scroll container");
        assert_eq!(scroll.viewport.width, 200.0);
        assert_eq!(scroll.viewport.height, 100.0);
        assert!(scroll.content.height >= 400.0, "{scroll:?}");
        assert!(scroll.vertical);
    }

    #[test]
    fn iframe_canvas_background_covers_the_complete_scrolling_area() {
        // CSS Backgrounds 3 §§2.11.1–2: the nested document's BODY
        // background is propagated to its canvas and the painting area covers
        // the complete canvas. It must not end after the BODY's 100%-of-
        // viewport box when descendants extend the child viewport's scrolling
        // area (the midosuji.neocities.org iframe regression).
        let mut dom = Dom::parse_document(
            r#"<style>iframe{width:120px;height:60px;overflow:scroll}</style>
               <body style="margin:0;background:#ff00ff"><iframe id=f></iframe></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<html style="height:100%;background:transparent"><body style="height:100%;margin:0;background:#123456"><div id=tail style="height:240px">tail</div></body></html>"#,
            "https://frame.test/",
        )
        .unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let scroll = layout
            .paint
            .scroll_containers
            .iter()
            .find(|scroll| scroll.node == frame)
            .expect("iframe viewport scroll metadata");
        assert!(scroll.content.height >= 240.0, "{scroll:?}");

        let mut frame_scroll_depth = 0usize;
        let mut canvas = None;
        let mut nested_color_fills = 0usize;
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::BeginScroll(node) if *node == frame => {
                    frame_scroll_depth += 1;
                }
                crate::render::DisplayCommand::EndScroll if frame_scroll_depth > 0 => {
                    frame_scroll_depth -= 1;
                }
                crate::render::DisplayCommand::Fill {
                    shape: crate::render::PaintShape::Rect(rect),
                    brush:
                        crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                            18,
                            52,
                            86,
                            255,
                        )),
                } => {
                    nested_color_fills += 1;
                    if frame_scroll_depth > 0 {
                        canvas = Some(*rect);
                    }
                }
                _ => {}
            }
        }
        let canvas = canvas.expect("nested canvas fill inside iframe scroll scope");
        assert_eq!(
            nested_color_fills, 1,
            "the propagated BODY background must not repaint on its finite box"
        );
        assert!((canvas.x - scroll.viewport.x).abs() < 0.01, "{canvas:?}");
        assert!((canvas.y - scroll.viewport.y).abs() < 0.01, "{canvas:?}");
        assert!(
            canvas.height >= scroll.content.height,
            "canvas must cover the full scrolling area: {canvas:?} {scroll:?}"
        );
        let max_scroll = (scroll.content.height - scroll.viewport.height).max(0.0);
        assert!(
            canvas.y + canvas.height - max_scroll >= scroll.viewport.y + scroll.viewport.height,
            "canvas must still cover the scrollport at its maximum offset"
        );
    }

    #[test]
    fn indefinite_percentage_iframe_height_uses_replaced_fallback() {
        // CSS Sizing 3 §§3.2.1 and 5.1: the percentage cannot resolve against
        // this content-sized containing block, so it behaves as auto; an
        // iframe has no natural dimensions and uses the 300x150 fallback.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div style="width:200px">
               <iframe id=f style="width:100%;height:100%;overflow:scroll"></iframe>
               </div></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<body style="margin:0"><div style="height:400px">child</div></body>"#,
            "https://frame.test/",
        )
        .unwrap();
        let base = Url::parse("https://page.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(400.0, 300.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let geometry = layout.boxes.get(&frame).expect("iframe geometry");
        assert!((geometry.height - 150.0).abs() < 0.1, "{geometry:?}");
        let scroll = layout
            .paint
            .scroll_containers
            .iter()
            .find(|scroll| scroll.node == frame)
            .expect("iframe viewport is independently scrollable");
        assert!((scroll.viewport.height - 150.0).abs() < 0.1, "{scroll:?}");
        assert!(scroll.content.height >= 400.0, "{scroll:?}");
    }

    #[test]
    fn fixed_subtree_retains_nested_scroll_container_metadata() {
        // CSS Position 3 §2 changes the fixed box's placement, not the CSS
        // Overflow 3 scroll-container status of its descendants. Graphical
        // fixed fragments are retained separately from normal flow, so their
        // scrollports must be collected explicitly for wheel hit testing.
        let layout = lay_graphical(
            r#"<body style="margin:0"><div style="position:fixed;inset:0">
               <div id=scroller style="width:200px;height:100px;overflow:auto">
               <div style="height:400px">child</div></div></div></body>"#,
            400.0,
            &HashMap::new(),
        );
        let scroller = layout
            .paint
            .scroll_containers
            .iter()
            .find(|scroll| scroll.viewport.width == 200.0 && scroll.viewport.height == 100.0)
            .expect("scrollport inside a fixed layer remains interactive");
        assert!(scroller.content.height >= 400.0, "{scroller:?}");
        assert!(scroller.vertical);
    }

    #[test]
    fn css_outline_paints_without_changing_control_layout() {
        // CSS UI 4 §3: outlines are drawn over/around the box, contribute only
        // to ink overflow, and must not consume layout space. The graphical
        // path retains the authored color; the terminal adapter expresses the
        // same paint-only decoration as box-drawing chrome even when ordinary
        // CSS borders are disabled.
        let html = r#"<body style="margin:0"><input name="q" value="text"
            style="width:80px;outline:2px solid #ff0000;outline-offset:2px"></body>"#;
        let graphics = lay_graphical(html, 320.0, &HashMap::new());
        assert!(graphics.paint.primitives.iter().any(|primitive| matches!(
            primitive,
            crate::render::DisplayCommand::Stroke {
                brush: crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                    255, 0, 0, 255
                )),
                style,
                ..
            } if (style.width - 2.0).abs() < 0.01
        )));
        let terminal = lay_with_forms(html, 40, &HashMap::new());
        assert!(
            terminal
                .rows
                .iter()
                .flat_map(|row| row.items.iter())
                .any(|item| item.kind == ItemKind::Border),
            "outline should remain visible in the terminal adapter"
        );
    }

    #[test]
    fn percentage_control_width_is_auto_during_intrinsic_contribution() {
        // CSS Sizing 3 §5.2.1: a percentage size that is cyclic against an
        // intrinsically-sized grid track behaves as auto for the contribution.
        // Resolving 100% against the max-content probe's 10,000,000px sentinel
        // used to materialize about 2.6 million padding spaces and shape them.
        let dom =
            Dom::parse_document(r#"<form action="/s"><input name="q" style="width:100%"></form>"#);
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let (&node, &(form, field)) = controls.iter().next().expect("input control");
        let label = inline::control_label(
            &dom,
            node,
            &forms[form].fields[field],
            inline::ControlWidthBasis::Intrinsic,
            10_000_000.0,
            &style::InlineStyle::root(),
            value::Vp { w: 640.0, h: 480.0 },
        );
        assert!(!label.starts_with('['));
        assert!(!label.ends_with(']'));
        assert!(
            label.len() < 128,
            "intrinsic percentage width must use the finite UA/size fallback, got {} bytes",
            label.len()
        );
    }

    #[test]
    fn formless_icon_button_paints_its_sized_svg_child() {
        // WHATWG HTML §4.10.6/Rendering §15.5.3: a <button> is labeled by
        // its contents, whose child boxes form the anonymous button content
        // box. This is the archive.org search-control shape: a formless live
        // type=button containing only a ratio-only SVG image. Treating it as
        // a synthetic form control discarded the <img> and emitted
        // "[ Button ]"; the authored 18px square must paint and retain the
        // live click-marker link instead.
        let svg = "data:image/svg+xml,%3csvg%20viewBox='0%200%20100%20100'%20xmlns='http://www.w3.org/2000/svg'%3e%3cpath%20d='M0%200h100v100H0z'/%3e%3c/svg%3e";
        let html = format!(
            r#"<body style="margin:0"><a href="x-trust-js:42:"><button type="button" aria-label="Search" style="display:flex"><img src="{svg}" alt="" style="width:18px;height:18px"></button></a></body>"#
        );
        let mut images = ImageSizes::new();
        images.insert(svg.to_string(), (150, 150));
        let out = lay_graphical(&html, 320.0, &images);
        assert!(
            !out.paint.primitives.iter().any(|primitive| matches!(
                primitive,
                crate::render::Primitive::GlyphRun { shaped, .. }
                    if shaped.text.contains("[ Button ]")
            )),
            "the button's children, not a synthetic label, are its content"
        );
        let (rect, link) = out
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::Image { rect, link, .. } => Some((*rect, link)),
                _ => None,
            })
            .expect("the button's SVG image paints");
        assert_eq!(
            (rect.width, rect.height),
            (18.0, 18.0),
            "desktop geometry keeps the authored CSS size, not the SVG default object size"
        );
        assert!(
            matches!(link, Some(crate::doc::Link::JsClick { node: 42, .. })),
            "the painted icon inherits the living button's activation link"
        );
    }

    #[test]
    fn video_direct_source_renders_quality_label() {
        let out = lay(
            r#"<body style="margin:0"><video><source src="clip.mp4" res="720" label="HD"></video></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Video · 720p HD");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("clip.mp4"))
        );
    }

    #[test]
    fn audio_with_src_renders_audio_label() {
        let out = lay(
            r#"<body style="margin:0"><audio src="a.mp3"></audio></body>"#,
            60,
        );
        let (_, it) = find(&out, "♪ Audio");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("a.mp3"))
        );
    }

    #[test]
    fn sourceless_video_targets_enclosing_card_link() {
        let out = lay(
            r#"<body style="margin:0"><a href="/watch/1"><video></video></a></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.path() == "/watch/1"),
            "the card's anchor names the playable page"
        );
    }

    #[test]
    fn sourceless_video_targets_a_live_anchors_preserved_href() {
        let out = lay(
            r#"<body style="margin:0"><a href="x-trust-js:42:/channel"><video></video></a></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.path() == "/channel"),
            "a live-page preview keeps the anchor URL for mpv instead of running its player"
        );
    }

    #[test]
    fn sourceless_video_uses_associated_schema_video_object() {
        let images = img_sizes(&[("http://e.com/frame.jpg", 16, 9)]);
        let out = lay_images(
            r#"<body style="margin:0"><article>
                 <div><video poster="frame.jpg"></video><button>Play</button></div>
                 <div itemprop="video" itemscope itemtype="https://schema.org/VideoObject">
                   <meta itemprop="name" content="A clip">
                   <meta itemprop="contentUrl" content="https://cdn.e.com/clip.mp4?quality=1080">
                   <meta itemprop="thumbnailUrl" content="frame.jpg">
                 </div>
               </article></body>"#,
            60,
            &images,
        );
        let (_, poster) = first_image(&out);
        assert!(
            matches!(&poster.link, Some(crate::doc::Link::Media(u)) if u.as_str() == "https://cdn.e.com/clip.mp4?quality=1080"),
            "Schema.org contentUrl is the actual media bytes"
        );
    }

    #[test]
    fn duplicated_custom_player_poster_keeps_the_media_link() {
        let images = img_sizes(&[("http://e.com/frame.jpg", 16, 9)]);
        let out = lay_images(
            r#"<body style="margin:0"><article>
                 <div style="position:relative;overflow:hidden;width:128px;height:72px">
                   <video poster="frame.jpg"></video>
                   <img src="frame.jpg" alt="" style="position:absolute;inset:0;width:100%;height:100%">
                 </div>
                 <div itemprop="video" itemscope itemtype="https://schema.org/VideoObject">
                   <meta itemprop="contentUrl" content="https://cdn.e.com/clip.mp4">
                 </div>
               </article></body>"#,
            60,
            &images,
        );
        let posters: Vec<_> = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.as_deref() == Some("http://e.com/frame.jpg"))
            .collect();
        assert!(!posters.is_empty(), "the custom poster paints");
        assert!(
            posters.iter().all(|i| {
                matches!(&i.link, Some(crate::doc::Link::Media(u)) if u.as_str() == "https://cdn.e.com/clip.mp4")
            }),
            "{posters:#?}"
        );
    }

    #[test]
    fn repeated_schema_video_cards_keep_their_own_targets() {
        let out = lay(
            r#"<body style="margin:0">
                 <article><video></video><div itemprop="video" itemscope itemtype="http://schema.org/VideoObject"><link itemprop="contentUrl" href="/one.mp4"></div></article>
                 <article><video></video><div itemprop="video" itemscope itemtype="https://schema.org/VideoObject"><link itemprop="contentUrl" href="/two.mp4"></div></article>
               </body>"#,
            60,
        );
        let targets: Vec<_> = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter_map(|i| match &i.link {
                Some(crate::doc::Link::Media(u)) => Some(u.path().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(targets, ["/one.mp4", "/two.mp4"]);
    }

    #[test]
    fn schema_media_microdata_honors_itemref_and_nested_item_scope() {
        let out = lay(
            r#"<body style="margin:0">
                 <video></video>
                 <div itemscope itemtype="https://schema.org/VideoObject" itemref="media-details">
                   <div itemprop="encoding" itemscope itemtype="https://schema.org/MediaObject">
                     <meta itemprop="contentUrl" content="/nested-decoy.mp4">
                   </div>
                 </div>
                 <div id="media-details"><meta itemprop="name contentUrl" content="/referred.mp4"></div>
               </body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Video");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.path() == "/referred.mp4"),
            "HTML's itemref target participates, while a nested item stops the property walk"
        );
    }

    #[test]
    fn every_sourceless_video_is_an_external_player_activation() {
        let out = lay(r#"<body style="margin:0"><video></video></body>"#, 60);
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(matches!(
            &it.link,
            Some(crate::doc::Link::Media(url)) if url.as_str() == "http://e.com/"
        ));
    }

    #[test]
    fn custom_player_video_box_remains_an_mpv_hit_surface() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:160px">
                 <video style="position:absolute;inset:0;width:100%;height:100%"></video>
                 <a href="x-trust-js:42:" style="position:absolute;inset:0;z-index:1">Play</a>
               </div></body>"#,
            60,
        );
        let (row, hit) = out
            .rows
            .iter()
            .enumerate()
            .flat_map(|(row, r)| r.hits.iter().map(move |hit| (row, hit)))
            .find(|(row, hit)| {
                matches!(
                    out.rows[*row].items[hit.item].link,
                    Some(crate::doc::Link::Media(_))
                )
            })
            .expect("video box media surface");
        assert_eq!((hit.col, hit.width, hit.height), (0, 40, 10));
        assert!(matches!(
            &out.rows[row].items[hit.item].link,
            Some(crate::doc::Link::Media(url)) if url.as_str() == "http://e.com/"
        ));
    }

    #[test]
    fn og_video_page_gets_page_level_fallback() {
        let out = lay(
            r#"<html><head><meta property="og:video" content="https://cdn.e.com/v.m3u8"></head><body style="margin:0"><p style="margin:0">a watch page</p></body></html>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str() == "http://e.com/"),
            "the page itself is the yt-dlp target"
        );
    }

    #[test]
    fn video_poster_thumbnail_is_the_link() {
        let images = img_sizes(&[("http://e.com/p.jpg", 8, 4)]);
        let out = lay_images(
            r#"<body style="margin:0"><video src="clip.mp4" poster="p.jpg"></video></body>"#,
            60,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(img.image.as_deref(), Some("http://e.com/p.jpg"));
        assert!(
            matches!(&img.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("clip.mp4"))
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains('▶')),
            "the drawn preview IS the affordance — no text line under it"
        );
    }

    #[test]
    fn suppressed_out_of_flow_video_renders_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">cap</p><video src="t.mp4" style="opacity:0;position:absolute"></video></body>"#,
            60,
        );
        assert_eq!(
            out.rows.iter().filter(|r| !r.items.is_empty()).count(),
            1,
            "the lingering opacity:0 abspos microtrailer adds no row"
        );
    }

    #[test]
    fn a_videojs_transformed_player_still_renders_its_caption() {
        // erome.com's real shape after video.js initialises: the aspect-ratio
        // hack (`height:0;padding-top:NN%`) wrapper holds the out-of-flow
        // `<video>` tech PLUS an out-of-flow poster-overlay chrome div with
        // its own opaque `background-color`. In our paint model a declared
        // background is an opaque fill (§Appendix E — needed for the modal/
        // card-stack cases), and the poster overlay sits LATER in the tree
        // than the tech, so it legally painted over our synthesized "▶ Video"
        // mpv-link text, leaving the whole player area blank. The player-
        // wrapper chrome skip must drop the poster/chrome siblings from the
        // box tree entirely so they can never clobber the media
        // representation.
        let out = lay(
            r#"<body style="margin:0"><div class="player video-js" style="height:0;padding-top:56.25%;position:relative;background-color:#000">
                 <video class="vjs-tech" src="clip_720p.mp4" style="position:absolute;width:100%;height:100%;top:0;left:0"></video>
                 <div class="vjs-poster" style="position:absolute;top:0;left:0;width:100%;height:100%;background-color:#000"></div>
               </div></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Video");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("clip_720p.mp4")),
            "the caption still links mpv to the source"
        );
    }

    #[test]
    fn player_wrapper_skip_is_gated_on_the_aspect_ratio_hack() {
        // The player-wrapper chrome skip must NOT fire on a generic
        // in-flow container (no `height:0`) that happens to hold real
        // content beside an unrelated out-of-flow video — only the
        // aspect-ratio-hack shape (which video.js/Plyr/JW always use, since
        // every real child must escape the zero-height content box) opts
        // in. A plain `<div>` here keeps its normal auto height, so its
        // `<p>` sibling must render untouched.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative">
                 <p style="margin:0">cap</p>
                 <video src="t.mp4" style="position:absolute"></video>
               </div></body>"#,
            60,
        );
        find(&out, "cap");
    }

    // ---- the P2 gate: flexbox (the old engine's minefield, §9 as written) ----

    #[test]
    fn flex_row_places_items_side_by_side() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="width:80px">aa<br>bb<br>cc</div>
                <div style="width:80px">dd</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (ra, a) = find(&out, "aa");
        assert_eq!((ra, a.col), (0, 0));
        let (rd, d) = find(&out, "dd");
        assert_eq!((rd, d.col), (0, 10), "second item beside the first");
        let (raf, _) = find(&out, "after");
        assert_eq!(raf, 3, "container height = tallest item (3 lines)");
    }

    #[test]
    fn inline_blocks_sit_side_by_side_on_one_line() {
        // CSS-Display-3 §2.5: an atomic inline box flows on its parent's line at
        // its used width (not block-stacked, not transparent). 80px = 10 cells.
        let out = lay(
            r#"<body style="margin:0"><span style="display:inline-block;width:80px">AAA</span><span style="display:inline-block;width:80px">BBB</span><span style="display:inline-block;width:80px">CCC</span></body>"#,
            80,
        );
        let (ra, a) = find(&out, "AAA");
        let (rb, b) = find(&out, "BBB");
        let (rc, c) = find(&out, "CCC");
        assert_eq!((ra, a.col), (0, 0));
        assert_eq!((rb, b.col), (0, 10), "second box beside the first");
        assert_eq!((rc, c.col), (0, 20), "third box beside the second");
    }

    #[test]
    fn a_multi_row_inline_block_lays_beside_its_sibling() {
        // The media-button shape: an inline-block whose content is icon-over-
        // count (2 rows). Its box occupies both rows on the line, and the next
        // box sits to its RIGHT (not below). 48px = 6 cells.
        let out = lay(
            r#"<body style="margin:0"><a style="display:inline-block;width:48px"><div>ICON</div><div>ONE</div></a><a style="display:inline-block;width:48px"><div>PICT</div><div>TWO</div></a></body>"#,
            80,
        );
        let (ri, icon) = find(&out, "ICON");
        let (ro, one) = find(&out, "ONE");
        let (rp, pict) = find(&out, "PICT");
        assert_eq!((ri, icon.col), (0, 0), "first box's icon at the top-left");
        assert_eq!((ro, one.col), (1, 0), "its count on the second row");
        assert_eq!(
            (rp, pict.col),
            (0, 6),
            "the second box sits to the right, not stacked below"
        );
    }

    #[test]
    fn inline_flex_items_flow_horizontally() {
        // `inline-flex` is an atomic inline box too (flex internally, inline on
        // the line). 40px = 5 cells.
        let out = lay(
            r#"<body style="margin:0"><nav><a style="display:inline-flex;width:40px">Home</a><a style="display:inline-flex;width:40px">News</a></nav></body>"#,
            80,
        );
        let (rh, h) = find(&out, "Home");
        let (rn, n) = find(&out, "News");
        assert_eq!((rh, h.col), (0, 0));
        assert_eq!((rn, n.col), (0, 5), "inline-flex boxes flow on one line");
    }

    #[test]
    fn auto_width_inline_block_shrinks_to_fit() {
        // No explicit width → shrink-to-fit (§10.3.9): the box is as wide as its
        // content, so the next box abuts it. "Hi" = 2 cells.
        let out = lay(
            r#"<body style="margin:0"><span style="display:inline-block">Hi</span><span style="display:inline-block">There</span></body>"#,
            80,
        );
        let (rh, h) = find(&out, "Hi");
        let (rt, t) = find(&out, "There");
        assert_eq!((rh, h.col), (0, 0));
        assert_eq!(
            (rt, t.col),
            (0, 2),
            "shrink-to-fit: next box abuts at col 2"
        );
    }

    #[test]
    fn an_inline_block_that_overflows_wraps_to_the_next_line() {
        // Two 80px (10-cell) boxes don't both fit in a 12-cell line, so the
        // second wraps (it is unbreakable — placed whole on the next line).
        let out = lay(
            r#"<body style="margin:0"><span style="display:inline-block;width:80px">AAA</span><span style="display:inline-block;width:80px">BBB</span></body>"#,
            12,
        );
        let (ra, a) = find(&out, "AAA");
        let (rb, b) = find(&out, "BBB");
        assert_eq!((ra, a.col), (0, 0));
        assert!(rb > ra, "the second box wraps to the next line");
        assert_eq!(b.col, 0, "and starts at the line's left edge");
    }

    #[test]
    fn a_button_wrapped_in_a_click_marker_anchor_stays_clickable() {
        // The live serializer wraps clickables (a `<button>` with a JS click
        // listener) in `<a href="x-trust-js:<id>:<href>">` so the terminal can
        // follow them (Link::JsClick). `<button>` defaults to `display:
        // inline-block` (CSS-Display-3 §2.5), which layout2 lays as an ATOMIC
        // inline box (`Inline::AtomBox`) — its own independent formatting
        // context, pre-laid and spliced onto the line as one opaque unit. The
        // pre-lay pass used to run with the ANCHOR'S ancestor context
        // discarded (the enclosing `<a>`'s derived link/emphasis never
        // reached the atom box's own content), so a wrapped button's label
        // rendered as plain, unlinked text — the reported regression: cookie-
        // banner "Accept"/"Customize"/"Reject" buttons stopped being
        // clickable.
        let out = lay(
            r#"<body style="margin:0"><a href="x-trust-js:501:#"><button type="button">Accept</button></a></body>"#,
            40,
        );
        let (_, it) = find(&out, "Accept");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::JsClick { node: 501, .. })),
            "the button's label keeps the wrapping anchor's click-marker link: {:?}",
            it.link
        );
    }

    #[test]
    fn a_direct_live_button_stays_the_flex_item_and_click_target() {
        // CSS Flexbox §4: every in-flow CHILD is the flex item.  The live
        // serializer must not invent an anchor parent around a native button:
        // doing so makes the anchor stretch to the 40px line while the nested
        // button keeps its intrinsic height.  The marker supplies activation
        // without introducing another CSS box.
        let html = r#"<body style="margin:0"><div style="display:flex;width:320px;height:40px">
            <div style="flex:1">query</div>
            <button id="search" data-trust-click="x-trust-js:42:" data-trust-node="42" style="box-sizing:border-box;width:64px;padding:0;border:0"><span aria-hidden="true" data-trust-ua-icon="" style="display:inline-block;width:24px;height:24px;font-size:30px;font-family:sans-serif;line-height:24px;text-align:center">⌕</span></button>
        </div></body>"#;
        let dom = Dom::parse_document(html);
        let button = dom.get_by_id("search").expect("search button");
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let graphics = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let rect = graphics.boxes.get(&button).expect("button box");
        assert_eq!(rect.height, 40.0, "auto cross-size stretches to the line");
        let (origin, shaped) = graphics
            .paint
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun {
                    origin,
                    shaped,
                    link,
                    ..
                } if shaped.text.contains('⌕')
                    && matches!(link, Some(crate::doc::Link::JsClick { node: 42, .. })) =>
                {
                    Some((*origin, shaped))
                }
                _ => None,
            })
            .expect("the pictogram paints with the live activation target");
        assert!(
            ((origin.x + shaped.advance / 2.0) as f64 - (rect.left + rect.width / 2.0)).abs() < 0.1,
            "HTML button content is centered horizontally"
        );
        assert!(
            ((origin.y + shaped.line_height / 2.0) as f64 - (rect.top + rect.height / 2.0)).abs()
                < 0.1,
            "HTML button content is centered vertically after flex stretch"
        );

        let terminal = lay(html, 40);
        let (_, item) = find(&terminal, "⌕");
        assert!(matches!(
            item.link,
            Some(crate::doc::Link::JsClick { node: 42, .. })
        ));
    }

    #[test]
    fn overflowing_button_content_stays_at_the_start_edges() {
        // WHATWG HTML Rendering §15.5.3 centers the anonymous button content
        // box on an axis only when it does not overflow on that axis.
        let html = r#"<body style="margin:0"><button id="button" style="box-sizing:border-box;width:40px;height:40px;padding:0;border:0"><span id="content" style="display:inline-block;width:80px;height:60px">wide</span></button></body>"#;
        let dom = Dom::parse_document(html);
        let button = dom.get_by_id("button").expect("button");
        let content = dom.get_by_id("content").expect("content");
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let graphics = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let button = graphics.boxes.get(&button).expect("button box");
        let content = graphics.boxes.get(&content).expect("content box");
        assert_eq!(
            content.left, button.left,
            "horizontal overflow starts inline-start"
        );
        assert_eq!(
            content.top, button.top,
            "vertical overflow starts block-start"
        );
    }

    #[test]
    fn block_button_uses_the_same_anonymous_content_alignment() {
        // A non-inline, non-flex/grid button behaves as `flow-root`, but HTML's
        // anonymous button content box still centers its child boxes.
        let html = r#"<body style="margin:0"><button id="button" style="display:block;box-sizing:border-box;width:100px;height:40px;padding:0;border:0"><span id="content" style="display:inline-block;width:20px;height:10px"></span></button></body>"#;
        let dom = Dom::parse_document(html);
        let button = dom.get_by_id("button").expect("button");
        let content = dom.get_by_id("content").expect("content");
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let graphics = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let button = graphics.boxes.get(&button).expect("button box");
        let content = graphics.boxes.get(&content).expect("content box");
        assert_eq!(
            content.left + content.width / 2.0,
            button.left + button.width / 2.0
        );
        assert_eq!(
            content.top + content.height / 2.0,
            button.top + button.height / 2.0
        );
    }

    #[test]
    fn flex_grow_distributes_by_factor() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1">a</div><div style="flex:3">b</div>
               </div></body>"#,
            80,
        );
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 20, "1:3 split of 640px → second item at 160px");
    }

    #[test]
    fn terminal_flex_nav_does_not_reflow_at_overflow_visible_item_edges() {
        // CSS Overflow 3 §3.1: `overflow:visible` renders content outside a
        // box instead of clipping it. The fixed-cell adapter must therefore
        // not reinterpret every proportional flex-item edge as a new wrapping
        // boundary. At a 10px terminal cell, these 14px sans-serif labels need
        // more cells than their graphical advances, matching Steam's store nav.
        let out = lay_with_cells(
            r#"<body style="margin:0"><nav style="display:flex;width:730px;gap:32px;font-size:14px">
                <a>Browse</a><a>Recommendations</a><a>Categories</a>
                <a>Hardware</a><a>Ways to Play</a><a>Special Sections</a>
               </nav></body>"#,
            73,
            10.0,
            18.0,
        );
        let painted = out.rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
        for label in [
            "Browse",
            "Recommendations",
            "Categories",
            "Hardware",
            "Ways to Play",
        ] {
            assert!(
                painted.contains(label),
                "terminal adaptation must preserve {label:?} as one line: {painted:?}"
            );
        }
    }

    #[test]
    fn terminal_reflows_inside_the_inline_formatting_context_band() {
        // CSS Inline 3 §2.1: a block container's content edges form the
        // containing block for its line boxes. The terminal adapter must use
        // that retained band, not the viewport edge. This is the essential
        // geometry of rubymaelstrom.com: a centered 65vw body with padding.
        let text = "One proportional line can contain substantially more terminal cells than its shaped CSS width, but every adapted continuation must remain inside this centered content box instead of escaping across the right margin.";
        let html = format!(
            r#"<body style="box-sizing:border-box;margin:0 17.5vw;width:65vw;padding:20px;font-size:15px">
                 <p style="margin:0">{text}</p>
               </body>"#
        );
        let out = lay_with_cells(&html, 100, 10.0, 20.0);
        let content_right = 81u16; // ceil((17.5vw + 65vw - 20px) / 10px)
        for item in out.rows.iter().flat_map(|row| &row.items) {
            assert!(
                item.col.saturating_add(item.width) <= content_right,
                "adapted text escaped the line-box band: {item:?}"
            );
        }
        let painted_words = out
            .rows
            .iter()
            .flat_map(|row| &row.items)
            .flat_map(|item| item.text.split_whitespace())
            .collect::<Vec<_>>();
        assert_eq!(
            painted_words,
            text.split_whitespace().collect::<Vec<_>>(),
            "terminal reflow must preserve the canonical word sequence"
        );
    }

    #[test]
    fn terminal_reflow_preserves_soft_line_carries_before_the_next_block() {
        // Exact structural reduction of rubymaelstrom.com/200226.html. Long
        // proportional lines reflow into a centered fixed-cell band, including
        // a styled inline in the following paragraph. No continuation may be
        // overwritten by the next block or reordered at a canonical soft-line
        // handoff.
        let first = "I've always found it to be a lot of fun when there are books in video games. Finding books that I could actually read in the Elder Scrolls, books and newspapers on merchants in Everquest, or player-written books left in libraries private and public in Ultima Online, have always been things which brought me joy over the years. There's lots of more convenient ways to read a book than in a tiny window inside of a video game, but reading them in that context gives the world more life than it had before. It makes it feel like there's actually people occupying the environment, or at the very least that somebody cared enough about the setting to write something down about it for me to find.";
        let second_before = "Second Life isn't";
        let second_after = "a video game, but books in Second Life occupy a similar space in my head as books in games do. In Second Life, everything is intentional. If a book is there, somebody put it there for you to read. There have been people taking the time to either write or copy over writing into Second Life ever since it began in 2003, and of course there are plenty doing so to this very day. There are writers groups, libraries, bookstores, museums, exhibitions, etc. Heck, I found a Linden area from the mid-2000's the other day where someone had attempted to keep an in-world log of every single patch note for Second Life since the beginning. Is that a great work of literature? No. Is it neat to find and adds more of that \"flavor\" to the world like I was talking about before? Very much yes.";
        let html = format!(
            r#"<body style="box-sizing:border-box;margin:0 17.5vw;width:65vw;padding:20px;font-family:sans-serif;font-size:15px">
                 <p>{first}</p>
                 <p>{second_before} <i>exactly</i> {second_after}</p>
               </body>"#
        );
        let out = lay_with_cells(&html, 98, 10.0, 20.0);
        let painted_words = out
            .rows
            .iter()
            .flat_map(|row| &row.items)
            .flat_map(|item| item.text.split_whitespace())
            .collect::<Vec<_>>();
        let expected = format!("{first} {second_before} exactly {second_after}");
        assert_eq!(
            painted_words,
            expected.split_whitespace().collect::<Vec<_>>(),
            "terminal reflow must preserve every word across lines and blocks"
        );
    }

    #[test]
    fn terminal_clip_preserves_a_fitting_run_before_later_overflow() {
        // CSS Overflow 3 §5.1 clips at the character/glyph level. A later
        // run can make the line overflow without changing the fact that an
        // earlier run is wholly inside the clip. With the box starting at a
        // half-cell offset, deciding preservation for the whole line used to
        // turn the fitting "Browse" into "Brows" after terminal quantization.
        let out = lay_with_cells(
            r#"<body style="margin:0;font:14px/20px Arial,sans-serif">
                 <div style="margin-left:5px;width:54px;overflow:hidden;white-space:nowrap">
                   <span>Browse</span><span> trailing overflow</span>
                 </div>
               </body>"#,
            20,
            10.0,
            20.0,
        );
        let painted = out.rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
        assert!(
            painted.contains("Browse"),
            "a canonically fitting run must not inherit later line overflow: {painted:?}"
        );
    }

    #[test]
    fn terminal_fixed_nav_preserves_every_label_character() {
        let html = r#"<body style="margin:0;font:14px/20px Arial,sans-serif">
                 <nav style="position:fixed;left:42px;top:40px;display:flex;gap:18px">
                   <a style="width:60px;overflow:hidden;white-space:nowrap">Browse</a>
                   <a style="width:130px;overflow:hidden;white-space:nowrap">Recommendations</a>
                   <a style="width:85px;overflow:hidden;white-space:nowrap">Categories</a>
                   <a style="width:75px;overflow:hidden;white-space:nowrap">Hardware</a>
                 </nav>
               </body>"#;
        // A viewport rebuild follows the same path as the user's reliable
        // resize reproduction. Repeat the original width after narrower and
        // wider layouts so no later pass may turn an intact label into a
        // character-count-clipped spelling.
        for columns in [60, 114, 180, 114] {
            let out = lay_with_cells(html, columns, 10.0, 20.0);
            let painted = out
                .fixed
                .iter()
                .flat_map(|fixed| fixed.rows.iter())
                .map(row_text)
                .collect::<Vec<_>>()
                .join("\n");
            for label in ["Browse", "Recommendations", "Categories", "Hardware"] {
                assert!(
                    painted.contains(label),
                    "fixed-layer adaptation at {columns} columns omitted characters from {label:?}: {painted:?}"
                );
            }
        }
    }

    #[test]
    fn terminal_collision_recovery_cannot_cross_independent_css_bands() {
        fn item(col: u16, width: u16, text: &str, image: Option<&str>, band: (u16, u16)) -> Item {
            Item {
                col,
                width,
                height: 1,
                text: text.to_owned(),
                kind: if image.is_some() {
                    ItemKind::Image
                } else {
                    ItemKind::Text
                },
                image: image.map(str::to_owned),
                emph: Emphasis::default(),
                node: NO_NODE,
                link: None,
                crop: false,
                pixelated: false,
                invisible: false,
                terminal_band: Some(band),
            }
        }

        // Reduction of DistroWatch's three table cells. Proportional layout
        // put the sponsor at col 9, but an unrelated left-cell line that
        // quantized onto the same row occupied through col 35. The old global
        // cursor moved the 18-cell image to col 35, across the left cell's
        // right edge at 44 and into the middle article. Center text could then
        // similarly run directly into the right rankings cell.
        let row = Row {
            items: vec![
                item(1, 34, "left headline expansion", None, (0, 44)),
                item(9, 18, "", Some("https://e.test/3cx.png"), (0, 44)),
                item(44, 55, "middle article", None, (44, 103)),
                item(103, 20, "right rankings", None, (103, 130)),
            ],
            hits: Vec::new(),
        };
        let placed = crate::layout2::visual_columns(&row, &[], 0);
        let at = |index| {
            placed
                .iter()
                .find(|(item, ..)| *item == index)
                .map(|(_, col, width, _)| (*col, *width))
                .unwrap()
        };
        assert!(
            placed.iter().all(|(item, ..)| *item != 1),
            "an irreconcilable atomic overlap is omitted instead of overwriting text or crossing its panel"
        );
        assert_eq!(at(2), (44, 55), "the middle cell keeps its own origin");
        assert_eq!(at(3), (103, 20), "the right cell keeps its own origin");
        for (index, col, width, _) in placed {
            if let Some((left, right)) = row.items[index].terminal_band {
                assert!(col >= left && col + width <= right);
            }
        }
    }

    #[test]
    fn terminal_table_cell_expansion_pushes_later_content_within_that_cell() {
        let mut images = ImageSizes::new();
        images.insert("http://e.com/sponsor.png".to_owned(), (160, 80));
        let out = lay_images(
            r#"<body style="margin:0;font:13px/16px Arial,sans-serif">
               <table style="width:100%;table-layout:fixed"><tr>
                 <td style="width:34%;vertical-align:top">
                   <table style="width:88%"><tr><td>iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii iiii</td></tr></table>
                   <table style="width:100%"><tr><td style="text-align:center"><img src="/sponsor.png"></td></tr></table>
                   <table style="width:100%"><tr><td>First sponsor link must follow the full image box</td></tr></table>
                 </td>
                 <td style="width:46%;vertical-align:top">middle article text remains in the middle panel</td>
                 <td style="width:20%;vertical-align:top">right rankings</td>
               </tr></table></body>"#,
            100,
            &images,
        );
        let image_row = out
            .rows
            .iter()
            .position(|row| row.items.iter().any(|item| item.image.is_some()))
            .expect("decoded sponsor image");
        let last_headline_row = out
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.items.iter().any(|item| item.text.contains("iiii")))
            .map(|(row, _)| row)
            .max()
            .expect("headline text");
        assert!(
            image_row > last_headline_row,
            "the later sponsor block must follow expanded left-cell text: headline row {last_headline_row}, image row {image_row}"
        );
        let image = out.rows[image_row]
            .items
            .iter()
            .find(|item| item.image.is_some())
            .unwrap();
        let image_end = image_row + usize::from(image.height);
        let first_link_row = out
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.items.iter().any(|item| item.text.contains("First")))
            .map(|(row, _)| row)
            .expect("following sponsor link");
        assert!(
            first_link_row >= image_end,
            "the following table must start after every painted image row: image {image_row}..{image_end}, link row {first_link_row}"
        );
        let (left, right) = image.terminal_band.expect("table-cell band");
        assert!(image.col >= left && image.col + image.width <= right);
    }

    #[test]
    fn terminal_atomic_inline_and_following_text_keep_their_characters() {
        // Steam's global action row combines an inline-block install button
        // with following sign-in/language text. Their canonical boxes do not
        // overlap, but independently-rounded terminal runs can share a cell.
        let out = lay_with_cells(
            r#"<body style="margin:0"><div style="width:241px;font:12px/24px Arial,sans-serif">
                 <a style="display:inline-block;position:relative;z-index:0;width:116px"><span style="display:block;padding-left:35px">Install Steam</span></a><a>Sign in</a> | <a>language</a>
               </div></body>"#,
            40,
            10.0,
            18.0,
        );
        let rows = out.rows.iter().map(row_text).collect::<Vec<_>>();
        let painted = rows.join("\n");
        for label in ["Install Steam", "Sign in", "language"] {
            assert!(
                painted.contains(label),
                "nested inline adaptation must preserve {label:?}: {painted:?}"
            );
        }
        assert!(
            rows.iter().any(|row| {
                row.contains("Install Steam") && row.contains("Sign in") && row.contains("language")
            }),
            "an atomic paint context and its following CSS line must adapt together: {painted:?}"
        );
    }

    #[test]
    fn terminal_preserves_a_canonically_fitting_run_across_a_real_clip() {
        // A proportional run can fit its CSS overflow clip while its terminal
        // spelling needs several fixed cells more. The clip must distinguish
        // canonical overflow from adapter expansion; truncating to one extra
        // cell produces the widespread `Security` -> `Securit` failure.
        let out = lay_with_cells(
            r#"<body style="margin:0;font:12px/20px monospace">
                 <nav style="display:flex;gap:40px">
                   <span style="display:block;width:60px;overflow:hidden;white-space:nowrap">Security</span>
                   <span style="display:block;width:132px;overflow:hidden;white-space:nowrap">Apache-2.0 License</span>
                 </nav>
               </body>"#,
            40,
            10.0,
            20.0,
        );
        let painted = out.rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
        for label in ["Security", "Apache-2.0 License"] {
            assert!(
                painted.contains(label),
                "a run that fits canonically must survive terminal clipping: {label:?} in {painted:?}"
            );
        }
    }

    #[test]
    fn flex_grow_freezes_at_max_width_and_redistributes() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1;max-width:160px">a</div><div style="flex:1">b</div>
               </div></body>"#,
            80,
        );
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 20, "a frozen at 160px; b takes the remaining 480px");
    }

    #[test]
    fn flex_shrink_floors_at_min_content() {
        // The §4.5 automatic minimum: a shrinking item can't compress below
        // its longest word (the class of bug that collapsed Steam's QR pane
        // in the old engine).
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:160px">
                <div style="width:320px">verylongword</div><div style="width:320px">x</div>
               </div></body>"#,
            80,
        );
        let (r, w) = find(&out, "verylongword");
        assert_eq!((r, w.col), (0, 0), "min-content floor keeps the word whole");
        let (_, x) = find(&out, "x");
        assert!(
            x.col > 0,
            "the neighbor starts after the proportional min-content word"
        );
    }

    #[test]
    fn flex_item_min_width_max_content_prevents_slide_shrinking() {
        // CSS Sizing 3 §3.2 + Flexbox §9.2: the hypothetical main size is
        // clamped by the USED min-width. `max-content` therefore keeps each
        // carousel slide at its 240px intrinsic width even when the scroll
        // container is narrower than all slides combined.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:320px;overflow-x:scroll">
                <div style="min-width:max-content"><div style="width:240px">one</div></div>
                <div style="min-width:max-content"><div style="width:240px">two</div></div>
                <div style="min-width:max-content"><div style="width:240px">three</div></div>
               </div></body>"#,
            80,
        );
        let (_, one) = find(&out, "one");
        let (_, two) = find(&out, "two");
        let (_, three) = find(&out, "three");
        assert_eq!(one.col, 0);
        assert_eq!(two.col, 30, "the second 240px slide must not shrink");
        assert_eq!(three.col, 60, "overflow retains the third full-width slide");
        assert_eq!(out.carousels.len(), 1, "overflow creates one scroll region");
    }

    #[test]
    fn flex_basis_zero_non_growing_item_keeps_content_minimum() {
        // `flex: 0 1 0px`: base 0, but the hypothetical main size clamps to
        // the §4.5 content minimum — the item shows its content.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:0 1 0px">QRCODE</div><div style="flex:1">rest</div>
               </div></body>"#,
            80,
        );
        let (_, q) = find(&out, "QRCODE");
        assert_eq!(q.col, 0);
        assert_eq!(display_width(&q.text), 6, "not collapsed to zero");
        let (_, rest) = find(&out, "rest");
        assert!(
            rest.col >= display_width(&q.text) as u16,
            "flexible neighbor starts after it"
        );
    }

    #[test]
    fn justify_content_center_and_space_between() {
        let out = lay(
            r#"<body style="margin:0">
               <div style="display:flex;justify-content:center"><div style="width:160px">mid</div></div>
               <div style="display:flex;justify-content:space-between">
                 <div style="width:80px">l</div><div style="width:80px">r</div>
               </div></body>"#,
            80,
        );
        let (_, mid) = find(&out, "mid");
        assert_eq!(mid.col, 30, "(640−160)/2 = 240px");
        let (_, l) = find(&out, "l");
        let (_, r) = find(&out, "r");
        assert_eq!(l.col, 0);
        assert_eq!(r.col, 70, "pushed to the far edge");
    }

    #[test]
    fn align_items_center_offsets_shorter_item() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;align-items:center">
                <div style="width:80px">a<br>b<br>c</div><div style="width:80px">mid</div>
               </div></body>"#,
            80,
        );
        let (r, _) = find(&out, "mid");
        assert_eq!(r, 1, "one-line item centers against the 3-line one");
    }

    #[test]
    fn row_flex_hypothetical_cross_size_honors_item_max_height() {
        // Flexbox §9.4 obtains the hypothetical cross size through ordinary
        // block layout, including CSS Sizing 3's max-height constraint.
        let layout = lay_graphical(
            r#"<body style="margin:0"><div id="row" style="display:flex;width:200px"><div id="item" style="max-height:100px;overflow:hidden"><div style="height:300px"></div></div></div><div id="after" style="height:10px"></div></body>"#,
            320.0,
            &HashMap::new(),
        );
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="row" style="display:flex;width:200px"><div id="item" style="max-height:100px;overflow:hidden"><div style="height:300px"></div></div></div><div id="after" style="height:10px"></div></body>"#,
        );
        assert_eq!(layout.boxes[&node_by_id(&dom, "item")].height, 100.0);
        assert_eq!(layout.boxes[&node_by_id(&dom, "row")].height, 100.0);
        assert_eq!(layout.boxes[&node_by_id(&dom, "after")].top, 100.0);
    }

    #[test]
    fn auto_height_column_uses_max_clamped_item_contributions() {
        // Flexbox §§9.2 and 9.4: a max-height on an item constrains both that
        // item and the intrinsic automatic main size of its column container.
        let html = r#"<body style="margin:0"><div id="col" style="display:flex;flex-direction:column"><div id="bounded" style="max-height:100px;overflow:auto"><div style="height:300px"></div></div><div style="height:20px"></div></div><div id="after" style="height:10px"></div></body>"#;
        let dom = Dom::parse_document(html);
        let layout = lay_graphical(html, 320.0, &HashMap::new());
        assert_eq!(layout.boxes[&node_by_id(&dom, "bounded")].height, 100.0);
        assert_eq!(layout.boxes[&node_by_id(&dom, "col")].height, 120.0);
        assert_eq!(layout.boxes[&node_by_id(&dom, "after")].top, 120.0);
    }

    #[test]
    fn overflowing_flex_item_center_alignment_remains_unsafe_by_default() {
        // CSS Box Alignment 3 §4.4.1.3: unspecified overflow alignment on a
        // flex item is unsafe, so negative free space remains equally split.
        let html = r#"<body style="margin:0"><div id="row" style="display:flex;align-items:center;width:100px;height:100px"><div id="item" style="width:20px;height:200px"></div></div></body>"#;
        let dom = Dom::parse_document(html);
        let layout = lay_graphical(html, 320.0, &HashMap::new());
        assert_eq!(layout.boxes[&node_by_id(&dom, "item")].top, -50.0);
    }

    #[test]
    fn flex_baseline_alignment_uses_typographic_baselines() {
        let layout = lay_graphical(
            r#"<body style="margin:0"><div style="display:flex;align-items:baseline"><div style="font-size:12px">small</div><div style="font-size:28px;font-style:italic">BIG</div></div></body>"#,
            320.0,
            &HashMap::new(),
        );
        assert_eq!(layout.paint.lines.len(), 2);
        let a = layout.paint.lines[0].baseline;
        let b = layout.paint.lines[1].baseline;
        assert!((a - b).abs() < 0.01, "flex baselines differ: {a} vs {b}");
    }

    #[test]
    fn main_axis_auto_margin_pushes_to_the_end() {
        // The nav idiom: `margin-left:auto` absorbs the free space (§9.5 —
        // auto margins eat it BEFORE justify-content sees any).
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div>logo</div><div style="margin-left:auto">login</div>
               </div></body>"#,
            80,
        );
        let (_, login) = find(&out, "login");
        assert_eq!(login.col, 75, "80 − 5 cells");
    }

    #[test]
    fn order_reorders_items() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="order:2;width:80px">second</div><div style="order:1;width:80px">first</div>
               </div></body>"#,
            80,
        );
        let (_, f) = find(&out, "first");
        let (_, s) = find(&out, "second");
        assert_eq!(f.col, 0);
        assert_eq!(s.col, 10);
    }

    #[test]
    fn row_reverse_mirrors_main_axis() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:row-reverse">
                <div style="width:80px">one</div><div style="width:80px">two</div>
               </div></body>"#,
            80,
        );
        let (_, one) = find(&out, "one");
        assert_eq!(one.col, 70, "first item lands at the right edge");
        let (_, two) = find(&out, "two");
        assert_eq!(two.col, 60);
    }

    #[test]
    fn flex_wrap_breaks_lines_and_honors_gaps() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-wrap:wrap;gap:16px">
                <div style="width:240px">a</div><div style="width:240px">b</div><div style="width:240px">c</div>
               </div></body>"#,
            80,
        );
        let (ra, a) = find(&out, "a");
        let (rb, b) = find(&out, "b");
        let (rc, c) = find(&out, "c");
        assert_eq!((ra, a.col), (0, 0));
        assert_eq!((rb, b.col), (0, 32), "240px + 16px gap = 32 cells");
        assert_eq!((rc, c.col), (2, 0), "wrapped; 16px row-gap = 1 blank row");
    }

    #[test]
    fn flex_calc_basis_wraps_two_items_per_line() {
        // Flexbox §§7.1/9.3: each `calc(50% - 5px)` basis plus the 10px
        // gap fits exactly twice in a 220px line. This is Steam's 2x2 hero
        // screenshot grid; losing the calc basis turns it into four tiny
        // equally-grown items on one line.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div style="display:flex;flex-wrap:wrap;gap:10px;width:220px">
               <div id=a style="flex:1 1 calc(50% - 5px);height:20px"></div>
               <div id=b style="flex:1 1 calc(50% - 5px);height:20px"></div>
               <div id=c style="flex:1 1 calc(50% - 5px);height:20px"></div>
               <div id=d style="flex:1 1 calc(50% - 5px);height:20px"></div>
               </div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(400.0, 300.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let box_of = |id| layout.boxes[&dom.get_by_id(id).unwrap()];
        let (a, b, c, d) = (box_of("a"), box_of("b"), box_of("c"), box_of("d"));
        assert!((a.width - 105.0).abs() < 0.1, "{a:?}");
        assert!((b.left - 115.0).abs() < 0.1, "{b:?}");
        assert!((c.top - 30.0).abs() < 0.1, "{c:?}");
        assert!((d.left - 115.0).abs() < 0.1, "{d:?}");
    }

    #[test]
    fn column_with_definite_height_grows_items() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column;height:320px">
                <div style="flex:1">top</div><div style="flex:1">bottom</div>
               </div></body>"#,
            80,
        );
        let (rt, _) = find(&out, "top");
        let (rb, _) = find(&out, "bottom");
        assert_eq!(rt, 0);
        assert_eq!(rb, 10, "two 160px halves of the 320px column");
    }

    #[test]
    fn column_align_items_center_centers_fixed_width_item() {
        // The Steam-login-card shape: a bounded card centered in a column.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column;align-items:center">
                <div style="width:160px">card</div>
               </div></body>"#,
            80,
        );
        let (_, card) = find(&out, "card");
        assert_eq!(card.col, 30, "(640−160)/2 = 240px = col 30");
    }

    #[test]
    fn column_stretch_fills_and_pct_child_resolves() {
        // Stretch (the default) gives the item the full cross size; a
        // percentage child resolves against the item's USED width — the
        // grown-flex-base percentage class of bug.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1"><div style="width:50%">aaaa bbbb cccc</div></div>
                <div style="flex:1">right</div>
               </div></body>"#,
            80,
        );
        // Item = 320px; child 50% = 160px = 20 cells: the text wraps there.
        let (r1, t) = find(&out, "aaaa bbbb cccc");
        assert_eq!((r1, t.col), (0, 0));
        assert!(display_width(&t.text) <= 20, "wrapped at the child's 160px");
        let (_, right) = find(&out, "right");
        assert_eq!(right.col, 40, "sibling at the flexed 320px boundary");
    }

    #[test]
    fn overflow_hidden_zeroes_the_automatic_minimum() {
        // §4.5: a scroll container's automatic minimum is zero — the
        // standards answer to the `min-width:0` hack.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:160px">
                <div style="flex:1;overflow:hidden">unshrinkablelongword</div>
                <div style="flex:1">x</div>
               </div></body>"#,
            80,
        );
        let (_, x) = find(&out, "x");
        assert_eq!(x.col, 10, "equal halves — no min-content floor applies");
    }

    #[test]
    fn overflow_clip_keeps_the_content_based_automatic_minimum() {
        // Flexbox §4.5 keys the automatic minimum on CSS Overflow's
        // scrollable/non-scrollable values. `clip` is non-scrollable, unlike
        // `hidden`, so the long word retains its min-content floor.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:160px">
                <div style="flex:1;overflow:clip">unshrinkablelongword</div>
                <div style="flex:1">x</div>
               </div></body>"#,
            80,
        );
        let (_, x) = find(&out, "x");
        assert!(
            x.col > 10,
            "clip preserves the first item's min-content floor: col {}",
            x.col
        );
    }

    #[test]
    fn nested_flex_tower_lays_out() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column">
                <div style="display:flex"><div style="width:80px">a1</div><div style="width:80px">a2</div></div>
                <div style="display:flex"><div style="width:80px">b1</div><div style="width:80px">b2</div></div>
               </div></body>"#,
            80,
        );
        let (r, a2) = find(&out, "a2");
        assert_eq!((r, a2.col), (0, 10));
        let (r2, b2) = find(&out, "b2");
        assert_eq!((r2, b2.col), (1, 10));
    }

    #[test]
    fn anonymous_text_becomes_an_item() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex"><div style="width:80px">a</div>loose<div style="width:80px">b</div></div></body>"#,
            80,
        );
        let (_, loose) = find(&out, "loose");
        assert_eq!(loose.col, 10, "text run wraps into an anonymous item");
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 15, "after the 5-cell anonymous item");
    }

    #[test]
    fn templateless_grid_stacks_one_column() {
        // A grid with no template is a single implicit column, one row per
        // item — exactly what a browser renders (the old engine's
        // shelf-pack fallback is gone; real templates now do the packing).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid">
                <div style="width:240px">t1</div><div>t2</div><div>t3</div>
               </div></body>"#,
            80,
        );
        let (r1, t1) = find(&out, "t1");
        let (r2, _) = find(&out, "t2");
        let (r3, _) = find(&out, "t3");
        assert_eq!((r1, t1.col), (0, 0));
        assert_eq!(r2, 1);
        assert_eq!(r3, 2);
    }

    #[test]
    fn flexed_image_item_scales_through_its_ratio() {
        // A replaced flex item flexed wider keeps its aspect through the
        // §9.4 replaced hypothetical cross size.
        // Natural 20×5 cells = 160×80px (2:1).
        let mut images = HashMap::new();
        images.insert("http://e.com/i.png".to_string(), (160u32, 80u32));
        let out = lay_images(
            r#"<body style="margin:0"><div style="display:flex"><img src="i.png" style="flex:1"></div></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(
            (img.width, img.height),
            (40, 10),
            "320px wide → 160px tall (2:1)"
        );
    }

    #[test]
    fn cyclic_percentage_max_width_compresses_images_in_flex_items() {
        // css-sizing-3 §5.2.1: a cyclic percentage in a replaced element's
        // max-width resolves against zero for its min-content contribution.
        // Thus `max-width:100%` makes these natural 616px images compressible;
        // the flex items' automatic minimums do not pin them at 616px.
        let images = img_sizes(&[("http://e.com/tile.png", 77, 22)]);
        let out = lay_images(
            r#"<body style="margin:0"><div style="display:flex;width:1520px;column-gap:16px">
                <a style="display:block;flex:1 1 0"><div><img src="tile.png" style="display:block;max-width:100%"></div></a>
                <a style="display:block;flex:1 1 0"><div><img src="tile.png" style="display:block;max-width:100%"></div></a>
                <a style="display:block;flex:1 1 0"><div><img src="tile.png" style="display:block;max-width:100%"></div></a>
               </div></body>"#,
            240,
            &images,
        );
        let boxes: Vec<(u16, u16)> = out
            .rows
            .iter()
            .flat_map(|row| &row.items)
            .filter(|item| item.kind == ItemKind::Image)
            .map(|item| (item.col, item.width))
            .collect();
        assert_eq!(
            boxes,
            vec![(0, 62), (64, 62), (128, 62)],
            "three flex:1 items share 1520px minus two 16px gaps equally"
        );
    }

    // ---- the P3 gate: grid (real tracks + placement on real widths) ----

    #[test]
    fn github_layout_shape_lays_side_by_side() {
        // The GitHub `Layout` gate: an auto sidebar beside a flexible
        // minmax(0, calc(100% − 296px)) main, placed by line numbers. The
        // §11.8 stretch hands the auto track the leftover — the sidebar
        // comes out exactly 296px, the design's intent.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:auto minmax(0, calc(100% - 296px))">
                <div style="grid-column:1">nav</div>
                <div style="grid-column:2">main content</div>
               </div></body>"#,
            80,
        );
        let (_, nav) = find(&out, "nav");
        assert_eq!(nav.col, 0);
        let (r, main) = find(&out, "main content");
        assert_eq!((r, main.col), (0, 37), "296px = col 37");
    }

    #[test]
    fn archive_org_minmax_tile_grid() {
        // repeat(auto-fill, minmax(16rem, 1fr)) at 640px: two 256px minimums
        // fit; fr grows each to 320px.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:repeat(auto-fill, minmax(16rem, 1fr))">
                <div>tile one</div><div>tile two</div><div>tile three</div>
               </div></body>"#,
            80,
        );
        let (r1, t1) = find(&out, "tile one");
        let (r2, t2) = find(&out, "tile two");
        let (r3, t3) = find(&out, "tile three");
        assert_eq!((r1, t1.col), (0, 0));
        assert_eq!((r2, t2.col), (0, 40), "second 320px track");
        assert_eq!((r3, t3.col), (1, 0), "wraps to the second grid row");
    }

    #[test]
    fn grid_used_track_sizes_are_captured_for_getcomputedstyle() {
        // The CSSOM resolved value of `grid-template-columns` is the USED track
        // list in px (a grid-measuring library counts
        // `getComputedStyle(el).gridTemplateColumns.split(' ')`), NOT the
        // declared `repeat(auto-fill, …)`. `repeat(auto-fill, minmax(80px, 1fr))`
        // at 640px = 8 tracks of 80px (the JavaScript prelude serializes them
        // to "80px 80px …").
        // The grid is EMPTY on purpose: a virtualized feed reads its resolved
        // columns BEFORE it has any cells (archive.org's infinite-scroller), and
        // a browser still sizes an empty grid's template.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="g" style="display:grid;grid-template-columns:repeat(auto-fill, minmax(80px, 1fr))"></div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (_, tracks) = measure_boxes_terminal(
            &dom,
            &base,
            (80, 24),
            &[],
            &HashMap::new(),
            (8, 16),
            &HashMap::new(),
        );
        let g = dom.get_by_id("g").unwrap();
        let (cols, _rows) = tracks.get(&g).expect("the grid's used tracks are recorded");
        assert_eq!(cols.len(), 8, "640px / 80px = 8 auto-fill columns");
        assert!(
            cols.iter().all(|&w| (w - 80.0).abs() < 0.5),
            "each 1fr track resolves to 80px: {cols:?}"
        );
    }

    #[test]
    fn danbooru_auto_fill_gap_grid() {
        // repeat(auto-fill, minmax(80px, 1fr)) with an 8px gap at 640px:
        // ⌊648/88⌋ = 7 columns; the six 8px gaps leave 592px for the fr
        // expansion (84.57px each).
        let html = r#"<body style="margin:0"><div style="display:grid;gap:8px;grid-template-columns:repeat(auto-fill, minmax(80px, 1fr))">
            <div>p1</div><div>p2</div><div>p3</div><div>p4</div><div>p5</div><div>p6</div><div>p7</div><div>p8</div>
           </div></body>"#;
        let out = lay(html, 80);
        let rows0: Vec<u16> = out.rows[0].items.iter().map(|i| i.col).collect();
        assert_eq!(rows0.len(), 7, "seven thumbnails on the first grid row");
        let (r8, p8) = find(&out, "p8");
        // The 8px row-gap lands the second grid row at y = 24px, which
        // edge-snaps to doc row 2 (a full blank gap row).
        assert_eq!((r8, p8.col), (2, 0), "eighth wraps");
    }

    #[test]
    fn fixed_fr_tracks_and_gaps_position_exactly() {
        // 96px 1fr 2fr with 16px gaps at 640px: 608px of track space,
        // 512px flexed 1:2 → columns at 0 / 112px / 298.67px.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:96px 1fr 2fr;gap:16px">
                <div>a</div><div>b</div><div>c</div>
               </div></body>"#,
            80,
        );
        let (_, a) = find(&out, "a");
        let (_, b) = find(&out, "b");
        let (_, c) = find(&out, "c");
        assert_eq!((a.col, b.col, c.col), (0, 14, 37));
    }

    #[test]
    fn auto_fit_collapses_empty_tracks_for_fr() {
        // The responsive-card idiom: auto-fit + minmax(96px, 1fr) with two
        // items at 640px — four empty repetitions collapse and the two live
        // tracks split the full width.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:repeat(auto-fit, minmax(96px, 1fr))">
                <div>left</div><div>right</div>
               </div></body>"#,
            80,
        );
        let (_, l) = find(&out, "left");
        let (_, r) = find(&out, "right");
        assert_eq!(l.col, 0);
        assert_eq!(r.col, 40, "two 320px halves, not six 96px slots");
    }

    #[test]
    fn negative_lines_and_row_placement() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px 80px 80px">
                <div style="grid-column:-2">endish</div>
                <div style="grid-column:1;grid-row:2">below</div>
               </div></body>"#,
            80,
        );
        let (r1, e) = find(&out, "endish");
        assert_eq!((r1, e.col), (0, 20), "line -2 of 4 = third track");
        let (r2, b) = find(&out, "below");
        assert_eq!((r2, b.col), (1, 0));
    }

    #[test]
    fn template_areas_place_named_items() {
        let out = lay(
            r#"<body style="margin:0"><div style='display:grid;grid-template-columns:160px 1fr;grid-template-areas:"head head" "nav main"'>
                <div style="grid-area:main">the main pane</div>
                <div style="grid-area:head">header</div>
                <div style="grid-area:nav">nav</div>
               </div></body>"#,
            80,
        );
        let (rh, h) = find(&out, "header");
        assert_eq!((rh, h.col), (0, 0));
        let (rn, n) = find(&out, "nav");
        assert_eq!((rn, n.col), (1, 0));
        let (rm, m) = find(&out, "the main pane");
        assert_eq!(
            (rm, m.col),
            (1, 20),
            "main starts after the 160px nav track"
        );
    }

    #[test]
    fn auto_flow_column_fills_down_then_across() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-rows:16px 16px;grid-auto-flow:column;grid-auto-columns:80px">
                <div>one</div><div>two</div><div>three</div><div>four</div>
               </div></body>"#,
            80,
        );
        let (r1, c1) = find(&out, "one");
        let (r2, c2) = find(&out, "two");
        let (r3, c3) = find(&out, "three");
        let (r4, c4) = find(&out, "four");
        assert_eq!((r1, c1.col), (0, 0));
        assert_eq!((r2, c2.col), (1, 0));
        assert_eq!((r3, c3.col), (0, 10), "third fills the next column");
        assert_eq!((r4, c4.col), (1, 10));
    }

    #[test]
    fn dense_packing_backfills_holes() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px 80px;grid-auto-flow:row dense">
                <div style="grid-column:2">pinned</div>
                <div style="grid-column:span 2">wide</div>
                <div>filler</div>
               </div></body>"#,
            80,
        );
        let (rf, f) = find(&out, "filler");
        assert_eq!(
            (rf, f.col),
            (0, 0),
            "dense fills the hole beside the pinned item"
        );
    }

    #[test]
    fn definite_row_tracks_reserve_height() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px;grid-template-rows:64px 32px">
                <div>a</div><div>b</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        let (raf, _) = find(&out, "after");
        assert_eq!(ra, 0);
        assert_eq!(rb, 4, "64px first row = 4 rows");
        assert_eq!(raf, 6, "container = 96px = 6 rows");
    }

    #[test]
    fn justify_and_align_self_position_within_areas() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:320px">
                <div style="width:80px;justify-self:center">mid</div>
                <div style="width:80px;justify-self:end">end</div>
               </div></body>"#,
            80,
        );
        let (_, mid) = find(&out, "mid");
        assert_eq!(mid.col, 15, "(320−80)/2 = 120px");
        let (_, end) = find(&out, "end");
        assert_eq!(end.col, 30, "320−80 = 240px");
    }

    #[test]
    fn fit_content_track_caps_at_argument() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:fit-content(160px) 80px">
                <div>a very long run of grid content here</div><div>side</div>
               </div></body>"#,
            80,
        );
        let (_, side) = find(&out, "side");
        assert_eq!(side.col, 20, "first track capped at 160px");
    }

    #[test]
    fn spanning_item_grows_intrinsic_columns() {
        // A span-2 item wider than both auto tracks' single-track content
        // forces the pair to accommodate it (§11.5 spanning distribution).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:auto auto;justify-content:start">
                <div>ab</div><div>cd</div>
                <div style="grid-column:1 / span 2">wwwwwwwwwwwwwwwwwwww</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (_, w) = find(&out, "wwwwwwwwwwwwwwwwwwww");
        assert_eq!(display_width(&w.text), 20, "spanner fits unwrapped");
    }

    #[test]
    fn grid_items_stretch_to_row_height() {
        // Default align-items stretch: the shorter item's box fills the
        // row (visible via the following content's position).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:160px 160px">
                <div>tall<br>tall<br>tall</div><div>short</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (rs, s) = find(&out, "short");
        assert_eq!((rs, s.col), (0, 20));
        let (raf, _) = find(&out, "after");
        assert_eq!(raf, 3, "row height = the tall item");
    }

    #[test]
    fn stretched_grid_item_relays_definite_height_to_descendants() {
        // CSS Grid §11.6 and Box Alignment §6.2: after stretch changes an
        // auto-sized item's used block size, percentage descendants resolve
        // against that definite size. Archive-style result cards rely on the
        // inner 100%-height surface filling every equal-height grid item.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><main style="display:grid;grid-template-columns:200px 200px">
               <article id=tall><div id=tall-fill style="height:100%">a<br>b<br>c</div></article>
               <article id=short><div id=short-fill style="height:100%">x</div></article>
               </main></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 300.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let geometry = |id| *layout.boxes.get(&dom.get_by_id(id).unwrap()).unwrap();
        let tall = geometry("tall");
        let short = geometry("short");
        let tall_fill = geometry("tall-fill");
        let short_fill = geometry("short-fill");
        assert!(
            (tall.height - short.height).abs() < 0.1,
            "{tall:?} {short:?}"
        );
        assert!(
            (tall_fill.height - short_fill.height).abs() < 0.1,
            "stretched descendants diverged: {tall_fill:?} {short_fill:?}"
        );
        assert!((short_fill.height - short.height).abs() < 0.1);
    }
    // ---- the P4 gate: positioned + stacking + paint order + transforms ----
    // (Stacked cards paint with the top card visible, arrows land where
    // written, fixed rails pin, modals cover — Appendix E + cell compositing.)

    #[test]
    fn relative_offset_shifts_box_without_affecting_flow() {
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:relative;left:16px;top:32px">moved</div>
               <div>after</div></body>"#,
            80,
        );
        let (rm, m) = find(&out, "moved");
        assert_eq!(
            (rm, m.col),
            (2, 2),
            "offset by (16px, 32px) = (2 cols, 2 rows)"
        );
        let (ra, a) = find(&out, "after");
        assert_eq!(
            (ra, a.col),
            (1, 0),
            "§9.4.3: the following box is placed as if the offset never happened"
        );
    }

    #[test]
    fn relative_negative_top_paints_over_earlier_content() {
        // §9.4.3 allows overlap; the positioned box paints in Appendix E
        // step 8, over the in-flow text — later cells win at the overlap.
        let out = lay(
            r#"<body style="margin:0"><div>AAAA</div><div style="position:relative;top:-16px">BB</div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "BBAA");
    }

    #[test]
    fn abspos_insets_position_against_positioned_ancestor() {
        // §10.1: the containing block is the positioned ancestor's padding
        // box; §9.3.2 insets offset from its edges.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:64px">
                <div style="position:absolute;left:16px;top:32px">X</div>
               </div></body>"#,
            80,
        );
        let (r, x) = find(&out, "X");
        assert_eq!((r, x.col), (2, 2));
    }

    #[test]
    fn abspos_right_bottom_anchor_and_shrink_to_fit() {
        // right/bottom anchoring solves left/top through the §10.3.7/§10.6.4
        // constraint equations — which needs the real shrink-to-fit width
        // (3 cells for "END") to come out at col 37.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:64px">
                <div style="position:absolute;right:0;bottom:0">END</div>
               </div></body>"#,
            80,
        );
        let (r, e) = find(&out, "END");
        assert_eq!(r, 3);
        assert!(
            e.col >= 35,
            "right anchoring accounts for proportional text width"
        );
    }

    #[test]
    fn abspos_all_auto_lands_at_static_position() {
        // §10.3.7/§10.6.4 rule sets with everything auto: the box sits where
        // it would have flowed; being positioned it paints OVER the sibling
        // that flows into the same place.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">one</p><div style="position:absolute">abs</div><p style="margin:0">two</p></body>"#,
            80,
        );
        let (r, a) = find(&out, "abs");
        assert_eq!((r, a.col), (1, 0), "static position: after the first <p>");
        assert_eq!(
            row_text(&out.rows[1]),
            "abs",
            "the covered sibling's cells belong to the later-painted abspos box"
        );
    }

    #[test]
    fn abspos_without_positioned_ancestor_uses_the_icb() {
        let out = lay(
            r#"<body style="margin:8px"><div style="position:absolute;left:0;top:0">corner</div><p>content</p></body>"#,
            80,
        );
        let (r, c) = find(&out, "corner");
        assert_eq!(
            (r, c.col),
            (0, 0),
            "§10.1: no positioned ancestor → the initial containing block"
        );
    }

    #[test]
    fn abspos_left_and_right_solve_the_width() {
        // §10.3.7 rule 5: width = cb − left − right; proven through the
        // right-aligned line landing at the solved content edge.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:32px">
                <div style="position:absolute;left:16px;right:16px;text-align:right">end</div>
               </div></body>"#,
            80,
        );
        let (_, e) = find(&out, "end");
        assert!(
            e.col >= 34,
            "right-aligned inside the solved 288px content box"
        );
    }

    #[test]
    fn z_index_orders_overlapping_siblings_not_tree_order() {
        // The z:5 box is FIRST in the document but paints LAST (§9.9).
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:16px">
                <div style="position:absolute;left:0;top:0;z-index:5">BB</div>
                <div style="position:absolute;left:0;top:0;z-index:2">AAAA</div>
               </div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "BBAA");
    }

    #[test]
    fn empty_overlay_anchor_exposes_its_css_box_without_painting_a_marker() {
        // Reddit's community cards use exactly this shape: an empty anchor is
        // stretched over visible sibling content. The anchor has no text item,
        // but its generated box still participates in CSS point hit testing.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:160px;height:32px">
               <a href="/r/Fish/" style="position:absolute;inset:0;z-index:1"></a>
               <span>r/Fish</span>
               </div>
               <a href="/r/Fish/">same destination elsewhere</a></body>"#,
            80,
        );
        let (row_idx, title) = find(&out, "r/Fish");
        assert_eq!((row_idx, title.col), (0, 0));
        let surfaces: Vec<_> = out.rows.iter().flat_map(|row| &row.hits).collect();
        assert_eq!(
            surfaces.len(),
            1,
            "the empty anchor contributes one box even when a different painted link has the same URL"
        );
        let surface = surfaces[0];
        assert_eq!(
            (surface.col, surface.width, surface.height),
            (0, 20, 2),
            "the hit surface is the anchor's 160×32px border box"
        );
        let target = &out.rows[0].items[surface.item];
        assert_eq!(target.kind, ItemKind::HitRegion);
        assert!(target.text.is_empty(), "no synthetic glyph is painted");
        assert!(
            crate::layout2::visual_columns(&out.rows[0], &[], 0)
                .iter()
                .all(|(i, ..)| *i != surface.item),
            "the geometry-only item never enters terminal rendering"
        );
        assert!(matches!(
            target.link.as_ref(),
            Some(crate::doc::Link::Http(url)) if url.path() == "/r/Fish/"
        ));
    }

    #[test]
    fn an_empty_div_does_not_become_an_activation_target() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:160px;height:32px">
               <div style="position:absolute;inset:0;z-index:1"></div>
               <span>plain card</span>
               </div></body>"#,
            80,
        );
        assert!(
            out.rows.iter().all(|row| row.hits.is_empty()),
            "geometry alone is not interactive semantics"
        );
    }

    #[test]
    fn box_hit_testing_respects_pointer_events_visibility_inert_and_opacity() {
        let cases = [
            ("pointer-events:none", "", false),
            ("visibility:hidden", "", false),
            ("interactivity:inert", "", false),
            ("opacity:0", "", true),
            ("", "inert", false),
        ];
        for (style, attr, expected) in cases {
            let out = lay(
                &format!(
                    r#"<body style="margin:0"><div style="position:relative;width:80px;height:16px">
                       <a href="/go" {attr} style="position:absolute;inset:0;{style}"></a>
                       <span>card</span>
                       </div></body>"#
                ),
                40,
            );
            assert_eq!(
                out.rows.iter().any(|row| !row.hits.is_empty()),
                expected,
                "style={style:?}, attr={attr:?}"
            );
        }
    }

    #[test]
    fn overlapping_empty_anchors_keep_css_paint_order() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:80px;height:16px">
               <a href="/top" style="position:absolute;inset:0;z-index:2"></a>
               <a href="/bottom" style="position:absolute;inset:0;z-index:1"></a>
               </div></body>"#,
            40,
        );
        let row = &out.rows[0];
        assert_eq!(row.hits.len(), 2);
        let top = row.hits.iter().max_by_key(|hit| hit.order).unwrap();
        assert!(matches!(
            row.items[top.item].link.as_ref(),
            Some(crate::doc::Link::Http(url)) if url.path() == "/top"
        ));
    }

    #[test]
    fn negative_z_paints_under_in_flow_content() {
        // Appendix E step 3 (negative-z stacking contexts) precedes the
        // in-flow content steps — the page text wins the contested cells.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <div style="position:absolute;left:0;top:0;z-index:-1">XXXXXX</div>text</div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "textXX");
    }

    #[test]
    fn modal_background_covers_the_page_inside_its_rect() {
        // A positioned box with a background is an OPAQUE FILL: the page
        // cells under its rect are erased, its own content paints on top.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">underneath content here</p>
               <div style="position:absolute;left:0;top:0;width:80px;height:16px;background:#000">MODAL</div></body>"#,
            80,
        );
        let (r, m) = find(&out, "MODAL");
        assert_eq!((r, m.col), (0, 0));
        let survivors: Vec<&Item> = out.rows[0]
            .items
            .iter()
            .filter(|it| it.text != "MODAL")
            .collect();
        assert_eq!(survivors.len(), 1, "one clipped remainder of the page text");
        assert_eq!(
            (survivors[0].col, survivors[0].text.as_str()),
            (10, " content here"),
            "the page text survives only past the modal's 80px (10-cell) rect"
        );
    }

    #[test]
    fn card_stack_paints_top_card_and_arrows_where_written() {
        // The Twitch-hero shape: stacked cards with backgrounds; only the
        // top card's content shows, the z:3 arrows land at the written
        // insets over it.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:48px">
                <div style="position:absolute;inset:0;background:#111">bottom card text</div>
                <div style="position:absolute;inset:0;background:#222">TOP CARD</div>
                <div style="position:absolute;left:0;top:16px;z-index:3">&lsaquo;</div>
                <div style="position:absolute;right:0;top:16px;z-index:3">&rsaquo;</div>
               </div></body>"#,
            80,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("bottom")),
            "the lower card is fully covered by the top card's opaque fill"
        );
        let (rt, t) = find(&out, "TOP CARD");
        assert_eq!((rt, t.col), (0, 0));
        let (rl, l) = find(&out, "‹");
        assert_eq!((rl, l.col), (1, 0));
        let (rr, r) = find(&out, "›");
        assert_eq!((rr, r.col), (1, 39), "right:0 → 320−8px = col 39");
    }

    #[test]
    fn fixed_rails_pin_into_the_fixed_layer() {
        // The Mastodon shape: two fixed side rails leave the document flow
        // entirely and pin at their viewport positions.
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:fixed;left:0;top:0;width:80px">LEFT RAIL</div>
               <div style="position:fixed;right:0;top:0;width:80px">RIGHT</div>
               <p style="margin:0">main content</p></body>"#,
            80,
        );
        let (r, m) = find(&out, "main content");
        assert_eq!((r, m.col), (0, 0), "fixed boxes take no flow space");
        assert_eq!(out.fixed.len(), 2);
        let left = &out.fixed[0];
        assert_eq!((left.col, left.row), (0, 0));
        assert!(left.rows[0].items.iter().any(|i| i.text == "LEFT RAIL"));
        let right = &out.fixed[1];
        assert_eq!(
            (right.col, right.row),
            (70, 0),
            "right:0 at 640px viewport → 560px = col 70"
        );
        assert!(right.rows[0].items.iter().any(|i| i.text == "RIGHT"));
    }

    #[test]
    fn viewport_fixed_backdrop_is_marked_below_document() {
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:fixed;top:0;right:0;bottom:0;left:0;background:#123">
                 <img src="backdrop.png" alt="backdrop">
               </div>
               <div style="position:relative">main content</div></body>"#,
            80,
        );
        assert_eq!(out.fixed.len(), 1);
        assert!(
            out.fixed[0].under_document,
            "a non-interactive viewport backdrop remains pinned but cannot cover later content"
        );
    }

    #[test]
    fn fixed_zero_context_before_positioned_sibling_is_composited_under_document() {
        // The fixed shell is still viewport-pinned, but its z:0 stacking
        // context precedes the later z:0 dialog in Appendix E step 8. The
        // terminal keeps the shell in a pinned buffer while compositing that
        // buffer before the document rows, so the dialog remains visible and
        // hit-testable.
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:fixed;inset:0;z-index:0;background:#111">
                 <a href="/inside">page</a>
               </div>
               <div style="position:relative;z-index:0;width:160px;height:80px;background:#eee">
                 <button>Allow</button>
               </div></body>"#,
            80,
        );
        assert_eq!(out.fixed.len(), 1);
        assert!(
            out.fixed[0].under_document,
            "a fixed z:0 shell before a later z:0 sibling follows tree order"
        );
        let text = out
            .rows
            .iter()
            .flat_map(|row| row.items.iter())
            .map(|item| item.text.as_str())
            .collect::<String>();
        assert!(
            text.contains("Allow"),
            "later dialog remains in document rows"
        );
    }

    #[test]
    fn fixed_bottom_anchors_to_the_viewport() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:fixed;left:0;bottom:0">status bar</div></body>"#,
            80,
        );
        assert_eq!(out.fixed.len(), 1);
        assert_eq!(
            out.fixed[0].row, 23,
            "bottom:0 at a 24-row viewport → 384−16px = row 23"
        );
    }

    #[test]
    fn fixed_inside_transformed_ancestor_stays_in_the_document() {
        // css-transforms-1 §3: a transformed ancestor is the containing
        // block for fixed descendants — the box positions against IT and
        // scrolls with the page instead of pinning.
        let out = lay(
            r#"<body style="margin:0"><div style="transform:translateX(0px);height:32px">
                <div style="position:fixed;left:16px;top:16px">inner</div>
               </div></body>"#,
            80,
        );
        assert!(out.fixed.is_empty(), "not pinned");
        let (r, i) = find(&out, "inner");
        assert_eq!((r, i.col), (1, 2));
    }

    #[test]
    fn transform_translate_offsets_paint_not_flow() {
        let out = lay(
            r#"<body style="margin:0"><div style="transform:translate(16px, 32px)">moved</div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (rm, m) = find(&out, "moved");
        assert_eq!((rm, m.col), (2, 2));
        let (ra, _) = find(&out, "after");
        assert_eq!(ra, 1, "surrounding flow is unaffected (transforms-1 §3)");
    }

    #[test]
    fn translate_property_percentage_of_own_border_box() {
        let out = lay(
            r#"<body style="margin:0"><div style="width:160px;translate:100%">x</div></body>"#,
            80,
        );
        let (_, x) = find(&out, "x");
        assert_eq!(x.col, 20, "100% of the box's own 160px = 20 cols");
    }

    #[test]
    fn sticky_rests_at_flow_position_and_hosts_abspos() {
        // css-position-3 §3.4: sticky offsets are scroll-driven — zero at
        // the initial position — and a sticky box is positioned, so it IS a
        // containing block for abspos descendants.
        let out = lay(
            r#"<body style="margin:0"><div style="position:sticky;top:0;height:32px">header
                <div style="position:absolute;right:0;top:16px">A</div>
               </div><p style="margin:0">body text</p></body>"#,
            80,
        );
        let (rh, h) = find(&out, "header");
        assert_eq!((rh, h.col), (0, 0), "no offset at rest");
        let (ra, a) = find(&out, "A");
        assert_eq!((ra, a.col), (1, 79), "right:0 of the sticky box's 640px");
        let (rb, _) = find(&out, "body text");
        assert_eq!(rb, 2);
    }

    #[test]
    fn opacity_zero_abspos_contributes_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">cap</p><div style="opacity:0;position:absolute;left:0;top:0">ghost</div></body>"#,
            80,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("ghost")),
            "a paint-suppressed out-of-flow box emits no cells at all"
        );
        assert_eq!(out.rows.iter().filter(|r| !r.items.is_empty()).count(), 1);
    }

    #[test]
    fn visibility_hidden_abspos_keeps_ghost_geometry() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:absolute;left:0;top:16px;visibility:hidden">ghost</div><p style="margin:0">real</p></body>"#,
            80,
        );
        let (rr, real) = find(&out, "real");
        assert_eq!((rr, real.col), (0, 0));
        let (rg, g) = find(&out, "ghost");
        assert_eq!((rg, g.col), (1, 0), "visibility keeps the box");
        assert!(g.invisible, "…but paints it blank");
    }

    #[test]
    fn inline_abspos_takes_the_pen_static_position() {
        // §10.3.7: the hypothetical box of an inline-level abspos element
        // sits at the pen; painted in step 8 it covers the following text.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">before<span style="position:absolute">tip</span>rest</p></body>"#,
            80,
        );
        let (r, t) = find(&out, "tip");
        assert_eq!((r, t.col), (0, 6));
        assert_eq!(row_text(&out.rows[0]), "beforetipt");
    }

    #[test]
    fn three_row_areas_via_stylesheet_keep_their_rows() {
        // The regression that hid behind two-row templates: a named area's
        // half-open end track must NOT gain an extra spanned row (the
        // footer landed on nav's row). Stylesheet-driven, like real pages.
        let out = lay(
            r#"<html><head><style>
              .page { display: grid; grid-template-columns: 160px 1fr;
                      grid-template-areas: "head head" "nav main" "foot foot"; }
              .h { grid-area: head; } .n { grid-area: nav; }
              .m { grid-area: main; } .f { grid-area: foot; }
            </style></head><body style="margin:0"><div class="page">
              <div class="h">HEADER</div><div class="n">NAV</div>
              <div class="m">MAINAREA</div><div class="f">FOOTER</div>
            </div></body></html>"#,
            80,
        );
        let (rh, h) = find(&out, "HEADER");
        assert_eq!((rh, h.col), (0, 0));
        let (rn, n) = find(&out, "NAV");
        assert_eq!((rn, n.col), (1, 0));
        let (rm, m) = find(&out, "MAINAREA");
        assert_eq!((rm, m.col), (1, 20));
        let (rf, f) = find(&out, "FOOTER");
        assert_eq!((rf, f.col), (2, 0), "foot spans exactly its own row");
    }

    // ---- P5a: overflow clipping (CSS Overflow L3 §2/§3) ----
    // A non-`visible` overflow value clips content to the padding box. Since
    // the engine computes real used heights, a definite-height overflow box
    // simply occupies its height and the compositor drops the overflowing
    // cells — no buffer/window (that is P5b scrolling).

    #[test]
    fn sr_only_box_clips_its_label_to_nothing() {
        // The visually-hidden idiom: a sub-cell box with overflow:hidden. Its
        // clip (the padding box) rounds to under a cell, so the overflowing
        // label paints NOTHING — a browser shows a ~1px speck; we faithfully
        // render nothing rather than a stray glyph. GEOMETRIC, not a heuristic.
        let out = lay(
            r#"<body style="margin:0"><div style="width:1px;height:1px;overflow:hidden">label</div><p style="margin:0">real</p></body>"#,
            20,
        );
        assert!(absent(&out, "label"), "sr-only label clips to nothing");
        assert_eq!(
            find(&out, "real").0,
            0,
            "the sub-cell box reserves no rows either"
        );
    }

    #[test]
    fn overflow_clip_keeps_a_mostly_visible_negative_margin_text_row() {
        // The clip begins 5px into the 16px glyph row. A graphical UA shows
        // most of the line; the cell paint boundary must not discard it merely
        // because the two independently snapped edges straddle a row boundary.
        let out = lay(
            r#"<body style="margin:0"><div style="height:9px"></div><div style="overflow:hidden"><p style="margin:-5px 0 0">mostly visible</p></div></body>"#,
            40,
        );
        assert!(
            !absent(&out, "mostly visible"),
            "11/16px of the glyph row remains inside the CSS clip"
        );
    }

    #[test]
    fn overflow_hidden_clips_content_below_the_box() {
        // A 2-row (32px) overflow:hidden box holding four 1-row lines: the two
        // lines past the box are clipped, and following content is NOT
        // overlapped by them (the whole reason clipping is load-bearing here).
        let out = lay(
            r#"<body style="margin:0"><div style="height:32px;overflow:hidden;margin:0"><p style="margin:0">L1</p><p style="margin:0">L2</p><p style="margin:0">L3</p><p style="margin:0">L4</p></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert_eq!(find(&out, "L1").0, 0);
        assert_eq!(find(&out, "L2").0, 1);
        assert!(absent(&out, "L3"), "third line is clipped below the box");
        assert!(absent(&out, "L4"), "fourth line is clipped below the box");
        assert_eq!(
            find(&out, "after").0,
            2,
            "following content follows the box, not the clipped overflow"
        );
    }

    #[test]
    fn oversized_clipped_abspos_does_not_inflate_document_height() {
        // The element-resize-detector sensor idiom (Twitch's front page, many
        // React apps): a huge (100000px) position:absolute sizing probe lives
        // inside an overflow:hidden container. A browser CLIPS it, so it adds
        // NOTHING to the document's scrollable overflow (CSS Overflow L3 §3.2).
        // Without clip-aware scrollable extent the whole page became a ~6250-row
        // blank scroll below one screen of content.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">head</p><div style="position:relative;height:16px;margin:0"><div style="position:absolute;top:0;right:0;bottom:0;left:0;overflow:hidden;visibility:hidden"><div style="position:absolute;top:0;left:0;width:100000px;height:100000px"></div></div></div><p style="margin:0">foot</p></body>"#,
            40,
        );
        assert_eq!(find(&out, "head").0, 0);
        assert_eq!(
            find(&out, "foot").0,
            2,
            "footer follows the detector, not the clipped 100000px probe"
        );
        assert!(
            out.rows.len() < 10,
            "the clipped probe must not inflate the document height (got {} rows)",
            out.rows.len()
        );
    }

    #[test]
    fn overflow_visible_does_not_clip() {
        // The control: the same box with overflow:visible keeps all four lines
        // painting past its 2-row height (only `visible` doesn't clip).
        let out = lay(
            r#"<body style="margin:0"><div style="height:32px;overflow:visible;margin:0"><p style="margin:0">L1</p><p style="margin:0">L2</p><p style="margin:0">L3</p><p style="margin:0">L4</p></div></body>"#,
            20,
        );
        assert_eq!(find(&out, "L3").0, 2);
        assert_eq!(find(&out, "L4").0, 3);
    }

    #[test]
    fn overflow_hidden_truncates_a_wide_line() {
        // Horizontal clipping is resolved from canonical shaped clusters,
        // before the 64px box becomes eight terminal cells. In the default
        // proportional face only seven complete/intersecting glyph clusters
        // fit; fixed-cell character counting must not invent an eighth.
        let out = lay(
            r#"<body style="margin:0"><div style="width:64px;overflow:hidden;white-space:nowrap;margin:0">abcdefghijklmnop</div></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "abcdefg");
    }

    #[test]
    fn overflow_hidden_keeps_text_in_an_intersected_terminal_cell() {
        // CSS Overflow 3 §3.1 clips overflow:hidden content at the padding
        // box, while §5.1 permits a character to be only partially rendered at
        // a clip edge. A 59px box reaches 3px into the eighth 8px terminal
        // cell; rounding that edge down made Steam's "Palworld" become
        // "Palworl" in terminal TRust even though the graphical text fit.
        let out = lay(
            r#"<body style="margin:0"><div style="width:59px;overflow:hidden;white-space:nowrap">Palworld</div></body>"#,
            20,
        );
        assert_eq!(row_text(&out.rows[0]), "Palworld");
    }

    #[test]
    fn overflow_hidden_preserves_a_fitting_word_at_a_fractional_internal_edge() {
        // The same case inside a page: the box starts one terminal cell in,
        // and its 51px right edge quantizes to seven cells although the shaped
        // word occupies eight terminal cells. The hidden line has no vertical
        // reflow room, so the intersected edge cell must be retained.
        let out = lay(
            r#"<body style="margin:0"><div style="margin-left:8px;width:51px;font-size:12px;overflow:hidden;white-space:nowrap">Palworld</div></body>"#,
            20,
        );
        assert!(row_text(&out.rows[0]).contains("Palworld"));
    }

    #[test]
    fn absolute_child_escapes_an_in_flow_overflow_hidden_ancestor() {
        // The abspos box's containing block is the positioned <body>, NOT the
        // in-flow overflow:hidden div between them — so that div does NOT clip
        // it (CSS Overflow L3 §3: a positioned box is clipped by its CB's clip
        // chain, which the CB-aware resolve_oof walk threads). It paints far
        // below the 1-row clip box's bottom.
        let out = lay(
            r#"<body style="margin:0;position:relative;height:200px"><div style="height:16px;overflow:hidden;margin:0"><span style="position:absolute;left:0;top:48px">escapee</span></div></body>"#,
            20,
        );
        assert_eq!(
            find(&out, "escapee").0,
            3,
            "abspos escapes the in-flow clip and paints at its CB offset (top:48px = row 3)"
        );
    }

    #[test]
    fn absolute_child_is_clipped_by_its_containing_block() {
        // Here the overflow:hidden box IS the abspos containing block
        // (position:relative), so its clip DOES apply — the child positioned
        // past the box bottom is clipped away.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:16px;overflow:hidden;margin:0"><span style="position:absolute;left:0;top:48px">gone</span></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert!(
            absent(&out, "gone"),
            "abspos clipped by its own CB's overflow"
        );
        assert_eq!(find(&out, "after").0, 1, "the clip box is 1 row tall");
    }

    // ---- P5b: vertical scroll regions (CSS Overflow L3 §2/§3, CSSOM View) ----
    // A definite-height overflow-y:auto|scroll box whose content overflows is a
    // scroll container: its content goes into a separate buffer (scrollHeight),
    // the doc reserves a blank band of its clientHeight, and the renderer
    // windows the buffer over the band.

    #[test]
    fn overflow_y_auto_with_overflow_becomes_a_scroll_region() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">TOP</p><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">R1</p><p style="margin:0">R2</p><p style="margin:0">R3</p><p style="margin:0">R4</p><p style="margin:0">R5</p><p style="margin:0">R6</p></div><p style="margin:0">BOTTOM</p></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1, "one scroll region");
        let r = &out.regions[0];
        assert_eq!(r.height, 3, "clientHeight = 48px = 3 rows");
        assert!(
            r.buffer.len() >= 6,
            "six proportional line boxes survive terminal row quantization"
        );
        assert_eq!(r.voffset, 0, "scroll origin is the top (CSSOM View)");
        assert_eq!((r.start_row, r.left, r.width), (1, 0, 20), "band geometry");
        assert!(
            r.buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("R6"))),
            "the buffer holds the full scrollable content"
        );
        assert!(
            absent(&out, "R4"),
            "region content is buffered, not in main rows"
        );
        assert_eq!(find(&out, "TOP").0, 0);
        assert_eq!(
            find(&out, "BOTTOM").0,
            4,
            "content below follows the 3-row band, not the 6-row content"
        );
    }

    #[test]
    fn overflow_y_auto_that_fits_is_not_a_region() {
        let out = lay(
            r#"<body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">A</p><p style="margin:0">B</p></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert!(
            out.regions.is_empty(),
            "content fits (2 rows < 3): no region"
        );
        assert_eq!(find(&out, "A").0, 0);
        assert_eq!(find(&out, "B").0, 1);
        assert_eq!(
            find(&out, "after").0,
            3,
            "the definite-height box still reserves its 3 rows"
        );
    }

    #[test]
    fn an_overflow_auto_box_becomes_a_region_even_under_a_locked_viewport() {
        // CSS Overflow L3 §2 makes `overflow:auto|scroll` a scroll container, so
        // the box's content rides its OWN buffer, never the flat document rows —
        // that stays true here. But when the viewport is block-LOCKED
        // (`html{overflow:hidden}`, §3.1) and this is the sole content spine of
        // the shell (`<body><div overflow:auto>`), the region is the PAGE'S own
        // scroller: the terminal presents it as "the page" (main scrollbar +
        // page keys drive it), so it is flagged `principal`.
        let out = lay(
            r#"<html style="overflow:hidden"><body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">P1</p><p style="margin:0">P2</p><p style="margin:0">P3</p><p style="margin:0">P4</p><p style="margin:0">P5</p><p style="margin:0">P6</p></div></body></html>"#,
            20,
        );
        assert_eq!(
            out.regions.len(),
            1,
            "the overflow:auto box is its own bounded region regardless of the locked viewport"
        );
        let r = &out.regions[0];
        assert!(
            r.principal,
            "the sole scroller under a locked viewport is the principal (page) region"
        );
        assert!(
            r.buffer.len() >= 6,
            "all six proportional line boxes ride the region's own scrollable buffer"
        );
        assert!(
            r.buffer[0].items.iter().any(|i| i.text.contains("P1")),
            "the region's own buffer holds its content"
        );
        assert!(
            r.buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("P6"))),
            "the buffer holds the full scrollable content, including what overflows the 3-row band"
        );
        assert!(
            absent(&out, "P1") && absent(&out, "P6"),
            "none of it flows into the flat document rows"
        );
    }

    #[test]
    fn overflow_auto_stays_bounded_inside_a_definite_height_overflow_hidden_shell() {
        // Twitch's front page shape: a locked-viewport app shell wraps an
        // `overflow:auto` panel in a definite-height `<main>{overflow:hidden}`
        // sized to the viewport. That inner panel stays a bounded region (its
        // content rides its own buffer, never the flat document), AND — because
        // the viewport is block-locked (§3.1) and the panel is the dominant
        // `<main>` content — it is the PRINCIPAL region: the terminal scrolls it
        // as "the page" (main scrollbar + PgUp/PgDn), user-locked across live
        // re-renders. This is what the reader means by "the main scroll".
        let mut lines = String::new();
        for i in 0..40 {
            lines += &format!(r#"<p style="margin:0">P{i:02}</p>"#);
        }
        let out = lay(
            &format!(
                r#"<html style="height:100%;overflow:hidden"><body style="height:100%;margin:0"><main style="height:100%;overflow:hidden;margin:0"><div style="height:100%;overflow-y:auto;margin:0">{lines}</div></main></body></html>"#
            ),
            20,
        );
        assert_eq!(
            out.regions.len(),
            1,
            "the overflow:auto panel is a bounded region"
        );
        let r = &out.regions[0];
        assert!(
            r.principal,
            "the <main> scroller under a locked viewport is the principal (page) region"
        );
        let content_items = r
            .buffer
            .iter()
            .flat_map(|row| &row.items)
            .filter(|item| item.text.starts_with('P'))
            .count();
        assert_eq!(content_items, 40, "the region buffer retains all 40 lines");
        assert!(
            r.buffer[0].items.iter().any(|i| i.text.contains("P00")),
            "the region's own buffer holds its content"
        );
        assert!(
            r.buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("P39"))),
            "the buffer holds the tail too, even though it overflows the shell's viewport-sized band"
        );
        assert!(
            absent(&out, "P00") && absent(&out, "P39"),
            "content stays confined to the region's own buffer, not flowing past the shell into the document"
        );
    }

    #[test]
    fn locked_viewport_sibling_panels_stay_bounded_regions() {
        // A locked viewport (`html{overflow:hidden}` — e.g. a modal scroll-lock)
        // over a landmark-LESS document with MULTIPLE scroll panels that are
        // SIBLINGS (two columns of a flex row) must NOT make them all the
        // "principal scroller": each is one panel among many, so each is a
        // bounded inner region and its overflow can't leak into the other
        // (the humantooth.neocities.org bug — panels overwriting each other).
        let mut a = String::from("A0 ");
        for i in 1..30 {
            a += &format!("A{i}<br>");
        }
        let mut b = String::from("B0 ");
        for i in 1..30 {
            b += &format!("B{i}<br>");
        }
        let out = lay(
            &format!(
                r#"<html style="overflow:hidden"><body style="margin:0"><div style="display:flex">
<div style="flex-grow:1;height:64px;overflow-y:scroll">{a}</div>
<div style="flex-grow:1;height:64px;overflow-y:scroll">{b}</div>
</div></body></html>"#
            ),
            60,
        );
        assert_eq!(
            out.regions.len(),
            2,
            "both sibling panels are bounded regions, neither is the principal scroller"
        );
        assert!(
            out.regions.iter().all(|r| !r.principal),
            "neither column of a two-panel flex row is the page's principal scroller (each has a rendered sibling — not the sole app-shell spine)"
        );
        // Neither panel's overflowing tail leaks into the flat document rows.
        assert!(
            absent(&out, "A29") && absent(&out, "B29"),
            "each panel clips its overflow into its own buffer, not the doc"
        );
        // Each region's buffer holds ITS OWN content only (no cross-leak).
        for r in &out.regions {
            let has_a = r
                .buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("A29")));
            let has_b = r
                .buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("B29")));
            assert!(has_a ^ has_b, "a region holds exactly one panel's content");
        }
    }

    #[test]
    fn stretched_auto_height_flex_item_relays_for_a_nested_scroll_region() {
        // Twitch's front-page shape: a definite-height flex ROW
        // (`overflow:hidden`, locking the app shell to one screen) holds a
        // side-nav column whose OWN wrapper declares NO height at all —
        // `height:auto`, stretched to the row's cross size by the default
        // `align-items:stretch` — and INSIDE that, `.side-nav{height:100%}` →
        // `.scrollable{height:100%;overflow:auto}` must resolve against that
        // STRETCHED size. Two bugs used to defeat this: (1) stretch only
        // patched the item's own reported height post-hoc instead of
        // re-laying its subtree at the new definite height, so every
        // percentage-height descendant still saw an indefinite containing
        // block and just flowed at its full natural size; (2) even once (1)
        // was fixed, the ancestor flex row's `overflow:hidden` clip baked
        // into every descendant fragment tree-wide, capping the scrollable
        // panel's OWN overflow test at the ancestor's bound before its
        // internal overflow was ever measured. Together they made the
        // sidebar's tall content flow in place instead of becoming its own
        // scroll region — the reported bug: "Live Channels" scrolled away
        // with the rest of the page instead of staying put.
        let mut rows = String::new();
        for i in 0..40 {
            rows += &format!(r#"<div>row{i}</div>"#);
        }
        let out = lay(
            &format!(
                r#"<html style="height:100%"><body style="margin:0;height:100%">
<div style="display:flex;height:100%;overflow:hidden">
  <div><div class="side-nav" style="height:100%;width:10ch">
    <div class="scrollable" style="height:100%;overflow:auto">{rows}</div>
  </div></div>
  <div style="flex-grow:1">main</div>
</div></body></html>"#
            ),
            40,
        );
        assert_eq!(
            out.regions.len(),
            1,
            "the tall sidebar content becomes its own bounded scroll region"
        );
        assert!(
            absent(&out, "row39"),
            "the sidebar's overflow doesn't flow into the flat document rows"
        );
        let content_items = out.regions[0]
            .buffer
            .iter()
            .flat_map(|row| &row.items)
            .filter(|item| item.text.starts_with("row"))
            .count();
        assert_eq!(
            content_items, 40,
            "the region's own buffer holds every line, scrollable internally"
        );
    }

    #[test]
    fn min_height_flex_column_grows_flex_child_to_fill() {
        // The full-height app-shell shape (chatgpt.com's composer column, and
        // every `min-h-screen`/`min-h-full` layout): a definite-height block
        // holds a `flex-direction:column` container whose height comes ONLY from
        // `min-height:100%` (NOT an explicit `height`), wrapping a `flex:1` child
        // that bottom-anchors a footer with `margin-top:auto`. The flex child
        // must GROW to fill the min-height so the footer reaches the bottom.
        // Before the fix `def_ch` carried only an explicit height, so grow
        // distributed against content height (no free space): the footer sat
        // right under the header and the lower two-thirds of the shell stayed
        // blank.
        let out = lay(
            r#"<body style="margin:0">
<div style="height:320px">
  <div style="display:flex;flex-direction:column;min-height:100%">
    <div style="display:flex;flex:1;flex-direction:column">
      <div>HEADER</div>
      <div style="margin-top:auto">FOOTER</div>
    </div>
  </div>
</div></body>"#,
            20,
        );
        let (r_header, _) = find(&out, "HEADER");
        let (r_footer, _) = find(&out, "FOOTER");
        assert_eq!(r_header, 0, "header at the top of the filled column");
        // 320px shell = 20 rows; the flex:1 child fills it and margin-top:auto
        // drops the footer to the last row (304px → row 19).
        assert_eq!(
            r_footer, 19,
            "flex:1 fills the min-height column so margin-top:auto reaches the bottom"
        );
    }

    #[test]
    fn shrunk_flex_column_item_relays_for_a_nested_scroll_region() {
        // Twitch's real front-page shape (the live-session bug this models):
        // a definite-height flex COLUMN holds an undismissed cookie-consent
        // banner (`flex-shrink:0`, tall natural content) ABOVE the row that
        // carries the side-nav + main content (`height:100%`, default
        // flex-shrink:1). The banner's own content oversubscribes the
        // column, so §9.7 shrinks the row's USED main size to a fraction of
        // its 100% basis (19 rows of banner in a 24-row viewport, sole
        // shrinkable sibling ⇒ the row lands at exactly 5 rows) — CSS
        // Flexbox §9.4 step 1 ("determine the hypothetical cross size ... by
        // performing layout ... with the used main size") requires the row's
        // subtree to be laid out AGAIN at that resolved size. Before the fix,
        // only the row's own reported height was patched post-hoc; its
        // `overflow:auto` descendant still saw the stale, pre-shrink height
        // and reserved a 24-row band instead of 5 — the reported bug: the
        // sidebar rendered as if nothing above it had taken any space.
        let mut banner_lines = String::new();
        for i in 0..19 {
            banner_lines += &format!("<div>consent line {i}</div>");
        }
        let mut rows = String::new();
        for i in 0..40 {
            rows += &format!(r#"<div>row{i}</div>"#);
        }
        let out = lay(
            &format!(
                r#"<html style="height:100%"><body style="margin:0;height:100%">
<div style="display:flex;flex-direction:column;height:100%;overflow:hidden">
  <div style="flex-shrink:0">{banner_lines}</div>
  <div style="display:flex;height:100%;overflow:hidden">
    <div><div class="side-nav" style="height:100%;width:10ch">
      <div class="scrollable" style="height:100%;overflow:auto">{rows}</div>
    </div></div>
    <div style="flex-grow:1">main</div>
  </div>
</div></body></html>"#
            ),
            40,
        );
        assert_eq!(
            out.regions.len(),
            1,
            "the sidebar becomes its own bounded scroll region even though the column was shrunk"
        );
        assert!(
            (1..24).contains(&out.regions[0].height),
            "the region uses the actual post-shrink size, not the stale 24-row basis"
        );
        assert!(
            absent(&out, "row39"),
            "the sidebar's overflow doesn't flow into the flat document rows"
        );
    }

    #[test]
    fn percentage_height_column_item_shrinks_to_leave_room_for_footer() {
        // CSS Flexbox §4.5: a non-scroll flex item's automatic minimum is
        // its content-size suggestion (its min-content main size), capped by
        // the specified-size suggestion. These are separate measurements.
        // A `height:100%` details pane therefore has a 225px flex base here,
        // but only a 175px automatic minimum from its 160px image + 15px
        // title. It can shrink to 185px and leave the fixed 40px stats footer
        // inside the card. Treating the specified 225px layout as the content
        // suggestion pins the details pane at 225px and spills the footer.
        let dom = Dom::parse_document(
            r#"<body style="margin:0">
<div id="card" style="display:flex;flex-direction:column;width:220px;height:225px">
  <div id="details" style="display:flex;flex-direction:column;height:100%">
    <div style="height:160px;flex-shrink:0"></div>
    <h3 style="height:15px;margin:0"></h3>
  </div>
  <div id="stats" style="height:40px;flex-shrink:0">stats</div>
</div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 400.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let card = layout.boxes.get(&node_by_id(&dom, "card")).unwrap();
        let details = layout.boxes.get(&node_by_id(&dom, "details")).unwrap();
        let stats = layout.boxes.get(&node_by_id(&dom, "stats")).unwrap();
        assert!((card.height - 225.0).abs() < 0.1, "{card:?}");
        assert!((details.height - 185.0).abs() < 0.1, "{details:?}");
        assert!((stats.top - 185.0).abs() < 0.1, "{stats:?}");
        assert!(
            stats.top + stats.height <= card.top + card.height + 0.1,
            "the stats footer must remain inside its card: {stats:?} in {card:?}"
        );
    }

    #[test]
    fn region_seeds_voffset_from_the_scroll_top_signal() {
        // The live serializer bakes data-trust-scroll-top in CSS pixels plus
        // data-trust-node; the terminal adapter quantizes the signal to rows,
        // clamps it, and emits a scroll_clip for the geometry push.
        let out = lay(
            r#"<body style="margin:0"><div data-trust-node="42" data-trust-scroll-top="32" style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">S1</p><p style="margin:0">S2</p><p style="margin:0">S3</p><p style="margin:0">S4</p><p style="margin:0">S5</p><p style="margin:0">S6</p></div></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1);
        let r = &out.regions[0];
        assert_eq!(r.voffset, 2, "CSS-pixel signal quantized to rows");
        assert!(r.voffset_from_page);
        assert_eq!(r.live_node, Some(42));
        assert_eq!(
            out.scroll_clips,
            vec![(42, 3, 20)],
            "(live node, clientHeight rows, scrollport width cells)"
        );
    }

    // ---- P5c: horizontal carousels (CSS Overflow L3 §2, CSS Scroll Snap) ----
    // An overflow-x:auto|scroll box whose content overflows to the right is a
    // horizontal scroll strip: cards stay inline in the doc rows at their strip
    // columns (the renderer windows them to the band), snap stops are the cards'
    // leading edges, and the UA emits a ‹ › control pair.

    #[test]
    fn overflow_x_auto_strip_becomes_a_carousel() {
        // A flex row of five 40px cards (200px) in an 80px scroll box: the strip
        // overflows, so it becomes a carousel windowed to the 10-col band. Cards
        // are laid at their REAL flex widths — never a guessed "N across" size.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">C1</div><div style="flex:0 0 auto;width:40px">C2</div><div style="flex:0 0 auto;width:40px">C3</div><div style="flex:0 0 auto;width:40px">C4</div><div style="flex:0 0 auto;width:40px">C5</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1, "one carousel");
        let c = &out.carousels[0];
        assert_eq!((c.left, c.right), (0, 10), "band = 80px = 10 cols");
        assert_eq!(c.width, 25, "strip = 200px = 25 cols");
        assert_eq!(c.offset, 0, "scroll origin is the strip start");
        // No scroll-snap declared ⇒ FREE scroll, no snap positions (CSS Scroll
        // Snap 1: snapping is opt-in; we don't impose card-snap).
        assert!(
            !c.snap && c.stops.is_empty(),
            "free scroll, no imposed snap"
        );
        // Every card stays in the doc rows at its strip column (windowed at
        // render), including ones past the band's right edge.
        assert_eq!(find(&out, "C1").1.col, 0);
        assert_eq!(
            find(&out, "C5").1.col,
            20,
            "5th card at strip col 20, past the 10-col band"
        );
    }

    #[test]
    fn overflow_x_auto_with_inline_overflow_becomes_a_carousel() {
        // The scrollable overflow of an `overflow-x:auto` box can come from its
        // INLINE content (a `white-space:pre` long line), not only from wide
        // child boxes (CSS Overflow L3 §2 — line boxes contribute to the
        // scrollable overflow region). A `<pre><code>` code block is exactly
        // this shape, so it must become a horizontal scroll strip too.
        let out = lay(
            r#"<body style="margin:0"><div style="overflow-x:auto;white-space:pre;width:80px;margin:0">ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 and the rest of this line keeps going far past the box edge</div></body>"#,
            40,
        );
        assert_eq!(
            out.carousels.len(),
            1,
            "inline (line-box) overflow forms a carousel"
        );
        let c = &out.carousels[0];
        assert_eq!((c.left, c.right), (0, 10), "band = 80px = 10 cols");
        assert!(
            c.width > c.view_width(),
            "strip ({}) wider than the band ({}) — the long line is reachable",
            c.width,
            c.view_width()
        );
    }

    #[test]
    fn carousel_snaps_only_when_the_page_declares_it() {
        // scroll-snap-type on the container + scroll-snap-align on the cards ⇒
        // snap to those positions (CSS Scroll Snap 1). Here align:start ⇒ the
        // stops are the card leading edges.
        let out = lay(
            r#"<body style="margin:0"><div data-trust-node="42" data-trust-scroll-left="40" style="display:flex;overflow-x:auto;width:80px;margin:0;scroll-snap-type:x mandatory"><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C1</div><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C2</div><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C3</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1);
        let c = &out.carousels[0];
        assert!(c.snap, "the page declared scroll-snap-type: x mandatory");
        assert_eq!(c.stops, vec![0, 5, 10], "card leading edges (align:start)");
        assert_eq!(c.live_node, Some(42), "live actor id round-trips");
        assert_eq!(c.offset, 5, "40 CSS px quantizes once to five cells");
    }

    #[test]
    fn carousel_injects_no_scroll_chrome() {
        // We synthesise NO prev/next controls: the page defines its own scroll
        // affordance (or relies on the UA behavioural scroll, like a scrollbar).
        // The only items on screen are the page's own — nothing we invented.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">HDR</p><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">C1</div><div style="flex:0 0 auto;width:40px">C2</div><div style="flex:0 0 auto;width:40px">C3</div><div style="flex:0 0 auto;width:40px">C4</div><div style="flex:0 0 auto;width:40px">C5</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1);
        assert!(
            absent(&out, "‹") && absent(&out, "›"),
            "no synthesised chrome"
        );
    }

    #[test]
    fn scrollbar_width_none_hides_terminal_chrome_without_disabling_scroll() {
        // CSS Scrollbars Styling 1 §3: `none` displays no scrollbar, while
        // the element remains scrollable through script, wheel, and keys.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;overflow-x:scroll;scrollbar-width:none;width:80px"><div style="flex:0 0 auto;width:80px">C1</div><div style="flex:0 0 auto;width:80px">C2</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1);
        let carousel = &out.carousels[0];
        assert!(carousel.hide_scrollbar, "terminal UI must not draw a rail");
        assert!(
            carousel.max_offset() > 0,
            "hiding chrome must not disable scrolling"
        );
    }

    // ---- P5-fidelity: nested scrollers (a scroll container inside another) ----

    #[test]
    fn a_region_nested_in_a_region_is_extracted_buffer_relative() {
        // An outer vertical scroll region whose content includes ANOTHER
        // vertical scroll region: the inner one is extracted into the OUTER's
        // `regions` (buffer-relative), independently scrollable within it.
        let out = lay(
            r#"<body style="margin:0"><div style="height:96px;overflow-y:auto;margin:0"><p style="margin:0">OT</p><div style="height:32px;overflow-y:auto;margin:0"><p style="margin:0">IN1</p><p style="margin:0">IN2</p><p style="margin:0">IN3</p><p style="margin:0">IN4</p></div><p style="margin:0">OB1</p><p style="margin:0">OB2</p><p style="margin:0">OB3</p><p style="margin:0">OB4</p><p style="margin:0">OB5</p></div></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1, "one top-level (outer) region");
        let outer = &out.regions[0];
        assert_eq!(outer.height, 6, "outer clientHeight = 96px = 6 rows");
        assert!(
            outer.buffer.len() >= 8,
            "the outer buffer contains its proportional lines plus the inner band"
        );
        assert_eq!(outer.regions.len(), 1, "one region nested inside the outer");
        let inner = &outer.regions[0];
        assert_eq!(
            inner.start_row, 1,
            "inner band is buffer-relative (after OT)"
        );
        assert_eq!(inner.height, 2, "inner clientHeight = 32px = 2 rows");
        assert_eq!(
            inner.buffer.len(),
            4,
            "inner scrollHeight = 4 rows (IN1..IN4)"
        );
        assert!(
            inner
                .buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("IN4"))),
            "inner content lives in the inner region's own buffer"
        );
        // The inner content is NOT in the outer buffer (its band is blank there).
        assert!(
            !outer
                .buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("IN1"))),
            "the inner region's band is blank in the outer buffer"
        );
    }

    #[test]
    fn a_carousel_nested_in_a_region_is_windowed_within_it() {
        // The streaming-home idiom: a vertical feed (region) of horizontal
        // shelves (carousels). The shelf is extracted into the region's
        // `carousels` (buffer-relative) and windowed within the region's window.
        let out = lay(
            r#"<body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">FEED-TOP</p><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">S1</div><div style="flex:0 0 auto;width:40px">S2</div><div style="flex:0 0 auto;width:40px">S3</div><div style="flex:0 0 auto;width:40px">S4</div></div><p style="margin:0">F1</p><p style="margin:0">F2</p><p style="margin:0">F3</p></div></body>"#,
            40,
        );
        assert_eq!(out.regions.len(), 1);
        let feed = &out.regions[0];
        assert_eq!(feed.carousels.len(), 1, "the shelf is nested in the feed");
        let shelf = &feed.carousels[0];
        assert_eq!(
            shelf.start, 1,
            "shelf band is buffer-relative (after FEED-TOP)"
        );
        assert!(
            shelf.width > (shelf.right - shelf.left),
            "the shelf overflows"
        );
        // The shelf cards live in the feed's buffer at their strip columns.
        assert!(
            feed.buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("S4"))),
            "shelf cards are in the feed buffer (windowed at render)"
        );
    }

    // ---- the P6 gate: tables (CSS 2.1 §17) ----
    // The cell of a test is the nominal 8×16 px, so 1 col = 8px and a
    // width:100% table in an N-col band is N·8 px wide.

    /// The 0-based (row, col) of the item containing `text`.
    fn cell_at(out: &Output, text: &str) -> (usize, usize) {
        let (r, it) = find(out, text);
        (r, it.col as usize)
    }

    #[test]
    fn table_cells_lay_side_by_side() {
        // The core of §17: cells of one row share the same grid rows in
        // distinct columns — not each `<td>` on its own line.
        let out = lay(
            "<body><table><tr><td>LeftCell</td><td>RightCell</td></tr></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "LeftCell").0,
            cell_at(&out, "RightCell").0,
            "both cells share a row"
        );
        assert!(
            cell_at(&out, "RightCell").1 > cell_at(&out, "LeftCell").1,
            "the second cell is to the right"
        );
    }

    #[test]
    fn table_rows_stack_and_columns_align() {
        let out = lay(
            "<body><table>\
             <tr><td>r1a</td><td>r1b</td></tr>\
             <tr><td>r2a</td><td>r2b</td></tr></table></body>",
            60,
        );
        assert!(
            cell_at(&out, "r2a").0 > cell_at(&out, "r1a").0,
            "rows stack"
        );
        assert_eq!(
            cell_at(&out, "r1a").1,
            cell_at(&out, "r2a").1,
            "col 0 aligns"
        );
        assert_eq!(
            cell_at(&out, "r1b").1,
            cell_at(&out, "r2b").1,
            "col 1 aligns"
        );
    }

    #[test]
    fn a_display_block_table_still_lays_as_a_table() {
        // Markdown CSS forces `display:block` onto a `<table>` (so a wide table
        // scrolls). The `<thead>`/`<tbody>` keep their table displays, so
        // §17.2.1 wraps them in an anonymous table and the cells STILL lay side
        // by side.
        let out = lay(
            "<body><table style=\"display:block\">\
             <thead><tr><th>Command</th><th>Effect</th></tr></thead>\
             <tbody><tr><td>website.com</td><td>opens it</td></tr></tbody></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "Command").0,
            cell_at(&out, "Effect").0,
            "header cells share a row"
        );
        assert!(cell_at(&out, "Effect").1 > cell_at(&out, "Command").1);
        assert_eq!(
            cell_at(&out, "Command").1,
            cell_at(&out, "website.com").1,
            "the header column aligns with the body column"
        );
        assert!(cell_at(&out, "website.com").0 > cell_at(&out, "Command").0);
    }

    #[test]
    fn a_colspan_cell_spans_its_columns() {
        let out = lay(
            "<body><table>\
             <tr><td colspan=\"2\">Header</td></tr>\
             <tr><td>colA</td><td>colB</td></tr></table></body>",
            60,
        );
        assert!(cell_at(&out, "Header").0 < cell_at(&out, "colA").0);
        assert_eq!(
            cell_at(&out, "colA").0,
            cell_at(&out, "colB").0,
            "the two spanned cells share a row"
        );
        assert!(cell_at(&out, "colB").1 > cell_at(&out, "colA").1);
    }

    #[test]
    fn a_rowspan_cell_spans_its_rows() {
        // A top-aligned cell spanning two rows sits beside both; the second
        // row's other cell is below the first row's, in the same column.
        let out = lay(
            "<body><table>\
             <tr><td rowspan=\"2\" style=\"vertical-align:top\">Side</td><td>Top</td></tr>\
             <tr><td>Bottom</td></tr></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "Side").0,
            cell_at(&out, "Top").0,
            "spans from row 0"
        );
        assert!(
            cell_at(&out, "Bottom").0 > cell_at(&out, "Top").0,
            "second row is below"
        );
        assert_eq!(
            cell_at(&out, "Top").1,
            cell_at(&out, "Bottom").1,
            "Top/Bottom share the second column"
        );
        assert!(
            cell_at(&out, "Top").1 > cell_at(&out, "Side").1,
            "Side is the first column"
        );
    }

    #[test]
    fn a_nested_table_lays_out_inside_its_cell() {
        // The slackware nested-table trick: an inner table inside a cell lays
        // out within its cell's column, not collapsed.
        let out = lay(
            "<body><table><tr>\
             <td><table><tr><td>InnerL</td><td>InnerR</td></tr></table></td>\
             <td>Outer</td></tr></table></body>",
            60,
        );
        assert_eq!(cell_at(&out, "InnerL").0, cell_at(&out, "Outer").0);
        assert!(cell_at(&out, "InnerR").1 > cell_at(&out, "InnerL").1);
        assert!(cell_at(&out, "Outer").1 > cell_at(&out, "InnerR").1);
    }

    #[test]
    fn col_elements_size_their_table_columns() {
        // §17.5.2: a `<col width="10%">` of a width:100% table in a 40-col
        // (320px) band is 32px = 4 cols, so the second column starts at col 4.
        let out = lay(
            r#"<body style="margin:0"><table width="100%"><colgroup><col width="10%"><col></colgroup>
                 <tr><td>a</td><td>bb</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "bb").1, 4);
    }

    #[test]
    fn col_span_repeats_its_width() {
        // `<col span="2" width="25%">` covers two 25% (80px = 10-col) columns.
        let out = lay(
            r#"<body style="margin:0"><table width="100%"><colgroup><col span="2" width="25%"></colgroup>
                 <tr><td>a</td><td>b</td><td>c</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "b").1, 10);
        assert_eq!(cell_at(&out, "c").1, 20);
    }

    #[test]
    fn declared_cell_width_holds_on_a_widthless_table() {
        // §17.5.2.2: a declared column width raises the column's max-content, so
        // an 80px (10-col) first column holds its width even when the TABLE
        // declares none.
        let out = lay(
            r#"<body style="margin:0"><table><tr><td width="80">a</td><td>b</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "b").1, 10);
    }

    #[test]
    fn a_narrow_menu_sits_beside_a_wide_content_column() {
        // The slackware.com layout-table pattern: a width:10% menu cell beside
        // an auto-width content cell, both on the same rows.
        let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do";
        let out = lay(
            &format!(
                "<body style=\"margin:0\"><table width=\"100%\"><tr valign=\"top\">\
                 <td width=\"10%\">Menu</td><td>{words}</td></tr></table></body>"
            ),
            80,
        );
        assert_eq!(
            cell_at(&out, "Menu").0,
            cell_at(&out, "lorem").0,
            "the menu sits beside the content"
        );
        assert!(
            cell_at(&out, "lorem").1 >= 8,
            "the content column starts past the narrow 10% menu"
        );
    }

    #[test]
    fn bare_cells_default_to_middle_vertical_alignment() {
        // §17.5.4 / Appendix D: `td,th { vertical-align: inherit }` +
        // `tbody { vertical-align: middle }` — a bare cell centers in its band.
        let out = lay(
            "<body><table><tr><td>l1<br>l2<br>l3</td><td>X</td></tr></table></body>",
            40,
        );
        assert_eq!(
            cell_at(&out, "X").0,
            cell_at(&out, "l2").0,
            "the undeclared cell centers"
        );
    }

    #[test]
    fn css_vertical_align_beats_the_valign_attribute() {
        // The `valign` presentational hint is an author-level rule preceding
        // all others, so an author `vertical-align` wins.
        let out = lay(
            "<body><table><tr>\
             <td>l1<br>l2<br>l3</td>\
             <td valign=\"bottom\" style=\"vertical-align:top\">X</td></tr></table></body>",
            40,
        );
        assert_eq!(
            cell_at(&out, "X").0,
            cell_at(&out, "l1").0,
            "CSS top beats the bottom hint"
        );
    }

    #[test]
    fn a_caption_renders_above_the_grid_and_centered() {
        // §17.4: a `table-caption` is a block box above the grid; the UA sheet
        // centers it (`caption { text-align: center }`).
        let out = lay(
            r#"<body style="margin:0"><table style="width:200px"><caption>Cap</caption>
                 <tr><td>cell</td></tr></table></body>"#,
            40,
        );
        assert!(
            cell_at(&out, "Cap").0 < cell_at(&out, "cell").0,
            "the caption is above the grid"
        );
        assert!(
            cell_at(&out, "Cap").1 >= 8,
            "the caption centers in the 200px (25-col) table, not flush left"
        );
    }

    #[test]
    fn caption_side_bottom_renders_below_the_grid() {
        let out = lay(
            r#"<body><table><caption style="caption-side:bottom">Cap</caption>
                 <tr><td>cell</td></tr></table></body>"#,
            40,
        );
        assert!(
            cell_at(&out, "Cap").0 > cell_at(&out, "cell").0,
            "the bottom caption is below the grid"
        );
    }

    #[test]
    fn an_auto_table_shrinks_and_centers_with_align_center() {
        // §17.5.2: a width:auto table shrinks to its content; `align=center`
        // centers it in a wide band (§17.4).
        let out = lay(
            r#"<body style="margin:0"><table align="center"><tr><td>Hi</td></tr></table></body>"#,
            80,
        );
        assert!(
            cell_at(&out, "Hi").1 > 20,
            "the shrunk table centers in the 80-col band, not flush left"
        );
    }

    #[test]
    fn css_padding_suppresses_cellpadding() {
        // The presentational-hint priority: a cell with ANY CSS padding ignores
        // the `cellpadding` attribute, so its content is not inset.
        let out = lay(
            r#"<body style="margin:0"><table cellpadding="8"><tr><td style="padding-bottom:4px">x</td></tr></table></body>"#,
            40,
        );
        assert_eq!(
            cell_at(&out, "x").1,
            0,
            "CSS padding wins — no cellpadding inset"
        );
    }

    #[test]
    fn cellpadding_insets_content_and_widens_the_column() {
        // With no CSS padding, `cellpadding="8"` insets the content by 8px
        // (1 col) and the auto column reserves room for it (content stays
        // unclipped — the width fold in `cell_min_max`).
        let out = lay(
            r#"<body style="margin:0"><table cellpadding="8"><tr><td>xy</td></tr></table></body>"#,
            40,
        );
        assert_eq!(
            cell_at(&out, "xy").1,
            1,
            "content inset by the 8px cellpadding"
        );
        assert!(!absent(&out, "xy"), "the content is not squeezed away");
    }

    #[test]
    fn deeply_nested_tables_still_render_the_innermost_content() {
        // Past MAX_TABLE_DEPTH a table degrades to block-stacked content; the
        // descent terminates and the innermost cell content still renders.
        let mut html = String::from("DEEPEST");
        for i in 0..40 {
            html = format!("<table><tr><td>L{i} {html}</td><td>x</td></tr></table>");
        }
        let out = lay(&format!("<body>{html}</body>"), 80);
        assert!(
            !absent(&out, "DEEPEST"),
            "the innermost content renders past the depth lid"
        );
    }

    #[test]
    fn inline_table_is_an_atomic_inline_box() {
        // `inline-table` (CSS-Display-3 §2.5) rides the line as one opaque box
        // whose content is a table — two sit side by side and text follows.
        // 48px = 6 cells.
        let out = lay(
            r#"<body style="margin:0"><table style="display:inline-table;width:48px"><tr><td>AA</td></tr></table><table style="display:inline-table;width:48px"><tr><td>BB</td></tr></table>after</body>"#,
            80,
        );
        let (ra, a) = find(&out, "AA");
        let (rb, b) = find(&out, "BB");
        let (raf, af) = find(&out, "after");
        assert_eq!((ra, a.col), (0, 0));
        assert_eq!((rb, b.col), (0, 6), "second inline-table beside the first");
        assert_eq!((raf, af.col), (0, 12), "text flows after on the same line");
    }

    #[test]
    fn a_shadow_hosted_table_composes_its_rows() {
        // A `display:table` host renders its rows FROM the FLAT tree (HTML
        // §4.8.2): rows built into its shadow, or light rows projected through a
        // `<slot>`. Without composing, the table's row scan saw the (empty or
        // slotted-away) light children and the cell never rendered — the same
        // class as archive.org's shadow app.
        let base = Url::parse("http://e.com/").unwrap();
        let lay_dom = |dom: &Dom| {
            lay_out_document(
                dom,
                &base,
                TerminalViewport::new(80, 24, 8.0, 16.0),
                &[],
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
        };
        // (a) rows built directly into the host's shadow root.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="t" style="display:table"></div></body>"#,
        );
        let host = dom.get_by_id("t").unwrap();
        let shadow = dom.attach_shadow(host);
        let row = dom.create_element("div");
        dom.set_attr(row, "style", "display:table-row");
        dom.append(shadow, row);
        let cell = dom.create_element("div");
        dom.set_attr(cell, "style", "display:table-cell");
        dom.append(row, cell);
        dom.append_text(cell, "SHADOWCELL");
        assert!(
            !absent(&lay_dom(&dom), "SHADOWCELL"),
            "a table row in the host's shadow renders"
        );
        // (b) light rows projected through a `<slot>` in the shadow.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="t" style="display:table"><div style="display:table-row"><div style="display:table-cell">SLOTCELL</div></div></div></body>"#,
        );
        let host = dom.get_by_id("t").unwrap();
        let shadow = dom.attach_shadow(host);
        let slot = dom.create_element("slot");
        dom.append(shadow, slot);
        assert!(
            !absent(&lay_dom(&dom), "SLOTCELL"),
            "a light table row projected through a <slot> renders"
        );
    }

    #[test]
    fn modal_auto_height_flex_chain_does_not_collapse_content() {
        let html = r#"<body style="margin:0"><div style="display:flex;align-items:center;justify-content:center;position:fixed;inset:0"><div style="display:flex;flex-direction:column;width:fit-content;max-width:calc(100% - 32px);max-height:calc(100% - 32px)"><div style="display:flex;flex-direction:column;align-items:center;height:100%;position:relative;box-sizing:border-box;max-height:inherit;min-height:inherit;overflow:auto"><div style="box-sizing:border-box;padding:16px;width:100%;display:flex;flex-direction:column;flex:1 1;overflow-y:auto;overflow-x:hidden"><div style="display:flex;flex-direction:column;min-height:0;flex-shrink:1;flex-grow:0;overflow:auto"><div style="display:block;padding:4px 0">Let us know your cookie preferences and why cookies are used.</div></div></div><div style="display:flex;flex-direction:column;width:100%;padding:16px;gap:8px"><button>Reject Optional Cookies</button><button>Accept All</button></div></div></div></div></body>"#;
        let out = lay(html, 40);
        let text = out
            .fixed
            .iter()
            .flat_map(|fixed| fixed.rows.iter())
            .flat_map(|row| row.items.iter())
            .map(|item| item.text.as_str())
            .chain(
                out.rows
                    .iter()
                    .flat_map(|row| row.items.iter())
                    .map(|item| item.text.as_str()),
            )
            .collect::<String>();
        assert!(text.contains("Let us know"), "{text:?}");
        assert!(text.contains("Accept All"), "{text:?}");
    }

    // ---- P7: JS geometry from fragments (measure::boxes) --------------------
    // Cells are the nominal 8×16 px; measured rects report the same integer
    // cell grid the paint pass stamps, × cell px.

    fn measure(html: &str, cols: usize, rows: usize) -> (Dom, HashMap<NodeId, PxRect>) {
        measure_images(html, cols, rows, &HashMap::new())
    }

    fn measure_images(
        html: &str,
        cols: usize,
        rows: usize,
        images: &ImageSizes,
    ) -> (Dom, HashMap<NodeId, PxRect>) {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        let boxes = measure_boxes_terminal(
            &dom,
            &base,
            (cols, rows),
            &[],
            &HashMap::new(),
            (8, 16),
            images,
        )
        .0;
        (dom, boxes)
    }

    fn node_by_id(dom: &Dom, id: &str) -> NodeId {
        dom.descendants(crate::dom::DOCUMENT)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap_or_else(|| panic!("no element #{id}"))
    }

    fn rect<'a>(dom: &Dom, boxes: &'a HashMap<NodeId, PxRect>, id: &str) -> &'a PxRect {
        boxes
            .get(&node_by_id(dom, id))
            .unwrap_or_else(|| panic!("no measured box for #{id}"))
    }

    #[test]
    fn geometry_reports_a_blocks_own_border_box() {
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><div id="a" style="height:48px">x</div><div id="b" style="height:32px">y</div></body>"#,
            40,
            24,
        );
        let a = rect(&dom, &boxes, "a");
        assert_eq!(
            (a.top, a.height),
            (0.0, 48.0),
            "first block at the top, 48px tall"
        );
        let b = rect(&dom, &boxes, "b");
        assert_eq!((b.top, b.height), (48.0, 32.0), "second block stacks below");
    }

    #[test]
    fn variable_display_hides_closed_popover_from_layout() {
        // CSS Variables L1 §3 + CSS Display: substitution must happen before
        // display is interpreted, so a custom-property-backed tooltip remains
        // out of the box tree until its visibility class changes.
        let (dom, boxes) = measure(
            r#"<head><style>
                .s-popover { --_state: none; display: var(--_state) }
                .s-popover.is-visible { --_state: block }
            </style></head>
            <body style="margin:0"><div id=closed class=s-popover>closed tooltip</div>
            <div id=open class="s-popover is-visible">open popover</div></body>"#,
            40,
            24,
        );
        assert!(
            !boxes.contains_key(&node_by_id(&dom, "closed")),
            "display:none removes the closed tooltip box"
        );
        assert!(
            boxes.contains_key(&node_by_id(&dom, "open")),
            "the visible state still generates a box"
        );
    }

    #[test]
    fn graphical_cssom_geometry_preserves_fractional_css_pixels() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="fractional" style="width:37.5px;height:22.25px;margin-left:3.75px"></div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(321.25, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let rect = layout.boxes.get(&node_by_id(&dom, "fractional")).unwrap();
        assert!((rect.left - 3.75).abs() < 0.01, "{rect:?}");
        assert!((rect.width - 37.5).abs() < 0.01, "{rect:?}");
        assert!((rect.height - 22.25).abs() < 0.01, "{rect:?}");
    }

    #[test]
    fn graphical_popover_uses_the_icb_and_top_layer() {
        // HTML popovers participate in CSS Positioned Layout's top layer. An
        // absolute popover is a root sibling for containing-block and clipping
        // purposes: the positioned/overflow-hidden DOM parent must neither add
        // its offset nor clip the menu. Steam's supernav computes document-
        // space left/top before showPopover(), which exposed both mistakes.
        let dom = Dom::parse_document(
            r#"<body style="margin:0">
            <div style="position:relative;margin-left:300px;width:100px;height:20px;overflow:hidden">
              <div id="tip" popover="manual" data-trust-popover-open="0" data-trust-hover="17"
                   style="display:block;position:absolute;left:120px;top:40px;width:80px;height:30px;background:red">TIP</div>
            </div>
            <div style="position:fixed;z-index:999999;left:0;top:0">ordinary overlay</div>
            <div popover="manual" data-trust-popover-open="1" data-trust-hover="18"
                 style="display:block;position:fixed;left:10px;top:10px">SECOND</div>
            </body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(800.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let tip = node_by_id(&dom, "tip");
        let tip_rect = layout.boxes.get(&tip).expect("popover box");
        assert!((tip_rect.left - 120.0).abs() < 0.01, "{tip_rect:?}");
        assert!((tip_rect.top - 40.0).abs() < 0.01, "{tip_rect:?}");
        assert_eq!(
            layout
                .paint
                .top_layer
                .iter()
                .map(|entry| entry.fixed)
                .collect::<Vec<_>>(),
            vec![false, true],
            "absolute and fixed entries retain one shared top-layer order"
        );
        assert!(
            layout.paint.primitives.iter().all(|command| !matches!(
                command,
                crate::render::DisplayCommand::HitRegion(region) if region.actor == Some(17)
            )),
            "top-layer commands must not remain in document paint"
        );
        assert!(
            layout
                .paint
                .top_layer
                .iter()
                .flat_map(|entry| &entry.primitives)
                .any(|command| matches!(command,
                    crate::render::DisplayCommand::HitRegion(region) if region.actor == Some(17)
                )),
            "popover paints in the dedicated post-document layer"
        );
    }

    #[test]
    fn graphical_paint_replay_matches_full_layout_without_changing_geometry() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><a id="link" data-trust-paint-node="42" style="color:red;background-color:#fee">paint me</a><p>stable sibling</p></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let viewport = Viewport::new(640.0, 480.0);
        let mut retained =
            lay_out_graphical(&dom, &base, viewport, &[], &HashMap::new(), &HashMap::new());
        assert_eq!(retained.paint_boundaries.len(), 1);
        assert_eq!(retained.paint_boundaries[0].actor, 42);
        let original_boxes = retained.boxes.clone();
        let link = node_by_id(&dom, "link");
        dom.set_attr(
            link,
            "style",
            "color:blue;background-color:#eef;text-decoration-color:green",
        );

        assert!(repaint_graphical(
            &mut retained,
            &dom,
            &base,
            &HashMap::new(),
        ));
        let full = lay_out_graphical(&dom, &base, viewport, &[], &HashMap::new(), &HashMap::new());
        assert_eq!(retained.boxes, original_boxes);
        assert_eq!(retained.boxes, full.boxes);
        assert_eq!(retained.paint.primitives, full.paint.primitives);
        assert_eq!(retained.paint.lines, full.paint.lines);
        assert_eq!(retained.paint.image_requests, full.paint.image_requests);
        assert_eq!(retained.boundaries, full.boundaries);
    }

    #[test]
    fn graphical_empty_positioned_pseudo_paints_border_and_transform() {
        // CSS Pseudo 4 §4.1: content:"" still generates a fully styleable
        // box. Steam draws its discounted-price slash as an absolutely
        // positioned, skewed bottom border on precisely this kind of
        // `::before`; generated fragments have no DOM node, but must retain
        // the pseudo's own computed paint style.
        let layout = lay_graphical(
            r#"<head><style>
               #price { position:relative;display:inline-block;font-size:20px }
               #price::before { content:"";position:absolute;left:0;right:0;top:43%;
                 border-bottom:1.5px solid rgb(115,136,149);transform:skewY(-8deg) }
               </style></head><body style="margin:0"><span id=price>79.99€</span></body>"#,
            400.0,
            &HashMap::new(),
        );
        assert!(
            layout.paint.primitives.iter().any(|command| matches!(
                command,
                crate::render::DisplayCommand::Stroke {
                    shape: crate::render::PaintShape::Path(path),
                    brush: crate::render::PaintBrush::Solid(
                        crate::render::PaintColor::Rgba(115, 136, 149, 255)
                    ),
                    style,
                } if path.len() == 2 && (style.width - 1.5).abs() < 0.01
            )),
            "the empty generated box must paint its authored bottom border"
        );
        assert!(
            layout.paint.primitives.iter().any(|command| matches!(
                command,
                crate::render::DisplayCommand::PushTransform(matrix)
                    if (matrix.0[1] - (-8.0f32).to_radians().tan()).abs() < 0.01
            )),
            "the generated border must retain its skew transform"
        );
    }

    #[test]
    fn graphical_paint_replay_updates_visibility_and_pointer_eligibility() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><a id="link" href="/next" data-trust-hover="42" data-trust-paint-node="42" style="visibility:hidden;pointer-events:none">paint me</a></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let viewport = Viewport::new(320.0, 200.0);
        let mut retained =
            lay_out_graphical(&dom, &base, viewport, &[], &HashMap::new(), &HashMap::new());
        let link = node_by_id(&dom, "link");
        dom.set_attr(
            link,
            "style",
            "visibility:visible;pointer-events:auto;text-decoration-line:underline",
        );
        assert!(repaint_graphical(
            &mut retained,
            &dom,
            &base,
            &HashMap::new(),
        ));
        let full = lay_out_graphical(&dom, &base, viewport, &[], &HashMap::new(), &HashMap::new());
        assert_eq!(retained.paint.primitives, full.paint.primitives);
        assert!(retained.paint.primitives.iter().any(|command| matches!(
            command,
            crate::render::DisplayCommand::GlyphRun { shaped, .. } if shaped.underline
        )));
        assert!(retained.paint.primitives.iter().any(|command| matches!(
            command,
            crate::render::DisplayCommand::HitRegion(region) if region.actor == Some(42)
        )));
    }

    #[test]
    fn graphical_link_descendants_keep_element_pointer_targets() {
        // Pointer Events target determination starts with the rendered hit-test
        // target, while DOM dispatch separately walks that target's event path
        // to choose an ancestor activation target. Text itself does not
        // generate an element box: glyphs target their generating element, but
        // an <img> remains its own target. In both cases the inherited link
        // still identifies the enclosing anchor's default action.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><a id=tile href="/details/item">
                 <img id=thumb src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw=="
                      width=40 height=30><span id=title>VHS Movies</span>
               </a></body>"#,
        );
        let anchor = node_by_id(&dom, "tile");
        let image = node_by_id(&dom, "thumb");
        let title = node_by_id(&dom, "title");
        dom.set_render_clickables(std::collections::HashSet::from([anchor]), true);
        let base = Url::parse("https://archive.example/details/collection").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );

        let linked_hits: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::HitRegion(region) if region.link.is_some() => {
                    Some(region)
                }
                _ => None,
            })
            .collect();
        let text_hit = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::HitRegion(region)
                    if region.node == title
                        && matches!(region.link,
                            Some(crate::doc::Link::JsClick { node, ref href })
                                if node == anchor && href == "/details/item") =>
                {
                    Some(region)
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!("linked text hit region: anchor={anchor} title={title} hits={linked_hits:?}")
            });
        assert_eq!(
            text_hit.actor,
            Some(title),
            "glyph hit testing targets the element which generated the text box"
        );

        let image_hit = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::HitRegion(region)
                    if region.node == image
                        && matches!(region.link,
                            Some(crate::doc::Link::JsClick { node, ref href })
                                if node == anchor && href == "/details/item") =>
                {
                    Some(region)
                }
                _ => None,
            })
            .expect("linked image hit region");
        assert_eq!(image_hit.actor, Some(image));
    }

    #[test]
    fn graphical_text_shadow_retains_front_to_back_layers_without_reflow() {
        let html = r#"<body style="margin:0"><span style="color:rgb(1,2,3);
            text-shadow:rgb(255,255,255) 3px 0 0,
                        rgb(4,5,6) -2px 1px 0">outlined</span></body>"#;
        let layout = lay_graphical(html, 320.0, &HashMap::new());
        let (shaped, shadows) = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::GlyphRun {
                    shaped, shadows, ..
                } if shaped.text == "outlined" => Some((shaped, shadows)),
                _ => None,
            })
            .expect("outlined glyph run");
        assert_eq!(shadows.len(), 2);
        assert_eq!(
            shadows[0].color,
            crate::render::PaintColor::Rgba(255, 255, 255, 255)
        );
        assert_eq!(shadows[0].offset, crate::core::CssPoint::new(3.0, 0.0));
        assert_eq!(
            shadows[1].color,
            crate::render::PaintColor::Rgba(4, 5, 6, 255)
        );
        assert_eq!(shadows[1].offset, crate::core::CssPoint::new(-2.0, 1.0));
        assert_eq!(
            shaped.advance,
            crate::text::shape("outlined", &crate::text::TextStyle::default()).advance,
            "text-shadow is ink overflow and must not alter layout"
        );
    }

    #[test]
    fn graphical_marquee_uses_html_ua_viewport_and_wraps_descendant_paint() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><marquee id=m direction=right scrollamount=9
               scrolldelay=20 style="width:200px;overflow:visible"><span id=tile
               style="display:inline-block;background:#123456">moving</span></marquee><div
               id=after style="width:40px;height:20px;background:#654321"></div></body>"#,
        );
        let marquee = node_by_id(&dom, "m");
        assert_eq!(
            dom.effective_display(marquee).as_deref(),
            Some("inline-block")
        );
        assert_eq!(
            dom.computed_value_resolved(marquee, "overflow").as_deref(),
            Some("hidden"),
            "HTML's UA-important marquee clip beats author overflow"
        );
        let base = Url::parse("https://example.test/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let scope = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::BeginMarquee(scope) => Some(scope),
                _ => None,
            })
            .expect("marquee translation scope");
        assert_eq!(scope.direction, crate::render::MarqueeDirection::Right);
        assert_eq!(scope.scroll_distance, 9.0);
        assert_eq!(
            scope.scroll_interval_seconds, 0.06,
            "sub-60ms delay is clamped without truespeed"
        );
        assert!((scope.viewport.width - 200.0).abs() < 0.1, "{scope:?}");

        let fill = layout
            .paint
            .primitives
            .iter()
            .position(|command| {
                matches!(
                    command,
                    crate::render::DisplayCommand::Fill {
                        brush: crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                            0x12, 0x34, 0x56, 0xff
                        )),
                        ..
                    }
                )
            })
            .expect("descendant box background");
        assert!(
            layout.paint.primitives[..fill]
                .iter()
                .rposition(|command| matches!(
                    command,
                    crate::render::DisplayCommand::BeginMarquee(_)
                ))
                .is_some()
                && layout.paint.primitives[fill + 1..]
                    .iter()
                    .position(|command| matches!(
                        command,
                        crate::render::DisplayCommand::EndMarquee
                    ))
                    .is_some(),
            "the marquee must translate complete descendant box paint"
        );

        let after = node_by_id(&dom, "after");
        let mut marquee_depth = 0usize;
        let mut found_after = false;
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::BeginMarquee(_) => marquee_depth += 1,
                crate::render::DisplayCommand::EndMarquee => marquee_depth -= 1,
                crate::render::DisplayCommand::HitRegion(region) if region.node == after => {
                    assert_eq!(
                        marquee_depth, 0,
                        "a following sibling must not inherit the marquee transform or clip"
                    );
                    found_after = true;
                }
                _ => {}
            }
        }
        assert!(
            found_after,
            "following sibling must remain in the display list"
        );

        let direct = Dom::parse_document(
            r#"<body style="margin:0"><div style="width:200px"><marquee>short text</marquee><br><marquee style="width:80px">fixed</marquee></div></body>"#,
        );
        let direct_layout = lay_out_graphical(
            &direct,
            &base,
            Viewport::new(320.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let direct_scope = direct_layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::BeginMarquee(scope) => Some(scope),
                _ => None,
            })
            .expect("direct-text marquee translation scope");
        assert!(
            (direct_scope.viewport.width - 200.0).abs() < 0.1,
            "{direct_scope:?}"
        );
        assert!(
            direct_scope.content.width < direct_scope.viewport.width / 2.0,
            "marquee travel uses the direct text's content extent, not the full line box: {direct_scope:?}"
        );
        let authored_scope = direct_layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::BeginMarquee(scope) => Some(scope),
                _ => None,
            })
            .find(|scope| (scope.viewport.width - 80.0).abs() < 0.1)
            .expect("an authored marquee width must still win over the auto-width behavior");
        assert!(authored_scope.content.width < authored_scope.viewport.width);
    }

    #[test]
    fn graphical_background_images_use_intrinsic_size_and_repeat() {
        let dom = Dom::parse_document(
            r#"<html style="margin:0;background-image:url('/tile.webp');background-repeat:repeat"><body style="margin:0;background:transparent"><div style="height:36px"></div></body></html>"#,
        );
        let base = Url::parse("https://example.test/").unwrap();
        let mut images = HashMap::new();
        images.insert("https://example.test/tile.webp".to_string(), (16, 12));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(64.0, 36.0),
            &[],
            &HashMap::new(),
            &images,
        );
        let tiles = layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            tiles.len() >= 12,
            "background should be tiled across the box"
        );
        assert!(
            tiles.iter().all(|rect| {
                (rect.width - 16.0).abs() < 0.01 && (rect.height - 12.0).abs() < 0.01
            })
        );
    }

    #[test]
    fn non_replaced_aspect_ratio_sizes_an_overflow_clip_for_absolute_image() {
        // CSS Sizing 4 §§4.1–4.2: an automatic height is transferred from the
        // definite used width through the preferred ratio. The resulting box
        // is also the scrollport/clip for its absolutely positioned image.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="frame" style="position:relative;width:320px;aspect-ratio:16/9;overflow:hidden"><img id="photo" src="photo.jpg" style="position:absolute;width:100%;height:100%;object-fit:cover"></div><p id="after" style="margin:0">after</p></body>"#,
        );
        let base = Url::parse("https://example.test/").unwrap();
        let mut images = HashMap::new();
        images.insert("https://example.test/photo.jpg".to_string(), (640, 360));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(400.0, 600.0),
            &[],
            &HashMap::new(),
            &images,
        );
        let frame = layout.boxes.get(&node_by_id(&dom, "frame")).unwrap();
        let after = layout.boxes.get(&node_by_id(&dom, "after")).unwrap();
        assert_eq!((frame.width, frame.height), (320.0, 180.0));
        assert_eq!(after.top, 180.0);
        let (image, clip) = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::Image {
                    node, rect, clip, ..
                } if *node == node_by_id(&dom, "photo") => Some((*rect, *clip)),
                _ => None,
            })
            .expect("photo paint command");
        assert_eq!((image.width, image.height), (320.0, 180.0));
        assert_eq!(clip.map(|rect| rect.height), Some(180.0));
    }

    #[test]
    fn non_replaced_aspect_ratio_transfers_into_flex_and_grid_items() {
        // CSS Sizing 4 §§4.1–4.2 applies preferred-ratio automatic sizing in
        // every formatting context. Flexbox §9.2 additionally calls out the
        // definite cross-size transfer when finding an item's main size. Ars'
        // responsive story cards use this exact column-flex ratio-box shape.
        for display in [
            "display:flex;flex-direction:column",
            "display:grid;grid-template-columns:320px",
        ] {
            let html = format!(
                r#"<body style="margin:0"><div style="{display};width:320px"><div id="frame" style="position:relative;width:100%;aspect-ratio:16/9;flex-shrink:0;overflow:hidden"><img id="photo" src="photo.jpg" style="position:absolute;width:100%;height:100%;object-fit:cover"></div><p id="after" style="margin:0">after</p></div></body>"#,
            );
            let dom = Dom::parse_document(&html);
            let base = Url::parse("https://example.test/").unwrap();
            let mut images = HashMap::new();
            images.insert("https://example.test/photo.jpg".to_string(), (640, 360));
            let layout = lay_out_graphical(
                &dom,
                &base,
                Viewport::new(400.0, 600.0),
                &[],
                &HashMap::new(),
                &images,
            );
            let frame = layout.boxes.get(&node_by_id(&dom, "frame")).unwrap();
            let after = layout.boxes.get(&node_by_id(&dom, "after")).unwrap();
            assert_eq!(
                (frame.width, frame.height, after.top),
                (320.0, 180.0, 180.0),
                "ratio transfer failed for {display}"
            );
        }
    }

    #[test]
    fn absolutely_positioned_inline_svg_does_not_occupy_normal_flow() {
        // CSS Position 3 §2: absolute positioning removes every principal box
        // from normal flow, including a replaced inline SVG. The SVG tail and
        // the in-flow label therefore share the badge's top edge; the tail is
        // positioned at the containing block's right edge.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="badge" style="position:relative;width:118px;height:40px;color:rgb(4 204 116/1)"><svg id="tail" viewBox="0 0 19.2 40" style="display:block;width:auto;height:40px;position:absolute;top:0;right:1px"><path fill="currentColor" d="M0 40V0h5.6c7.5 0 13.7 6.2 13.7 13.7v2.6c0 4.2-1.9 8-5 10.5"/></svg><div id="label" style="display:flex;height:30px">Featured</div></div></body>"#,
        );
        let layout = lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(400.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let tail = layout.boxes[&node_by_id(&dom, "tail")];
        let label = layout.boxes[&node_by_id(&dom, "label")];
        assert_eq!((tail.top, tail.height), (0.0, 40.0));
        assert!((tail.width - 19.2).abs() < 0.01, "viewBox ratio: {tail:?}");
        assert!((tail.left - 97.8).abs() < 0.01, "right:1px: {tail:?}");
        assert_eq!(label.top, 0.0, "the SVG must not push the label down");
    }

    #[test]
    fn graphical_background_size_scales_source_into_used_tile() {
        // CSS Backgrounds 3 §2.9: the resolved background-size is the image's
        // rendered size. A high-resolution source must be scaled into that
        // rectangle rather than painted at its larger intrinsic pixel size.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><section style="width:800px;height:214px;background:url('/hero.png') 95% bottom no-repeat;background-size:734px"></section></body>"#,
        );
        let base = Url::parse("https://example.test/").unwrap();
        let mut images = HashMap::new();
        images.insert("https://example.test/hero.png".to_string(), (1248, 361));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(800.0, 300.0),
            &[],
            &HashMap::new(),
            &images,
        );
        let (rect, fit) = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::Image { rect, fit, .. } => Some((*rect, *fit)),
                _ => None,
            })
            .expect("hero background image");
        assert!((rect.width - 734.0).abs() < 0.01, "{rect:?}");
        assert!(
            (rect.height - 734.0 * 361.0 / 1248.0).abs() < 0.01,
            "{rect:?}"
        );
        assert_eq!(fit, crate::render::ImageFit::Fill);
    }

    #[test]
    fn graphical_root_background_paints_the_canvas_edges() {
        let dom = Dom::parse_document(
            r#"<html style="margin:0;background-color:#ffaaaa;background-image:url('/tile.webp')"><body style="margin:8px;background:transparent"><div style="height:20px"></div></body></html>"#,
        );
        let base = Url::parse("https://example.test/").unwrap();
        let mut images = HashMap::new();
        images.insert("https://example.test/tile.webp".to_string(), (16, 12));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(64.0, 64.0),
            &[],
            &HashMap::new(),
            &images,
        );
        assert!(layout.paint.background.is_some());
        assert!(matches!(
            layout.paint.primitives.first(),
            Some(crate::render::DisplayCommand::PushClip(
                crate::render::PaintShape::Rect(rect)
            )) if (rect.width - 64.0).abs() < 0.01 && (rect.height - 64.0).abs() < 0.01
        ));
        let tiles = layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(tiles.iter().any(|rect| rect.y <= 0.0));
        assert!(tiles.iter().any(|rect| rect.y + rect.height >= 64.0));
    }

    #[test]
    fn graphical_body_background_propagates_to_the_canvas() {
        // CSS Backgrounds 3 §2.11.2: an HTML body's background is propagated
        // to the canvas when the root background is transparent and none. The
        // canvas fill must therefore remain present below a document whose
        // content is taller than the initial viewport, instead of falling back
        // to the renderer's white surface after scrolling.
        let dom = Dom::parse_document(
            r#"<html style="background:transparent"><head></head><body style="margin:0;background-color:#232323"><div style="height:1200px"></div></body></html>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 480.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            layout.paint.background,
            Some(crate::render::PaintColor::Rgba(35, 35, 35, 255))
        );
        assert!(layout.paint.height >= 1200.0);
    }

    #[test]
    fn graphical_background_paint_resolves_custom_properties_directly() {
        // CSS Custom Properties §3 substitutes var() at computed-value time.
        // The former presentation serializer performed that substitution while
        // baking styles; canonical direct paint must consume the same resolved
        // values without relying on a serialization side effect.
        let dom = Dom::parse_document(
            r#"<html style="--canvas:#121820;--panel:#334455;--art:url('/panel.png');background-color:var(--canvas)">
               <body style="margin:0"><div id="panel" style="width:80px;height:40px;background-color:var(--panel);background-image:var(--art);background-repeat:no-repeat"></div></body></html>"#,
        );
        let base = Url::parse("https://example.test/").unwrap();
        let mut images = HashMap::new();
        images.insert("https://example.test/panel.png".to_string(), (20, 10));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 200.0),
            &[],
            &HashMap::new(),
            &images,
        );
        assert_eq!(
            layout.paint.background,
            Some(crate::render::PaintColor::Rgba(18, 24, 32, 255))
        );
        assert!(layout.paint.primitives.iter().any(|command| matches!(
            command,
            crate::render::DisplayCommand::Fill {
                brush: crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                    51, 68, 85, 255
                )),
                ..
            }
        )));
        assert!(
            layout
                .paint
                .image_requests
                .iter()
                .any(|request| { request.source == "https://example.test/panel.png" })
        );
    }

    #[test]
    fn propagated_body_background_is_not_painted_again_on_the_body_box() {
        let dom = Dom::parse_document(
            r#"<html style="background:transparent"><body style="margin:0;background-image:url('/tile.webp');background-repeat:no-repeat"><div style="height:1200px"></div></body></html>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let mut images = HashMap::new();
        images.insert("http://e.com/tile.webp".to_string(), (16, 12));
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 480.0),
            &[],
            &HashMap::new(),
            &images,
        );
        let tiles = layout
            .paint
            .primitives
            .iter()
            .filter(|command| matches!(command, crate::render::DisplayCommand::Image { .. }))
            .count();
        assert_eq!(
            tiles, 1,
            "the propagated body image must have one canvas paint"
        );
    }

    #[test]
    fn graphical_atomic_boundary_patch_matches_full_appendix_e_paint() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><section id="box" data-trust-node="42" style="display:flow-root;position:relative;z-index:1;width:240px;height:48px"><span id="label" style="color:red">menu</span></section><p>after</p></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let viewport = Viewport::new(640.0, 480.0);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let before = lay_out_graphical(&dom, &base, viewport, &forms, &controls, &HashMap::new());
        let cached = before
            .boundaries
            .iter()
            .find(|boundary| boundary.actor == 42)
            .expect("flow-root stacking context is an atomic boundary")
            .clone();

        let label = node_by_id(&dom, "label");
        dom.set_attr(label, "style", "color:blue;background-color:#eee");
        let partial = lay_graphical_subtree(
            &dom,
            &base,
            node_by_id(&dom, "box"),
            cached.rect,
            viewport,
            &forms,
            &controls,
            &HashMap::new(),
        )
        .expect("subtree lays");
        let after = lay_out_graphical(&dom, &base, viewport, &forms, &controls, &HashMap::new());
        let full_boundary = after
            .boundaries
            .iter()
            .find(|boundary| boundary.actor == 42)
            .unwrap();
        let partial_boundary = partial
            .boundaries
            .iter()
            .find(|boundary| boundary.actor == 42)
            .unwrap();
        assert_eq!(partial_boundary.rect, full_boundary.rect);
        assert_eq!(
            partial.paint.primitives,
            after.paint.primitives[full_boundary.commands.clone()],
            "standalone subtree paint is byte-equivalent to the full Appendix-E segment"
        );
        assert_eq!(
            partial.paint.lines,
            after.paint.lines[full_boundary.lines.clone()]
        );
    }

    #[test]
    fn graphical_ifc_marker_does_not_capture_unrelated_hover() {
        let dom = Dom::parse_document(
            r#"<body><div id="ifc" data-trust-node="41" style="display:flow-root">plain</div><div id="hot" data-trust-node="42" data-trust-hover="42" style="display:flow-root">hover</div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(640.0, 480.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let ifc = node_by_id(&dom, "ifc");
        let hot = node_by_id(&dom, "hot");
        let actor_for = |node| {
            layout
                .paint
                .primitives
                .iter()
                .find_map(|command| match command {
                    crate::render::DisplayCommand::HitRegion(region) if region.node == node => {
                        Some(region.actor)
                    }
                    _ => None,
                })
        };
        assert_eq!(actor_for(ifc), Some(None));
        assert_eq!(actor_for(hot), Some(Some(42)));
    }

    #[test]
    fn graphical_flex_display_image_keeps_intrinsic_width() {
        let src = "data:image/svg+xml,<svg viewBox='0 0 184 148'></svg>";
        let mut images = ImageSizes::new();
        images.insert(src.to_string(), (184, 148));
        let dom = Dom::parse_document(&format!(
            "<body style=\"margin:0\"><picture style=\"display:contents\"><img id=\"logo\" src=\"{src}\" style=\"display:flex;width:auto;height:124px;margin:0 auto 32px;max-width:100%\"></picture><div id=\"search\">Search</div></body>"
        ));
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(960.0, 640.0),
            &forms,
            &controls,
            &images,
        );
        let logo = layout.boxes.get(&node_by_id(&dom, "logo")).unwrap();
        assert!(
            (logo.width - 184.0 * 124.0 / 148.0).abs() < 0.1,
            "a replaced image with display:flex and auto width must use its intrinsic ratio, got {logo:?}"
        );
        let search = layout.boxes.get(&node_by_id(&dom, "search")).unwrap();
        assert!(
            search.top >= logo.top + logo.height + 31.9,
            "content after the logo must follow its used margin, got {search:?} after {logo:?}"
        );
    }

    #[test]
    fn embedded_svg_auto_size_fills_its_definite_flex_viewport() {
        // SVG 2 Geometry §7.8: `auto` width/height on an `svg` element are
        // treated as 100%. A viewBox supplies the coordinate transform, not a
        // 300×150 box that may escape the author's definite icon viewport.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><div id=viewport
               style="display:flex;width:12px;height:12px;align-items:center">
               <svg id=arrows viewBox="0 0 64 100"
                    style="flex-grow:1;flex-shrink:1;flex-basis:0">
                 <path d="M0 0h64v100H0z"/>
               </svg></div></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(500.0, 200.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let viewport = layout.boxes.get(&node_by_id(&dom, "viewport")).unwrap();
        let arrows = layout.boxes.get(&node_by_id(&dom, "arrows")).unwrap();
        assert!((viewport.width - 12.0).abs() < 0.1, "{viewport:?}");
        assert!((viewport.height - 12.0).abs() < 0.1, "{viewport:?}");
        assert!((arrows.width - 12.0).abs() < 0.1, "{arrows:?}");
        assert!((arrows.height - 12.0).abs() < 0.1, "{arrows:?}");
    }

    #[test]
    fn graphical_outside_marker_uses_parent_paint_style_without_a_dom_node() {
        // CSS Display 3 §2.5: an anonymous box inherits through its box-tree
        // parent. The marker remains synthesized (`NO_NODE`) for interaction,
        // but paint must use the generating list item's style rather than
        // indexing the DOM arena with that sentinel.
        let layout = lay_graphical(
            r#"<body><ul><li style="color:rgb(12, 34, 56)">marker text</li></ul></body>"#,
            320.0,
            &HashMap::new(),
        );
        let marker = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                crate::render::DisplayCommand::GlyphRun {
                    shaped,
                    color,
                    node: NO_NODE,
                    ..
                } if !shaped.text.contains("marker text") => Some(*color),
                _ => None,
            })
            .expect("outside marker should remain renderer-neutral synthesized text");
        assert_eq!(marker, crate::render::PaintColor::Rgba(12, 34, 56, 255));
    }

    #[test]
    fn graphical_nested_scroller_keeps_pixel_clip_offset_and_actor_metadata() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div data-trust-node="42" data-trust-scroll-top="24" style="height:32px;overflow-y:auto"><p style="margin:0">first</p><p style="margin:0">second</p><p style="margin:0">third</p></div></body>"#,
        );
        let scroller = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|node| dom.attr(*node, "data-trust-node") == Some("42"))
            .unwrap();
        dom.set_scroll_pos(scroller, 24.0, 0.0, false);
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let container = layout
            .paint
            .scroll_containers
            .iter()
            .find(|container| container.actor == Some(42))
            .expect("live overflow box should retain its actor identity");
        assert_eq!(container.offset, crate::core::CssPoint::new(0.0, 24.0));
        assert_eq!(container.viewport.height, 32.0);
        assert!(container.content.height > container.viewport.height);

        let second = layout
            .paint
            .primitives
            .iter()
            .position(|command| matches!(command, crate::render::DisplayCommand::GlyphRun { shaped, .. } if shaped.text.contains("second")))
            .expect("scrolled descendant should still be painted");
        assert!(
            layout.paint.primitives[..second]
                .iter()
                .any(|command| matches!(
                    command,
                    crate::render::DisplayCommand::BeginScroll(node)
                        if *node == container.node
                ))
        );
        assert!(
            layout.paint.primitives[..second]
                .iter()
                .any(|command| matches!(
                    command,
                    crate::render::DisplayCommand::PushClip(crate::render::PaintShape::Rect(rect))
                        if (rect.height - 32.0).abs() < 0.01
                ))
        );
    }

    #[test]
    fn graphical_scrollport_clips_direct_anonymous_line_content() {
        // CSS Overflow 3 §§2.3 and 3.1: direct text is still content of the
        // scroll container and is viewed through its padding-box scrollport.
        // Anonymous line fragments retain the container as their generating
        // style node, so paint must include that node itself in the content
        // scroll chain rather than beginning with its DOM parent.
        let layout = lay_graphical(
            r#"<body style="margin:0"><div style="height:32px;overflow:auto">first<br>second<br>third</div></body>"#,
            320.0,
            &HashMap::new(),
        );
        let container = layout
            .paint
            .scroll_containers
            .first()
            .expect("overflow:auto must establish a graphical scroll container");
        assert_eq!(container.viewport.height, 32.0);

        let mut active_clips = Vec::new();
        let mut active_scrolls = Vec::new();
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::PushClip(shape) => {
                    active_clips.push(shape.clone());
                }
                crate::render::DisplayCommand::PopClip => {
                    active_clips.pop();
                }
                crate::render::DisplayCommand::BeginScroll(node) => active_scrolls.push(*node),
                crate::render::DisplayCommand::EndScroll => {
                    active_scrolls.pop();
                }
                crate::render::DisplayCommand::GlyphRun { shaped, .. }
                    if shaped.text.contains("third") =>
                {
                    assert!(
                        active_scrolls.contains(&container.node),
                        "direct line content must receive its own container's scroll transform"
                    );
                    assert!(
                        active_clips.iter().any(|shape| matches!(
                            shape,
                            crate::render::PaintShape::Rect(rect)
                                if (rect.height - 32.0).abs() < 0.01
                        )),
                        "direct line content must be clipped by the 32px scrollport: {active_clips:?}"
                    );
                    return;
                }
                _ => {}
            }
        }
        panic!("expected overflowing direct text in the retained display list");
    }

    #[test]
    fn legacy_clip_rect_suppresses_positioned_control_appearance() {
        // CSS 2.2 §11.1.2: equal rect() edges produce a zero-area clip over
        // the complete absolutely positioned box. This is the standard
        // accessible-control idiom used beside a separately painted label.
        let dom = Dom::parse_document(
            r#"<body style="margin:0"><form>
               <input id=hidden type=checkbox style="position:absolute;clip:rect(0,0,0,0)">
               <span>visible</span></form></body>"#,
        );
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 200.0),
            &forms,
            &controls,
            &HashMap::new(),
        );
        let hidden = dom.get_by_id("hidden").unwrap();
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::GlyphRun { node, clip, .. } if *node == hidden => {
                    assert!(
                        clip.is_some_and(|clip| clip.width == 0.0 && clip.height == 0.0),
                        "hidden control escaped clip: {clip:?}"
                    );
                    return;
                }
                _ => {}
            }
        }
        panic!("expected the clipped control's retained native glyph");
    }

    #[test]
    fn graphical_scrollport_clips_shadow_tree_content() {
        // CSS Overflow 3 §2.3: a scroll container's visual viewport is its
        // padding box. DOM §4.2.2 attaches a shadow tree through its host, so
        // that clip continues across the shadow boundary. Previously the host
        // card was clipped but its shadow image/text escaped the carousel.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="strip" style="width:100px;height:60px;overflow-x:scroll"><x-card id="card" style="display:block;width:240px;height:60px"></x-card></div></body>"#,
        );
        let card = node_by_id(&dom, "card");
        let shadow = dom.attach_shadow(card);
        let inner = dom.create_element("div");
        dom.set_attr(inner, "style", "width:240px;height:60px");
        dom.append(shadow, inner);
        dom.append_text(inner, "shadow overflow");
        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut clips = Vec::new();
        let mut active_at_text = Vec::new();
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::PushClip(shape) => clips.push(shape.clone()),
                crate::render::DisplayCommand::PopClip => {
                    clips.pop();
                }
                crate::render::DisplayCommand::GlyphRun { shaped, .. }
                    if shaped.text.contains("shadow overflow") =>
                {
                    active_at_text = clips.clone();
                    break;
                }
                _ => {}
            }
        }
        assert!(
            active_at_text.iter().any(|shape| matches!(
                shape,
                crate::render::PaintShape::Rect(rect)
                    if (rect.width - 100.0).abs() < 0.01 && (rect.height - 60.0).abs() < 0.01
            )),
            "shadow content must paint through the 100×60px scrollport clip: {active_at_text:?}"
        );
    }

    #[test]
    fn graphical_scrollport_clips_slotted_carousel_content() {
        // CSS Shadow 1 §4.1 makes an assigned node a child of its slot for
        // post-selector CSS operations. A composed-DOM ancestry walk jumps
        // from the card straight to the host and misses the shadow scroller
        // surrounding the slot, allowing archive-style carousel cards to
        // paint outside their scrollport.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><x-strip id="host"><x-card id="card" style="display:block;width:240px;height:60px">slotted overflow</x-card></x-strip></body>"#,
        );
        let host = node_by_id(&dom, "host");
        let shadow = dom.attach_shadow(host);
        let strip = dom.create_element("div");
        dom.set_attr(strip, "style", "width:100px;height:60px;overflow-x:scroll");
        dom.append(shadow, strip);
        let slot = dom.create_element("slot");
        dom.append(strip, slot);

        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let mut clips = Vec::new();
        let mut active_at_text = Vec::new();
        for command in &layout.paint.primitives {
            match command {
                crate::render::DisplayCommand::PushClip(shape) => clips.push(shape.clone()),
                crate::render::DisplayCommand::PopClip => {
                    clips.pop();
                }
                crate::render::DisplayCommand::GlyphRun { shaped, .. }
                    if shaped.text.contains("slotted overflow") =>
                {
                    active_at_text = clips.clone();
                    break;
                }
                _ => {}
            }
        }
        assert!(
            active_at_text.iter().any(|shape| matches!(
                shape,
                crate::render::PaintShape::Rect(rect)
                    if (rect.width - 100.0).abs() < 0.01 && (rect.height - 60.0).abs() < 0.01
            )),
            "slotted card must remain inside the 100×60px carousel: {active_at_text:?}"
        );
    }

    #[test]
    fn nested_carousel_slot_keeps_gallery_pages_in_one_contained_viewport() {
        // HTML §4.12.4's flattened slot assignment is what keeps a gallery's
        // light-DOM pages inside the nested faceplate carousel viewport. This
        // mirrors Reddit's gallery-carousel → faceplate-carousel forwarding
        // chain: three portrait pages must share one 700×540 viewport instead
        // of becoming three normal-flow blocks and tripling the post height.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><x-gallery id="gallery" style="display:block;width:700px;height:540px;overflow:hidden"><ul style="display:flex;width:100%;height:100%;margin:0;padding:0"><li slot="page-1" style="display:flex;flex:0 0 100%;width:100%;height:100%;justify-content:center"><img id="one" src="one.jpg" width="3024" height="4032" style="width:100%;height:100%;object-fit:contain"></li><li slot="page-2" style="display:flex;flex:0 0 100%;width:100%;height:100%;justify-content:center"><img id="two" src="two.jpg" width="3024" height="4032" style="width:100%;height:100%;object-fit:contain"></li><li slot="page-3" style="display:flex;flex:0 0 100%;width:100%;height:100%;justify-content:center"><img id="three" src="three.jpg" width="3024" height="4032" style="width:100%;height:100%;object-fit:contain"></li></ul></x-gallery></body>"#,
        );
        let gallery = node_by_id(&dom, "gallery");
        let gallery_shadow = dom.attach_shadow(gallery);
        let wrapper = dom.create_element("div");
        dom.set_attr(
            wrapper,
            "style",
            "display:block;width:100%;height:100%;position:relative",
        );
        dom.append(gallery_shadow, wrapper);
        let faceplate = dom.create_element("faceplate-carousel");
        dom.set_attr(
            faceplate,
            "style",
            "display:flex;position:relative;width:inherit;max-height:540px",
        );
        dom.append(wrapper, faceplate);
        let gallery_slot = dom.create_element("slot");
        dom.append(faceplate, gallery_slot);
        let faceplate_shadow = dom.attach_shadow(faceplate);
        let container = dom.create_element("div");
        dom.set_attr(
            container,
            "style",
            "display:flex;height:auto;align-items:center;position:relative;width:100%",
        );
        dom.append(faceplate_shadow, container);
        let window = dom.create_element("div");
        dom.set_attr(
            window,
            "style",
            "position:relative;overflow:hidden;display:flex;flex-direction:column;height:100%;flex:1",
        );
        dom.append(container, window);
        let list = dom.create_element("div");
        dom.set_attr(
            list,
            "style",
            "height:100%;overflow:hidden;position:relative",
        );
        dom.append(window, list);
        let faceplate_slot = dom.create_element("slot");
        dom.append(list, faceplate_slot);

        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(700.0, 540.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let gallery_box = layout.boxes.get(&gallery).expect("gallery box");
        assert!((gallery_box.height - 540.0).abs() < 0.01, "{gallery_box:?}");
        for id in ["one", "two", "three"] {
            let image = node_by_id(&dom, id);
            let rect = layout.boxes.get(&image).expect("slotted image box");
            assert!((rect.height - 540.0).abs() < 0.01, "{id}: {rect:?}");
            assert!((rect.width - 700.0).abs() < 0.01, "{id}: {rect:?}");
        }
    }

    #[test]
    fn outer_shadow_context_fixes_nested_tile_height_and_stretches_icon_inside() {
        // Archive.org's exact nested-component pattern: the outer shadow tree
        // fixes the custom-element card at 60px; its inner :host rule supplies
        // a 100% default plus the flex presentation. The outer normal height
        // wins by CSS Cascade 5 §6.1, then Flexbox §9.4 stretches the icon
        // only through the card's inner cross size.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><outer-carousel id="outer"></outer-carousel></body>"#,
        );
        let outer = node_by_id(&dom, "outer");
        let outer_root = dom.attach_shadow(outer);
        let outer_style = dom.create_element("style");
        dom.append_text(
            outer_style,
            "onboarding-tile{box-sizing:border-box;width:240px;height:60px}",
        );
        dom.append(outer_root, outer_style);
        let tile = dom.create_element("onboarding-tile");
        dom.append(outer_root, tile);

        let tile_root = dom.attach_shadow(tile);
        let tile_style = dom.create_element("style");
        dom.append_text(
            tile_style,
            ":host{display:flex;align-items:stretch;height:100%;border:2px solid #678}#icon{flex:0 0 60px;padding:5px;background:#def}#icon>img{width:100%;height:100%}#body{flex:1 1 auto;display:flex;align-items:center;padding:5px}",
        );
        dom.append(tile_root, tile_style);
        let icon = dom.create_element("div");
        dom.set_attr(icon, "id", "icon");
        let image = dom.create_element("img");
        dom.set_attr(
            image,
            "src",
            "data:image/svg+xml,%3Csvg viewBox='0 0 100 100' xmlns='http://www.w3.org/2000/svg'%3E%3Cpath d='M0 0h100v100H0z'/%3E%3C/svg%3E",
        );
        dom.append(icon, image);
        dom.append(tile_root, icon);
        let body = dom.create_element("div");
        dom.set_attr(body, "id", "body");
        dom.append_text(body, "How to search the archive");
        dom.append(tile_root, body);

        let base = Url::parse("http://e.com/").unwrap();
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(320.0, 600.0),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let tile_rect = layout.boxes.get(&tile).expect("tile border box");
        let icon_rect = layout.boxes.get(&icon).expect("icon flex item");
        assert!((tile_rect.height - 60.0).abs() < 0.01, "{tile_rect:?}");
        assert!(
            icon_rect.top >= tile_rect.top + 2.0 - 0.01
                && icon_rect.top + icon_rect.height
                    <= tile_rect.top + tile_rect.height - 2.0 + 0.01,
            "stretched icon stays inside the tile padding box: tile={tile_rect:?} icon={icon_rect:?}"
        );
    }

    #[test]
    fn geometry_composes_the_shadow_tree() {
        // measure_boxes lays the LIVE ARENA (real shadow roots), not the
        // pre-flattened Doc.raw the main render uses. Without composing the
        // shadow tree, a shadow-hosted element has NO box and reads 0 — which
        // broke archive.org's <router-slot>/<home-page> shadow app: its
        // infinite-scroller read 0 width, computed 0 columns, and rendered an
        // empty grid. A browser lays out the flat tree; so must the geometry map.
        let base = Url::parse("http://e.com/").unwrap();
        // (a) A shadow HOST renders its shadow root's children in place of light.
        let mut dom = Dom::parse_document(r#"<body style="margin:0"><div id="host"></div></body>"#);
        let host = node_by_id(&dom, "host");
        let shadow = dom.attach_shadow(host);
        let inner = dom.create_element("div");
        dom.set_attr(inner, "id", "inner");
        dom.set_attr(inner, "style", "height:32px");
        dom.append(shadow, inner);
        dom.append_text(inner, "shadow content");
        let boxes = measure_boxes_terminal(
            &dom,
            &base,
            (80, 24),
            &[],
            &HashMap::new(),
            (8, 16),
            &HashMap::new(),
        )
        .0;
        let r = boxes
            .get(&inner)
            .expect("a shadow-hosted div must have a box, not read as detached");
        assert_eq!(
            (r.width, r.height),
            (640.0, 32.0),
            "shadow content fills the 640px viewport width, not 0"
        );

        // (b) A <slot> projects the host's light children into the flat tree.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><div id="host"><div id="light" style="height:32px">L</div></div></body>"#,
        );
        let host = node_by_id(&dom, "host");
        let shadow = dom.attach_shadow(host);
        let slot = dom.create_element("slot");
        dom.append(shadow, slot);
        let light = node_by_id(&dom, "light");
        let boxes = measure_boxes_terminal(
            &dom,
            &base,
            (80, 24),
            &[],
            &HashMap::new(),
            (8, 16),
            &HashMap::new(),
        )
        .0;
        let r = boxes
            .get(&light)
            .expect("a slotted light child must be laid through the slot, not read as 0");
        assert_eq!(
            (r.width, r.height),
            (640.0, 32.0),
            "slotted content is laid at the host's width"
        );
    }

    #[test]
    fn geometry_gives_an_empty_sentinel_a_zero_height_box_in_flow() {
        // The IntersectionObserver idiom: an empty marker div paints nothing,
        // but has an honest zero-height box at its flow position (the old
        // engine faked this with element_tops; layout2 lays a real frag).
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><div style="height:80px">tall</div><div id="s"></div></body>"#,
            40,
            24,
        );
        let s = rect(&dom, &boxes, "s");
        assert_eq!(
            s.top, 80.0,
            "the sentinel sits at the flow position past the tall block"
        );
        assert_eq!(s.height, 0.0);
    }

    #[test]
    fn geometry_scroll_container_keeps_its_border_box() {
        // CSSOM View §6 keeps getBoundingClientRect distinct from scrollHeight:
        // a definite-height overflow:auto box reports its own 32px border box.
        // The separate scrolling-area map (covered through the JS CSSOM test)
        // carries the 192px scrollHeight.
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><div id="sc" style="height:32px;overflow-y:auto">
                 <div style="height:192px">tall content</div>
               </div></body>"#,
            40,
            24,
        );
        let sc = rect(&dom, &boxes, "sc");
        assert_eq!(sc.top, 0.0);
        assert_eq!(
            sc.height, 32.0,
            "getBoundingClientRect reports the 32px box, not its 192px scrolling area"
        );
    }

    #[test]
    fn geometry_resolves_custom_properties_before_intrinsic_flex_sizing() {
        // CSS Custom Properties §3: var() substitution happens at computed-
        // value time, before CSS Sizing §5.2 intrinsic contributions and
        // Flexbox §9.2 clamp a flex item's hypothetical main size. This is the
        // shape used by web-component carousels: a max-content slide contains
        // a definite-width tile, whose icon itself is percentage-sized. If the
        // tile's var()-backed width reaches layout unresolved, the percentage
        // image consumes the intrinsic probe's 10,000,000px sentinel and makes
        // scrollWidth enormous.
        let dom = Dom::parse_document(
            r#"<body style="margin:0;--tile-width:240px;--tile-gap:10px">
                 <div id="sc" style="display:flex;width:320px;overflow-x:scroll;gap:var(--tile-gap)">
                   <div id="one" style="min-width:max-content">
                     <div style="display:flex;width:var(--tile-width)">
                       <div><img src="https://e.com/i.png" style="width:100%"></div><div>one</div>
                     </div>
                   </div>
                   <div id="two" style="min-width:max-content">
                     <div style="display:flex;width:var(--tile-width)">
                       <div><img src="https://e.com/i.png" style="width:100%"></div><div>two</div>
                     </div>
                   </div>
                 </div>
               </body>"#,
        );
        let base = Url::parse("https://e.com/").unwrap();
        let mut images = ImageSizes::new();
        images.insert("https://e.com/i.png".to_string(), (100, 100));
        let (boxes, _, scrolling) = measure_boxes_css(
            &dom,
            &base,
            Viewport::new(800.0, 600.0),
            &[],
            &HashMap::new(),
            &images,
        );
        let one = rect(&dom, &boxes, "one");
        let two = rect(&dom, &boxes, "two");
        assert!((one.width - 240.0).abs() < 0.01, "first slide: {one:?}");
        assert!((two.left - 250.0).abs() < 0.01, "second slide: {two:?}");
        let sc = scrolling.get(&node_by_id(&dom, "sc")).unwrap();
        assert!((sc.width - 490.0).abs() < 0.25, "scrolling area: {sc:?}");
    }

    #[test]
    fn geometry_hidden_clip_box_reports_its_definite_height() {
        // overflow:hidden (a pure clip, not a scroll container) reports its own
        // clipped border box, not the taller content.
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><div id="c" style="height:48px;overflow:hidden">
                 <div style="height:200px">clipped</div>
               </div></body>"#,
            40,
            24,
        );
        assert_eq!(rect(&dom, &boxes, "c").height, 48.0);
    }

    #[test]
    fn geometry_inline_ancestor_aggregates_its_children_boxes() {
        // An inline <a> wrapping a <span> generates no frag of its own; its box
        // is the union of its descendants' pieces (composed-tree aggregation).
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><p style="margin:0"><a id="lnk"><span>hello</span></a></p></body>"#,
            40,
            24,
        );
        let a = rect(&dom, &boxes, "lnk");
        assert_eq!((a.left, a.top), (0.0, 0.0));
        let shaped = crate::text::shape("hello", &crate::text::TextStyle::default());
        assert!((a.width - f64::from(shaped.advance)).abs() < 0.01);
    }

    // ---- P7: incremental region patch (measure::region_buffer) -------------

    #[test]
    fn a_region_patch_buffer_matches_the_full_render_region() {
        // incremental-layout contract §9 differential guard, layout2 edition: the
        // region buffer laid from a serialized PATCH fragment (re-parsed,
        // ancestor-less, inherited context MATERIALIZED by serialize_patch) is
        // byte-for-byte the region a full `lay_out_document` produces. The region
        // inherits bold+uppercase from <body>; drop the materialization and the
        // fragment renders non-bold/lowercase and the buffers diverge.
        let base = Url::parse("https://example.com/").unwrap();
        let mut html = String::from(
            r#"<html style="height:100%"><body style="height:100%;font-weight:bold;text-transform:uppercase">"#,
        );
        html.push_str(r#"<div id="chat" style="height:100%;overflow-y:scroll;width:30ch">"#);
        for i in 0..12 {
            html.push_str(&format!("<div>msg{i:02}</div>"));
        }
        html.push_str("</div></body></html>");
        let dom = Dom::parse_document(&html);
        let viewport = TerminalViewport::new(40, 8, 8.0, 16.0);
        // FULL render: the region buffer as the page produces it.
        let full = lay_out_document(
            &dom,
            &base,
            viewport,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(full.regions.len(), 1, "one scroll region");
        let region = &full.regions[0];
        let boundary = region.node;
        // PATCH: serialize the boundary (materialized) → re-parse → re-lay.
        let frag = dom.serialize_patch(boundary, &std::collections::HashSet::new());
        let fdom = Dom::parse_document(&frag);
        let fnode = fdom
            .descendants(crate::dom::DOCUMENT)
            .find(|&n| fdom.attr(n, "data-trust-node").is_some())
            .expect("the patch fragment bakes data-trust-node on the boundary");
        let (rows, _car, _clips) = lay_region_fragment(
            &fdom,
            &base,
            region.width as usize,
            viewport,
            &HashMap::new(),
            &HashMap::new(),
            fnode,
        );
        assert_eq!(rows.len(), region.buffer.len(), "same buffer height");
        for (a, b) in rows.iter().zip(region.buffer.iter()) {
            assert_eq!(row_text(a), row_text(b), "same rendered text per row");
            let bolds_a: Vec<bool> = a.items.iter().map(|it| it.emph.bold).collect();
            let bolds_b: Vec<bool> = b.items.iter().map(|it| it.emph.bold).collect();
            assert_eq!(
                bolds_a, bolds_b,
                "materialized font-weight matches per item"
            );
        }
        // Guard against both being wrong-but-equal: the inherited styling really
        // reached the content (uppercase + bold).
        assert!(
            region
                .buffer
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.emph.bold),
            "inherited bold reached the region content"
        );
        assert!(
            row_text(&region.buffer[0]).contains("MSG00"),
            "inherited text-transform:uppercase applied"
        );
    }

    #[test]
    fn an_inline_ifc_boundary_is_captured_with_its_band() {
        // A block-filling IFC container (display:flow-root) baked with
        // data-trust-node is captured as an inline boundary; a plain block is
        // NOT (it doesn't establish an independent formatting context).
        let out = lay(
            r#"<body style="margin:0"><div data-trust-node="7" style="display:flow-root"><p style="margin:0">a</p><p style="margin:0">b</p></div><div data-trust-node="8"><p>plain</p></div></body>"#,
            40,
        );
        assert_eq!(out.boundaries.len(), 1, "only the IFC box is a boundary");
        let b = &out.boundaries[0];
        assert_eq!(b.node, 7);
        assert_eq!(b.origin_col, 0);
        assert!(!b.sub_box);
        assert_eq!(b.row_range, 0..2, "two 1-row paragraphs");
    }

    #[test]
    fn an_inline_boundary_fragment_lays_like_the_full_document() {
        // incremental-layout contract §9: a block-filling IFC boundary re-laid
        // from a serialized PATCH fragment (materialized inheritance) is byte-
        // for-byte the rows the FULL render produced for it. The boundary
        // inherits bold from <body>; without §4a materialization the fragment
        // renders non-bold and diverges.
        let base = Url::parse("http://e.com/").unwrap();
        let mut html = String::from(
            r#"<html><body style="margin:0;font-weight:bold"><div id="feed" data-trust-node="7" style="display:flow-root">"#,
        );
        for i in 0..6 {
            html.push_str(&format!(r#"<p style="margin:0">item{i:02} word</p>"#));
        }
        html.push_str("</div></body></html>");
        let dom = Dom::parse_document(&html);
        let viewport = TerminalViewport::new(30, 24, 8.0, 16.0);
        let full = lay_out_document(
            &dom,
            &base,
            viewport,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(full.boundaries.len(), 1);
        let b = full.boundaries[0].clone();
        let full_rows = &full.rows[b.row_range.clone()];
        // PATCH: serialize the boundary (materialized) → re-parse → re-lay.
        let bnode = node_by_id(&dom, "feed");
        let frag = dom.serialize_patch(bnode, &std::collections::HashSet::new());
        let fdom = Dom::parse_document(&frag);
        let fnode = fdom
            .descendants(crate::dom::DOCUMENT)
            .find(|&n| fdom.attr(n, "data-trust-node").is_some())
            .expect("serialize_patch bakes data-trust-node on the boundary");
        let sub = lay_subtree_fragment(
            &fdom,
            &base,
            b.content_width as usize,
            viewport,
            &HashMap::new(),
            &HashMap::new(),
            fnode,
            false,
            b.quantization_phase,
        );
        assert_eq!(sub.rows.len(), b.row_range.len(), "same height");
        assert_eq!(b.origin_col, 0, "at the body's left edge");
        for (fr, fullr) in sub.rows.iter().zip(full_rows.iter()) {
            assert_eq!(row_text(fr), row_text(fullr), "same rendered text per row");
            let a: Vec<bool> = fr.items.iter().map(|it| it.emph.bold).collect();
            let c: Vec<bool> = fullr.items.iter().map(|it| it.emph.bold).collect();
            assert_eq!(a, c, "materialized bold matches per item");
        }
        assert!(
            full_rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.emph.bold),
            "inherited bold reached the boundary content"
        );
    }

    #[test]
    fn geometry_documentelement_covers_the_whole_document_height() {
        let (dom, boxes) = measure(
            r#"<body style="margin:0"><div style="height:100px">a</div><div style="height:60px">b</div></body>"#,
            40,
            24,
        );
        let html = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&n| dom.tag_name(n) == Some("html"))
            .unwrap();
        assert_eq!(
            boxes.get(&html).unwrap().height,
            160.0,
            "documentElement covers the full 160px document"
        );
    }

    // ---- the P8 gate: alpha-composite of transparent image overlaps ----

    /// Two `<img>` abspos-stacked in a positioned box, `badge` offset `left_px`
    /// from `base` so they partially overlap. Both are 6×4 cells; `badge`'s
    /// transparency is `badge_alpha`.
    fn overlap_page(left_px: u32, base_alpha: bool, badge_alpha: bool) -> Output {
        let images = img_sizes(&[
            ("http://e.com/base.png", 6, 4),
            ("http://e.com/badge.png", 6, 4),
        ]);
        let mut alpha = HashMap::new();
        alpha.insert("http://e.com/base.png".to_string(), base_alpha);
        alpha.insert("http://e.com/badge.png".to_string(), badge_alpha);
        let html = format!(
            r#"<body style="margin:0"><div style="position:relative;width:120px;height:64px">
                <img src="base.png" style="position:absolute;left:0;top:0">
                <img src="badge.png" style="position:absolute;left:{left_px}px;top:0">
               </div></body>"#
        );
        lay_full(&html, 80, &images, &alpha)
    }

    fn image_items(out: &Output) -> Vec<&Item> {
        out.rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| it.kind == ItemKind::Image)
            .collect()
    }

    #[test]
    fn a_transparent_image_over_another_folds_into_one_composite() {
        // base at col 0, badge at col 2 (16px) — they overlap cols 2..6. The
        // badge is transparent, so the pair becomes ONE synthetic composite the
        // app alpha-blends (the base shows through the badge's holes).
        let out = overlap_page(16, false, true);
        let imgs = image_items(&out);
        assert_eq!(imgs.len(), 1, "the overlap folds into one emission");
        let key = imgs[0].image.as_deref().unwrap();
        assert!(
            key.starts_with("x-trust-composite:"),
            "the emission is a composite ({key})"
        );
        // Union box: base cols 0..6 ∪ badge cols 2..8 = cols 0..8, 6→8 wide, 4 tall.
        assert_eq!((imgs[0].col, imgs[0].width, imgs[0].height), (0, 8, 4));
        // The side-table holds both layers in paint order (base first), with the
        // badge offset two cells into the union.
        let layers = out.composites.get(key).expect("layers registered");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].url, "http://e.com/base.png");
        assert_eq!(
            (layers[0].dcol, layers[0].drow, layers[0].w, layers[0].h),
            (0, 0, 6, 4)
        );
        assert_eq!(layers[1].url, "http://e.com/badge.png");
        assert_eq!(
            (layers[1].dcol, layers[1].drow, layers[1].w, layers[1].h),
            (2, 0, 6, 4)
        );
    }

    #[test]
    fn an_opaque_image_overlap_stays_two_separate_items() {
        // Same geometry, but the badge is OPAQUE — cell-overwrite is pixel-exact,
        // so the pair stays two cheap separate items (no composite, no re-encode
        // cost on a mutation of one).
        let out = overlap_page(16, false, false);
        assert!(
            out.composites.is_empty(),
            "no composite for an opaque overlap"
        );
        let imgs = image_items(&out);
        assert_eq!(imgs.len(), 2, "both images emit separately");
        assert!(
            imgs.iter().all(|it| !it
                .image
                .as_deref()
                .unwrap()
                .starts_with("x-trust-composite:")),
            "neither item is a composite"
        );
    }

    #[test]
    fn non_overlapping_transparent_images_are_not_grouped() {
        // badge 96px (12 cells) right of base — no overlap, so even a transparent
        // badge is left as its own item (nothing to composite through).
        let out = overlap_page(96, false, true);
        assert!(out.composites.is_empty(), "no overlap ⇒ no composite");
        assert_eq!(image_items(&out).len(), 2);
    }

    #[test]
    fn a_lone_transparent_image_is_never_a_composite() {
        // A single transparent image is unchanged — grouping needs ≥2 overlapping
        // images, so the zero-regression single-image path stays byte-identical.
        let images = img_sizes(&[("http://e.com/solo.png", 6, 4)]);
        let mut alpha = HashMap::new();
        alpha.insert("http://e.com/solo.png".to_string(), true);
        let out = lay_full(
            r#"<body style="margin:0"><img src="solo.png"></body>"#,
            80,
            &images,
            &alpha,
        );
        assert!(out.composites.is_empty());
        let imgs = image_items(&out);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].image.as_deref(), Some("http://e.com/solo.png"));
    }

    /// P8 perf gate (ignored — a timing measurement, not a pass/fail assert):
    /// `TRUST_LAYOUT2_BENCH=1 cargo test p8_layout_bench -- --ignored --nocapture`.
    /// Lays out a GitHub-scale synthetic page (a header, a nav flex row, and a
    /// grid of ~300 cards each with heading/text/image, several levels deep —
    /// ~5–6k elements) and reports ms/layout. Budget: low tens of ms.
    #[test]
    #[ignore = "manual perf measurement"]
    fn p8_layout_bench() {
        let mut html =
            String::from(r#"<body style="margin:0"><header style="display:flex;gap:8px">"#);
        for i in 0..12 {
            html.push_str(&format!(
                r#"<a href="/n{i}" style="padding:4px">Nav item {i}</a>"#
            ));
        }
        html.push_str(
            r#"</header><main style="display:grid;grid-template-columns:repeat(3,1fr);gap:16px">"#,
        );
        for i in 0..300 {
            html.push_str(&format!(
                r#"<article style="display:flex;flex-direction:column;gap:4px;padding:8px">
                    <img src="/thumb{}.png" alt="thumb">
                    <h3>Card heading number {i} with a fairly long title that wraps</h3>
                    <p>Body copy for card {i}: several words of running text that the
                       inline breaker has to lay across the card's flexed width, with a
                       <a href="/c{i}">link</a> in the middle for good measure.</p>
                    <div style="display:flex;gap:4px"><span>a</span><span>b</span><span>c</span></div>
                   </article>"#,
                i % 20
            ));
        }
        html.push_str("</main></body>");
        let images: ImageSizes = (0..20)
            .map(|i| (format!("http://e.com/thumb{i}.png"), (96u32, 128u32)))
            .collect();
        let dom = Dom::parse_document(&html);
        let base = Url::parse("http://e.com/").unwrap();
        // Warm once (fills the style index / rule-hash caches shared per epoch),
        // then time repeated full layouts (each builds a fresh Flow — no cross-
        // call intrinsic memo, the real per-render cost).
        let _ = lay_out_document(
            &dom,
            &base,
            TerminalViewport::new(120, 40, 8.0, 16.0),
            &[],
            &HashMap::new(),
            &images,
            &HashMap::new(),
        );
        let iters = 20;
        let t0 = std::time::Instant::now();
        let mut rows = 0;
        for _ in 0..iters {
            let out = lay_out_document(
                &dom,
                &base,
                TerminalViewport::new(120, 40, 8.0, 16.0),
                &[],
                &HashMap::new(),
                &images,
                &HashMap::new(),
            );
            rows = out.rows.len();
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(iters);
        println!(
            "p8_layout_bench: {} nodes → {rows} rows, {per:.2} ms/layout (budget: low tens of ms)",
            dom.node_count()
        );
    }

    #[test]
    fn an_empty_alpha_map_disables_grouping_entirely() {
        // The pre-decode / harness default (no alpha info) never groups, so the
        // always-correct separate-image path is preserved.
        let images = img_sizes(&[
            ("http://e.com/base.png", 6, 4),
            ("http://e.com/badge.png", 6, 4),
        ]);
        let out = lay_full(
            r#"<body style="margin:0"><div style="position:relative;width:120px;height:64px">
                <img src="base.png" style="position:absolute;left:0;top:0">
                <img src="badge.png" style="position:absolute;left:16px;top:0">
               </div></body>"#,
            80,
            &images,
            &HashMap::new(),
        );
        assert!(out.composites.is_empty(), "empty alpha ⇒ no grouping");
    }

    // ---- P8b-1: floats (CSS 2.1 §9.5) ----------------------------------------
    // Cells are 8×16px, body margin 0 → content at col 0. Image sizes are in
    // cells (12×4 = 96×64px = 12 cols, 4 rows).

    /// The row/col of the first item whose text contains `t`.
    fn at(out: &Output, t: &str) -> (usize, usize) {
        let (r, it) = find(out, t);
        (r, it.col as usize)
    }

    #[test]
    fn float_left_shortens_every_line_box_beside_it() {
        // A 12×3 left float and a paragraph long enough to overflow it: the text
        // flows in the shortened band beside the float on EVERY overlapping row
        // (not just the first), then returns to full width below — the
        // humantooth readability case.
        let images = img_sizes(&[("http://e.com/f.png", 12, 3)]);
        let words = (0..60).map(|_| "aa").collect::<Vec<_>>().join(" ");
        let out = lay_images(
            &format!(
                r#"<body style="margin:0"><img src="f.png" style="float:left" alt="F"><p style="margin:0">{words}</p></body>"#
            ),
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.col, img.width, img.height), (0, 12, 3));
        // Rows 0..3 (the float's height) each carry text, shortened into
        // [12, 40) — beside the float on EVERY row, not only the first.
        for r in 0..3 {
            let leftmost = out.rows[r]
                .items
                .iter()
                .filter(|i| i.image.is_none() && !i.text.trim().is_empty())
                .map(|i| i.col)
                .min();
            assert_eq!(
                leftmost,
                Some(12),
                "row {r} text sits just right of the float"
            );
        }
        // Below the float, text returns to the left edge.
        let below = out.rows.iter().skip(3).any(|row| {
            row.items
                .iter()
                .any(|i| !i.text.trim().is_empty() && i.col == 0)
        });
        assert!(below, "text returns to full width below the 3-row float");
    }

    #[test]
    fn graphical_float_bands_follow_variable_height_line_boxes() {
        let images = HashMap::from([("http://e.com/f.png".to_string(), (48u32, 50u32))]);
        let words = (0..30).map(|_| "word").collect::<Vec<_>>().join(" ");
        let layout = lay_graphical(
            &format!(
                r#"<body style="margin:0"><img src="f.png" style="float:left"><p style="margin:0;font-size:24px;line-height:1.1">{words}</p></body>"#
            ),
            180.0,
            &images,
        );
        let origins: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                crate::render::Primitive::GlyphRun { origin, .. } => Some(*origin),
                _ => None,
            })
            .collect();
        assert!(
            origins
                .iter()
                .filter(|origin| origin.y < 50.0)
                .all(|origin| origin.x >= 48.0),
            "every overlapping variable-height line is shortened by the float"
        );
        assert!(
            origins
                .iter()
                .any(|origin| origin.y >= 50.0 && origin.x < 1.0),
            "a later line returns to the full band below the 50px float"
        );
    }

    #[test]
    fn float_right_pins_to_the_right_edge() {
        let images = img_sizes(&[("http://e.com/f.png", 10, 3)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="f.png" style="float:right" alt="F"><p style="margin:0">alpha beta gamma delta epsilon zeta eta theta</p></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(img.col, 30, "float:right pinned to 40-10");
        // No text crosses into the float's columns.
        let max_right = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_none())
            .map(|i| i.col + i.width)
            .max()
            .unwrap_or(0);
        assert!(
            max_right <= 30,
            "text stays left of the right float: {max_right}"
        );
    }

    #[test]
    fn two_left_floats_stack_then_the_third_drops() {
        // Three 15×3 left floats in 40 cols: two fit side by side (0, 15); the
        // third can't (30+15 > 40) so it drops to a fresh shelf below (§9.5.1
        // rule 2 — later same-side floats go right OR lower).
        let images = img_sizes(&[
            ("http://e.com/a.png", 15, 3),
            ("http://e.com/b.png", 15, 3),
            ("http://e.com/c.png", 15, 3),
        ]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="a.png" style="float:left" alt="A"><img src="b.png" style="float:left" alt="B"><img src="c.png" style="float:left" alt="C"></body>"#,
            40,
            &images,
        );
        let imgs: Vec<&Item> = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .collect();
        let a = imgs
            .iter()
            .find(|i| i.image.as_deref() == Some("http://e.com/a.png"))
            .unwrap();
        let b = imgs
            .iter()
            .find(|i| i.image.as_deref() == Some("http://e.com/b.png"))
            .unwrap();
        assert_eq!(a.col, 0);
        assert_eq!(b.col, 15, "second left float sits beside the first");
        // The third dropped: it must occupy a row at/after the first shelf's
        // bottom (row 3), back at the left edge.
        let c_row = out
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| {
                row.items
                    .iter()
                    .any(|i| i.image.as_deref() == Some("http://e.com/c.png"))
            })
            .map(|(r, _)| r)
            .expect("third float placed");
        assert!(
            c_row >= 3,
            "third float dropped to a new shelf: row {c_row}"
        );
    }

    #[test]
    fn clear_on_float_drops_below_the_entire_previous_shelf() {
        // CSS 2.2 §9.5.2 adds constraint #10 for a floated box with `clear`:
        // its outer top must be below every earlier float on the cleared side.
        // Bootstrap-style album grids rely on this on every sixth child. If
        // the constraint is ignored, a shorter sixth column leaves room beside
        // a taller earlier column and produces the site's recurring 6/2/6 rows.
        let html = r#"
            <style>
              #albums { width: 1320px }
              .album { float: left; width: 220px; height: 100px }
              #a4 { height: 130px }
              .album:nth-child(6n + 1) { clear: left }
            </style>
            <body style="margin:0"><div id="albums">
              <div id="a1" class="album"></div><div id="a2" class="album"></div>
              <div id="a3" class="album"></div><div id="a4" class="album"></div>
              <div id="a5" class="album"></div><div id="a6" class="album"></div>
              <div id="a7" class="album"></div><div id="a8" class="album"></div>
              <div id="a9" class="album"></div><div id="a10" class="album"></div>
              <div id="a11" class="album"></div><div id="a12" class="album"></div>
              <div id="a13" class="album"></div>
            </div></body>
        "#;
        let dom = Dom::parse_document(html);
        let layout = lay_graphical(html, 1320.0, &HashMap::new());
        let rect = |id: &str| {
            let node = dom.get_by_id(id).unwrap();
            layout.boxes.get(&node).copied().unwrap_or_else(|| {
                panic!("missing graphical box for #{id}");
            })
        };
        let a4 = rect("a4");
        let a7 = rect("a7");
        let a8 = rect("a8");
        let a13 = rect("a13");
        assert!(
            a7.top >= a4.top + a4.height - 0.01,
            "clear:left float starts below the tallest previous float: a4={a4:?}, a7={a7:?}"
        );
        assert!(
            (a8.top - a7.top).abs() < 0.01,
            "the cleared shelf remains a six-column row: a7={a7:?}, a8={a8:?}"
        );
        assert!(
            a13.top >= a7.top + a7.height - 0.01,
            "the next clear:left float starts below the complete second shelf: a7={a7:?}, a13={a13:?}"
        );
    }

    #[test]
    fn left_and_right_float_frame_the_text_between() {
        // A left float and a right float on the same band; text fills the gap
        // between them (§9.5.1 rule 3 — a left float's right edge stays left of
        // an adjacent right float).
        let images = img_sizes(&[("http://e.com/l.png", 8, 3), ("http://e.com/r.png", 8, 3)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="l.png" style="float:left" alt="L"><img src="r.png" style="float:right" alt="R"><p style="margin:0">one two three four five six</p></body>"#,
            40,
            &images,
        );
        let (_, txt) = find(&out, "one");
        assert!(txt.col >= 8, "text starts right of the left float");
        let right_edge = out.rows[0]
            .items
            .iter()
            .filter(|i| i.image.is_none())
            .map(|i| i.col + i.width)
            .max()
            .unwrap_or(0);
        assert!(right_edge <= 32, "text ends left of the right float (40-8)");
    }

    #[test]
    fn clear_both_drops_a_block_below_the_float() {
        let images = img_sizes(&[("http://e.com/f.png", 12, 5)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="f.png" style="float:left" alt="F"><p style="margin:0">beside</p><p style="margin:0;clear:both">cleared</p></body>"#,
            40,
            &images,
        );
        let beside = at(&out, "beside");
        let cleared = at(&out, "cleared");
        assert!(beside.1 >= 12, "first para sits beside the float");
        assert_eq!(cleared.1, 0, "cleared para is full width");
        assert!(
            cleared.0 >= 5,
            "clear:both drops it below the 5-row float: {}",
            cleared.0
        );
    }

    #[test]
    fn a_float_wraps_content_across_following_blocks() {
        // A tall float beside two separate paragraphs: both flow beside it
        // (floats persist across sibling blocks in the same BFC — §9.5).
        let images = img_sizes(&[("http://e.com/f.png", 12, 6)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="f.png" style="float:left" alt="F"><p style="margin:0">one two</p><p style="margin:0">four five</p></body>"#,
            40,
            &images,
        );
        let one = at(&out, "one");
        let four = at(&out, "four");
        assert!(one.1 >= 12, "first block beside the float");
        assert!(
            four.1 >= 12,
            "second block ALSO beside the float across blocks"
        );
        assert!(four.0 < 6, "both within the 6-row float's height");
    }

    #[test]
    fn auto_width_float_shrinks_to_fit_its_content() {
        // A width:auto float sizes to its content (shrink-to-fit, §10.3.5), so
        // the text beside it starts just past that content, not at some full
        // column width.
        let out = lay(
            r#"<body style="margin:0"><div style="float:left">Hi</div><p style="margin:0">beside the tag</p></body>"#,
            40,
        );
        let f = at(&out, "Hi");
        let beside = at(&out, "beside");
        assert_eq!(f.1, 0, "float at the left edge");
        assert_eq!(beside.1, 2, "text starts past the 2-cell 'Hi' float");
    }

    #[test]
    fn a_bfc_container_grows_to_contain_its_float() {
        // An `overflow:hidden` (BFC) box grows to enclose a float taller than
        // its other content (the clearfix idiom), so the following block starts
        // below the whole float, at full width.
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let out = lay_images(
            r#"<body style="margin:0"><div style="overflow:hidden"><img src="f.png" style="float:left" alt="F"></div><p style="margin:0">after</p></body>"#,
            40,
            &images,
        );
        let after = at(&out, "after");
        assert_eq!(after.1, 0, "following block is full width");
        assert!(
            after.0 >= 4,
            "the BFC contained the 4-row float, pushing `after` below it: row {}",
            after.0
        );
    }

    #[test]
    fn an_auto_width_bfc_uses_the_remaining_band_beside_a_float() {
        // CSS 2.2 §9.5: the BFC root's BORDER box may be narrowed beside a
        // float, but must not overlap the float's MARGIN box. This is the
        // canonical media-object/article-list pattern used by 9to5linux.com.
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let (dom, boxes) = measure_images(
            r#"<body style="margin:0"><img src="f.png" style="float:left;margin-right:16px"><div id="content" style="overflow:hidden"><p style="margin:0">headline and summary</p></div></body>"#,
            40,
            20,
            &images,
        );
        let content = rect(&dom, &boxes, "content");
        assert_eq!(content.left, 96.0, "80px image + 16px float margin");
        assert_eq!(content.width, 224.0, "the BFC uses the remaining 28 cols");
    }

    #[test]
    fn a_right_float_narrows_an_auto_width_bfc_from_the_other_side() {
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let (dom, boxes) = measure_images(
            r#"<body style="margin:0"><img src="f.png" style="float:right;margin-left:16px"><div id="content" style="overflow:hidden"><p style="margin:0">headline and summary</p></div></body>"#,
            40,
            20,
            &images,
        );
        let content = rect(&dom, &boxes, "content");
        assert_eq!(content.left, 0.0);
        assert_eq!(content.width, 224.0, "the right float excludes 12 cols");
    }

    #[test]
    fn a_definite_bfc_that_cannot_fit_is_placed_below_the_float() {
        // CSS 2.2 §9.5 permits clearing the BFC root when its used width
        // cannot fit beside the preceding float.
        let images = img_sizes(&[("http://e.com/f.png", 12, 4)]);
        let (dom, boxes) = measure_images(
            r#"<body style="margin:0"><img src="f.png" style="float:left"><div id="content" style="overflow:hidden;width:280px"><p style="margin:0">wide</p></div></body>"#,
            40,
            20,
            &images,
        );
        let content = rect(&dom, &boxes, "content");
        assert_eq!(content.left, 0.0);
        assert!(
            content.top >= 64.0,
            "the 280px BFC clears the 96px-wide, 64px-tall float: {content:?}"
        );
        assert_eq!(content.width, 280.0);
    }

    #[test]
    fn overflow_clip_does_not_establish_a_bfc() {
        // CSS Overflow 3 §3.1 explicitly distinguishes `clip` from
        // `hidden`: clip does not establish an independent formatting context,
        // so the block box remains full-width while its LINE boxes wrap.
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let (dom, boxes) = measure_images(
            r#"<body style="margin:0"><img src="f.png" style="float:left"><div id="content" style="overflow:clip"><p style="margin:0">headline and summary</p></div></body>"#,
            40,
            20,
            &images,
        );
        let content = rect(&dom, &boxes, "content");
        assert_eq!(content.left, 0.0);
        assert_eq!(content.width, 320.0);
    }

    #[test]
    fn a_clearing_pseudo_contains_the_media_float_without_isolating_text() {
        // The generated `::after { clear:both }` is a final in-flow child, not
        // a new formatting context. It expands the article through the float;
        // the overflow-hidden sibling independently takes the adjacent band.
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let (dom, boxes) = measure_images(
            r#"<style>.clearfix::after{content:"";display:table;clear:both}</style><body style="margin:0"><article id="entry" class="clearfix"><img src="f.png" style="float:left;margin-right:16px"><div id="content" style="overflow:hidden"><p style="margin:0">headline and summary</p></div></article><div id="next">next article</div></body>"#,
            40,
            20,
            &images,
        );
        let entry = rect(&dom, &boxes, "entry");
        let content = rect(&dom, &boxes, "content");
        let next = rect(&dom, &boxes, "next");
        assert_eq!((content.left, content.width), (96.0, 224.0));
        assert!(
            entry.height >= 64.0,
            "clearfix contains the float: {entry:?}"
        );
        assert!(
            next.top >= 64.0,
            "the next article starts below the contained float: {next:?}"
        );
    }

    #[test]
    fn clear_on_an_ungenerated_pseudo_does_not_create_a_clearfix() {
        // CSS Content 3 §1: `content:normal` computes to `none` on ::after,
        // inhibiting pseudo-element creation. Its `clear` declaration therefore
        // cannot make the float-only parent contain its float.
        let images = img_sizes(&[("http://e.com/f.png", 10, 4)]);
        let (dom, boxes) = measure_images(
            r#"<style>.not-a-clearfix::after{clear:both}</style><body style="margin:0"><div id="entry" class="not-a-clearfix"><img src="f.png" style="float:left"></div><div id="next">next</div></body>"#,
            40,
            20,
            &images,
        );
        assert_eq!(rect(&dom, &boxes, "entry").height, 0.0);
        assert_eq!(
            rect(&dom, &boxes, "next").top,
            0.0,
            "the following block remains beside the uncontained float"
        );
    }

    // ---- P8b-2: multi-column (css-multicol-1) --------------------------------
    // 40 cols = 320px, `column-gap:normal` = 1em = 16px = 2 cells. column-count:2
    // ⇒ N=2, W=(320-16)/2 = 152px = 19 cells; column 0 at col 0, column 1 at
    // 168px = col 21.

    /// The set of distinct starting columns of non-blank text items.
    fn text_cols(out: &Output) -> Vec<usize> {
        let mut cs: Vec<usize> = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| !i.text.trim().is_empty())
            .map(|i| i.col as usize)
            .collect();
        cs.sort_unstable();
        cs.dedup();
        cs
    }

    #[test]
    fn column_count_2_balances_into_two_columns() {
        let words = (0..40)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = lay(
            &format!(
                r#"<body style="margin:0"><div style="column-count:2"><p style="margin:0">{words}</p></div></body>"#
            ),
            40,
        );
        // Column 0 sits at the left edge; column 1 past the 19-cell column + gap.
        let cols = text_cols(&out);
        assert!(cols.contains(&0), "column 0 at the left edge: {cols:?}");
        assert!(
            cols.iter().any(|&c| (21..40).contains(&c)),
            "column 1 starts past the gap (col ~21): {cols:?}"
        );
        // No text sits in the gap [19, 21).
        assert!(
            !cols.iter().any(|&c| (19..21).contains(&c)),
            "the column gap is empty: {cols:?}"
        );
        // Column 1 was lifted to the top by balancing, not stacked below.
        let col1_top = out.rows.iter().position(|r| {
            r.items
                .iter()
                .any(|i| i.col >= 21 && !i.text.trim().is_empty())
        });
        assert!(
            col1_top.is_some_and(|r| r <= 1),
            "column 1 begins near the top: {col1_top:?}"
        );
    }

    #[test]
    fn column_width_resolves_the_count_from_available_width() {
        // §3.4 case 2: column-width:150px in 320px, gap 16px →
        // N = floor((320+16)/(150+16)) = floor(2.02) = 2.
        let words = (0..40)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = lay(
            &format!(
                r#"<body style="margin:0"><div style="column-width:150px"><p style="margin:0">{words}</p></div></body>"#
            ),
            40,
        );
        let cols = text_cols(&out);
        assert!(cols.contains(&0), "column 0: {cols:?}");
        assert!(
            cols.iter().any(|&c| c >= 21),
            "a second column resolved: {cols:?}"
        );
    }

    #[test]
    fn column_count_caps_the_width_derived_count() {
        // §3.4 case 3: both specified — count is the min of the two. A narrow
        // column-width would allow 3 columns in 60 cols, but column-count:2 caps
        // it at 2.
        let words = (0..60)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = lay(
            &format!(
                r#"<body style="margin:0"><div style="column-count:2;column-width:80px"><p style="margin:0">{words}</p></div></body>"#
            ),
            60,
        );
        // 60 cols = 480px; column-width 80px would give floor((480+16)/96)=5,
        // capped to 2. Two columns: 0 and past the midpoint gap.
        let cols = text_cols(&out);
        let far = cols.iter().copied().max().unwrap_or(0);
        assert!(cols.contains(&0), "column 0: {cols:?}");
        assert!(
            (28..40).contains(&far),
            "exactly two columns (2nd near mid): far col {far}"
        );
    }

    #[test]
    fn column_count_1_is_plain_block_flow() {
        // A single column is a no-op: text fills the full width and never leaves
        // a mid-column gap.
        let words = (0..30)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = lay(
            &format!(
                r#"<body style="margin:0"><div style="column-count:1"><p style="margin:0">{words}</p></div></body>"#
            ),
            40,
        );
        // Full-width flow reaches well past a 19-cell column boundary on line 0.
        let line0_right = out.rows[0]
            .items
            .iter()
            .map(|i| i.col + i.width)
            .max()
            .unwrap_or(0);
        assert!(
            line0_right > 30,
            "single column fills the full width: {line0_right}"
        );
    }

    #[test]
    fn column_count_3_makes_three_columns() {
        // 60 cols = 480px, gap 16px, N=3, W=(480-32)/3 ≈ 149px ≈ 18 cells;
        // columns at 0, ~21, ~41.
        let words = (0..60)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = lay(
            &format!(
                r#"<body style="margin:0"><div style="column-count:3"><p style="margin:0">{words}</p></div></body>"#
            ),
            60,
        );
        let cols = text_cols(&out);
        assert!(cols.contains(&0), "col 0: {cols:?}");
        assert!(
            cols.iter().any(|&c| (18..26).contains(&c)),
            "col 1 region: {cols:?}"
        );
        assert!(
            cols.iter().any(|&c| (38..48).contains(&c)),
            "col 2 region: {cols:?}"
        );
    }
}
