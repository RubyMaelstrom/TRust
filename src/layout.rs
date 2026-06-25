//! HTTP-mode page layout: arena DOM → rows of positioned inline items.
//!
//! This is the foundation of the HTML layout arc (L1). Unlike the
//! gopher/gemini line model (one selectable link per row), an HTTP page
//! lays out as a vertical stack of `Row`s, each a left-to-right sequence
//! of positioned `Item`s — so a row can hold several links (multi-link
//! rows) and, later, inline-image and form boxes. Vertical scroll still
//! indexes by row; lateral navigation (L2) indexes by item.
//!
//! The pass is a minimal block/inline flow over the arena DOM
//! (`dom.rs`): block elements break the line and stack; inline content
//! flows and word-wraps into rows at the content width. It reads the
//! DOM's own visibility cascade (`Dom::is_hidden`) so `display:none`
//! subtrees never render. This replaces html2text for HTTP — we own the
//! tree, so there is no rcdom round-trip and no marker-`<img>` splice.
//!
//! L1 here is TEXT ONLY: images render their `alt` text and form
//! controls render simple stubs; real inline images (L3) and live form
//! controls fold in later.

use std::collections::HashMap;
use std::collections::HashSet;

use ratatui::symbols::line;
use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::doc::{Form, Link};
use crate::dom::{DOCUMENT, Dom, NodeData, NodeId};

/// The terminal display width of a string in cells. Wide glyphs (CJK, many
/// emoji) occupy two cells, combining marks zero — `chars().count()` gets
/// both wrong and drifts aligned/`pre` text. We measure with the SAME
/// `unicode-width` ratatui renders with, so an item's `width`/`col` match
/// where the glyphs actually land on screen.
pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest leading prefix of `s` whose display width is `<= max` cells —
/// for truncating a clipped (`overflow:hidden`) line before its ellipsis.
fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Map from a control element's `NodeId` to its `(form, field)` indices
/// (built by `http::extract_forms_arena`), so the layout can surface
/// form controls as selectable `Link::Form` items.
pub type ControlMap = HashMap<NodeId, (usize, usize)>;

/// Map from an image's absolute URL to its decoded cell box `(width,
/// height)`, built by the app's decode pipeline. An image present here
/// lays out as a real W×H box (reserving rows); one absent falls back to
/// alt text.
pub type ImageSizes = HashMap<String, (u16, u16)>;

/// Recorded flow position `(col, row)` in cells of each element entered during
/// a measurement pass — the geometry an EMPTY element (an infinite-scroll
/// sentinel) gets when it paints no cells. Carried on `LaidBox` so `blit`
/// translates it up through nested sub-layouts.
type ElementTops = HashMap<NodeId, (u16, u16)>;

/// Sentinel `NodeId` for an item that came from no single element
/// (synthesized text like list markers).
pub const NO_NODE: NodeId = usize::MAX;

/// A laid-out element's box in CSS pixels — the backing for the JS geometry
/// APIs (`getBoundingClientRect`, `offset*`/`client*`, IntersectionObserver/
/// ResizeObserver records). `left`/`top` are the element's document-origin
/// position and `width`/`height` its size, each a whole number of terminal
/// cells scaled by the cell's pixel size. The cell quantization is deliberate:
/// the geometry a page reads back must agree with what we actually paint, so a
/// page that measures and then renders sees the box it will really get (we
/// cannot draw sub-cell, so reporting sub-cell precision would be a fiction).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PxRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// Whether CSS borders render as box-drawing chrome. Session-global,
/// default OFF (her call — terminal vertical space is at a premium and most
/// page borders are subtle 1px underlines not worth a cell row each). The
/// `set borders on` command flips it; `parse_seeded` reads it when laying a
/// document out. See `Layout::borders`.
static BORDERS_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_borders_enabled(on: bool) {
    BORDERS_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn borders_enabled() -> bool {
    BORDERS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Semantic/styling class of a laid-out item. The view maps these to
/// terminal styles much as it maps `doc::Kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Ordinary flowed text.
    Text,
    /// Heading text, level 1-6.
    Heading(u8),
    /// Inside a `<blockquote>`.
    Quote,
    /// Preformatted (`<pre>`) text — never wrapped or collapsed.
    Pre,
    /// A followable anchor (carries a `link`).
    Link,
    /// A form-control stub (carries the control's element `node`).
    Form,
    /// An image placeholder (alt text for now; real pixels in L3).
    Image,
    /// A generated border glyph (box-drawing) — rendered as quiet structural
    /// chrome (the theme's DIM), never selectable or wrapped.
    Border,
}

/// One positioned inline box on a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// 0-based start column within the content width, in terminal cells.
    pub col: u16,
    /// Display width in cells (chars, matching the rest of the codebase).
    pub width: u16,
    /// Cell height. 1 for text; an inline image reserves its full box
    /// height here and pads `height-1` blank rows beneath it so vertical
    /// scroll/selection stay one-row-per-line.
    pub height: u16,
    pub text: String,
    pub kind: ItemKind,
    /// Absolute image URL, on an `Image` item whose pixels are decoded
    /// (the renderer looks up its encoded protocol by this key). `None`
    /// for an image rendered only as alt text.
    pub image: Option<String>,
    /// Inline emphasis (bold/italic/underline/strike), orthogonal to
    /// `kind` so a link or heading can also carry it.
    pub emph: Emphasis,
    /// The arena node this item came from, for re-anchoring selection
    /// across re-layout. `NO_NODE` when synthesized.
    pub node: NodeId,
    /// Present on followable items (anchors).
    pub link: Option<Link>,
    /// `object-fit: cover` on an `Image` item: the renderer encodes with
    /// `Resize::Crop` (fill the box, clipping overflow) instead of the default
    /// `Resize::Fit` (letterbox). Only meaningful when a CSS box forces an
    /// aspect different from the image's intrinsic one. Always `false` for
    /// non-image items.
    pub crop: bool,
}

/// Inline text emphasis, set by tags (`<b>`/`<i>`/`<u>`/`<s>`) and by CSS
/// (`font-weight`/`font-style`/`text-decoration`). All inherit/propagate,
/// so it threads down the inline `Ctx`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Emphasis {
    /// `<b>`/`<strong>` or CSS `font-weight`.
    pub bold: bool,
    /// `<i>`/`<em>` or CSS `font-style`.
    pub italic: bool,
    /// `<u>` or CSS `text-decoration: underline`.
    pub underline: bool,
    /// `<s>`/`<del>` or CSS `text-decoration: line-through`.
    pub strike: bool,
}

impl Item {
    /// Whether the user can select and act on this item.
    pub fn is_interactive(&self) -> bool {
        self.link.is_some()
    }
}

/// One visual row: a left-to-right sequence of inline items. Empty rows
/// are vertical spacing between blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub items: Vec<Item>,
}

/// A horizontally-scrollable strip (an `overflow-x` container whose content
/// is wider than the viewport — a carousel). Its items live in `Doc.rows`
/// spanning rows `[start, end)`, laid at their full strip columns offset by
/// `left`; the view shows the window `[offset, offset + width)` clipped to
/// the on-screen band `[left, right)`, snapping `offset` to `stops` (the
/// left column of each card) so a card or image is never cut at the edge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carousel {
    pub start: usize,
    pub end: usize,
    /// On-screen band the strip is clipped to (cells).
    pub left: u16,
    pub right: u16,
    /// Full strip width in cells (the scrollable extent).
    pub width: u16,
    /// Left column (strip coords) of each card — the snap stops.
    pub stops: Vec<u16>,
    /// Current scroll position: a strip column shown at the band's left.
    pub offset: u16,
    /// Column of the enclosing bordered box's RIGHT frame bar, when this
    /// carousel sits inside a right-bordered box. The bar lands at the band's
    /// right edge — inside the strip's column span — so without flagging it as
    /// static frame chrome `visible_col` would clip it as off-screen strip
    /// content and the right border would vanish on every strip row. `None`
    /// when the box has no right border. Set by `frame_box`, moved by `blit`.
    pub frame_right: Option<u16>,
}

impl Carousel {
    /// The band's visible width in cells.
    pub fn view_width(&self) -> u16 {
        self.right.saturating_sub(self.left)
    }

    /// Whether a doc row index falls inside this strip.
    pub fn contains_row(&self, row: usize) -> bool {
        row >= self.start && row < self.end
    }

    /// Whether a strip item at absolute column `col` (width `w`) is fully
    /// inside the band at the current scroll offset (so it's drawn).
    pub fn shows(&self, col: u16, w: u16) -> bool {
        col.checked_sub(self.offset)
            .is_some_and(|rc| rc >= self.left && rc + w <= self.right)
    }

    /// Advance the scroll by one card (`dir` ±1), snapping `offset` to a
    /// card's left edge and never scrolling past the last card.
    pub fn scroll_cards(&mut self, dir: i32) {
        let view = self.view_width();
        // The furthest offset worth scrolling to: the first stop from which
        // the strip's tail already fits the band.
        let need = self.width.saturating_sub(view);
        let max_stop = self
            .stops
            .iter()
            .copied()
            .find(|&s| s >= need)
            .unwrap_or_else(|| self.stops.last().copied().unwrap_or(0));
        if dir > 0 {
            if let Some(&next) = self
                .stops
                .iter()
                .find(|&&s| s > self.offset && s <= max_stop)
            {
                self.offset = next;
            }
        } else if let Some(&prev) = self.stops.iter().rev().find(|&&s| s < self.offset) {
            self.offset = prev;
        }
    }

    /// The furthest offset worth scrolling to: the first card stop from
    /// which the strip's tail already fills the band.
    fn max_stop(&self) -> u16 {
        let need = self.width.saturating_sub(self.view_width());
        self.stops
            .iter()
            .copied()
            .find(|&s| s >= need)
            .unwrap_or_else(|| self.stops.last().copied().unwrap_or(0))
    }

    /// Whether the strip can still page in `dir` (±1) — drives the
    /// `:disabled`/greyed state of a generated scroll control at the ends.
    pub fn can_scroll(&self, dir: i32) -> bool {
        if dir > 0 {
            self.offset < self.max_stop()
        } else {
            self.offset > 0
        }
    }

    /// Page the strip by ~one visible width (`dir` ±1), snapping to a card
    /// edge and clamping at the ends — what a prev/next button does in the
    /// CSS carousel model (a `::scroll-button` scrolls by a page, then the
    /// scroll-snap pulls to the nearest item). Falls back to one card when a
    /// whole page would make no progress (a card wider than the band).
    pub fn scroll_page(&mut self, dir: i32) {
        let view = self.view_width();
        let max_stop = self.max_stop();
        if dir > 0 {
            let target = self.offset.saturating_add(view).min(max_stop);
            self.offset = self
                .stops
                .iter()
                .rev()
                .find(|&&s| s > self.offset && s <= target)
                .or_else(|| {
                    self.stops
                        .iter()
                        .find(|&&s| s > self.offset && s <= max_stop)
                })
                .copied()
                .unwrap_or(self.offset);
        } else {
            let target = self.offset.saturating_sub(view);
            self.offset = self
                .stops
                .iter()
                .find(|&&s| s < self.offset && s >= target)
                .or_else(|| self.stops.iter().rev().find(|&&s| s < self.offset))
                .copied()
                .unwrap_or(0);
        }
    }
}

/// The on-screen column for an item in doc row `row`, applying any carousel
/// scroll offset and clipping. `None` means the item is scrolled out of its
/// carousel's band (don't draw it). Items left of a carousel's band (a
/// sidebar beside it) and items in non-carousel rows pass through unchanged.
/// Shared by the renderer AND the image-encode pass so they agree on which
/// items are visible — a strip image scrolled into the band must be encoded,
/// one scrolled out must not (the encode pass keying on the raw strip column
/// is why later cards rendered blank after scrolling).
pub fn visible_col(carousels: &[Carousel], row: usize, item: &Item) -> Option<u16> {
    for c in carousels {
        if !c.contains_row(row) {
            continue;
        }
        // The enclosing box's right frame bar sits at the band edge, inside
        // the strip span: it's static chrome, always drawn at its fixed column
        // (never scrolled or clipped like strip cards).
        if item.kind == ItemKind::Border && Some(item.col) == c.frame_right {
            return Some(item.col);
        }
        if item.col >= c.left {
            return c.shows(item.col, item.width).then(|| item.col - c.offset);
        }
    }
    Some(item.col)
}

/// The on-screen start column of every item in `row` after carousel clipping,
/// gap-fill, and overlap-append — the EXACT placement `ui::browser_rows` draws,
/// so hit-testing lands on what's actually on screen. A terminal can't overlay
/// text, so an item whose visible column falls inside an earlier item is
/// appended right after it (never drawn on top); thus each on-screen column
/// maps to exactly one item. Without this the renderer (which appends overlaps)
/// and the hit-test (which read raw `item.col`) disagreed: a clickable overlay
/// placed over an input — the homepage search bar's clear button — was drawn in
/// one place but only hoverable in another. Returns `(item_index,
/// visual_start_col)` left to right; `row.items[i].width` gives the extent.
/// Carousel-clipped items (not drawn) are omitted.
pub fn visual_columns(row: &Row, carousels: &[Carousel], row_idx: usize) -> Vec<(usize, u16)> {
    let mut placed: Vec<(u16, usize)> = row
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| Some((visible_col(carousels, row_idx, item)?, i)))
        .collect();
    placed.sort_by_key(|&(c, _)| c);
    let mut out = Vec::with_capacity(placed.len());
    let mut col = 0u16;
    for (scol, i) in placed {
        let start = scol.max(col);
        out.push((i, start));
        col = start + row.items[i].width;
    }
    out
}

/// How far above the scroll top to look for an image whose box reaches down
/// into the viewport (a tall banner scrolled partly off the top). Bounds the
/// per-frame back-scan; an image taller than this many cells (~5000px) is not
/// realistic.
pub const MAX_IMAGE_LOOKBACK: usize = 256;

/// An element subtree laid out as an independent box, positioned relative
/// to its own top-left. `width` is the widest used column and `height` is
/// `rows.len()`. `blit` places it into a parent at a `(col, row)` offset —
/// the primitive under flex-wrap grids (and later columns and floats).
struct LaidBox {
    rows: Vec<Row>,
    width: u16,
    height: u16,
    /// Carousels found inside this box (relative to its top-left); `blit`
    /// translates and propagates them so a carousel inside a float/flex
    /// column still reaches the document.
    carousels: Vec<Carousel>,
    /// Recorded flow positions of EMPTY elements inside this box (measure
    /// pass only — `tag_all_nodes`), relative to its top-left; `blit`
    /// translates and propagates them so a boxless element nested in a
    /// float/flex/grid/abspos sub-layout still gets honest geometry (an
    /// IntersectionObserver sentinel hidden in a web component's positioned
    /// shadow subtree). Empty for the render path.
    element_tops: ElementTops,
}

/// A cell placed in a table grid (CSS 2.1 §17.5): its element and the
/// top-left grid coordinates + span it occupies after `colspan`/`rowspan`
/// resolution.
struct TableCell {
    id: NodeId,
    row: usize,
    col: usize,
    rowspan: usize,
    colspan: usize,
}

/// A declared `width` on a table/cell/column: a pixel length (already in
/// cells) or a percentage fraction of the table width (CSS 2.1 §17.5.2 —
/// "a percentage value for a column width is relative to the table width").
#[derive(Clone, Copy)]
enum TrackWidth {
    Px(usize),
    Pct(f32),
}

/// Reconstruct a row's plain text, honoring item start columns (gaps
/// become spaces). Test/diagnostic helper.
#[cfg(test)]
pub fn render_row(row: &Row) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for it in &row.items {
        let start = it.col as usize;
        while col < start {
            out.push(' ');
            col += 1;
        }
        out.push_str(&it.text);
        col = start + display_width(&it.text);
    }
    out
}

/// Lay an HTML document out into rows of items at the given content
/// width. `base` resolves anchor hrefs to `Link`s; `forms`/`controls`
/// (from `http::extract_forms_arena`) make form controls selectable.
#[cfg(test)]
pub fn lay_out(
    dom: &Dom,
    base: &Url,
    width: usize,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    borders: bool,
) -> Vec<Row> {
    lay_out_with_carousels(dom, base, width, forms, controls, images, borders).0
}

/// Lay a document out, also returning the horizontally-scrollable strips
/// (carousels) found so the view can clip/scroll them. The strips' items
/// are already in the returned rows; the `Carousel`s are the scroll
/// metadata keyed to those rows.
pub fn lay_out_with_carousels(
    dom: &Dom,
    base: &Url,
    width: usize,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    borders: bool,
) -> (Vec<Row>, Vec<Carousel>) {
    let mut layout = Layout::new(dom, base, width.max(10), forms, controls, images, borders);
    layout.flow_all();
    let (rows, carousels, _element_tops) = layout.finish();
    (rows, carousels)
}

/// A node's bounding box in whole terminal cells, accumulated as items and
/// descendants are unioned in. Half-open: `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy)]
struct CellRect {
    x0: u16,
    y0: u16,
    x1: u16,
    y1: u16,
}

impl CellRect {
    fn union(&mut self, o: &CellRect) {
        self.x0 = self.x0.min(o.x0);
        self.y0 = self.y0.min(o.y0);
        self.x1 = self.x1.max(o.x1);
        self.y1 = self.y1.max(o.y1);
    }
}

/// Lay `dom` out and return each element's box in CSS pixels, keyed by
/// `NodeId` — the backing for the JS geometry APIs. Runs the same flow as
/// `lay_out_with_carousels` (so the reported boxes match the rendered page),
/// harvests every laid item's cell rectangle onto its source node, then unions
/// each node's rectangle up into its ancestors (so a block's box spans its
/// content, as `getBoundingClientRect` requires) and scales cells to pixels by
/// `cell_px`. Coordinates are document-origin (the top-left of the page); the
/// live viewport scroll is not threaded in yet, so they read as
/// viewport-relative at the top of the page, which is where load-time
/// measurement happens. A node with no laid items (an un-rendered or
/// `display:none` subtree) is simply absent — the JS getter falls back to the
/// viewport box for those, preserving the generous measurement-gate behavior.
/// Shadow-tree nodes keep their own box but do not union into their light-DOM
/// host (the walk follows the light tree). Images lay out from CSS/attribute
/// sizing only — this pass has no decoded intrinsic dimensions (no `ImageSizes`
/// in the JS thread), so an unsized image's box is approximate.
///
/// The box is the element's RENDERED content extent — deliberately, so a page
/// that measures and then renders sees the geometry it will really get (the
/// binding rule: report what we paint). A plain block's declared CSS `width`/
/// `height` is therefore NOT reflected unless the layout actually reserves it
/// (as it does for flex/grid tracks, sized images, etc.); making blocks reserve
/// their declared box is a layout change that geometry would then follow for
/// free — see the geometry notes in CLAUDE.md.
pub fn measure_boxes(
    dom: &Dom,
    base: &Url,
    width: usize,
    forms: &[Form],
    controls: &ControlMap,
    cell_px: (u16, u16),
    borders: bool,
) -> HashMap<NodeId, PxRect> {
    let images = ImageSizes::new();
    let mut layout = Layout::new(dom, base, width.max(10), forms, controls, &images, borders);
    layout.tag_all_nodes = true;
    layout.flow_all();
    let declared = std::mem::take(&mut layout.declared_boxes);
    // `finish` returns `element_tops` already remapped through its blank-row
    // collapse (and accumulated from every sub-layout via `blit`), so an
    // empty element's recorded row matches the kept-row grid the cells use.
    let (rows, _carousels, element_tops) = layout.finish();

    // Each laid item contributes a cell rectangle to its source node. An
    // inline image's `height` already counts the rows it reserves.
    let mut cells: HashMap<NodeId, CellRect> = HashMap::new();
    for (y, row) in rows.iter().enumerate() {
        let y = y as u16;
        for item in &row.items {
            if item.node == NO_NODE {
                continue;
            }
            let r = CellRect {
                x0: item.col,
                y0: y,
                x1: item.col.saturating_add(item.width),
                y1: y.saturating_add(item.height.max(1)),
            };
            cells
                .entry(item.node)
                .and_modify(|c| c.union(&r))
                .or_insert(r);
        }
    }

    // Honest geometry for EMPTY elements — the IntersectionObserver standard
    // depends on it. A modern infinite-scroll "sentinel" is typically an empty
    // marker `<div>` (often inside a web component's shadow root, e.g.
    // archive.org's `<infinite-scroller>`): it paints no cells, so the cells
    // pass leaves it with no box and `getBoundingClientRect` would have to lie —
    // the viewport-fallback that forced the guesswork making an IO scroller
    // either never fire or loop forever. A browser gives such an element a real
    // ZERO-HEIGHT box at its position in the flow; do the same, using the
    // position the flow RECORDED as it entered each element (`element_tops`), so
    // even an empty element in an otherwise-empty container — where a sibling
    // guess has nothing to go on — lands at its true flow position. Done BEFORE
    // the ancestor union so the box also contributes to its ancestors' extent (a
    // sentinel pinned past the loaded tiles grows the document's scrollable
    // height, which is what lets the page scroll far enough to reveal the next
    // batch). Measurement-only (never the render path); `display:none`/
    // `visibility:hidden` elements are skipped (they stay boxless ⇒ honestly
    // not-intersecting).
    for (&id, &(col, row)) in &element_tops {
        if cells.contains_key(&id) || dom.is_hidden(id) {
            continue;
        }
        cells.insert(
            id,
            CellRect {
                x0: col,
                y0: row,
                x1: col,
                y1: row,
            },
        );
    }

    // Union each node's rectangle up into its ancestors through the COMPOSED
    // tree, so a shadow host's box (and the document's scrollable height) counts
    // the content rendered into its shadow root — a slotted/virtualized web
    // component (archive.org's `<infinite-scroller>` behind a `<router-slot>`)
    // otherwise reported a box covering only its light children (none), leaving
    // `documentElement.scrollHeight` too short to scroll the loaded tiles into
    // view. `composed_descendants` is document (pre-)order, so visiting it in
    // reverse reaches every child before its parent — one O(n) bottom-up pass.
    // Each child has already absorbed its own subtree, so unioning the direct
    // composed children suffices.
    for &id in dom.composed_descendants(DOCUMENT).iter().rev() {
        let mut acc = cells.get(&id).copied();
        for child in dom.composed_children(id) {
            if let Some(cr) = cells.get(&child) {
                match acc {
                    Some(ref mut a) => a.union(cr),
                    None => acc = Some(*cr),
                }
            }
        }
        // Geometry Phase 2: raise this block's box to its declared definite
        // floor (recorded during the tagged flow), so a sized block reports its
        // CSS box even when its content paints fewer cells. Done here, AFTER it
        // has absorbed its own/child content (so it has an origin even when all
        // its content lives in child elements — e.g. `<div w><span>·</span>`),
        // and BEFORE its parent unions it in (so a floored child also grows its
        // ancestors). A block with no content at all has no origin → no floor →
        // viewport fallback (a Phase-1 limit; measure-then-render containers
        // carry placeholder content).
        if let (Some(a), Some(&(fw, fh))) = (acc.as_mut(), declared.get(&id)) {
            let fw = u16::try_from(fw).unwrap_or(u16::MAX);
            let fh = u16::try_from(fh).unwrap_or(u16::MAX);
            a.x1 = a.x1.max(a.x0.saturating_add(fw));
            a.y1 = a.y1.max(a.y0.saturating_add(fh));
        }
        if let Some(acc) = acc {
            cells.insert(id, acc);
        }
    }

    let (cw, ch) = (f64::from(cell_px.0.max(1)), f64::from(cell_px.1.max(1)));
    cells
        .into_iter()
        .map(|(id, c)| {
            (
                id,
                PxRect {
                    left: f64::from(c.x0) * cw,
                    top: f64::from(c.y0) * ch,
                    width: f64::from(c.x1 - c.x0) * cw,
                    height: f64::from(c.y1 - c.y0) * ch,
                },
            )
        })
        .collect()
}

/// The `<body>` element, or the document node if there isn't one.
fn body_or_document(dom: &Dom) -> NodeId {
    dom.descendants(DOCUMENT)
        .into_iter()
        .find(|&id| dom.tag_name(id) == Some("body"))
        .unwrap_or(DOCUMENT)
}

/// Whether a CSS length is zero (`0`, `0px`, `0%`, `0em`, …) — its leading
/// numeric part parses to 0. `auto`/empty/non-numeric → false.
fn is_zero_length(value: &str) -> bool {
    let num: String = value
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        .collect();
    num.parse::<f32>().map(|n| n == 0.0).unwrap_or(false)
}

/// The narrowest a flexible flex-row column may be before the row stacks
/// vertically instead (the responsive fallback) — below this, columns are
/// too thin to read.
const MIN_COL: usize = 12;

/// How deeply tables may nest before a table degrades to block-stacked cells.
/// Real pages rarely nest more than a few (slackware's border trick is ~4);
/// the lid keeps the per-cell content measurement — which re-descends each
/// cell's subtree — from overflowing the layout stack on a pathologically deep
/// table tree.
const MAX_TABLE_DEPTH: usize = 8;

/// How a flex container lays its items out (Phase A/B of the 2D arc).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FlexMode {
    /// Wrapping container: shelf-packed 2D grid (e.g. a thumbnail list).
    Grid,
    /// Non-wrapping row: side-by-side columns (e.g. sidebar | content).
    Row,
    /// Non-wrapping column: stacked block-level items (e.g. a card).
    Column,
}

/// One `grid-template-columns`/`-rows` track sizing function. Fixed lengths
/// are pre-resolved to cells (against the grid's content box) at parse time;
/// intrinsic (`auto`/content) and flexible (`fr`) tracks size during the
/// track-sizing pass once item content widths are known. The subset CSS Grid
/// callers actually use; `repeat()` is expanded before this, named lines are
/// dropped, and an unparseable token bails the whole grid to the shelf-pack
/// fallback (so `display:grid` without a usable template is unchanged).
#[derive(Clone)]
enum TrackSpec {
    /// A definite length (`px`/`em`/`%`/`ch`/`calc()`/`min|max|clamp()`), cells.
    Fixed(f32),
    /// A flexible `<flex>` track (`Nfr`): shares leftover space by weight.
    Fr(f32),
    /// `auto`: content-sized, and stretches to fill leftover when no `fr` track
    /// claims it (CSS grid §11.5 — auto maximums absorb free space).
    Auto,
    /// `min-content`: the largest unbreakable content size (we approximate with
    /// the item's measured content width, same as max-content).
    MinContent,
    /// `max-content`: the content's preferred (unconstrained) width.
    MaxContent,
    /// `minmax(min, max)`: a floor and a growth limit.
    Minmax(Box<TrackSpec>, Box<TrackSpec>),
    /// `fit-content(L)`: content-sized but capped at `L` cells.
    FitContent(f32),
}

/// A grid item's resolved placement: zero-based column/row start and span.
#[derive(Clone, Copy)]
struct GridPlace {
    col: usize,
    col_span: usize,
    row: usize,
    row_span: usize,
}

/// Which edge a `float` pins to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FloatSide {
    Left,
    Right,
}

/// A floated box taken out of normal flow (Phase C). It pins to one edge
/// and narrows the content band of every row it spans `[start_row,
/// bottom)`; following content flows beside it across blocks (true BFC
/// behavior — her call) until content passes `bottom` or a `clear`. The
/// `boxed` content is blitted into those rows when the float is resolved.
struct Float {
    side: FloatSide,
    /// Left column of the float box.
    col: u16,
    /// Box width in cells (what it reserves from the band).
    width: usize,
    start_row: usize,
    /// First row past the float (`start_row + boxed.height`).
    bottom: usize,
    boxed: LaidBox,
}

/// How an element participates in flow, resolved from `display`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// `display:none` — generates no box (also caught by `is_hidden`).
    None,
    /// Flows horizontally within the current line.
    Inline,
    /// Breaks the line and stacks vertically.
    Block,
    /// Block, plus a list marker.
    ListItem,
}

/// Horizontal alignment of a block's lines, from CSS `text-align`. It
/// inherits, so it is threaded down the block recursion (replaced when an
/// element sets it, restored on exit) rather than read per-element.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    fn from_css(value: &str) -> Option<Align> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" | "start" | "justify" => Some(Align::Left),
            "center" => Some(Align::Center),
            "right" | "end" => Some(Align::Right),
            _ => None,
        }
    }
}

/// CSS `white-space`: how whitespace collapses and whether lines wrap.
/// Inherits, so it rides on the `Layout` as a saved/restored field (like
/// the old `<pre>` bool it generalizes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WhiteSpace {
    /// Collapse runs of whitespace to one space; wrap at the width.
    Normal,
    /// Collapse, but never wrap.
    Nowrap,
    /// Preserve spaces and newlines; never wrap (the `<pre>` default).
    Pre,
    /// Preserve spaces and newlines; wrap at the width.
    PreWrap,
    /// Collapse spaces but preserve newlines; wrap.
    PreLine,
}

impl WhiteSpace {
    fn from_css(value: &str) -> Option<WhiteSpace> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(WhiteSpace::Normal),
            "nowrap" => Some(WhiteSpace::Nowrap),
            "pre" => Some(WhiteSpace::Pre),
            "pre-wrap" => Some(WhiteSpace::PreWrap),
            "pre-line" => Some(WhiteSpace::PreLine),
            _ => None,
        }
    }
    /// Whether runs of spaces collapse to a single space.
    fn collapses_spaces(self) -> bool {
        matches!(
            self,
            WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
        )
    }
    /// Whether literal `\n` forces a line break.
    fn preserves_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }
    /// Whether lines wrap at the content width.
    fn wraps(self) -> bool {
        !matches!(self, WhiteSpace::Nowrap | WhiteSpace::Pre)
    }
}

/// CSS `text-transform`: alters the rendered text of a run. Inherits, so
/// it rides on the inline `Ctx`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextTransform {
    None,
    Upper,
    Lower,
    Capitalize,
}

impl TextTransform {
    fn from_css(value: &str) -> Option<TextTransform> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(TextTransform::None),
            "uppercase" => Some(TextTransform::Upper),
            "lowercase" => Some(TextTransform::Lower),
            "capitalize" => Some(TextTransform::Capitalize),
            _ => None,
        }
    }
    /// Apply the transform to a text run (borrowing unchanged when `None`).
    fn apply<'t>(self, s: &'t str) -> std::borrow::Cow<'t, str> {
        use std::borrow::Cow;
        match self {
            TextTransform::None => Cow::Borrowed(s),
            TextTransform::Upper => Cow::Owned(s.to_uppercase()),
            TextTransform::Lower => Cow::Owned(s.to_lowercase()),
            TextTransform::Capitalize => Cow::Owned(capitalize_words(s)),
        }
    }
}

/// Uppercase the first letter of each whitespace-separated word, leaving
/// the rest as-is (CSS `text-transform: capitalize`).
fn capitalize_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            at_word_start = true;
            out.push(c);
        } else if at_word_start {
            at_word_start = false;
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A border's terminal rendering weight, chosen from the CSS `border-style`
/// (and width): solid→light, `thick`/≥3px→heavy, `double`→double, dashed/
/// dotted→dashed. `border-color` is ignored (borders render in the theme DIM).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BorderWeight {
    Light,
    Heavy,
    Double,
    Dashed,
}

/// The box-drawing glyph set for a weight, from `ratatui::symbols::line`
/// (dashed reuses the light corners with dashed edges).
fn line_set(w: BorderWeight) -> line::Set<'static> {
    match w {
        BorderWeight::Light => line::NORMAL,
        BorderWeight::Heavy => line::THICK,
        BorderWeight::Double => line::DOUBLE,
        BorderWeight::Dashed => line::Set {
            horizontal: "┄",
            vertical: "┊",
            ..line::NORMAL
        },
    }
}

/// A `border-*-width` value as approximate CSS px (`thin`=1, `medium`=3,
/// `thick`=5; lengths via their unit), or `None` if unparseable.
fn border_px(w: &str) -> Option<f32> {
    match w.trim() {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(5.0),
        t => {
            let split = t
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(t.len());
            let n: f32 = t[..split].parse().ok()?;
            Some(match t[split..].trim() {
                "em" | "rem" => n * 16.0,
                "pt" => n * 4.0 / 3.0,
                _ => n, // px / unitless
            })
        }
    }
}

/// The weight for a visible border from its `border-style` and `border-width`.
fn border_weight(style: &str, width: Option<&str>) -> BorderWeight {
    match style {
        "double" => BorderWeight::Double,
        "dashed" | "dotted" => BorderWeight::Dashed,
        _ => {
            // solid/groove/ridge/inset/outset: heavy only for an explicitly
            // thick or ≥3px border (the default `medium` stays light).
            let heavy = match width.map(str::trim) {
                Some("thick") => true,
                Some("thin") | Some("medium") | None => false,
                Some(w) => border_px(w).is_some_and(|px| px >= 3.0),
            };
            if heavy {
                BorderWeight::Heavy
            } else {
                BorderWeight::Light
            }
        }
    }
}

/// Build a framed box's top or bottom edge string of `width` cells: a corner
/// at each end that has a side border, horizontals between.
fn edge_string(width: usize, left: Option<&str>, right: Option<&str>, horiz: &str) -> String {
    let mut s = String::new();
    for col in 0..width {
        s.push_str(match col {
            0 => left.unwrap_or(horiz),
            c if c + 1 == width => right.unwrap_or(horiz),
            _ => horiz,
        });
    }
    s
}

/// Render a list marker for `list-style-type` `kind` at counter `n`: a bullet
/// glyph, a formatted ordinal (`N. `/`a. `/`i. `), or empty for `none`. Each
/// ordinal carries its trailing `". "`; bullets a trailing space. Unknown
/// types fall back to a disc, matching the UA default.
fn format_list_marker(kind: &str, n: u32) -> String {
    match kind {
        "none" => String::new(),
        "circle" => "◦ ".to_owned(),
        "square" => "▪ ".to_owned(),
        "decimal" => format!("{n}. "),
        "decimal-leading-zero" => format!("{n:02}. "),
        "lower-alpha" | "lower-latin" => format!("{}. ", alpha_marker(n, false)),
        "upper-alpha" | "upper-latin" => format!("{}. ", alpha_marker(n, true)),
        "lower-roman" => format!("{}. ", roman_marker(n, false)),
        "upper-roman" => format!("{}. ", roman_marker(n, true)),
        _ => "• ".to_owned(),
    }
}

/// A bijective base-26 alphabetic ordinal: 1→a, 26→z, 27→aa, … (`0` keeps a
/// literal `0`). Upper-cased when `upper`.
fn alpha_marker(mut n: u32, upper: bool) -> String {
    if n == 0 {
        return "0".to_owned();
    }
    let mut buf = Vec::new();
    while n > 0 {
        n -= 1;
        buf.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    buf.reverse();
    let s = String::from_utf8(buf).unwrap_or_default();
    if upper { s.to_uppercase() } else { s }
}

/// A Roman-numeral ordinal (1→i, 4→iv, …); out of range (0 or >3999) falls
/// back to the decimal number. Upper-cased when `upper`.
fn roman_marker(mut n: u32, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const VALS: &[(u32, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for &(v, sym) in VALS {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper { s.to_uppercase() } else { s }
}

/// The active inline formatting context, threaded down the recursion.
#[derive(Clone)]
struct Ctx {
    kind: ItemKind,
    emph: Emphasis,
    transform: TextTransform,
    /// CSS `letter-spacing` resolved to whole cells of gap inserted between
    /// adjacent characters (inherits; 0 = none). Sub-cell values round to 0,
    /// so the common subtle tracking is a faithful no-op in a cell grid.
    letter_spacing: usize,
    node: NodeId,
    link: Option<Link>,
}

impl Ctx {
    fn root() -> Self {
        Ctx {
            kind: ItemKind::Text,
            emph: Emphasis::default(),
            transform: TextTransform::None,
            letter_spacing: 0,
            node: NO_NODE,
            link: None,
        }
    }
}

/// Insert `cells` spaces between each pair of characters (CSS `letter-spacing`
/// rendered as whole-cell tracking). A no-op for `cells == 0` or a single
/// character, so it borrows in the common case.
fn letter_space(word: &str, cells: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if cells == 0 || word.chars().count() < 2 {
        return Cow::Borrowed(word);
    }
    let gap = " ".repeat(cells);
    let mut out = String::with_capacity(word.len() + cells * word.chars().count());
    for (i, c) in word.chars().enumerate() {
        if i > 0 {
            out.push_str(&gap);
        }
        out.push(c);
    }
    Cow::Owned(out)
}

/// Block-level elements: they break the current line before and after
/// their content so it stacks vertically.
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "details",
    "div",
    "dl",
    "dd",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// Blocks that also get a blank spacer row after them (paragraph-like).
const SPACING: &[&str] = &[
    "article",
    "blockquote",
    "dl",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

/// Elements whose subtree never renders as page text. (`<video>`/`<audio>`
/// are NOT here — they render as a media representation via `flow_media`.)
const SKIP: &[&str] = &[
    "base", "canvas", "head", "iframe", "link", "math", "meta", "noscript", "object", "script",
    "style", "svg", "template", "title",
];

struct Layout<'a> {
    dom: &'a Dom,
    base: &'a Url,
    forms: &'a [Form],
    controls: &'a ControlMap,
    images: &'a ImageSizes,
    width: usize,
    /// The full terminal width in cells — the CSS viewport for `vw` units,
    /// kept distinct from `width` (which floats/centering narrow as blocks
    /// nest).
    viewport_w: usize,
    rows: Vec<Row>,
    /// The line currently being built.
    line: Vec<Item>,
    /// Next free column on the current line.
    col: usize,
    /// Left indent of the current block.
    indent: usize,
    /// The current line's left/right content boundaries in cells —
    /// `indent`/`width` narrowed by any floats active on this row. The
    /// inline flow starts at `line_left` and wraps at `line_right`;
    /// `begin_line` recomputes them per row.
    line_left: usize,
    line_right: usize,
    /// Floats taken out of normal flow, narrowing the band of the rows
    /// they span until content passes their bottom (or a `clear`). Their
    /// boxes are blitted into those rows once resolved.
    floats: Vec<Float>,
    /// When laying a float's OWN box (a fresh sub-layout rooted at the
    /// floated element), its float is ignored so it doesn't recurse — the
    /// float only matters to the parent.
    float_skip: Option<NodeId>,
    /// Inherited horizontal alignment of the current block's lines.
    align: Align,
    /// An inter-word space is owed before the next word.
    pending_space: bool,
    /// Inherited `white-space` mode of the current text run.
    ws: WhiteSpace,
    /// Tallest item (in cell rows) on the line being built. >1 only when an
    /// inline image rides the line; `break_line` reserves `line_height-1`
    /// spacer rows beneath so the image's box doesn't overwrite later text.
    line_height: u16,
    /// Open lists' item counters (one per nesting level, next index). Every
    /// level counts; whether the marker shows as a number, letter, roman
    /// numeral, bullet, or nothing comes from each item's `list-style-type`.
    list_stack: Vec<u32>,
    /// Horizontally-scrollable strips discovered during the pass.
    carousels: Vec<Carousel>,
    /// Author-supplied carousel/slideshow controls we replace with our own
    /// generated glyphs — skipped when flowed (their markup may come AFTER
    /// the strip, so removing already-laid items isn't enough).
    suppressed_controls: std::collections::HashSet<NodeId>,
    /// This pass is measuring an element's intrinsic width (a flex basis),
    /// not placing it — so `text-align` offsets are ignored (they don't
    /// change content width) and the result is the natural left-packed extent.
    measuring: bool,
    /// How many `display:table` formatting contexts enclose this layout. A
    /// table cell lays its content in a fresh sub-layout that inherits this +1,
    /// so a nested table knows its depth. Two uses: beyond `MAX_TABLE_DEPTH` a
    /// table degrades to block-stacked cells (a hard recursion lid — a page can
    /// nest tables arbitrarily, and the per-cell content measurement re-descends
    /// the subtree, so unbounded nesting would overflow the stack); and the
    /// expensive auto column measurement is skipped while `measuring` (a nested
    /// table contributes an approximate width to its ancestor's measurement
    /// instead of recursively measuring again — without this the per-cell
    /// min/max passes compound multiplicatively with nesting depth).
    table_depth: usize,
    /// The element this sub-layout was spun up to lay out (a float / flex / grid
    /// item, or a bordered interior) — the ROOT of `layout_subtree_inner`. Its
    /// own `width` was ALREADY consumed by the parent to size the box this
    /// subtree fills, so `constrain_block_width` must NOT re-narrow on it again
    /// (a `float:left;width:16.66%` column would otherwise resolve its `%`
    /// against the already-narrowed band a second time and collapse — erome's
    /// thumbnail grid). `None` for the page's top-level layout.
    subtree_root: Option<NodeId>,
    /// When laying the INTERIOR of a bordered box (`flow_bordered`'s sub-pass),
    /// this is that element: its own border routing is skipped (no recursion)
    /// and its margin is suppressed (handled outside the frame; padding kept).
    inner_border_box: Option<NodeId>,
    /// Whether CSS borders render as box-drawing chrome. Default OFF (her
    /// call: in a terminal, vertical space is at a premium and most page
    /// borders are subtle 1px underlines not worth a whole cell row). The
    /// `set borders on` session flag turns them on. When off, `flow_bordered`
    /// never triggers — the border properties still cascade/bake (so
    /// getComputedStyle stays correct), they just aren't drawn.
    borders: bool,
    /// A `white-space:nowrap` + `overflow:hidden` box clips its single line at
    /// this content-right edge: `Some(right)` while inside one. Nowrap text
    /// can't wrap, so without this a long string overflows its box and paints
    /// over neighbours (a truncated post title bleeding into the sidebar). The
    /// classic single-line-ellipsis card idiom.
    clip_right: Option<usize>,
    /// Set once the active clip box has been truncated, so the rest of its
    /// (unwrappable) words are dropped instead of laid past the cut.
    clip_done: bool,
    /// The topmost page-covering modal overlay, if any — an out-of-flow
    /// element that covers the viewport (or is a semantic dialog) AND holds
    /// real content (an age gate, consent wall, login modal, lightbox). A
    /// cell grid can't composite transparent layers, so instead of stacking
    /// the overlay below the page in document order we surface ONLY this
    /// subtree (the page behind it is deferred — the live-page JS that
    /// dismisses the overlay reveals it). `is_out_of_flow` exempts this node
    /// so its content flows as a normal block, not the compact inline overlay.
    modal_root: Option<NodeId>,
    /// Measurement pass only (`measure_boxes`): tag every laid item with the
    /// nearest enclosing element so each element's box can be recovered (and
    /// unioned up) for the JS geometry APIs. OFF for rendering, where items
    /// coalesce by source node — tagging would fragment inline runs at element
    /// boundaries (same visual output, but a different item vector), so the two
    /// passes are kept separate and the render path is untouched.
    tag_all_nodes: bool,
    /// Measurement pass only: each block's declared definite box `(width
    /// cells, height rows)` — the floor `measure_boxes` raises a node's
    /// content-extent rect to, so a `<div style="width:240px;height:135px">`
    /// reports its CSS box to `getBoundingClientRect`/`offset*` even when its
    /// content paints fewer cells. Only DEFINITE lengths are recorded (px/em/
    /// ch/%-of-a-definite-container); `auto`/indefinite `%`-height/`vh` are
    /// left out. Populated under `tag_all_nodes`, so the render path never
    /// touches it. See [[js-geometry-real-boxes]] Phase 2.
    declared_boxes: HashMap<NodeId, (usize, usize)>,
    /// Measurement pass only: every element's flow position `(col, row)` at the
    /// moment the flow enters it — captured for EVERY element, including empty
    /// ones that paint no cells. `measure_boxes` uses it to give a boxless
    /// element (an infinite-scroll sentinel — often empty, often in a web
    /// component's shadow root) a real ZERO-HEIGHT box at its true position, so
    /// `getBoundingClientRect`/IntersectionObserver have honest geometry instead
    /// of a viewport-fallback lie. Populated under `tag_all_nodes`; the render
    /// path never touches it.
    element_tops: ElementTops,
}

impl<'a> Layout<'a> {
    fn new(
        dom: &'a Dom,
        base: &'a Url,
        width: usize,
        forms: &'a [Form],
        controls: &'a ControlMap,
        images: &'a ImageSizes,
        borders: bool,
    ) -> Self {
        Layout {
            dom,
            base,
            forms,
            controls,
            images,
            width,
            viewport_w: width,
            rows: Vec::new(),
            line: Vec::new(),
            col: 0,
            indent: 0,
            line_left: 0,
            line_right: width,
            floats: Vec::new(),
            float_skip: None,
            align: Align::Left,
            pending_space: false,
            ws: WhiteSpace::Normal,
            line_height: 1,
            list_stack: Vec::new(),
            carousels: Vec::new(),
            suppressed_controls: std::collections::HashSet::new(),
            measuring: false,
            table_depth: 0,
            subtree_root: None,
            inner_border_box: None,
            borders,
            clip_right: None,
            clip_done: false,
            modal_root: None,
            tag_all_nodes: false,
            declared_boxes: HashMap::new(),
            element_tops: HashMap::new(),
        }
    }

    /// Whether an element clips overflow (a non-`visible` `overflow`), so a
    /// `white-space:nowrap` line inside it is truncated at its box edge.
    fn clips_overflow(&self, id: NodeId) -> bool {
        self.dom.computed_style(id, "overflow").is_some_and(|v| {
            v.split_whitespace()
                .any(|t| matches!(t, "hidden" | "clip" | "auto" | "scroll"))
        })
    }

    /// Whether an element clips/scrolls its horizontal (flex main) axis —
    /// `overflow` or `overflow-x` set to `auto`/`scroll`/`hidden`/`clip`. Such
    /// a container is a scroll context, so a non-wrapping flex row that
    /// overflows keeps its items side by side (clipped) instead of reflowing
    /// into a vertical stack. See `flow_flex_row`.
    fn clips_x(&self, id: NodeId) -> bool {
        [
            self.dom.computed_style(id, "overflow"),
            self.dom.computed_style(id, "overflow-x"),
        ]
        .into_iter()
        .flatten()
        .any(|v| {
            v.split_whitespace()
                .any(|t| matches!(t, "hidden" | "clip" | "auto" | "scroll"))
        })
    }

    /// Flow the whole document into rows (shared by `lay_out_with_carousels`
    /// and `measure_boxes`). Caller then takes `finish()`.
    fn flow_all(&mut self) {
        let root = body_or_document(self.dom);
        let ctx = Ctx::root();
        // A page-covering modal overlay (age gate, consent wall, login dialog,
        // lightbox) is painted ON TOP of the page in a real browser — the page
        // behind it is inert. We can't composite layers in a cell grid, so we
        // surface only that overlay's subtree and defer the page; the live-page
        // JS that dismisses the overlay reveals the page on the next render.
        self.modal_root = self.find_modal_overlay();
        if let Some(modal) = self.modal_root {
            self.flow_node(modal, &ctx);
            self.flush_block();
            self.finish_floats();
        } else {
            for child in self.dom.children(root) {
                self.flow_node(child, &ctx);
            }
            self.flush_block();
            self.finish_floats();
            // The initial containing block (the viewport) places out-of-flow
            // boxes with no positioned ancestor (and `fixed` ones) at their
            // computed coordinates, after the body's in-flow content.
            let cb_h = self.rows.len();
            self.place_positioned_children(None, 0, 0, self.width, cb_h);
        }
    }

    fn flow_node(&mut self, id: NodeId, ctx: &Ctx) {
        match &self.dom.node(id).data {
            NodeData::Text(s) => {
                let s = s.clone();
                self.place_text(&s, ctx);
            }
            NodeData::Element { .. } => self.flow_element(id, ctx),
            _ => {}
        }
    }

    /// The children to flow for `id`: a shadow HOST renders its SHADOW tree (a
    /// web component renders into its shadow root — archive.org's
    /// `<infinite-scroller>` puts its cells and its IntersectionObserver sentinel
    /// there), so we flow the shadow root's children; everyone else flows their
    /// light children. This composes the shadow DOM in the MEASUREMENT pass so
    /// getBoundingClientRect / IntersectionObserver see shadow elements at their
    /// real position. The render path lays out the already-composed serialized
    /// HTML (which carries no shadow roots), so `shadow_root` is `None` there and
    /// this is identical. Slots aren't projected (a host's light children aren't
    /// flowed into its shadow `<slot>`s) — the common virtualized-scroller case
    /// renders directly into shadow with no slots; slotted geometry is a deferred
    /// refinement.
    fn flow_children(&self, id: NodeId) -> Vec<NodeId> {
        // A `<slot>` is replaced by the host's light children assigned to it
        // (its fallback content only when nothing is assigned) — the flat-tree
        // projection that lets a routed component nested behind a shadow
        // `<slot>` flow at all (archive.org's `<router-slot>`).
        if self.dom.tag_name(id) == Some("slot") {
            let assigned = self.dom.slot_assigned_nodes(id);
            return if assigned.is_empty() {
                self.dom.children(id)
            } else {
                assigned
            };
        }
        match self.dom.shadow_root(id) {
            Some(shadow) => self.dom.children(shadow),
            None => self.dom.children(id),
        }
    }

    fn flow_element(&mut self, id: NodeId, ctx: &Ctx) {
        let Some(tag) = self.dom.tag_name(id).map(str::to_owned) else {
            return;
        };
        if SKIP.contains(&tag.as_str())
            || self.dom.is_hidden(id)
            || self.suppressed_controls.contains(&id)
            || self.is_clipped_offscreen(id)
        {
            return;
        }
        // An out-of-flow box (`position:absolute`/`fixed`, CSS 2.1 §9.6) is
        // removed from normal flow — its containing block places it at its
        // computed coordinates (`place_positioned_children`). This guard MUST
        // precede the early `<img>`/form-control/`<video>` dispatch below:
        // those tags `return` before the general `flow_of`→`Flow::None` skip
        // (further down), so without it an absolutely-positioned `<img>` or
        // control is laid BOTH in normal flow AND at its placed position — the
        // double-render of Steam's capsule banner images (an abspos `<img>`
        // filling each positioned capsule) and their overlay badges. Plain
        // abspos `<div>`s already skip via that later guard; this makes the
        // replaced/control tags consistent. The exception is when WE are laying
        // this very box as its own sub-box (`subtree_root`) — then flow it.
        if self.is_out_of_flow(id) && self.subtree_root != Some(id) {
            return;
        }
        // A `<video>`/`<audio>` element renders as a media representation — its
        // poster (when present) plus a labelled link to the playable source —
        // not as an inline player a terminal can't run. Following it auto-opens
        // mpv. Handled before float/slideshow dispatch so it always applies.
        if matches!(tag.as_str(), "video" | "audio") {
            self.flow_media(id, &tag, ctx);
            return;
        }
        // A media-player WRAPPER: an element directly holding a `position:
        // absolute` `<video>`/`<audio>` tech (the video.js / Plyr / JW pattern —
        // the tech fills an aspect-ratio box, surrounded by control chrome:
        // big-play button, poster overlay, control bar). Render only the media
        // representation and SKIP the whole subtree, so the chrome (empty
        // buttons → `[  ]`, a poster-click anchor → `·`) doesn't leak. A plain
        // in-flow `<video>` (e.g. in a `<figure>` with a real `<figcaption>`)
        // isn't a player tech, so its siblings still flow.
        if let Some(media) = self.dom.children(id).into_iter().find(|&c| {
            matches!(self.dom.tag_name(c), Some("video" | "audio")) && self.is_out_of_flow(c)
        }) {
            let mtag = self.dom.tag_name(media).unwrap_or("video").to_owned();
            self.flow_media(media, &mtag, ctx);
            return;
        }
        // A floated element leaves normal flow: pin it to an edge and let the
        // following content wrap beside it (across blocks, until cleared or
        // its bottom is passed). Checked before the tag dispatch so a floated
        // `<img>` floats too; skipped when laying the float's own box. CSS
        // ignores `float` on a flex item entirely, so we drop it for ANY item
        // of a BLOCK-level `display:flex`/`grid` container — those lay their
        // children as flex columns (`flow_flex_row`/`flow_grid_*`), which
        // positions them side by side without the float (archive.org's
        // `.right-side-section` is `display:flex` holding `display:block;
        // float:right` Sign-up/Upload items — floated, `.upload` shot off-canvas
        // right). An `inline-flex`/`inline-grid` container is the carve-out: we
        // lay it by INLINE recursion (not real flex columns), so a BLOCK-level
        // child there still needs its float to sit beside its siblings (a
        // latest-post avatar `<div float:left>`); only its INLINE-level children
        // drop the float (the nav split-button's `inline-flex;float:left` toggle
        // that otherwise took its own row). Same INLINE-only rule inside an
        // out-of-flow (absolute/fixed) ancestor: we render small overlays as a
        // compact inline run, so an inline-level float there would split it (a
        // search bar's magnifier `<i float:left>` in an absolutely-positioned
        // icon span) — but a BLOCK-level float inside an absolutely-positioned
        // CONTAINER must still float (the abspos box is its formatting context).
        // That's Steam's `.supernav_container` (`position:absolute`) holding
        // `display:block;float:left` nav items: a horizontal floated row.
        let drop_float = (self.parent_is_flex_container(id)
            && (!self.parent_is_inline_flex(id) || self.is_inline_level(id)))
            || (self.parent_out_of_flow(id) && self.is_inline_box(id));
        if self.float_skip != Some(id)
            && !drop_float
            && let Some(side) = self.float_side(id)
        {
            self.flow_float(id, side, ctx);
            return;
        }
        // A `contenteditable` host bound to a field (the form walk made it a
        // synthetic textarea) renders as ONE editable widget — its value or
        // placeholder — and its subtree is skipped (the editor's own markup is
        // not ours to flow). Same path as a real textarea control.
        if self.controls.contains_key(&id) && self.dom.is_contenteditable_host(id) {
            self.flow_form_control(id, "textarea", ctx.link.clone());
            return;
        }
        match tag.as_str() {
            "br" => {
                // A <br> right after an out-of-flow overlay is spurious in our
                // flow (the overlay's siblings are absolutely positioned, not
                // stacked) — skip it so overlay controls stay on one line.
                if !self.prev_sibling_out_of_flow(id) {
                    self.break_line();
                }
                return;
            }
            "hr" => {
                self.flush_block();
                self.push_rule();
                if SPACING.contains(&"hr") {
                    self.push_blank();
                }
                return;
            }
            "img" => {
                self.place_image(id, ctx);
                return;
            }
            "input" | "textarea" | "select" | "button" => {
                self.flow_form_control(id, &tag, ctx.link.clone());
                return;
            }
            _ => {}
        }

        // Build the child formatting context for inline elements. The inline
        // `Ctx` carries only STRUCTURAL state (kind/link/node); the text
        // styling — emphasis, decoration, transform — now comes straight from
        // the cascade per element, not threaded by hand. `computed_value`
        // inherits and applies the UA tag defaults (`<b>` bold, `<i>` italic),
        // and `text_decoration` propagates `<u>`/`<s>` + author rules.
        let mut cctx = ctx.clone();
        // Geometry measurement: attribute every item to its nearest element so
        // `measure_boxes` can recover per-element boxes. A descendant element
        // overrides this with its own id, so each item carries the closest
        // enclosing element. No effect on rendering (flag is off there).
        if self.tag_all_nodes {
            cctx.node = id;
            // Record the element's flow position (its top-left in cells) as the
            // flow ENTERS it — for EVERY element, even an empty one that paints
            // no cells. `measure_boxes` uses it to give a boxless sentinel a real
            // zero-height box at its true position (the flow knows where an empty
            // element sits; a post-hoc sibling guess does not). `rows.len()` is
            // the current row, `col` the current column.
            self.element_tops.insert(
                id,
                (
                    u16::try_from(self.col).unwrap_or(u16::MAX),
                    u16::try_from(self.rows.len()).unwrap_or(u16::MAX),
                ),
            );
        }
        match tag.as_str() {
            "a" => {
                if let Some(href) = self.dom.attr(id, "href") {
                    // Nav bars pack `<a>`s with no whitespace between
                    // them, leaning on CSS margins for separation. Give
                    // two abutting links a thin gap so they don't fuse
                    // into one unreadable run.
                    if self
                        .line
                        .last()
                        .is_some_and(|it| it.is_interactive() && it.node != id)
                    {
                        self.pending_space = true;
                    }
                    let href = href.to_owned();
                    cctx.link = Some(crate::http::resolve(self.base, &href));
                    cctx.kind = ItemKind::Link;
                    cctx.node = id;
                }
            }
            "blockquote" => cctx.kind = ItemKind::Quote,
            "pre" => cctx.kind = ItemKind::Pre,
            _ => {
                if let Some(level) = heading_level(&tag) {
                    cctx.kind = ItemKind::Heading(level);
                }
            }
        }

        // font-weight / font-style: inherited, with UA defaults for the
        // emphasis tags, resolved by the cascade. An author rule can turn
        // emphasis OFF (`strong{font-weight:normal}`) by winning over the UA
        // default. text-decoration propagates/accumulates across the subtree.
        cctx.emph.bold = self
            .dom
            .computed_value(id, "font-weight")
            .is_some_and(|w| css_is_bold(&w));
        cctx.emph.italic = self
            .dom
            .computed_value(id, "font-style")
            .is_some_and(|s| css_is_italic(&s));
        (cctx.emph.underline, cctx.emph.strike) = self.dom.text_decoration(id);
        cctx.transform = self
            .dom
            .computed_value(id, "text-transform")
            .as_deref()
            .and_then(TextTransform::from_css)
            .unwrap_or(TextTransform::None);
        // `letter-spacing` inherits; resolve it once to whole cells of inter-
        // character gap (em/px etc. — `%`/`vw` don't apply, so the contextual
        // args are inert). `normal`/sub-cell values resolve to 0.
        cctx.letter_spacing = self
            .dom
            .computed_value(id, "letter-spacing")
            .as_deref()
            .and_then(|v| resolve_cells(v, 1, self.viewport_w))
            .unwrap_or(0);

        // Block vs inline is driven by the cascaded `display` (baked into
        // the serialized HTML by the engine, which has the sheets), with
        // the HTML tag default as the fallback. This is what lets an
        // inline-styled `<li>` nav flow across one row instead of becoming
        // a vertical bullet tower.
        let mut flow = self.flow_of(id, &tag);
        if flow == Flow::None {
            return;
        }
        // A block-level flex/grid box that is itself a flex item of an
        // INLINE-level flex container (`inline-flex` parent) and whose content
        // is a single inline run (icon + text) is laid inline, not as its own
        // block row — a nav link / split button sits in the parent's row (which
        // we lay by inline recursion). XenForo's `display:flex` "Log in" /
        // "Register" links each dropped to a row of their own otherwise. A flex
        // item with block rows (a stacked title/date column) is left alone
        // (`flex_items_all_inline` is keyed on each child's tag default).
        if matches!(flow, Flow::Block)
            && self.is_flex_or_grid(id)
            && self.parent_is_inline_flex(id)
            && self.flex_items_all_inline(id)
        {
            flow = Flow::Inline;
        }
        let block_like = matches!(flow, Flow::Block | Flow::ListItem);
        // Geometry Phase 2: record this block's declared definite box as the
        // floor `measure_boxes` raises its content-extent rect to, so a sized
        // block reports its CSS box to `getBoundingClientRect`/`offset*` even
        // when its content paints fewer cells. Done here (before the band is
        // narrowed below) so `%` widths resolve against the CONTAINING block,
        // and before the border/flex branches so every block-like element is
        // covered. Measurement pass only — never touches rendering. Width takes
        // the larger of `width`/`min-width` (clamped to the band, our model has
        // no horizontal scroll); height the larger of `height`/`min-height` as
        // whole rows (`%`/`vh`/`auto` are indefinite → no floor, matching CSS).
        if self.tag_all_nodes && block_like {
            let avail = self.width.saturating_sub(self.indent).max(1);
            let floor_w = self
                .css_cells(id, "width")
                .into_iter()
                .chain(self.css_cells(id, "min-width"))
                .max()
                .map(|w| w.min(avail));
            let floor_h = ["height", "min-height"]
                .into_iter()
                .filter_map(|p| self.dom.computed_style(id, p))
                .filter_map(|v| css_length_rows(&v))
                .max();
            if floor_w.is_some() || floor_h.is_some() {
                self.declared_boxes
                    .insert(id, (floor_w.unwrap_or(0), floor_h.unwrap_or(0)));
            }
        }
        // A `display:table` element establishes a table formatting context: its
        // rows lay their cells side by side into computed columns (CSS 2.1 §17),
        // instead of stacking every `<td>` as its own block. Routed before the
        // border/flex/block dispatch so the whole table subtree is laid by the
        // table algorithm. `flow_table` does its own block framing.
        if block_like
            && matches!(
                self.dom.effective_display(id).as_deref(),
                Some("table" | "inline-table")
            )
        {
            self.flow_table(id, ctx);
            return;
        }
        // A block-level element with a visible border is laid as its own
        // framed sub-box: lay its interior, draw the bordered sides as
        // box-drawing, blit. `inner_border_box` guards the recursion (the
        // interior pass lays this same element without re-entering here).
        if self.borders && block_like && self.inner_border_box != Some(id) {
            let sides = self.border_sides(id);
            if sides.iter().any(Option::is_some) {
                // Pass the inherited context so a clickable/styled ANCESTOR
                // (e.g. an `<a>` wrapping a bordered tab `<div>`) still reaches
                // the interior — the sub-pass re-enters `flow_element(id)` and
                // rebuilds the element's context from this, exactly as the
                // non-bordered child path does. Root context would drop the
                // enclosing link and the bordered tab would stop being a link.
                self.flow_bordered(id, sides, ctx);
                return;
            }
        }
        // A flex container lays its children out as boxes: a wrapping one
        // as a 2D grid, a row one as side-by-side columns, a column one as
        // stacked block-level items. Everything else flows normally.
        let flex = if block_like { self.flex_mode(id) } else { None };
        // A horizontal-scroll container (an `overflow-x` box with content
        // wider than the viewport — a carousel) lays its content as one wide
        // strip, clipped to the band and scrolled by the view.
        let hscroll = block_like && flex.is_none() && self.is_hscroll(id);
        if block_like {
            self.flush_block();
            // CSS `clear` drops this block below the floats it clears.
            self.clear_floats(id);
            if self.gap_before(id, &tag) {
                self.push_blank();
            }
        }
        // A block that establishes a block formatting context (the classic
        // `overflow:hidden` clearfix, or `display:flow-root`) CONTAINS its
        // descendant floats: they size the box and never leak out to
        // following siblings. We model this by stashing the outer floats so
        // only the floats this subtree creates are in scope, then flushing
        // them within the block at exit. Without it a wide float (e.g. a
        // page's main column) leaks past its wrapper and the footer renders
        // on top of it. (A carousel lays its own contained strip, so it
        // doesn't take this path.)
        let saved_floats = if block_like && flex.is_none() && !hscroll && self.establishes_bfc(id) {
            Some(std::mem::take(&mut self.floats))
        } else {
            None
        };

        // text-align inherits; a block that sets it changes alignment for
        // its own lines and its descendants until they override it. The CSS
        // value wins; otherwise the legacy presentational hints `<center>` and
        // the `align` attribute (HTML §15.3 maps `align` to `text-align`) still
        // align old table-layout pages — slackware centers its title cell and
        // right-aligns its footer this way.
        let saved_align = self.align;
        if block_like {
            if let Some(a) = self
                .dom
                .computed_value(id, "text-align")
                .as_deref()
                .and_then(Align::from_css)
            {
                self.align = a;
            } else if tag == "center" {
                self.align = Align::Center;
            } else if let Some(a) = self.dom.attr(id, "align").and_then(Align::from_css) {
                self.align = a;
            }
        }

        let indent_add = if block_like {
            self.block_indent(id, &tag)
        } else {
            0
        };
        self.indent += indent_add;
        // A block with an explicit `width`/`max-width` AND horizontal
        // `margin:auto` constrains its content to that width and positions it
        // (centered for `margin:0 auto`, the common centered-content wrapper —
        // e.g. the SL Marketplace's `width:1082px;margin:0 auto` page box).
        // Narrows the band and shifts it; restored at block exit.
        let saved_width = self.width;
        let center_pad = if block_like {
            self.constrain_block_width(id)
        } else {
            0
        };
        // At a block boundary the line is empty; reset to the (new) band
        // left (indent narrowed by any active floats). Inline elements
        // don't touch it.
        if block_like {
            self.begin_line();
        }

        // white-space inherits: `<pre>` defaults to Pre, and CSS overrides
        // either (so `pre{white-space:pre-wrap}` or `white-space:nowrap` on
        // any element both work).
        // white-space inherits and the `<pre>` UA default rides the cascade
        // now (computed_value), so a single read covers `<pre>`,
        // `pre{white-space:pre-wrap}`, and an inline `white-space:nowrap`
        // alike. The field is still saved/restored to isolate siblings.
        let saved_ws = self.ws;
        if let Some(w) = self
            .dom
            .computed_value(id, "white-space")
            .as_deref()
            .and_then(WhiteSpace::from_css)
        {
            self.ws = w;
        }
        // A `white-space:nowrap` + `overflow:hidden` box truncates its single
        // line at its content edge (the single-line-ellipsis card idiom). The
        // band is already set (`begin_line` above), so `line_right` is this
        // box's right; clip there. Saved/restored so siblings/children that
        // inherit `nowrap` but DON'T clip overflow lay normally.
        let saved_clip = (self.clip_right, self.clip_done);
        if block_like && self.ws == WhiteSpace::Nowrap && self.clips_overflow(id) {
            self.clip_right = Some(self.line_right);
            self.clip_done = false;
        }
        let pushed_list = match tag.as_str() {
            "ul" => {
                self.list_stack.push(1);
                true
            }
            "ol" => {
                // `<ol start=N>` seeds the counter (default 1).
                let start = self
                    .dom
                    .attr(id, "start")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(1);
                self.list_stack.push(start);
                true
            }
            _ => false,
        };
        // The list marker is deferred: a block-level child (a flex row, a
        // nested `<div>`) flushes the line, so emitting the marker up front
        // would strand it on a row of its own. Instead we drop the marker into
        // the item's first content row at block exit (`place_list_marker`).
        //
        // `list-style-position`: `outside` (the default) hangs the marker in a
        // gutter to the LEFT of the content — the content is indented by the
        // marker width, and wrapped lines align under the content. `inside`
        // flows the marker as the first token of the FIRST line — the content
        // is not extra-indented, and wrapped lines align under the marker.
        let list_marker = (flow == Flow::ListItem).then(|| self.next_list_marker(id));
        let marker_indent = list_marker.as_ref().map_or(0, |m| display_width(m));
        let inside_marker = marker_indent > 0
            && self
                .dom
                .computed_value(id, "list-style-position")
                .as_deref()
                == Some("inside");
        // Cells of block indent the marker adds (outside hangs it in a gutter;
        // inside keeps the content at the margin and reserves first-line space).
        let marker_added = if inside_marker { 0 } else { marker_indent };
        if marker_added > 0 {
            self.indent += marker_added;
            self.begin_line();
        }
        let marker_start_row = self.rows.len();
        // `inside`: reserve the marker's width at the start of the first line
        // (like a text-indent) so the deferred marker sits before the text.
        if inside_marker && !self.measuring {
            self.col += marker_indent;
        }

        // CSS `text-indent` shifts the start of this block's FIRST line (it
        // inherits, so each block applies it to its own first line). Skipped
        // while measuring intrinsic width, like `text-align`.
        if block_like
            && !self.measuring
            && let Some(v) = self.dom.computed_value(id, "text-indent")
            && let Some(cells) = resolve_cells(
                &v,
                self.width.saturating_sub(self.indent).max(1),
                self.viewport_w,
            )
        {
            self.col += cells;
        }

        // An inline element's left margin becomes a leading gap (block margins
        // go through `block_indent` / the band instead).
        if !block_like && self.inline_h_margin(id, "margin-left") {
            self.pending_space = true;
        }

        // CSS `::before` generated content opens the element's content.
        if let Some(t) = self.pseudo_text(id, crate::dom::PseudoEl::Before) {
            self.place_text(&t, &cctx);
        }

        // An icon-only link (its only content is an `<svg>` we don't
        // rasterize, leaving the anchor empty and so invisible/unselectable)
        // surfaces its accessible name as text — e.g. a logo `<a>` wrapping
        // an SVG renders "Second Life Marketplace".
        if cctx.link.is_some()
            && let Some(label) = self.icon_only_label(id)
        {
            self.place_text(&label, &cctx);
        }

        // The top-left of this block's content box and its band — the origin
        // and width that `place_positioned_children` resolves its out-of-flow
        // descendants' coordinates against (a positioned block is their
        // containing block). Captured before the content is laid.
        let corner_start_row = self.rows.len();
        let corner_band = (self.line_left, self.line_right);

        if hscroll {
            self.flow_hscroll(id);
        } else {
            match flex {
                // An explicit `grid-template-columns` lays the page's own
                // tracks (`flow_grid_tracks`); a bare/auto-fill-less grid or an
                // unparseable template falls back to the shelf-packed grid.
                Some(FlexMode::Grid) => {
                    if !self.flow_grid_tracks(id, &cctx) {
                        self.flow_flex_wrap(id, &cctx);
                    }
                }
                Some(FlexMode::Row) => self.flow_flex_row(id, &cctx),
                Some(FlexMode::Column) => self.stack_flex_items(id, &cctx),
                None => {
                    // A block whose children are all atomic inline boxes lays
                    // them as a wrapping row of sub-boxes (the inline formatting
                    // context the spec calls for); the line model alone would
                    // stack each multi-row tile onto its own row.
                    if block_like && self.is_inline_box_grid(id) {
                        self.flow_inline_box_grid(id, &cctx);
                    } else {
                        for child in self.flow_children(id) {
                            self.flow_node(child, &cctx);
                        }
                    }
                }
            }
            // Out-of-flow children are skipped by the in-flow walk above
            // (`flow_of` → `Flow::None`) and placed by their containing block in
            // the block-tail `place_positioned_children` below.
        }

        // ...and `::after` closes it.
        if let Some(t) = self.pseudo_text(id, crate::dom::PseudoEl::After) {
            self.place_text(&t, &cctx);
        }

        // An inline element's right margin becomes a trailing gap (the icon's
        // `margin-right` separating it from the label that follows).
        if !block_like && self.inline_h_margin(id, "margin-right") {
            self.pending_space = true;
        }

        // A button-less form carries its synthetic submit on the form node. A
        // BLOCK form emits it on its own row at the end; an INLINE-level form
        // (the header search bar, `display:inline-flex`) keeps it inline so the
        // bar reads `[Search…] [ Submit ]` on one row instead of two.
        if tag == "form"
            && let Some(&(form, field)) = self.controls.get(&id)
        {
            let label = self.field_label(form, field);
            if !label.is_empty() {
                let inline_form = matches!(
                    self.dom.computed_display(id).as_deref(),
                    Some("inline" | "inline-block" | "inline-flex" | "inline-grid")
                );
                if !inline_form {
                    self.flush_block();
                }
                self.place_atom(label, ItemKind::Form, id, Some(Link::Form { form, field }));
            }
        }

        if block_like {
            self.flush_block();
            // Flush this BFC's own floats within it (sizing the box to
            // contain them), then restore the outer float context.
            if let Some(outer) = saved_floats {
                self.finish_floats();
                self.floats = outer;
            } else if self.dom.has_clearing_pseudo(id) {
                // The clearfix idiom: a `::after{clear:both}` is a generated box
                // that drops below the floats preceding it, so the element ends
                // BELOW them. This both contains a float grid that's its own
                // children (`.row::after`) and clears leaked floats for what
                // follows (a standalone `<div class="clearfix">`) — the
                // universal pre-flexbox containment pattern. Without it the next
                // section (pagination, "suggested users") paints over the grid.
                self.clear_to(true, true);
            }
            if let Some(marker) = list_marker {
                // outside: the gutter is the original margin (indent − the
                // width we added); inside: the marker sits at the margin itself.
                let gutter = self.indent.saturating_sub(marker_added);
                self.place_list_marker(&marker, marker_start_row, gutter);
            }
            // This block is the containing block for any `position:absolute`
            // descendant whose nearest positioned ancestor it is (and `fixed`
            // ones when it is the root). Place them at their computed
            // coordinates relative to this block's content box, now that its
            // in-flow content (and so its height) is known.
            if self.is_positioned(id) {
                let cb_w = corner_band.1.saturating_sub(corner_band.0);
                let content_h = self.rows.len().saturating_sub(corner_start_row);
                let cb_h = self
                    .dom
                    .computed_style(id, "height")
                    .and_then(|v| css_length_rows(&v))
                    .unwrap_or(content_h)
                    .max(content_h);
                self.place_positioned_children(
                    Some(id),
                    corner_band.0,
                    corner_start_row,
                    cb_w,
                    cb_h,
                );
            }
            if self.gap_after(id, &tag) {
                self.push_blank();
            }
        }
        self.ws = saved_ws;
        (self.clip_right, self.clip_done) = saved_clip;
        self.align = saved_align;
        if pushed_list {
            self.list_stack.pop();
        }
        self.indent -= center_pad;
        self.width = saved_width;
        self.indent -= marker_added;
        self.indent -= indent_add;
        if block_like {
            self.begin_line();
        }
    }

    /// The resolved `::before`/`::after` text for an element. On the JS
    /// path the layout arena has no `<style>`, so the serializer baked the
    /// content into a `data-trust-{before,after}` attr; on the static path
    /// we cascade the page sheets directly.
    fn pseudo_text(&self, id: NodeId, which: crate::dom::PseudoEl) -> Option<String> {
        let attr = match which {
            crate::dom::PseudoEl::Before => "data-trust-before",
            crate::dom::PseudoEl::After => "data-trust-after",
        };
        if let Some(v) = self.dom.attr(id, attr) {
            return (!v.is_empty()).then(|| v.to_owned());
        }
        self.dom.pseudo_content(id, which)
    }

    /// Whether an inline element's horizontal margin (`margin-left` or
    /// `-right`) is positive — rendered as a single-cell gap (terminal cells
    /// are too coarse for sub-cell spacing). An icon `<i style="margin-
    /// right:.3em">` separating from its label, a nav `<li style="margin-
    /// right:1em">` separating links: the margin is the only thing keeping the
    /// glyph off the next word, and we were dropping it. `pending_space` is a
    /// flag, so this only ENSURES a gap — it never doubles an existing one.
    fn inline_h_margin(&self, id: NodeId, prop: &str) -> bool {
        self.dom
            .computed_style(id, prop)
            .and_then(|v| {
                resolve_cells(
                    &v,
                    self.width.saturating_sub(self.indent).max(1),
                    self.viewport_w,
                )
            })
            .is_some_and(|c| c > 0)
    }

    /// The left indent (cells) a block contributes: CSS `margin-left` +
    /// `padding-left` when set (even to 0), else the HTML tag default.
    fn block_indent(&self, id: NodeId, tag: &str) -> usize {
        if self.inner_border_box == Some(id) {
            // Bordered interior: the frame sits at the margin; only padding
            // indents the content inside it.
            return indent_cells(self.dom.computed_style(id, "padding-left").as_deref())
                .min(self.width / 4);
        }
        let ml = self.dom.computed_style(id, "margin-left");
        let pl = self.dom.computed_style(id, "padding-left");
        if ml.is_some() || pl.is_some() {
            let avail = self.width.saturating_sub(self.indent);
            // A fixed-width block whose width alone meets/exceeds the space it
            // sits in has no room for a left margin beside it — that margin is
            // a desktop centering gutter (`margin:0 auto` resolved to px for a
            // wide reported viewport). Drop it at terminal width; padding,
            // which is *inside* the box, still applies.
            let centering_gutter = self
                .css_cells(id, "width")
                .or_else(|| self.css_cells(id, "max-width"))
                .is_some_and(|w| w >= avail);
            let margin = if centering_gutter {
                0
            } else {
                indent_cells(ml.as_deref())
            };
            (margin + indent_cells(pl.as_deref())).min(self.width / 4)
        } else {
            match tag {
                "ul" | "ol" | "blockquote" | "dd" => 2,
                _ => 0,
            }
        }
    }

    /// Constrain a block to its definite `width`/`max-width`, narrowing the
    /// content band so its content wraps within the declared width (geometry
    /// Phase 2 — honoring the box the page asks for). Auto margins position it:
    /// centered for both-auto, right for left-auto, otherwise left-aligned.
    /// Mutates `indent`/`width`; returns the left pad added (restored at block
    /// exit). 0 = left unconstrained (no definite width, or it meets/exceeds the
    /// band — we never cramp below the available width, and `auto`/intrinsic
    /// widths resolve to `None` so they flow wide as before).
    fn constrain_block_width(&mut self, id: NodeId) -> usize {
        // The root of this sub-layout (a float / flex / grid item) was already
        // sized to its `width` by the parent pass — the constraint this subtree
        // fills IS that width. Re-narrowing on the element's own `width` here
        // double-counts it: a `float:left;width:16.66%` column would resolve its
        // `%` against the already-narrowed band a second time (16.66% of a 27-
        // cell float box → a 5-cell band), collapsing the column's content (the
        // erome thumbnail grid regressed exactly this way). Leave the band whole.
        if self.subtree_root == Some(id) {
            return 0;
        }
        let avail = self.width.saturating_sub(self.indent).max(1);
        // A definite `width` is an explicit target — narrow to it regardless of
        // margins. A bare `max-width` (no `width`) is only a CEILING: an
        // auto-width block under it already fills the band, so we narrow to it
        // ONLY to position it via an auto margin (the centered/`margin:0 auto`
        // content wrapper). Treating `max-width` as the width WITHOUT an auto
        // margin breaks flex/auto layouts — Steam's `.sale_capsule{max-width:
        // 50%}` flex capsules narrowed the band to 50% and shrank their
        // `width:100%` thumbnails (the cap is the flex item's, sized by the flex
        // pass, not a block width to re-narrow here).
        let definite_w = self.css_cells(id, "width");
        let Some(w) = definite_w.or_else(|| self.css_cells(id, "max-width")) else {
            return 0;
        };
        let w = w.min(avail);
        if w >= avail {
            return 0; // no room to spare → nothing to position
        }
        let ml_auto = self.dom.computed_style(id, "margin-left").as_deref() == Some("auto");
        let mr_auto = self.dom.computed_style(id, "margin-right").as_deref() == Some("auto");
        let extra = avail - w;
        let pad = match (ml_auto, mr_auto) {
            (true, true) => extra / 2, // margin:0 auto → centered
            (true, false) => extra,    // margin-left:auto → right-aligned
            (false, true) => 0,        // margin-right:auto → left-aligned
            // No auto margin: narrow only for an explicit `width`. A bare
            // `max-width` is just a ceiling here → leave the band alone.
            (false, false) if definite_w.is_some() => 0,
            (false, false) => return 0,
        };
        self.indent += pad;
        self.width = self.indent + w;
        pad
    }

    /// Whether a block opens with a blank spacer row: CSS top
    /// margin/padding when set, else the tag default (`SPACING`).
    fn gap_before(&self, id: NodeId, tag: &str) -> bool {
        if self.inner_border_box == Some(id) {
            // Bordered interior: margin is applied outside the frame; only its
            // own top padding spaces the content inside.
            return self
                .dom
                .computed_style(id, "padding-top")
                .is_some_and(|v| vertical_space(&v));
        }
        let mt = self.dom.computed_style(id, "margin-top");
        let pt = self.dom.computed_style(id, "padding-top");
        if mt.is_some() || pt.is_some() {
            [mt, pt].into_iter().flatten().any(|v| vertical_space(&v))
        } else {
            SPACING.contains(&tag)
        }
    }

    /// Whether a block closes with a blank spacer row (bottom side).
    fn gap_after(&self, id: NodeId, tag: &str) -> bool {
        if self.inner_border_box == Some(id) {
            return self
                .dom
                .computed_style(id, "padding-bottom")
                .is_some_and(|v| vertical_space(&v));
        }
        let mb = self.dom.computed_style(id, "margin-bottom");
        let pb = self.dom.computed_style(id, "padding-bottom");
        if mb.is_some() || pb.is_some() {
            [mb, pb].into_iter().flatten().any(|v| vertical_space(&v))
        } else {
            SPACING.contains(&tag)
        }
    }

    /// How an element flows, from its cascaded `display` (falling back to
    /// the HTML tag default when no rule sets it).
    fn flow_of(&self, id: NodeId, tag: &str) -> Flow {
        if self.dom.computed_display(id).as_deref() == Some("none") {
            return Flow::None;
        }
        // An out-of-flow box (`position:absolute`/`fixed`) is removed from
        // normal flow (CSS 2.1 §9.6): its containing block places it at its
        // computed coordinates (`place_positioned_children`), so the in-flow
        // walk skips it. The exception is when WE are laying this very box as
        // its own sub-box (it is the `subtree_root`).
        if self.is_out_of_flow(id) {
            if self.subtree_root != Some(id) {
                return Flow::None;
            }
            // Laying the out-of-flow box itself: CSS §9.7 BLOCKIFIES an
            // absolutely-positioned/fixed box (computed `display:inline` /
            // `inline-*` / `list-item` → block). As a block it runs the
            // block-tail `place_positioned_children` for its OWN out-of-flow
            // descendants — e.g. a fill `<img>` inside an abspos overlay `<a>`
            // (a default-inline anchor): without blockification the anchor laid
            // inline, never placed the image, and the thumbnail vanished.
            // `flex_mode` still reads the raw display, so an abspos
            // `inline-flex`/`inline-grid` lays as a flex/grid container.
            return Flow::Block;
        }
        if let Some(d) = self.dom.computed_display(id) {
            return match d.as_str() {
                "none" => Flow::None,
                "inline" | "inline-block" | "inline-flex" | "inline-grid" | "contents" => {
                    Flow::Inline
                }
                "list-item" => Flow::ListItem,
                _ => Flow::Block,
            };
        }
        if tag == "li" {
            Flow::ListItem
        } else if BLOCK.contains(&tag) {
            Flow::Block
        } else {
            Flow::Inline
        }
    }

    /// Whether `id` sits inside an atomic inline box — an `inline-block` /
    /// `inline-flex` / `inline-grid` ancestor reached before any block-level
    /// ancestor. Such a box is inline-level from the OUTSIDE: its block-level
    /// content (the classic case is a `display:block` avatar `<img>` inside an
    /// `inline-flex` wrapper) must NOT break the surrounding line, or each one
    /// towers onto its own row — XenForo's "most reactions" grid and every
    /// forum-row avatar. Plain inline ancestors (`<a>`, `<span>`) are
    /// transparent (keep walking); a block/flex/grid ancestor is a real block
    /// formatting context and stops the walk (the element stays block-level).
    fn in_atomic_inline_context(&self, id: NodeId) -> bool {
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..8 {
            let Some(p) = cur else { return false };
            match self.dom.computed_display(p).as_deref() {
                Some("inline-block" | "inline-flex" | "inline-grid") => return true,
                Some("inline" | "contents") => {} // transparent — keep walking
                Some(_) => return false,          // an explicit block-level box
                None => {
                    // No explicit display rule: the tag default decides.
                    let tag = self.dom.tag_name(p).unwrap_or("");
                    if tag == "li" || BLOCK.contains(&tag) {
                        return false;
                    }
                    // inline by default (`<a>`/`<span>`/…): transparent.
                }
            }
            cur = self.dom.parent_composed(p);
        }
        false
    }

    /// Whether an element's computed `display` is a flex/grid container.
    fn is_flex_or_grid(&self, id: NodeId) -> bool {
        matches!(
            self.dom.computed_display(id).as_deref(),
            Some("flex" | "inline-flex" | "grid" | "inline-grid")
        )
    }

    /// Whether `id`'s parent is taken out of normal flow (`position:absolute`/
    /// `fixed`) — which we render as a compact inline overlay, so a float
    /// inside it shouldn't break the line.
    fn parent_out_of_flow(&self, id: NodeId) -> bool {
        self.dom
            .parent_composed(id)
            .is_some_and(|p| self.is_out_of_flow(p))
    }

    /// Whether `id`'s parent is a flex/grid container — so `id` is a flex item
    /// (CSS ignores `float` on flex items).
    fn parent_is_flex_container(&self, id: NodeId) -> bool {
        self.dom
            .parent_composed(id)
            .is_some_and(|p| self.is_flex_or_grid(p))
    }

    /// Whether `id`'s parent is an INLINE-level flex/grid container
    /// (`inline-flex`/`inline-grid`), which we lay by inline recursion — so a
    /// block-level flex child of it would wrongly take its own row.
    fn parent_is_inline_flex(&self, id: NodeId) -> bool {
        self.dom.parent_composed(id).is_some_and(|p| {
            matches!(
                self.dom.computed_display(p).as_deref(),
                Some("inline-flex" | "inline-grid")
            )
        })
    }

    /// Whether `id`'s computed `display` is an inline-level box (so a `float`
    /// on it, when it's a flex item, can be dropped and it flows inline).
    fn is_inline_level(&self, id: NodeId) -> bool {
        matches!(
            self.dom.computed_display(id).as_deref(),
            Some("inline" | "inline-block" | "inline-flex" | "inline-grid")
        )
    }

    /// Whether `id` is an inline-level box by its EFFECTIVE display — the
    /// author's cascaded `display` if set, otherwise the tag's UA default
    /// (`<i>`/`<span>`/`<a>` inline, `<div>`/`<li>`/headings block). Unlike
    /// `is_inline_level` (cascade-only, blind to UA defaults) this sees an
    /// un-styled `<i>` as inline. Distinguishes a small floated overlay glyph
    /// we render compactly (erome's `<i float:left>`) from a real floated
    /// layout block (Steam's `display:block;float:left` nav item).
    fn is_inline_box(&self, id: NodeId) -> bool {
        match self.dom.computed_display(id).as_deref() {
            Some(d) => matches!(d, "inline" | "inline-block" | "inline-flex" | "inline-grid"),
            None => {
                let tag = self.dom.tag_name(id).unwrap_or("");
                tag != "li" && !BLOCK.contains(&tag)
            }
        }
    }

    /// Whether every flex item of `id` is an inline-by-default element (a nav
    /// link's icon `<i>` + text `<span>`, a split button) — so the box is a
    /// single inline run, not a column of block rows. Keyed on each child's
    /// TAG default (a `<div>` row stays block even when styled `inline-block`),
    /// so a stacked title/date column (`<div>` items) is NOT pulled inline.
    fn flex_items_all_inline(&self, id: NodeId) -> bool {
        let items = self.flex_items(id);
        !items.is_empty()
            && items.iter().all(|&c| {
                let tag = self.dom.tag_name(c).unwrap_or("");
                tag != "li" && !BLOCK.contains(&tag)
            })
    }

    /// Whether a block establishes an inline formatting context of ATOMIC INLINE
    /// boxes that the LINE MODEL CANNOT PLACE, so it must be laid by
    /// `flow_inline_box_grid` (each child a sub-box on wrapping line boxes).
    /// Three conditions: (1) every in-flow child is an element whose computed
    /// display is `inline-block`/`inline-flex`/`inline-grid` (CSS Display §2:
    /// outer `inline`, inner `flow-root`); (2) at least two such children
    /// (one lays fine inline already); and (3) at least one child carries
    /// block-level content (`box_has_block_content`) — a multi-row tile (icon
    /// over caption) that the line model would tower into its own row. A run of
    /// single-row inline-block text (a nav bar's `<a>`s) stays in the line model,
    /// which spaces them by their collapsed inter-element whitespace; routing
    /// those here would drop that whitespace and fuse them ("ABOUTBLOGEVENTS").
    /// An anonymous text run among the children (`tag_name` is `None`) fails (1).
    fn is_inline_box_grid(&self, id: NodeId) -> bool {
        let items = self.flex_items(id);
        items.len() >= 2
            && items.iter().all(|&c| {
                self.dom.tag_name(c).is_some()
                    && matches!(
                        self.dom.computed_display(c).as_deref(),
                        Some("inline-block" | "inline-flex" | "inline-grid")
                    )
            })
            && items.iter().any(|&c| self.box_has_block_content(c))
    }

    /// Whether an atomic inline box holds block-level content — a child element
    /// whose effective display is block-level (`block`/`flex`/`grid`/`table`/
    /// `list-item`). Such a child opens a new line inside the box, so the box is
    /// multi-row and the line model can't place it (it would break the row). A
    /// box of only text/inline content (a text link) is single-row and the line
    /// model handles it. Direct children only — cheap, and the icon-over-caption
    /// tile pattern this targets puts its blocks at depth one.
    fn box_has_block_content(&self, id: NodeId) -> bool {
        self.dom.children(id).into_iter().any(|c| {
            self.dom.tag_name(c).is_some()
                && matches!(
                    self.dom.effective_display(c).as_deref(),
                    Some("block" | "flex" | "grid" | "table" | "inline-table" | "list-item")
                )
        })
    }

    /// Whether an element is positioned (so it's the containing block for its
    /// `position:absolute` descendants).
    fn is_positioned(&self, id: NodeId) -> bool {
        matches!(
            self.dom.computed_style(id, "position").as_deref(),
            Some("relative" | "absolute" | "fixed" | "sticky")
        )
    }

    /// The children that form a positioned (virtual-scroll) stack: ≥2
    /// `position:absolute` element children that set `top`, with ≥2 DISTINCT
    /// `top` values (so they tile vertically — a virtual list — rather than
    /// overlapping at one corner like stacked badges). Returned sorted by `top`
    /// ascending, so a scroller's pixel-positioned rows lay in their true
    /// vertical order instead of collapsing into one inline pile. Corner
    /// overlays are excluded (they keep their right-aligned first-row path);
    /// when the children don't look like a positioned list this returns empty,
    /// leaving the compact inline-overlay behavior for badges/backdrops intact.
    /// Whether an element is taken out of normal flow by `position:absolute`
    /// or `fixed` (CSS 2.1 §9.6) — its containing block places it at its
    /// computed coordinates (`place_positioned_children`); the in-flow walk
    /// skips it.
    fn is_out_of_flow(&self, id: NodeId) -> bool {
        // The surfaced modal flows as a normal block (its content IS the page
        // now), not an out-of-flow box placed by a containing block.
        if Some(id) == self.modal_root {
            return false;
        }
        matches!(
            self.dom.computed_style(id, "position").as_deref(),
            Some("absolute" | "fixed")
        )
    }

    /// Whether `id` is the surfaced modal overlay or sits inside it — so its
    /// images count as the modal's foreground content (the page-backdrop drops
    /// must spare them). When a modal is surfaced the layout only flows that
    /// subtree, but the check walks ancestors so it's correct regardless.
    fn within_modal(&self, id: NodeId) -> bool {
        let Some(m) = self.modal_root else {
            return false;
        };
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == m {
                return true;
            }
            cur = self.dom.parent_composed(c);
        }
        false
    }

    /// The topmost page-covering modal overlay in the document, if any. A cell
    /// grid can't composite transparent layers; instead of stacking such an
    /// overlay below the page in document order (where it reads as duplicate
    /// content — an age gate UNDER the login it should cover), we surface only
    /// this subtree and defer the page behind it. Topmost = the largest
    /// `(z-index, document order)`, matching CSS paint order (equal z-index
    /// paints later-in-tree on top). See `is_modal_overlay` for what qualifies.
    fn find_modal_overlay(&self) -> Option<NodeId> {
        let root = body_or_document(self.dom);
        let candidate = self
            .dom
            .descendants(root)
            .into_iter()
            .enumerate()
            .filter(|&(_, id)| self.is_modal_overlay(id))
            .max_by_key(|&(order, id)| (self.z_order(id), order))
            .map(|(_, id)| id)?;
        // The geometry test also matches a full-viewport BACKGROUND layer (a
        // hero / `position:fixed; inset:0` slideshow). That isn't a modal — a
        // modal paints ON TOP of the page, a background sits BEHIND it. Reject
        // the candidate when the page's own in-flow content stacks above it
        // (pixiv's logged-out top is a fixed slideshow at `z-index:auto` behind
        // a `position:relative; z-index:1` signup card — treating the slide as a
        // modal hid the entire login page). Explicit dialog semantics
        // (`role=dialog`/`aria-modal`/`<dialog open>`) are always honored.
        if !self.is_semantic_dialog(candidate) && self.content_paints_above(candidate) {
            return None;
        }
        Some(candidate)
    }

    /// Whether the page's normal-flow content paints ABOVE `overlay` — the proof
    /// that `overlay` is a full-viewport BACKGROUND layer, not a modal. A real
    /// modal lifts above the document flow; a background sits below it. We
    /// compare `z-index` only against in-flow (`relative`/`sticky`)
    /// content-bearing boxes OUTSIDE the overlay's own subtree and ancestry —
    /// out-of-flow chrome (a fixed footer/header) isn't "the page behind the
    /// modal", so excluding it avoids false negatives that would hide a genuine
    /// modal sitting beneath such chrome.
    fn content_paints_above(&self, overlay: NodeId) -> bool {
        let oz = self.z_order(overlay);
        // Exclude the overlay, its subtree (its own content) and its ancestors
        // (whose stacking context carries the overlay along).
        let mut excluded: HashSet<NodeId> = self.dom.descendants(overlay).into_iter().collect();
        excluded.insert(overlay);
        let mut up = self.dom.parent_composed(overlay);
        while let Some(p) = up {
            excluded.insert(p);
            up = self.dom.parent_composed(p);
        }
        let root = body_or_document(self.dom);
        self.dom.descendants(root).into_iter().any(|id| {
            !excluded.contains(&id)
                && matches!(
                    self.dom.computed_style(id, "position").as_deref(),
                    Some("relative" | "sticky")
                )
                && self.z_order(id) > oz
                && self.overlay_has_content(id)
        })
    }

    /// Whether `id` is a page-covering modal overlay: out of flow
    /// (`absolute`/`fixed`), not hidden, holding real content (so a bare
    /// decorative backdrop doesn't qualify), AND either covering the viewport
    /// geometrically or carrying dialog semantics (`role=dialog`/`aria-modal`/
    /// `<dialog open>`). The agreed detection — geometry OR semantics — for
    /// age gates, consent walls, login modals, and lightboxes.
    fn is_modal_overlay(&self, id: NodeId) -> bool {
        self.is_out_of_flow(id)
            && !self.dom.is_hidden(id)
            && (self.covers_viewport(id) || self.is_semantic_dialog(id))
            && self.overlay_has_content(id)
    }

    /// Whether `id`'s used box covers the whole viewport — width and height
    /// each either fill it (`100%`/`100vw`/`100vh`) or pin both opposite
    /// offsets to zero (`inset:0`). A `%`/inset fill is viewport-sized only
    /// when the containing block IS the viewport: always for `position:fixed`,
    /// for `absolute` only with no positioned ancestor. Viewport units
    /// (`vw`/`vh`) are viewport-sized regardless of containing block.
    fn covers_viewport(&self, id: NodeId) -> bool {
        let val = |p: &str| {
            self.dom
                .computed_style(id, p)
                .map(|v| v.trim().to_ascii_lowercase())
        };
        let is_zero = |v: Option<String>| v.is_some_and(|v| is_zero_length(&v));
        let w = val("width");
        let h = val("height");
        let w_vw = w.as_deref() == Some("100vw");
        let h_vh = h.as_deref() == Some("100vh");
        let w_pct = w.as_deref() == Some("100%") || (is_zero(val("left")) && is_zero(val("right")));
        let h_pct = h.as_deref() == Some("100%") || (is_zero(val("top")) && is_zero(val("bottom")));
        if !((w_vw || w_pct) && (h_vh || h_pct)) {
            return false;
        }
        if w_vw && h_vh {
            return true;
        }
        match self.dom.computed_style(id, "position").as_deref() {
            Some("fixed") => true,
            Some("absolute") => !self.has_positioned_ancestor(id),
            _ => false,
        }
    }

    /// Whether an out-of-flow (`absolute`/`fixed`) box is positioned outside the
    /// clip of its containing block — so a browser CLIPS it to invisibility and
    /// we must not render it. This is the one faithful, representable piece of
    /// absolute positioning in a line model: "off-screen" simply means "emit
    /// nothing" (no layers, no compositing, no pixel canvas). It generalizes the
    /// carousel-next-slide case (Steam's spotlight slide 2 is `position:absolute;
    /// left:90%` inside `.carousel_items{overflow:hidden}` — parked off-canvas)
    /// to any off-screen drawer / off-canvas menu / peeked slide.
    ///
    /// STANDARDS: an `overflow:hidden`/`clip` element clips an absolutely
    /// positioned descendant ONLY when it is that descendant's containing block
    /// (the nearest positioned ancestor) — the classic "abspos escapes
    /// `overflow:hidden` unless the overflow box is positioned" rule. We resolve
    /// the box's rect in containing-block-FRACTION units (so `%` offsets/sizes
    /// need no pixel geometry) and drop it only when it is MAJORITY-clipped. The
    /// one terminal deviation: a browser would paint a thin visible sliver of a
    /// barely-overlapping box; we can't render a fractional slice of an arbitrary
    /// positioned box, and such a sliver (the edge of the next carousel page) is
    /// never useful, so ≥75%-clipped is treated as gone. A length/`auto`/`calc`
    /// offset we can't resolve to a fraction stays INDETERMINATE → kept (we never
    /// drop on uncertain geometry).
    fn is_clipped_offscreen(&self, id: NodeId) -> bool {
        if !self.is_out_of_flow(id) {
            return false;
        }
        // The clip comes from the containing block (nearest positioned ancestor);
        // for `fixed` / no positioned ancestor it is the viewport, which always
        // clips. A containing block that doesn't clip can't hide the box.
        let cb = self.positioned_containing_block(id);
        let (clip_x, clip_y) = match cb {
            Some(c) => (self.clips_hard(c, false), self.clips_hard(c, true)),
            None => (true, true),
        };
        if !clip_x && !clip_y {
            return false;
        }
        // Visible fraction per clipped axis; an unclipped or indeterminate axis
        // contributes 1.0 (can't conclude "hidden" from it).
        let vx = if clip_x {
            self.axis_visible_fraction(id, false).unwrap_or(1.0)
        } else {
            1.0
        };
        let vy = if clip_y {
            self.axis_visible_fraction(id, true).unwrap_or(1.0)
        } else {
            1.0
        };
        vx * vy < OFFSCREEN_VISIBLE_MIN
    }

    /// The containing block of an out-of-flow box: its nearest ancestor with a
    /// non-`static` `position` (`relative`/`absolute`/`fixed`/`sticky`). `None`
    /// at `body`/`html` — i.e. the initial containing block (viewport).
    fn positioned_containing_block(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if matches!(self.dom.tag_name(p), Some("body" | "html")) {
                return None;
            }
            if matches!(
                self.dom.computed_style(p, "position").as_deref(),
                Some("relative" | "absolute" | "fixed" | "sticky")
            ) {
                return Some(p);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// The element's `overflow` on one axis, honoring the `overflow-x`/`-y`
    /// longhands over the `overflow: <x> <y>` shorthand (single value = both).
    fn axis_overflow(&self, id: NodeId, vertical: bool) -> Option<String> {
        let longhand = if vertical { "overflow-y" } else { "overflow-x" };
        if let Some(v) = self.dom.computed_style(id, longhand) {
            return Some(v.trim().to_ascii_lowercase());
        }
        let sh = self.dom.computed_style(id, "overflow")?;
        let mut toks = sh.split_whitespace();
        let x = toks.next();
        let y = toks.next().or(x);
        (if vertical { y } else { x }).map(|s| s.to_ascii_lowercase())
    }

    /// Whether an element hard-clips one axis (`overflow:hidden`/`clip`) — the
    /// cases where content positioned outside the box is painted nowhere
    /// (`scroll`/`auto` are excluded: their off-box content is reachable).
    fn clips_hard(&self, id: NodeId, vertical: bool) -> bool {
        matches!(
            self.axis_overflow(id, vertical).as_deref(),
            Some("hidden" | "clip")
        )
    }

    /// The fraction of an out-of-flow box VISIBLE within its containing block on
    /// one axis (`0.0` fully clipped … `1.0` fully inside), resolved purely from
    /// `%`/zero offsets+size (in CB-fraction units). `None` when the span is
    /// indeterminate (a length/`auto`/`calc` we won't convert without pixel
    /// geometry) — the caller then keeps the box.
    fn axis_visible_fraction(&self, id: NodeId, vertical: bool) -> Option<f32> {
        let (start_p, end_p, size_p) = if vertical {
            ("top", "bottom", "height")
        } else {
            ("left", "right", "width")
        };
        let frac = |p: &str| {
            self.dom
                .computed_style(id, p)
                .and_then(|v| css_axis_fraction(&v))
        };
        let (start, end, size) = (frac(start_p), frac(end_p), frac(size_p));
        // Box start (s) and size (w) as fractions of the CB content box [0,1].
        let (s, w) = match (start, end, size) {
            (Some(s), _, Some(w)) => (s, Some(w)),
            (Some(s), Some(e), None) => (s, Some((1.0 - s - e).max(0.0))),
            (Some(s), None, None) => (s, None),
            (None, Some(e), Some(w)) => (1.0 - e - w, Some(w)),
            (None, Some(e), None) => {
                // Right/bottom-anchored, size unknown: only a far edge at/past
                // the near edge proves it fully off (e.g. `right:100%`).
                return if 1.0 - e <= 0.0 { Some(0.0) } else { None };
            }
            _ => return None,
        };
        match w {
            Some(w) if w > 0.0 => {
                let visible = (s + w).min(1.0) - s.max(0.0);
                Some((visible.max(0.0) / w).clamp(0.0, 1.0))
            }
            // Unknown width: conclude only when the start is itself off-screen.
            _ if s >= 1.0 => Some(0.0),
            _ => None,
        }
    }

    /// Whether any ancestor (up to the body) is positioned — so a `%`/inset
    /// fill on `id` is relative to that ancestor, not the viewport.
    fn has_positioned_ancestor(&self, id: NodeId) -> bool {
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if matches!(self.dom.tag_name(p), Some("body" | "html")) {
                break;
            }
            if matches!(
                self.dom.computed_style(p, "position").as_deref(),
                Some("relative" | "absolute" | "fixed" | "sticky")
            ) {
                return true;
            }
            cur = self.dom.parent_composed(p);
        }
        false
    }

    /// Whether `id` carries dialog semantics — `role=dialog`/`alertdialog`,
    /// `aria-modal=true`, or an open `<dialog>`.
    fn is_semantic_dialog(&self, id: NodeId) -> bool {
        matches!(self.dom.attr(id, "role"), Some("dialog" | "alertdialog"))
            || self.dom.attr(id, "aria-modal") == Some("true")
            || (self.dom.tag_name(id) == Some("dialog") && self.dom.attr(id, "open").is_some())
    }

    /// Whether an overlay holds real content (visible text or a clickable/form
    /// control) rather than being a bare backdrop (a full-bleed `<img>` layer).
    /// The guard that keeps decorative `position:fixed` backdrops from being
    /// mistaken for the modal they sit behind.
    fn overlay_has_content(&self, id: NodeId) -> bool {
        self.dom
            .descendants(id)
            .into_iter()
            .any(|d| match &self.dom.node(d).data {
                NodeData::Text(s) => !s.trim().is_empty(),
                NodeData::Element { .. } => {
                    let tag = self.dom.tag_name(d).unwrap_or("");
                    matches!(tag, "button" | "input" | "select" | "textarea" | "summary")
                        || (tag == "a" && self.dom.attr(d, "href").is_some())
                        || self.dom.attr(d, "role") == Some("button")
                        || self.dom.attr(d, "onclick").is_some()
                }
                _ => false,
            })
    }

    /// The `z-index` of `id` as an integer (`auto`/unset/unparseable → 0), for
    /// ordering overlapping overlays.
    fn z_order(&self, id: NodeId) -> i32 {
        self.dom
            .computed_style(id, "z-index")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
    }

    /// Whether a node's nearest preceding element sibling is out of flow — so
    /// a `<br>` that merely trails an overlay control is spurious and can be
    /// dropped (keeping the overlay's controls on one line).
    fn prev_sibling_out_of_flow(&self, id: NodeId) -> bool {
        let Some(parent) = self.dom.parent_composed(id) else {
            return false;
        };
        let sibs = self.dom.children(parent);
        let Some(pos) = sibs.iter().position(|&s| s == id) else {
            return false;
        };
        sibs[..pos]
            .iter()
            .rev()
            .find(|&&s| self.dom.tag_name(s).is_some())
            .is_some_and(|&s| self.is_out_of_flow(s))
    }

    /// How a flex container lays out, or `None` if it isn't one. A wrapping
    /// container is a `Grid` (shelf-packed, regardless of direction); a
    /// non-wrapping one is `Row` (side-by-side columns) or `Column`
    /// (stacked block-level items) per `flex-direction`/`flex-flow`.
    fn flex_mode(&self, id: NodeId) -> Option<FlexMode> {
        match self.dom.computed_display(id).as_deref() {
            // CSS grid always wraps into tracks; we approximate it as a
            // shelf-packed flex-wrap grid (template tracks ignored). Without
            // this, `display:grid` containers (danbooru's post list) fell to
            // block layout and stacked one item per row.
            Some("grid" | "inline-grid") => return Some(FlexMode::Grid),
            Some("flex" | "inline-flex") => {}
            _ => return None,
        }
        let flow = self.dom.computed_style(id, "flex-flow");
        let has = |prop: Option<&String>, words: &[&str]| {
            prop.is_some_and(|v| v.split_whitespace().any(|t| words.contains(&t)))
        };
        let wrap = has(
            self.dom.computed_style(id, "flex-wrap").as_ref(),
            &["wrap", "wrap-reverse"],
        ) || has(flow.as_ref(), &["wrap", "wrap-reverse"]);
        if wrap {
            return Some(FlexMode::Grid);
        }
        let column = self
            .dom
            .computed_style(id, "flex-direction")
            .is_some_and(|v| v.trim().starts_with("column"))
            || has(flow.as_ref(), &["column", "column-reverse"]);
        Some(if column {
            FlexMode::Column
        } else {
            FlexMode::Row
        })
    }

    /// Lay a non-wrapping flex-row out as side-by-side columns using the CSS
    /// flexbox main-axis algorithm: each item gets a *basis* (`flex-basis`,
    /// else `width`, resolving `%` against the row; else its content width),
    /// then free space is handed to `flex-grow` items and overflow is absorbed
    /// by `flex-shrink` items (down to their min-content width; `flex-shrink:0`
    /// holds firm). An empty, non-growing item collapses to nothing. If the
    /// row can't fit even at every item's minimum, it stacks vertically (the
    /// terminal has no horizontal scroll — her responsive default).
    fn flow_flex_row(&mut self, id: NodeId, ctx: &Ctx) {
        // Lay within the float-narrowed band (the block boundary already ran
        // `begin_line`), not the raw block box — so a flex row beside a float
        // (a latest-post avatar floated left of its title/date flex) flows to
        // the float's right instead of painting over it. `flow_flex_wrap`
        // already uses the band; this keeps the two consistent. With no active
        // float the band IS the block box, so behaviour is unchanged.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let gap = self.flex_gap(id, avail, false);
        let mut nodes = Vec::new();
        let mut basis = Vec::new();
        let mut grow = Vec::new();
        let mut shrink = Vec::new();
        for k in self.flex_items(id) {
            let (b_css, g, s) = self.flex_props(k, avail);
            let b = match b_css {
                Some(w) => w.min(avail),
                None => {
                    // `flex-basis:auto`/`width:auto`: size to content. An empty,
                    // non-growing item takes no column.
                    if g == 0.0 && self.is_empty_box(k) {
                        continue;
                    }
                    self.measure_width(k, avail)
                }
            };
            nodes.push(k);
            basis.push(b);
            grow.push(g);
            shrink.push(s);
        }
        let n = nodes.len();
        if n == 0 {
            return;
        }
        let gaps = (n - 1) * gap;
        let total_basis: usize = basis.iter().sum();
        let mut widths = basis.clone();
        if total_basis + gaps <= avail {
            // Free space is distributed to the grow items by their flex-grow.
            let free = avail - total_basis - gaps;
            let total_grow: f32 = grow.iter().sum();
            if total_grow > 0.0 && free > 0 {
                for i in 0..n {
                    widths[i] += (free as f32 * grow[i] / total_grow).round() as usize;
                }
            }
        } else {
            // Overflow: shrink. A shrinkable item can shrink to its minimum —
            // its explicit `min-width` if set (a hard floor; flexbox never
            // shrinks an item below it), else its min-content width capped at
            // its basis. A `flex-shrink:0` item keeps its basis.
            let floor: Vec<usize> = (0..n)
                .map(|i| {
                    let auto_min = if shrink[i] > 0.0 {
                        self.measure_width(nodes[i], 1).min(basis[i])
                    } else {
                        basis[i]
                    };
                    self.css_cells(nodes[i], "min-width")
                        .map_or(auto_min, |mw| mw.min(avail))
                })
                .collect();
            let sum_floor: usize = floor.iter().sum();
            if sum_floor + gaps > avail {
                // Even at every item's minimum the row overflows. Normally we
                // reflow it into a vertical stack (the terminal's responsive
                // default — a too-wide nav or card row stacks into a column).
                // But a flex container that CLIPS/scrolls its main axis
                // (`overflow`/`overflow-x: auto|scroll|hidden|clip`) is a scroll
                // context: a browser keeps its items side by side and scrolls
                // the overflow rather than reflowing. The terminal has no
                // horizontal scroll, so we keep them side by side and let the
                // overflow clip at the box edge — this is what keeps a code
                // view's fixed line-number gutter beside its (clipped) code
                // instead of dropping the gutter above it.
                if !self.clips_x(id) {
                    self.stack_boxes(&nodes, avail, ctx);
                    return;
                }
                widths = floor;
            } else {
                // CSS flexbox shrink resolution (§9.7 "Resolving Flexible
                // Lengths"): each item shrinks from its basis proportional to
                // `flex-shrink × flex-base-size`; an item that would shrink past
                // its minimum freezes there and the remaining overflow re-absorbs
                // across the rest. So EQUAL-basis items shrink to EQUAL widths — a
                // row of same-size cards stays uniform — instead of being spread
                // by their min-content (the old `floor + proportional-extra` split
                // gave a wider-captioned card a wider column, hence a wider image).
                widths = basis.clone();
                let mut frozen: Vec<bool> = shrink.iter().map(|&s| s <= 0.0).collect();
                loop {
                    let frozen_w: usize = (0..n).filter(|&i| frozen[i]).map(|i| widths[i]).sum();
                    let remaining = avail.saturating_sub(gaps).saturating_sub(frozen_w);
                    let live: Vec<usize> = (0..n).filter(|&i| !frozen[i]).collect();
                    let live_basis: usize = live.iter().map(|&i| basis[i]).sum();
                    if live.is_empty() || live_basis <= remaining {
                        // The unfrozen items now fit at their basis — done.
                        for &i in &live {
                            widths[i] = basis[i];
                        }
                        break;
                    }
                    let scaled: f32 = live.iter().map(|&i| shrink[i] * basis[i] as f32).sum();
                    if scaled <= 0.0 {
                        break;
                    }
                    let overflow = (live_basis - remaining) as f32;
                    let mut froze = false;
                    for &i in &live {
                        let reduce = overflow * (shrink[i] * basis[i] as f32) / scaled;
                        let w = (basis[i] as f32 - reduce).round().max(0.0) as usize;
                        if w <= floor[i] {
                            // Hit the minimum: freeze and let the rest re-absorb.
                            widths[i] = floor[i];
                            frozen[i] = true;
                            froze = true;
                        } else {
                            widths[i] = w;
                        }
                    }
                    if !froze {
                        break;
                    }
                }
            }
        }
        // `justify-content` distributes any leftover free space (when grow
        // didn't consume it): a leading offset and/or extra spacing between
        // items. No-op when the row is full (free == 0) or left-packed.
        let used: usize = widths.iter().map(|w| (*w).max(1)).sum::<usize>() + gaps;
        let free = avail.saturating_sub(used);
        let (lead, between) = self.justify_offsets(id, free, n);
        let row_base = self.rows.len();
        // Lay every column box first, so `align-items` can offset a column
        // shorter than the tallest within the row's (cross-axis) height.
        let mut boxes: Vec<LaidBox> = Vec::with_capacity(n);
        for i in 0..n {
            let cw = widths[i].max(1);
            let mut b = self.layout_subtree(nodes[i], cw, ctx);
            // A text/search input that grew (flex-grow) fills its box like a
            // real input field, rather than leaving a gap after its short
            // placeholder. (Buttons/selects don't stretch — see the helper.)
            if grow[i] > 0.0 {
                fill_input_box(&mut b, cw);
            }
            boxes.push(b);
        }
        let line_h = boxes.iter().map(|b| b.height as usize).max().unwrap_or(0);
        let mut x = lead;
        for i in 0..n {
            let cw = widths[i].max(1);
            if boxes[i].height > 0 {
                let dy = self.align_offset(id, boxes[i].height as usize, line_h);
                self.blit(&boxes[i], (self.line_left + x) as u16, row_base + dy);
            }
            x += cw + if i + 1 < n { gap + between } else { 0 };
        }
        self.col = self.line_left;
        self.pending_space = false;
    }

    /// `justify-content` main-axis distribution of `free` leftover cells across
    /// `n` items: `(leading offset, extra spacing per inter-item gap)`. Packing
    /// (`flex-start`/`normal`/unknown) and a full row leave both zero; grow
    /// items having eaten the free space makes this moot.
    fn justify_offsets(&self, id: NodeId, free: usize, n: usize) -> (usize, usize) {
        // While measuring intrinsic (max-content) width, `justify-content` must
        // not apply: the flex base size is the items packed at their natural
        // widths, never spread across the row. A nested `justify-content:flex-end`
        // block (a capsule's right-aligned price) otherwise pushed its content to
        // the row's right edge, so the whole item measured ~`avail` and the
        // shrink pass split the columns by caption length instead of evenly.
        if free == 0 || self.measuring {
            return (0, 0);
        }
        match self
            .dom
            .computed_style(id, "justify-content")
            .as_deref()
            .map(str::trim)
        {
            Some("flex-end" | "end" | "right") => (free, 0),
            Some("center") => (free / 2, 0),
            Some("space-between") if n > 1 => (0, free / (n - 1)),
            Some("space-around") => (free / (2 * n), free / n),
            Some("space-evenly") => (free / (n + 1), free / (n + 1)),
            _ => (0, 0),
        }
    }

    /// `align-items` cross-axis offset (rows from the top of the line/shelf)
    /// for an item of height `item_h` within a band of height `line_h`. We
    /// don't stretch item heights, so `stretch`/`baseline`/`normal`/unknown and
    /// `flex-start` all top-align; only `center` and `flex-end` shift down.
    fn align_offset(&self, id: NodeId, item_h: usize, line_h: usize) -> usize {
        let free = line_h.saturating_sub(item_h);
        if free == 0 {
            return 0;
        }
        match self
            .dom
            .computed_style(id, "align-items")
            .as_deref()
            .map(str::trim)
        {
            Some("center") => free / 2,
            Some("flex-end" | "end") => free,
            _ => 0,
        }
    }

    /// The flex/grid gap in cells along one axis: the `column-gap`/`row-gap`
    /// longhand, else the matching part of the `gap` shorthand (`gap: <row>
    /// [<col>]`, one value sets both), else the default — 1 cell between
    /// columns (readability, so items never fuse) and 0 between rows/shelves.
    fn flex_gap(&self, id: NodeId, avail: usize, row_axis: bool) -> usize {
        let longhand = if row_axis { "row-gap" } else { "column-gap" };
        if let Some(v) = self.dom.computed_style(id, longhand)
            && let Some(c) = resolve_cells(&v, avail, self.viewport_w)
        {
            return c;
        }
        if let Some(g) = self.dom.computed_style(id, "gap") {
            let toks: Vec<&str> = g.split_whitespace().collect();
            let tok = if row_axis {
                toks.first()
            } else {
                toks.get(1).or_else(|| toks.first())
            };
            if let Some(t) = tok
                && let Some(c) = resolve_cells(t, avail, self.viewport_w)
            {
                return c;
            }
        }
        usize::from(!row_axis)
    }

    /// The natural width (cells) of an element's subtree laid out at
    /// `constraint` — its content basis (at `avail`) or min-content (at 1).
    fn measure_width(&self, id: NodeId, constraint: usize) -> usize {
        self.layout_subtree_inner(id, constraint, None, true, &Ctx::root())
            .width as usize
    }

    /// A flex item's `(basis, grow, shrink)`. The `flex` shorthand is expanded
    /// into `flex-grow`/`flex-shrink`/`flex-basis` in the CASCADE (so source
    /// order resolves correctly — see `expand_box_shorthand`), so this just
    /// reads the three longhands. `basis` resolves `flex-basis` (else `width`,
    /// `%` against `avail`, capped by `max-width`); `None` means auto (size to
    /// content). Defaults: grow 0, shrink 1, basis auto.
    fn flex_props(&self, id: NodeId, avail: usize) -> (Option<usize>, f32, f32) {
        let grow = self.flex_number(id, "flex-grow").unwrap_or(0.0).max(0.0);
        let shrink = self.flex_number(id, "flex-shrink").unwrap_or(1.0).max(0.0);
        let basis = match self
            .dom
            .computed_style(id, "flex-basis")
            .as_deref()
            .map(str::trim)
        {
            None | Some("auto") => self.len_or_pct(id, "width", avail),
            Some("content" | "max-content" | "min-content" | "fit-content") => None,
            Some(v) => resolve_cells(v, avail, self.viewport_w),
        };
        // `max-width` caps the basis; if there is no basis yet, an explicit
        // max-width still bounds an auto (content-sized) item via the caller.
        let basis = match (basis, self.len_or_pct(id, "max-width", avail)) {
            (Some(b), Some(m)) => Some(b.min(m)),
            (b, _) => b,
        };
        (basis, grow, shrink)
    }

    /// A unitless flex number (`flex-grow`/`flex-shrink`), or `None`.
    fn flex_number(&self, id: NodeId, prop: &str) -> Option<f32> {
        self.dom.computed_style(id, prop)?.trim().parse().ok()
    }

    /// A `width`/`max-width`-style property as cells, resolving `%` against
    /// `avail` (the flex row's content width) and `vw` against the viewport.
    fn len_or_pct(&self, id: NodeId, prop: &str, avail: usize) -> Option<usize> {
        resolve_cells(&self.dom.computed_style(id, prop)?, avail, self.viewport_w)
    }

    /// Whether a block is a horizontal-scroll container (a carousel): it
    /// clips on the x axis AND has a `hscroll_track` (an over-wide child
    /// holding several cards).
    fn is_hscroll(&self, id: NodeId) -> bool {
        let scrolls = self
            .dom
            .computed_style(id, "overflow")
            .or_else(|| self.dom.computed_style(id, "overflow-x"))
            .is_some_and(|v| {
                v.split_whitespace()
                    .any(|t| matches!(t, "hidden" | "auto" | "scroll" | "clip"))
            });
        scrolls && self.hscroll_track(id).is_some()
    }

    /// The over-wide "track" inside a scroll container `id`: a child wider
    /// than the viewport whose own element children are the cards (≥3, so a
    /// real carousel rail — NOT a clearfix wrapping a single wide layout
    /// column, whose one float child isn't a rail of cards).
    fn hscroll_track(&self, id: NodeId) -> Option<NodeId> {
        let avail = self.width.saturating_sub(self.indent).max(1);
        self.dom.children(id).into_iter().find(|&c| {
            matches!(self.dom.node(c).data, NodeData::Element { .. })
                && self.css_cells(c, "width").is_some_and(|w| w > avail)
                && self
                    .dom
                    .children(c)
                    .iter()
                    .filter(|&&g| matches!(self.dom.node(g).data, NodeData::Element { .. }))
                    .count()
                    >= 3
        })
    }

    /// Lay a carousel: a row of card boxes side by side at their full strip
    /// width, blitted into the doc rows (the view clips this to the band and
    /// scrolls it). Records a `Carousel` with each card's left column as a
    /// snap stop so scrolling never cuts a card or image.
    fn flow_hscroll(&mut self, id: NodeId) {
        self.flush_block();
        self.begin_line();
        let band_left = self.line_left;
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        // The visible band is the scroll container's own width (an
        // `overflow:hidden` viewport with an explicit `width`/`max-width`
        // shows exactly that much — e.g. a 700px box reveals ~3 cards), or
        // the full available width when it sizes from its parent (the SL
        // Marketplace carousel inherits its width from a sized float
        // ancestor, which already narrowed `avail`). Cards still lay at their
        // own widths; only the visible window is clamped.
        let band_w = self
            .css_cells(id, "width")
            .or_else(|| self.css_cells(id, "max-width"))
            .map(|w| w.min(avail))
            .unwrap_or(avail)
            .max(1);
        // The over-wide inner "track" holds the cards.
        let track = self.hscroll_track(id).unwrap_or(id);
        let gap = 1usize;
        let row_base = self.rows.len();
        let mut x = 0usize;
        let mut stops = Vec::new();
        let mut height = 0usize;
        // A rail card is narrower than the rail. A card whose declared width
        // reaches or exceeds the band is full-bleed (a `width:100%` slide) or
        // carries a JS-computed width sized against a fictional viewport (slick
        // sets a per-slide pixel width that's unusable in our cell/geometry
        // model) — laying it at the full band shows ONE slide and makes a
        // fixed-aspect tile image fill the whole screen. Size such a card to
        // show a few across the band instead (the defining behaviour of a
        // carousel; ~3 mirrors the common rail). A reliable sub-band width is
        // honored as-is, and a card with no width keeps its measured content
        // width, so existing rails (SL Marketplace) are unchanged.
        // Subtract the inter-card gaps so THREE cards + their two gaps fit the
        // band EXACTLY: a card whose right edge spills even one cell past the
        // band fails `Carousel::shows` (full-containment) and its whole image is
        // dropped — `band_w/3` overshoots by the gaps, which is why the 3rd
        // featured card rendered its title but not its (band-wide) image.
        let multi_card_w = (band_w.saturating_sub(2 * gap) / 3).max(1);
        for card in self.flex_items(track) {
            let cw = match self
                .css_cells(card, "width")
                .or_else(|| self.css_cells(card, "max-width"))
            {
                Some(w) if w >= band_w => multi_card_w,
                Some(w) => w.clamp(1, avail),
                None => self.measure_width(card, avail).clamp(1, avail),
            };
            // Lay the card as a block (ignore its own float), then place it.
            let b = self.layout_subtree_inner(card, cw, Some(card), false, &Ctx::root());
            if b.height == 0 {
                continue;
            }
            stops.push(x as u16);
            self.blit(&b, (band_left + x) as u16, row_base);
            x += cw + gap;
            height = height.max(b.height as usize);
        }
        let strip_w = x.saturating_sub(gap);
        // Only a strip that actually overflows the band is a scroll region.
        if !stops.is_empty() && strip_w > band_w {
            // Generate our own prev/next scroll controls (the CSS
            // `::scroll-button` model — the browser, not the page, provides
            // them) and hide any the page authored, so a carousel always has
            // a working pair regardless of what markup it ships.
            self.emit_scroll_buttons(id, row_base, band_left, band_w, false);
            self.carousels.push(Carousel {
                start: row_base,
                end: row_base + height,
                left: band_left as u16,
                right: (band_left + band_w) as u16,
                width: strip_w as u16,
                stops,
                offset: 0,
                frame_right: None,
            });
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// Place a containing block's out-of-flow (`position:absolute|fixed`)
    /// descendants at their CSS-computed coordinates (CSS 2.1 §9.6 / §10.3.7 /
    /// §10.6.4), relative to the CB's content box whose top-left is
    /// (`origin_col`, `origin_row`) in cells and whose size is `cb_w`×`cb_h`
    /// cells. Each box is laid independently (`layout_subtree_inner`) and `blit`
    /// in document order — CSS paint order, so a later/topmost box wins shared
    /// cells. Hidden boxes (`opacity:0`/`display:none`/`visibility:hidden`) and
    /// boxes clipped off-screen by the CB are skipped, exactly as the in-flow
    /// walk would. A layout wider than the on-screen band is compressed to fit
    /// (the terminal has no horizontal scroll — her call, mirrors grid tracks).
    ///
    /// This single mechanism replaces the former slideshow / positioned-row-
    /// stack / inline-overlay heuristics: positioned columns land side by side,
    /// virtual-list rows land at their distinct `top`s, corner badges land in
    /// their corner — all because we lay each box where its own CSS says.
    fn place_positioned_children(
        &mut self,
        cb: Option<NodeId>,
        origin_col: usize,
        origin_row: usize,
        cb_w: usize,
        cb_h: usize,
    ) {
        let root = cb.unwrap_or_else(|| body_or_document(self.dom));
        // Compose shadow: a positioned box living in a web component's shadow
        // root (archive.org's `<infinite-scroller>` keeps its sentinel there)
        // is placed by its composed-tree containing block like any other, so
        // gather candidates through the shadow boundary, not light-DOM only.
        let kids: Vec<NodeId> = self
            .dom
            .composed_descendants(root)
            .into_iter()
            .filter(|&d| {
                matches!(self.dom.node(d).data, NodeData::Element { .. })
                    && self.is_out_of_flow(d)
                    && !self.dom.is_hidden(d)
                    && !self.is_clipped_offscreen(d)
                    && self.positioned_containing_block(d) == cb
            })
            .collect();
        if kids.is_empty() {
            return;
        }
        // Lay each box and resolve its used (left, top) in cells.
        struct Placed {
            node: NodeId,
            left: i32,
            top: i32,
            used_w: usize,
            bottom_pinned: bool,
            b: LaidBox,
        }
        let mut placed: Vec<Placed> = Vec::new();
        let mut union_w = 0i32;
        for k in kids {
            // An explicit `width` (or a `left`+`right` stretch) is the used
            // width; otherwise the box is shrink-to-fit, which we lay at the
            // full band and read back its content extent — laying it at a
            // pre-measured narrower width would re-wrap content that already
            // fit (Steam's nav floats wrapped their last item that way).
            let explicit_w = self.abs_used_width(k, cb_w);
            let lay_w = explicit_w.unwrap_or(cb_w).clamp(1, cb_w.max(1));
            // A positioned descendant still inherits an enclosing `<a>`'s link
            // (a badge/menu wrapped in an anchor stays clickable), even though
            // its containing block — not its DOM parent — places it.
            let inherit = self.ancestor_link_ctx(k);
            let b = self.layout_subtree_inner(k, lay_w, Some(k), false, &inherit);
            // Geometry: record this box's COMPUTED top-left (the coordinate its
            // containing block places it at) so an EMPTY abspos box — an
            // infinite-scroll sentinel paints no cells — still gets an honest
            // zero-height `getBoundingClientRect`/IntersectionObserver box.
            // Recorded at the placed coordinate, never the static in-flow DOM
            // position: a `bottom:0`/content-tracking sentinel descends as the
            // scroller's content grows (`cb_h` grows with the laid tiles), so
            // IO fires on real scroll instead of latching at the top and
            // looping. A non-empty box is covered by its laid cells, so this
            // only fills the gap left by the `b.height == 0` skip below.
            if self.tag_all_nodes {
                let uw = explicit_w.unwrap_or(b.width as usize).max(1);
                let gc = (origin_col as i32 + self.abs_used_left(k, cb_w as i32, uw as i32)).max(0);
                let gr = (origin_row as i32
                    + self.abs_used_top(k, cb_h as i32, b.height as i32).max(0))
                .max(0);
                self.element_tops.insert(
                    k,
                    (
                        u16::try_from(gc).unwrap_or(u16::MAX),
                        u16::try_from(gr).unwrap_or(u16::MAX),
                    ),
                );
            }
            if b.height == 0 {
                continue;
            }
            let used_w = explicit_w.unwrap_or(b.width as usize).max(1);
            let left = self.abs_used_left(k, cb_w as i32, used_w as i32);
            // Box height is content-driven: a cell grid has no internal scroll,
            // so a fixed-height `overflow` panel can't be honored. Per §10.6.4
            // that makes height auto; `top` is then clamped to the CB so a
            // bottom-anchored tall panel rides the top instead of off-screen.
            let top = self.abs_used_top(k, cb_h as i32, b.height as i32).max(0);
            // A box anchored AT or BELOW the CB's bottom edge (`top:auto` and
            // `bottom ≤ 0`, e.g. a footer at `bottom:-1.5rem`) follows the
            // content: in a scroll-free model the CB grows to contain its
            // positioned children, so this box sits after their extent (fixed
            // up below). An INSET bottom (`bottom > 0`) keeps the §10.6.4 clamp.
            let bottom_pinned = self.pos_len(k, "top", cb_h).is_none()
                && self.pos_len(k, "bottom", cb_h).is_some_and(|b| b <= 0.0);
            union_w = union_w.max(left.max(0) + used_w as i32);
            placed.push(Placed {
                node: k,
                left,
                top,
                used_w,
                bottom_pinned,
                b,
            });
        }
        if placed.is_empty() {
            return;
        }
        // The CB's used height grows to contain its non-bottom-pinned content;
        // re-place each bottom-pinned box just past that extent so a footer
        // lands below the columns instead of riding their top (the CB's own
        // declared height was indefinite — e.g. `vh` we can't resolve).
        let extent = placed
            .iter()
            .filter(|p| !p.bottom_pinned)
            .map(|p| p.top + p.b.height as i32)
            .max()
            .unwrap_or(cb_h as i32)
            .max(cb_h as i32);
        for p in placed.iter_mut().filter(|p| p.bottom_pinned) {
            p.top = self.abs_used_top(p.node, extent, p.b.height as i32).max(0);
        }
        // Compress-to-fit: scale columns down when the layout is wider than the
        // band actually on screen from `origin_col` (no horizontal scroll).
        let band = self.width.saturating_sub(origin_col).max(1) as i32;
        let scale = (band as f32 / union_w as f32).min(1.0);
        // Lay each box to its final (col, box, top); placement happens after so
        // a lift can shift the boxes below it.
        let mut blits: Vec<(u16, LaidBox, i32)> = placed
            .into_iter()
            .map(|p| {
                let (col, b) = if scale < 1.0 {
                    // Re-lay at the compressed width so the box's content reflows.
                    let w = ((p.used_w as f32 * scale).round() as usize).max(1);
                    let inherit = self.ancestor_link_ctx(p.node);
                    let b = self.layout_subtree_inner(p.node, w, Some(p.node), false, &inherit);
                    let col = (origin_col as i32 + (p.left as f32 * scale).round() as i32).max(0);
                    (col as u16, b)
                } else {
                    ((origin_col as i32 + p.left).max(0) as u16, p.b)
                };
                (col, b, p.top)
            })
            .collect();
        // A cell grid has no z-axis, and a decoded image is an atomic blit (a
        // sixel can't be partially overwritten — see the ratatui image model),
        // so an overlay positioned ON an image cannot be composited over it.
        // Rather than garble it (two glyphs fighting for a cell) or shove it off
        // to the side (the renderer's overlap-append), LIFT the overlay onto its
        // own row(s) at the same column by inserting blank rows: the image — and
        // the CB's content below it — shifts down, so the overlay reads just
        // above the image (a corner deal-badge over a store capsule). Processed
        // top-down so each insert shifts the boxes below it consistently. The
        // insert is confined to THIS containing block's row range (each flex
        // capsule is its own sub-box), so sibling capsules re-align instead of
        // drifting. Text-on-text overlap is left to the coordinate model
        // (topmost wins); only an image — which genuinely can't be composited —
        // forces the lift.
        blits.sort_by_key(|&(_, _, top)| top);
        let mut inserted = 0usize;
        for (col, b, top) in blits {
            let h = (b.height as usize).max(1);
            let target = ((origin_row as i32 + top).max(0) as usize) + inserted;
            if self.overlay_hits_image(target, h, col, b.width) {
                self.insert_blank_rows(target, h);
                inserted += h;
            }
            self.blit(&b, col, target);
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// Whether placing a box at rows `[at, at+h)` over columns `[col, col+w)`
    /// would land on a decoded image already laid in those cells. A terminal
    /// cell has no z-axis and a sixel is an atomic blit, so an overlay can't be
    /// composited over an image — `place_positioned_children` lifts it onto its
    /// own row instead.
    fn overlay_hits_image(&self, at: usize, h: usize, col: u16, w: u16) -> bool {
        let hi = col.saturating_add(w);
        (at..(at + h).min(self.rows.len())).any(|r| {
            self.rows[r].items.iter().any(|it| {
                matches!(it.kind, ItemKind::Image)
                    && it.col < hi
                    && it.col.saturating_add(it.width) > col
            })
        })
    }

    /// Insert `n` blank rows at row `at`, pushing the existing rows — and the
    /// row references of any carousels/floats at or below `at` — down. Lets a
    /// lifted overlay (see `place_positioned_children`) take its own row without
    /// painting over what was there.
    fn insert_blank_rows(&mut self, at: usize, n: usize) {
        if n == 0 || at > self.rows.len() {
            return;
        }
        self.rows
            .splice(at..at, std::iter::repeat_with(Row::default).take(n));
        for c in &mut self.carousels {
            if c.start >= at {
                c.start += n;
            }
            if c.end >= at {
                c.end += n;
            }
        }
        for f in &mut self.floats {
            if f.start_row >= at {
                f.start_row += n;
            }
            if f.bottom >= at {
                f.bottom += n;
            }
        }
    }

    /// The inline context a positioned box inherits from its DOM ancestors —
    /// the nearest enclosing `<a href>` (so an anchor-wrapped badge/overlay
    /// stays clickable). Other inline styling now comes from the cascade
    /// per-element, so only the link/kind need threading.
    fn ancestor_link_ctx(&self, id: NodeId) -> Ctx {
        let mut ctx = Ctx::root();
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if matches!(self.dom.tag_name(p), Some("body" | "html")) {
                break;
            }
            if self.dom.tag_name(p) == Some("a")
                && let Some(href) = self.dom.attr(p, "href")
            {
                ctx.link = Some(crate::http::resolve(self.base, href));
                ctx.kind = ItemKind::Link;
                break;
            }
            cur = self.dom.parent_composed(p);
        }
        ctx
    }

    /// The used `width` of a positioned box in cells per CSS 2.1 §10.3.7: an
    /// explicit `width` (clamped by `min`/`max-width`), or — when `width` is
    /// `auto` but both `left` and `right` are set — the stretch width
    /// `cb_w − left − right − margins`. `None` means `width:auto` with a free
    /// side, i.e. shrink-to-fit (the caller measures content).
    fn abs_used_width(&self, id: NodeId, cb_w: usize) -> Option<usize> {
        if let Some(w) = self.pos_len(id, "width", cb_w) {
            let w = self
                .pos_len(id, "max-width", cb_w)
                .map_or(w, |mx| w.min(mx));
            let w = self
                .pos_len(id, "min-width", cb_w)
                .map_or(w, |mn| w.max(mn));
            return Some(w.round().max(1.0) as usize);
        }
        match (
            self.pos_len(id, "left", cb_w),
            self.pos_len(id, "right", cb_w),
        ) {
            (Some(l), Some(r)) => {
                let ml = self.pos_len(id, "margin-left", cb_w).unwrap_or(0.0);
                let mr = self.pos_len(id, "margin-right", cb_w).unwrap_or(0.0);
                Some((cb_w as f32 - l - r - ml - mr).round().max(1.0) as usize)
            }
            _ => None,
        }
    }

    /// The used `left` of a positioned box (cells, may be negative) per §10.3.7,
    /// given its used width. `left` set → `left + margin-left`; `right` set
    /// (left auto) → `cb_w − right − width − margin-right`; both auto → the
    /// static position (CB content origin) plus `margin-left`. Over-constrained
    /// (both set) keeps `left` (ltr).
    fn abs_used_left(&self, id: NodeId, cb_w: i32, used_w: i32) -> i32 {
        let left = self.pos_len(id, "left", cb_w as usize);
        let right = self.pos_len(id, "right", cb_w as usize);
        let ml = self
            .pos_len(id, "margin-left", cb_w as usize)
            .unwrap_or(0.0);
        let mr = self
            .pos_len(id, "margin-right", cb_w as usize)
            .unwrap_or(0.0);
        match (left, right) {
            (Some(l), _) => (l + ml).round() as i32,
            (None, Some(r)) => (cb_w as f32 - r - used_w as f32 - mr).round() as i32,
            (None, None) => ml.round() as i32,
        }
    }

    /// The used `top` of a positioned box (cells, may be negative; caller
    /// clamps ≥0) per §10.6.4, given its content height. `top` set →
    /// `top + margin-top`; `bottom` set (top auto) → `cb_h − bottom − height −
    /// margin-bottom`; both auto → the static position (CB content origin) plus
    /// `margin-top`.
    fn abs_used_top(&self, id: NodeId, cb_h: i32, used_h: i32) -> i32 {
        let top = self.pos_len(id, "top", cb_h as usize);
        let bottom = self.pos_len(id, "bottom", cb_h as usize);
        let mt = self.pos_len(id, "margin-top", cb_h as usize).unwrap_or(0.0);
        let mb = self
            .pos_len(id, "margin-bottom", cb_h as usize)
            .unwrap_or(0.0);
        match (top, bottom) {
            (Some(t), _) => (t + mt).round() as i32,
            (None, Some(b)) => (cb_h as f32 - b - used_h as f32 - mb).round() as i32,
            (None, None) => mt.round() as i32,
        }
    }

    /// A positioning length (`left`/`right`/`top`/`bottom`/`width`/margins) in
    /// cells, resolving `%`/`vw`/`calc()` against `extent`. `None` for unset or
    /// `auto` (the caller's resolution rules then apply).
    fn pos_len(&self, id: NodeId, prop: &str, extent: usize) -> Option<f32> {
        let v = self.dom.computed_style(id, prop)?;
        let t = v.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("auto") {
            return None;
        }
        resolve_cells_f32(t, extent, self.viewport_w)
    }

    /// Generate the carousel's prev/next scroll buttons as glyph items on the
    /// row just above the band — `‹` at the band's left edge, `›` at its
    /// right — and remove any author-supplied controls (so there's no
    /// duplicate text button). Mirrors CSS `::scroll-button(left|right)`:
    /// browser-generated, always both present, flanking the strip. The view
    /// greys whichever can't scroll; activation pages the nearest carousel.
    fn emit_scroll_buttons(
        &mut self,
        container: NodeId,
        row_base: usize,
        band_left: usize,
        band_w: usize,
        all_clickable: bool,
    ) {
        // Drop the page's own controls. They may have been laid already
        // (a carousel's controls precede its track) or be still to come
        // (a slideshow's arrows/dots follow the deck) — so remove the laid
        // ones AND mark the nodes to skip when flowed.
        let page_ctrls = self.carousel_controls(container, all_clickable);
        for &n in &page_ctrls {
            self.suppressed_controls.insert(n);
        }
        if !page_ctrls.is_empty() {
            for row in &mut self.rows {
                row.items.retain(|it| !page_ctrls.contains(&it.node));
            }
        }
        // No room above the band (the strip is the first thing in its box):
        // skip rather than overwrite the band's top row. Callers that can be
        // first (slideshows) reserve a row up front so this doesn't bite.
        if row_base == 0 || band_w < 2 {
            return;
        }
        let row = row_base - 1;
        let right = band_left + band_w - 1;
        for (col, glyph, dir) in [(band_left, "‹", -1i8), (right, "›", 1i8)] {
            self.rows[row].items.push(Item {
                col: col as u16,
                width: 1,
                height: 1,
                text: glyph.to_string(),
                kind: ItemKind::Link,
                image: None,
                crop: false,
                emph: Emphasis::default(),
                node: NO_NODE,
                link: Some(Link::CarouselScroll(dir)),
            });
        }
        self.rows[row].items.sort_by_key(|it| it.col);
    }

    /// The page's own author-supplied prev/next control nodes: clickable
    /// elements with a prev/next signal that share the scroll container's
    /// wrapper (the div holding BOTH the scroller and its buttons — how a
    /// page ties them together) but live OUTSIDE the scrolled content. We
    /// generate our own glyph controls, so these are returned only to be
    /// suppressed (their rendered items removed) and avoid a duplicate.
    /// `all_clickable`: a carousel only suppresses prev/next-looking controls
    /// (a stray link near the rail must survive); a slideshow's wrapper holds
    /// ONLY its own navigation (arrows AND dots/thumbnails), so it suppresses
    /// every clickable to clear the dead dots too.
    fn carousel_controls(&self, container: NodeId, all_clickable: bool) -> Vec<NodeId> {
        let Some(wrapper) = self.dom.parent_composed(container) else {
            return Vec::new();
        };
        let inside: std::collections::HashSet<NodeId> =
            self.dom.descendants(container).into_iter().collect();
        let mut out = Vec::new();
        for d in self.dom.descendants(wrapper) {
            if d == container || inside.contains(&d) {
                continue; // the cards themselves, not a control
            }
            if self.is_clickable(d) && (all_clickable || scroll_control_dir(self.dom, d).is_some())
            {
                out.push(d);
            }
        }
        out
    }

    /// Whether an element is something the user can click/activate: an
    /// anchor with an href, a `<button>`, or anything carrying `onclick` or
    /// `role=button`.
    fn is_clickable(&self, id: NodeId) -> bool {
        match self.dom.tag_name(id) {
            Some("a") => self.dom.attr(id, "href").is_some(),
            Some("button") => true,
            _ => {
                self.dom.attr(id, "onclick").is_some()
                    || self.dom.attr(id, "role") == Some("button")
            }
        }
    }

    /// Whether an element's subtree has nothing to render — no non-blank
    /// text and no replaced/control element (`<img>`, form controls, …).
    /// Used to collapse empty flexible flex columns. Hidden descendants
    /// don't count as content.
    fn is_empty_box(&self, id: NodeId) -> bool {
        // The node ITSELF counts: a flex item that *is* a replaced element
        // (`descendants` excludes the root) — e.g. a bare `<img>` with no
        // explicit width — is not empty and must not be collapsed away.
        for d in std::iter::once(id).chain(self.dom.descendants(id)) {
            // A `::before`/`::after` glyph (an icon-only nav button, the
            // "Members ▾" split trigger) is visible content — don't collapse it.
            if self.pseudo_text(d, crate::dom::PseudoEl::Before).is_some()
                || self.pseudo_text(d, crate::dom::PseudoEl::After).is_some()
            {
                return false;
            }
            match &self.dom.node(d).data {
                NodeData::Text(s) if !s.trim().is_empty() => return false,
                NodeData::Element { .. }
                    if matches!(
                        self.dom.tag_name(d),
                        Some(
                            "img"
                                | "input"
                                | "button"
                                | "select"
                                | "textarea"
                                | "video"
                                | "canvas"
                                | "svg"
                                | "hr"
                        )
                    ) && !self.dom.is_hidden(d) =>
                {
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    /// Lay a flex container's items out as a vertical stack of block-level
    /// boxes, each at the full available width — `flex-direction:column`,
    /// and the responsive fallback for a too-narrow row. Blockifies inline
    /// children (so a card's image and caption stack instead of fusing).
    fn stack_flex_items(&mut self, id: NodeId, ctx: &Ctx) {
        let kids = self.flex_items(id);
        // Within the float-narrowed band (set by the block boundary's
        // `begin_line`), not the raw block box — so a stacked column beside a
        // float (a latest-post avatar floated left of its title/date column)
        // flows to its right instead of painting over it.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        self.stack_boxes(&kids, avail, ctx);
    }

    /// Stack a set of child boxes vertically at `width`, each below the
    /// last (shared by column flex and the row fallback). Blits at the band
    /// left so the column clears an active float (`line_left == indent` when
    /// none is active, leaving the common case unchanged).
    fn stack_boxes(&mut self, kids: &[NodeId], width: usize, ctx: &Ctx) {
        let mut row = self.rows.len();
        for &k in kids {
            let b = self.layout_subtree(k, width, ctx);
            if b.height == 0 {
                continue;
            }
            self.blit(&b, self.line_left as u16, row);
            row += b.height as usize;
        }
        self.col = self.line_left;
        self.pending_space = false;
    }

    /// The element children of a flex container that generate flex items
    /// (skipping hidden ones and whitespace/text nodes).
    fn flex_items(&self, id: NodeId) -> Vec<NodeId> {
        let mut kids: Vec<NodeId> = self
            .dom
            .children(id)
            .into_iter()
            .filter(|&c| match &self.dom.node(c).data {
                // Each contiguous run of text directly inside a flex/grid
                // container is wrapped in an anonymous flex item (CSS Flexbox
                // §4) — html5ever coalesces adjacent character data into one
                // Text node, so a run is one node. A whitespace-only run (HTML
                // indentation between items) generates no box, so it's skipped;
                // without this, a `display:flex` element whose only content is
                // text (`<div style=display:flex>-70%</div>`, a discount badge,
                // an icon-less label) rendered NOTHING — the text was dropped.
                NodeData::Text(s) => !s.trim().is_empty(),
                NodeData::Element { .. } => {
                    !self.dom.is_hidden(c)
                    // An absolutely-positioned child of a flex container is NOT
                    // a flex item (CSS Flexbox §4) — it's taken out of flow and
                    // positioned over the container. Including it laid decorative
                    // overlays (a blurred `object-fit:cover` backdrop behind a
                    // contained image, a badge) as real columns beside the
                    // content. Out-of-flow children flow via their own path.
                        && !self.is_out_of_flow(c)
                }
                _ => false,
            })
            .collect();
        // CSS `order`: flex/grid items render in ascending `order` (default 0,
        // negatives allowed), ties keeping source order — `sort_by_key` is
        // stable. Only the visual order changes; items keep their node/link so
        // selection and the source DOM are untouched.
        if kids.len() > 1 {
            kids.sort_by_key(|&c| self.order_of(c));
        }
        kids
    }

    /// A flex/grid item's `order` (default 0).
    fn order_of(&self, id: NodeId) -> i32 {
        self.dom
            .computed_style(id, "order")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
    }

    // ---- floats (Phase C) ------------------------------------------------

    /// The content band `[left, right]` for a given output row: the block's
    /// `indent`/`width` narrowed by every float spanning that row. Never
    /// collapses below a single usable cell (an over-floated row ignores the
    /// floats rather than render nothing).
    fn band(&self, row: usize) -> (usize, usize) {
        let mut left = self.indent;
        let mut right = self.width;
        for f in &self.floats {
            if row >= f.start_row && row < f.bottom {
                match f.side {
                    FloatSide::Left => left = left.max(f.col as usize + f.width + 1),
                    FloatSide::Right => {
                        right = right.min((f.col as usize).saturating_sub(1));
                    }
                }
            }
        }
        if left + 1 >= right {
            // Band collapsed under the floats: fall back to the block box.
            (self.indent, self.width.max(self.indent + 1))
        } else {
            (left, right)
        }
    }

    /// Like `band`, but floats ABUT — no inter-float readability gap. Adjacent
    /// floats in a grid pack tight (the `+1` in `band` is the gap between a
    /// float and the TEXT beside it, not between two floats), so a row of
    /// equal-width floats fits the same count a browser shows instead of losing
    /// one column to accumulated gaps. Used only to place the next float.
    fn float_band(&self, row: usize) -> (usize, usize) {
        let mut left = self.indent;
        let mut right = self.width;
        for f in &self.floats {
            if row >= f.start_row && row < f.bottom {
                match f.side {
                    FloatSide::Left => left = left.max(f.col as usize + f.width),
                    FloatSide::Right => right = right.min(f.col as usize),
                }
            }
        }
        (left, right.max(left + 1))
    }

    /// Where a `w`-wide float lands on `side`: its `(start_row, col)`. Floats
    /// march left→right on the current SHELF; when the next won't fit beside it,
    /// the float WRAPS to a fresh shelf BELOW the whole current one — CSS float
    /// flow turned into an aligned grid of rows (a browser keeps the cards equal
    /// height, so its rows align; we align the shelf instead of tucking a
    /// wrapped card into a shorter neighbour's gap, which reads as jank in a
    /// terminal). The tight `float_band` lets equal-width cards pack the same
    /// count per row a browser shows.
    fn float_slot(&self, side: FloatSide, w: usize) -> (usize, usize) {
        let here = self.rows.len();
        // The current shelf's top row. Floats don't push content rows, so
        // `self.rows.len()` can't track it on its own — the shelf is the highest
        // `start_row` among the floats placed so far (clamped to the cursor for
        // floats that follow real content).
        let shelf_top = self
            .floats
            .iter()
            .map(|f| f.start_row)
            .max()
            .unwrap_or(here)
            .max(here);
        let (l, r) = self.float_band(shelf_top);
        if r.saturating_sub(l) >= w {
            let col = match side {
                FloatSide::Left => l,
                FloatSide::Right => r - w,
            };
            return (shelf_top, col);
        }
        // No room beside the shelf → drop to a new shelf just below it (the
        // tallest card on this shelf sets the row, so the grid stays aligned).
        let next = self
            .floats
            .iter()
            .filter(|f| f.start_row == shelf_top)
            .map(|f| f.bottom)
            .max()
            .unwrap_or(shelf_top + 1)
            .max(here);
        let (l, r) = self.float_band(next);
        let col = match side {
            FloatSide::Left => l,
            FloatSide::Right => r.saturating_sub(w),
        };
        (next, col)
    }

    /// Recompute the line bounds (and reset the cursor) for the row about to
    /// be built, honoring any floats active on it.
    fn begin_line(&mut self) {
        let (l, r) = self.band(self.rows.len());
        self.line_left = l;
        self.line_right = r;
        self.col = l;
    }

    /// Whether a block establishes a new block formatting context, so its
    /// descendant floats are contained rather than leaking to following
    /// siblings. We detect the two statically-resolvable triggers: a
    /// non-`visible` `overflow` (the ubiquitous `overflow:hidden` clearfix)
    /// and `display:flow-root`. Flex/grid containers and floats already lay
    /// their content as self-contained boxes, so they're excluded by the
    /// caller (`flex.is_none()`).
    fn establishes_bfc(&self, id: NodeId) -> bool {
        if self.dom.computed_display(id).as_deref() == Some("flow-root") {
            return true;
        }
        self.dom.computed_style(id, "overflow").is_some_and(|v| {
            v.split_whitespace()
                .any(|t| matches!(t, "hidden" | "auto" | "scroll" | "clip"))
        })
    }

    /// The `float` side of an element (`left`/`right`), or `None`.
    fn float_side(&self, id: NodeId) -> Option<FloatSide> {
        match self
            .dom
            .computed_style(id, "float")?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "left" => Some(FloatSide::Left),
            "right" => Some(FloatSide::Right),
            _ => None,
        }
    }

    /// Take a floated element out of flow: lay it out as a box, pin it to its
    /// edge (beside any floats already there), and narrow the band so the
    /// following content flows past it. The box is blitted later, once
    /// content has filled the rows it spans (`resolve_floats`).
    fn flow_float(&mut self, id: NodeId, side: FloatSide, ctx: &Ctx) {
        // Floats begin at a line boundary; refresh the band first.
        self.flush_block();
        self.begin_line();
        // A float keeps its CSS width (a `%` already resolves against the FULL
        // containing block), NOT the band narrowed by earlier floats — a grid
        // item holds its column width and WRAPS to a new shelf rather than being
        // squeezed into the leftover gap. With no explicit width it sizes to
        // content within the current band (a floated image). The width is
        // FLOORED (`css_cells_floor`): `N` columns of `round(100/N %)` can sum
        // past 100% and drop the last column (a 25% grid rounds 23.5→24, so 4×24
        // overflows a 94-cell row and only 3 fit); flooring keeps all `N`.
        let full = self.width.saturating_sub(self.indent).max(1);
        let explicit = self
            .css_cells_floor(id, "width")
            .or_else(|| self.css_cells_floor(id, "max-width"))
            .map(|w| w.min(full));
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let constraint = explicit.unwrap_or(avail).max(1);
        let boxed = self.layout_subtree_inner(id, constraint, Some(id), false, ctx);
        if boxed.height == 0 {
            return;
        }
        let w = explicit.unwrap_or(boxed.width as usize).min(full).max(1);
        // Responsive fallback: a float so WIDE that even on an EMPTY row it
        // leaves too thin a band beside it (a desktop-width main column dropped
        // into a terminal-width viewport) becomes an in-flow block — stacked,
        // never overlapped. Measured against the FULL row, not the leftover gap,
        // so an ordinary grid float WRAPS (below) instead of being misread as a
        // too-wide column and stacked in-flow under its neighbour.
        if full.saturating_sub(w + 1) < MIN_COL {
            let row_base = self.rows.len();
            self.blit(&boxed, self.line_left as u16, row_base);
            self.col = self.line_left;
            self.pending_space = false;
            self.begin_line();
            return;
        }
        // Drop to the lowest shelf where it fits — beside the current row's
        // floats if there's room, else wrapped to a fresh row below them.
        let (start_row, col) = self.float_slot(side, w);
        let bottom = start_row + boxed.height as usize;
        self.floats.push(Float {
            side,
            col: col as u16,
            width: w,
            start_row,
            bottom,
            boxed,
        });
        // Re-narrow the current line for the content that follows.
        self.begin_line();
    }

    /// Blit every float whose bottom we've now reached into its reserved
    /// rows and drop it from the active set (called after each row is
    /// pushed). Rows it spans already exist (content filled them, or they
    /// were padded), so the box merges alongside the wrapped content.
    fn resolve_floats(&mut self) {
        let reached = self.rows.len();
        let mut i = 0;
        while i < self.floats.len() {
            if self.floats[i].bottom <= reached {
                let f = self.floats.remove(i);
                self.blit(&f.boxed, f.col, f.start_row);
                // The float merged at the left/right edge after the content
                // items; re-sort so each row stays column-ordered.
                for r in f.start_row..f.bottom {
                    if let Some(row) = self.rows.get_mut(r) {
                        row.items.sort_by_key(|it| it.col);
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    /// Pad blank rows up to the tallest remaining float, then blit them all
    /// (a float taller than its wrapped content reserves the rest). Called
    /// at the end of a layout pass.
    fn finish_floats(&mut self) {
        let max_bottom = self.floats.iter().map(|f| f.bottom).max().unwrap_or(0);
        while self.rows.len() < max_bottom {
            self.push_blank();
        }
        self.resolve_floats();
    }

    /// CSS `clear`: advance past the floats on the named side(s) so the
    /// cleared block starts below them. Returns whether anything cleared.
    fn clear_floats(&mut self, id: NodeId) {
        let Some(sides) = self
            .dom
            .computed_style(id, "clear")
            .map(|v| v.trim().to_ascii_lowercase())
        else {
            return;
        };
        let (l, r) = match sides.as_str() {
            "left" => (true, false),
            "right" => (false, true),
            "both" => (true, true),
            _ => return,
        };
        self.clear_to(l, r);
    }

    /// Advance below the active floats on the given side(s) and resolve them —
    /// the shared mechanic of CSS `clear` and the clearfix `::after`.
    fn clear_to(&mut self, l: bool, r: bool) {
        let target = self
            .floats
            .iter()
            .filter(|f| match f.side {
                FloatSide::Left => l,
                FloatSide::Right => r,
            })
            .map(|f| f.bottom)
            .max();
        if let Some(t) = target {
            while self.rows.len() < t {
                self.push_blank();
            }
            self.resolve_floats();
            self.begin_line();
        }
    }

    /// Lay a flex-wrap container's children as real flex lines (CSS Flexbox
    /// §9.2/§9.3/§9.7): each item is sized to its flex base size (clamped to
    /// its min/max), lines BREAK on that hypothetical size, then each line's
    /// free space is handed to `flex-grow` items (or overflow pulled from
    /// `flex-shrink` items) so the row fills. This is what makes a `flex:1`
    /// (basis 0) `min-width:40%` `max-width:50%` capsule grid lay TWO per row
    /// that grow to fill the band — packing by the *content/max-width* (the old
    /// behaviour) broke every line at ~50% and collapsed it to one column.
    /// Assumes a block boundary; appends finished rows via `blit` and leaves
    /// the cursor back at the indent.
    fn flow_flex_wrap(&mut self, id: NodeId, ctx: &Ctx) {
        // Lay within the float-narrowed band (the block boundary already ran
        // `begin_line`), not the raw block box — so a wrapping grid beside a
        // float clears it instead of painting over it. `line_left == indent`
        // with no active float, so the common case is unchanged.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let gap = self.flex_gap(id, avail, false);
        let row_gap = self.flex_gap(id, avail, true);

        // Per item: the flex inputs and the hypothetical main size used for
        // line-breaking (the flex base size clamped to [auto-min, max-width]).
        struct FlexItem {
            node: NodeId,
            hypo: usize,
            grow: f32,
            shrink: f32,
            floor: usize,
            max: usize,
        }
        let mut items: Vec<FlexItem> = Vec::new();
        for child in self.flex_items(id) {
            let (basis, grow, shrink) = self.flex_props(child, avail);
            // An empty, auto-sized, non-growing item takes no column.
            if grow == 0.0 && basis.is_none() && self.is_empty_box(child) {
                continue;
            }
            let max = self
                .css_cells(child, "max-width")
                .map(|w| w.min(avail))
                .unwrap_or(avail)
                .max(1);
            // The automatic minimum (§4.5): an explicit `min-width`, else — for
            // a DEFINITE flex base size (a `flex-basis:0` cell) — the item's
            // min-content width, so it never collapses below its content. An
            // auto-basis item is already sized to its content, so it needs no
            // extra floor (skip the measurement).
            let floor = match self.css_cells(child, "min-width") {
                Some(w) => w.min(avail),
                None if basis.is_some() => self.measure_width(child, 1),
                None => 1,
            }
            .clamp(1, avail);
            // Flex base size: the definite basis, else the content width.
            let base = basis.unwrap_or_else(|| self.measure_width(child, max));
            let hypo = base.min(max).max(floor).clamp(1, avail);
            items.push(FlexItem {
                node: child,
                hypo,
                grow,
                shrink,
                floor,
                max,
            });
        }

        let mut shelf_top = self.rows.len();
        let mut i = 0;
        while i < items.len() {
            // Collect a flex line: as many items as fit at their hypothetical
            // size (always at least one — an over-wide item takes its own line).
            let mut used = items[i].hypo;
            let mut end = i + 1;
            while end < items.len() && used + gap + items[end].hypo <= avail {
                used += gap + items[end].hypo;
                end += 1;
            }
            let line = &items[i..end];
            let n = line.len();
            let gaps = (n - 1) * gap;
            let mut widths: Vec<usize> = line.iter().map(|it| it.hypo).collect();
            let content: usize = widths.iter().sum();
            if content + gaps < avail {
                // Grow: distribute free space to `flex-grow` items, capping each
                // at its max-width and re-distributing the remainder.
                let mut free = avail - content - gaps;
                let mut frozen = vec![false; n];
                while free > 0 {
                    let total: f32 = (0..n).filter(|&k| !frozen[k]).map(|k| line[k].grow).sum();
                    if total <= 0.0 {
                        break;
                    }
                    let mut moved = false;
                    let free_f = free as f32;
                    for k in 0..n {
                        if frozen[k] || line[k].grow <= 0.0 {
                            continue;
                        }
                        let add = (free_f * line[k].grow / total).floor() as usize;
                        let give = add.min(line[k].max.saturating_sub(widths[k]));
                        if give > 0 {
                            widths[k] += give;
                            free -= give;
                            moved = true;
                        }
                        if widths[k] >= line[k].max {
                            frozen[k] = true;
                        }
                    }
                    if !moved {
                        break;
                    }
                }
            } else if content + gaps > avail {
                // Shrink: pull overflow from `flex-shrink` items (weighted by
                // shrink × base size), flooring each at its min and re-absorbing.
                let mut over = content + gaps - avail;
                let mut frozen = vec![false; n];
                while over > 0 {
                    let weight: f32 = (0..n)
                        .filter(|&k| !frozen[k])
                        .map(|k| line[k].shrink * line[k].hypo as f32)
                        .sum();
                    if weight <= 0.0 {
                        break;
                    }
                    let mut moved = false;
                    let over_f = over as f32;
                    for k in 0..n {
                        if frozen[k] || line[k].shrink <= 0.0 {
                            continue;
                        }
                        let take = (over_f * (line[k].shrink * line[k].hypo as f32) / weight).ceil()
                            as usize;
                        let cut = take.min(widths[k].saturating_sub(line[k].floor)).min(over);
                        if cut > 0 {
                            widths[k] -= cut;
                            over -= cut;
                            moved = true;
                        }
                        if widths[k] <= line[k].floor {
                            frozen[k] = true;
                        }
                    }
                    if !moved {
                        break;
                    }
                }
            }
            // Lay each item at its used main size, then place the line honoring
            // `justify-content` (leftover main-axis space) and `align-items`
            // (cross-axis offset of a short item within the line's height).
            let boxes: Vec<LaidBox> = (0..n)
                .map(|k| self.layout_subtree(line[k].node, widths[k].max(1), ctx))
                .collect();
            let shelf_h = boxes.iter().map(|b| b.height as usize).max().unwrap_or(0);
            let used_w: usize = widths.iter().map(|w| (*w).max(1)).sum::<usize>() + gaps;
            let (lead, between) = self.justify_offsets(id, avail.saturating_sub(used_w), n);
            let mut x = lead;
            for (k, b) in boxes.iter().enumerate() {
                if b.height > 0 {
                    let dy = self.align_offset(id, b.height as usize, shelf_h);
                    self.blit(b, (self.line_left + x) as u16, shelf_top + dy);
                }
                x += widths[k].max(1) + if k + 1 < n { gap + between } else { 0 };
            }
            shelf_top += shelf_h + row_gap;
            i = end;
        }
        self.col = self.line_left;
        self.pending_space = false;
    }

    /// Lay a block of ATOMIC INLINE boxes (`is_inline_box_grid`) as the inline
    /// formatting context the spec describes, NOT as flex (no grow/shrink, no
    /// justify-content). Each box is laid as its own block sub-box
    /// (`layout_subtree` — inner `flow-root`, CSS Display §2) at its used width,
    /// then placed left-to-right onto line boxes that break when the next box
    /// won't fit the remaining width (CSS 2.1 §9.4.2). Each line is aligned by
    /// the container's `text-align` (CSS 2.1 §16.2). Boxes top-align — we don't
    /// implement `vertical-align`/baseline (a terminal deviation). Inter-box
    /// spacing is the boxes' own horizontal margins (CSS 2.1 §8); the collapsed
    /// source whitespace between inline-blocks is a sub-cell advance that rounds
    /// to zero at terminal resolution (like a sub-cell `letter-spacing`).
    fn flow_inline_box_grid(&mut self, id: NodeId, ctx: &Ctx) {
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        // A box's horizontal margin in cells (`auto`/absent → 0). NOT `css_cells`,
        // which floors to 1 cell and so would invent gaps on zero margins.
        let margin = |me: &Self, c: NodeId, side: &str| -> usize {
            me.dom
                .computed_style(c, side)
                .filter(|v| v.trim() != "auto")
                .and_then(|v| resolve_cells(&v, avail, me.viewport_w))
                .unwrap_or(0)
        };
        struct Slot {
            node: NodeId,
            ml: usize,
            w: usize,
            mr: usize,
        }
        let slots: Vec<Slot> = self
            .flex_items(id)
            .into_iter()
            .map(|c| {
                // Used width (CSS 2.1 §10.3.9 shrink-to-fit): an explicit `width`,
                // else min(max-content, available); min/max-width clamp.
                let mut w = self
                    .css_cells(c, "width")
                    .unwrap_or_else(|| self.measure_width(c, avail));
                if let Some(mn) = self.css_cells(c, "min-width") {
                    w = w.max(mn);
                }
                if let Some(mx) = self.css_cells(c, "max-width") {
                    w = w.min(mx);
                }
                Slot {
                    node: c,
                    ml: margin(self, c, "margin-left"),
                    w: w.clamp(1, avail),
                    mr: margin(self, c, "margin-right"),
                }
            })
            .collect();

        let mut shelf_top = self.rows.len();
        let mut i = 0;
        while i < slots.len() {
            // Greedily fill a line box; always at least one box (an over-wide
            // box takes its own line, CSS 2.1 §9.4.2).
            let footprint = |s: &Slot| s.ml + s.w + s.mr;
            let mut used = footprint(&slots[i]);
            let mut end = i + 1;
            while end < slots.len() && used + footprint(&slots[end]) <= avail {
                used += footprint(&slots[end]);
                end += 1;
            }
            let line = &slots[i..end];
            let laid: Vec<LaidBox> = line
                .iter()
                .map(|s| self.layout_subtree(s.node, s.w, ctx))
                .collect();
            let shelf_h = laid.iter().map(|b| b.height as usize).max().unwrap_or(0);
            // text-align positions the line within the leftover width (CSS §16.2).
            let lead = match self.align {
                Align::Center => avail.saturating_sub(used) / 2,
                Align::Right => avail.saturating_sub(used),
                Align::Left => 0,
            };
            let mut x = lead;
            for (s, b) in line.iter().zip(&laid) {
                x += s.ml;
                if b.height > 0 {
                    self.blit(b, (self.line_left + x) as u16, shelf_top);
                }
                x += s.w + s.mr;
            }
            shelf_top += shelf_h;
            i = end;
        }
        self.col = self.line_left;
        self.pending_space = false;
    }

    /// Lay a `display:grid` container that declares an explicit
    /// `grid-template-columns`, honoring its track sizing and each item's
    /// `grid-column`/`grid-row` placement. This is what lets a page's own grid
    /// CSS drive the layout instead of a one-size approximation — GitHub's
    /// `Layout` is a fixed `auto` sidebar column beside a flexible
    /// `minmax(0, calc(…))` main column, and we now place them exactly where
    /// the stylesheet asks. Returns `false` (the caller falls back to the
    /// shelf-packed `flow_flex_wrap`) when there is no usable template:
    /// `repeat()` — including `auto-fill`/`auto-fit` — is supported, but a
    /// missing template, named grid areas, or any unparseable token bail, so a
    /// bare `display:grid` is unchanged.
    fn flow_grid_tracks(&mut self, id: NodeId, ctx: &Ctx) -> bool {
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let col_gap = self.flex_gap(id, avail, false);
        let row_gap = self.flex_gap(id, avail, true);
        let Some(template) = self.dom.computed_style(id, "grid-template-columns") else {
            return false;
        };
        let Some(specs) = self.parse_track_list(&template, avail as f32, col_gap as f32) else {
            return false;
        };
        let ncols = specs.len();
        let items = self.flex_items(id);
        if ncols == 0 || items.is_empty() {
            return false;
        }

        // Placement: resolve each item's column/row (explicit `grid-column`/
        // `grid-row`, else auto-flow).
        let column_flow = self
            .dom
            .computed_style(id, "grid-auto-flow")
            .is_some_and(|v| v.contains("column"));
        let places = self.place_grid_items(&items, ncols, column_flow);

        // Track sizing. An intrinsic (`auto`/content) track sizes to the widest
        // item that occupies ONLY it; a spanning item contributes to no single
        // track (a documented approximation — its own width is still honored
        // when laid). Fixed/`fr` tracks ignore content.
        let mut content_w = vec![0f32; ncols];
        for (it, pl) in items.iter().zip(&places) {
            if pl.col_span == 1 && pl.col < ncols && self.track_is_intrinsic(&specs[pl.col]) {
                content_w[pl.col] = content_w[pl.col].max(self.measure_width(*it, avail) as f32);
            }
        }
        let widths = self.size_grid_tracks(&specs, &content_w, avail, col_gap);

        // Column x offsets (cumulative track widths + gaps).
        let mut col_x = vec![0usize; ncols];
        let mut acc = 0usize;
        for c in 0..ncols {
            col_x[c] = acc;
            acc += widths[c] + col_gap;
        }

        // Lay every item across its spanned columns; remember its grid row.
        let grid_top = self.rows.len();
        let mut laid: Vec<(GridPlace, LaidBox)> = Vec::new();
        let mut nrows = 1usize;
        for (it, pl) in items.iter().zip(&places) {
            if pl.col >= ncols {
                continue;
            }
            let end = (pl.col + pl.col_span).min(ncols);
            let span = end - pl.col;
            let w = (widths[pl.col..end].iter().sum::<usize>() + col_gap * span.saturating_sub(1))
                .max(1);
            let b = self.layout_subtree(*it, w, ctx);
            if b.height == 0 {
                continue;
            }
            nrows = nrows.max(pl.row + pl.row_span);
            laid.push((*pl, b));
        }

        // Row heights = the tallest box starting in each row (a row-spanning
        // box is approximated to its first row). Then row y offsets.
        let mut row_h = vec![0usize; nrows];
        for (pl, b) in &laid {
            if pl.row < nrows {
                row_h[pl.row] = row_h[pl.row].max(b.height as usize);
            }
        }
        let mut row_y = vec![0usize; nrows];
        let mut acc = 0usize;
        for r in 0..nrows {
            row_y[r] = acc;
            acc += row_h[r] + row_gap;
        }

        // Blit each item at its column/row origin within the grid.
        for (pl, b) in &laid {
            let x = col_x.get(pl.col).copied().unwrap_or(0);
            let y = grid_top + row_y.get(pl.row).copied().unwrap_or(0);
            self.blit(b, (self.line_left + x) as u16, y);
        }
        self.col = self.line_left;
        self.pending_space = false;
        true
    }

    /// Lay a `display:table` element (CSS 2.1 §17 — the table formatting
    /// model). Builds the cell grid (honoring `colspan`/`rowspan`), computes
    /// column widths by the automatic table layout algorithm (§17.5.2.2; the
    /// fixed algorithm when `table-layout:fixed` + a definite width), lays each
    /// cell's content as its own sub-box at its column width (so nested tables
    /// recurse), sizes rows to their tallest cell, and blits the cells side by
    /// side. We draw NO cell borders/grid lines (her call: terminal rows are
    /// precious) — the columns alone carry the layout, which is the whole point
    /// for the ubiquitous old table-as-layout page (slackware.com).
    fn flow_table(&mut self, id: NodeId, ctx: &Ctx) {
        // Block framing: tables are block-level boxes.
        self.flush_block();
        self.clear_floats(id);
        if self.gap_before(id, "table") {
            self.push_blank();
        }

        // Recursion lid: a table nested past `MAX_TABLE_DEPTH` degrades to
        // block-stacked content (its rows/cells flow as ordinary blocks). The
        // per-cell content measurement re-descends each cell's subtree, so a
        // pathologically deep table tree (some wikis nest navboxes very deep)
        // would otherwise overflow the layout stack.
        if self.table_depth >= MAX_TABLE_DEPTH {
            for child in self.flow_children(id) {
                self.flow_node(child, ctx);
            }
            self.flush_block();
            if self.gap_after(id, "table") {
                self.push_blank();
            }
            return;
        }

        let band = self.line_right.saturating_sub(self.line_left).max(1);
        let rows = self.table_cell_rows(id);
        let (cells, ncols) = build_table_grid(self, &rows);
        if cells.is_empty() || ncols == 0 {
            if self.gap_after(id, "table") {
                self.push_blank();
            }
            return;
        }

        let bs = self.table_border_spacing(id); // horizontal cell spacing, cells
        let cellpad_px = self
            .dom
            .attr(id, "cellpadding")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);

        // While measuring an ancestor's width, size columns cheaply (no per-cell
        // min/max content measurement) — the table contributes an approximate
        // width and we don't recursively re-measure every nested table.
        let cheap = self.measuring;
        let widths = self.table_column_widths(id, &cells, ncols, band, bs, cheap);
        // Every cell sub-layout is one table level deeper.
        self.table_depth += 1;
        // Column x offsets (left edge of each column), with inter-column spacing.
        let mut col_x = vec![0usize; ncols];
        let mut acc = 0usize;
        for c in 0..ncols {
            col_x[c] = acc;
            acc += widths[c] + bs;
        }
        let table_w = acc.saturating_sub(bs).max(1);
        // A table narrower than its band is positioned by its horizontal auto
        // margins / `align`/`<center>` context: centered or right-aligned.
        let lead = self.table_lead(id, table_w, band);

        // Lay every cell at its spanned-column width (parallel to `cells`).
        // Each entry is `(box, horizontal pad, vertical pad)`.
        let table_top = self.rows.len();
        let mut laid: Vec<(LaidBox, usize, usize)> = Vec::with_capacity(cells.len());
        let mut nrows = 0usize;
        for cell in &cells {
            let end = (cell.col + cell.colspan).min(ncols);
            let span = end.saturating_sub(cell.col).max(1);
            let cell_w = (widths[cell.col..end].iter().sum::<usize>() + bs * (span - 1)).max(1);
            // Cellpadding (HTML attr) insets content when the cell sets no CSS
            // padding of its own (CSS padding, applied by the cell's own
            // `block_indent`, wins per the presentational-hint priority).
            let has_css_pad = ["padding", "padding-left", "padding-right", "padding-top"]
                .iter()
                .any(|p| self.dom.computed_style(cell.id, p).is_some());
            let (ph, pv) = if has_css_pad || cellpad_px == 0.0 {
                (0, 0)
            } else {
                (
                    (cellpad_px / 8.0).round() as usize,
                    (cellpad_px / 16.0).round() as usize,
                )
            };
            let inner_w = cell_w.saturating_sub(2 * ph).max(1);
            // Propagate `measuring`: when this whole table is being laid only to
            // measure an ancestor's width, its cells (and any nested tables in
            // them) are measured too — never re-entering the expensive auto
            // algorithm, which is what would compound exponentially with depth.
            let b = self.layout_subtree_inner(cell.id, inner_w, None, self.measuring, ctx);
            nrows = nrows.max(cell.row + cell.rowspan);
            laid.push((b, ph, pv));
        }
        self.table_depth -= 1;
        if nrows == 0 {
            if self.gap_after(id, "table") {
                self.push_blank();
            }
            return;
        }

        // Row heights: the tallest single-row cell sets each row; a row-spanning
        // cell whose box exceeds its spanned rows pushes the deficit onto its
        // last row (CSS 2.1 §17.5.3 — the row height is the max the cells need).
        let mut row_h = vec![0usize; nrows];
        for (cell, (b, _ph, pv)) in cells.iter().zip(&laid) {
            if cell.rowspan <= 1 && cell.row < nrows {
                row_h[cell.row] = row_h[cell.row].max(b.height as usize + 2 * pv);
            }
        }
        // Second pass for spanning cells: ensure the spanned rows together fit.
        for (cell, (b, _ph, pv)) in cells.iter().zip(&laid) {
            if cell.rowspan <= 1 {
                continue;
            }
            let need = b.height as usize + 2 * pv;
            let end = (cell.row + cell.rowspan).min(nrows);
            let have: usize = row_h[cell.row..end].iter().sum();
            if need > have && end > cell.row {
                row_h[end - 1] += need - have;
            }
        }
        let mut row_y = vec![0usize; nrows];
        let mut acc = 0usize;
        for r in 0..nrows {
            row_y[r] = acc;
            acc += row_h[r] + bs;
        }
        let table_h = acc.saturating_sub(bs);

        // Blit each cell at its column/row origin, vertically aligned in the
        // (possibly taller) row band per `valign`/`vertical-align`.
        for (cell, (b, ph, pv)) in cells.iter().zip(&laid) {
            let end = (cell.row + cell.rowspan).min(nrows);
            let span_h = row_h[cell.row..end].iter().sum::<usize>() + bs * (end - cell.row - 1);
            let cell_h = b.height as usize + 2 * pv;
            let dy = self.cell_valign_offset(Some(cell.id), cell_h, span_h);
            let x = self.line_left + lead + col_x.get(cell.col).copied().unwrap_or(0) + ph;
            self.blit(b, x as u16, table_top + row_y[cell.row] + dy + pv);
        }

        // Reserve the table's full height, then resume below it.
        while self.rows.len() < table_top + table_h {
            self.push_blank();
        }
        self.col = self.line_left;
        self.pending_space = false;
        if self.gap_after(id, "table") {
            self.push_blank();
        }
    }

    /// The cells of each table row, in visual order (header-group rows first,
    /// then body/implicit rows, then footer-group rows — CSS 2.1 §17.2.1).
    /// Each entry is one row's `table-cell` children; an empty row (a spacer
    /// `<tr>`) yields an empty inner vec so it still reserves its grid row.
    fn table_cell_rows(&self, table: NodeId) -> Vec<Vec<NodeId>> {
        let mut header = Vec::new();
        let mut body = Vec::new();
        let mut footer = Vec::new();
        // Anonymous-row generation (a misparented `table-cell` directly under
        // the table) is approximated by collecting stray cells into one row.
        let mut stray = Vec::new();
        for child in self.dom.children(table) {
            match self.dom.effective_display(child).as_deref() {
                Some("table-header-group") => header.extend(self.group_rows(child)),
                Some("table-footer-group") => footer.extend(self.group_rows(child)),
                Some("table-row-group") => body.extend(self.group_rows(child)),
                Some("table-row") => body.push(self.row_cells(child)),
                Some("table-cell") => stray.push(child),
                _ => {} // columns, captions, whitespace: not rows
            }
        }
        if !stray.is_empty() {
            body.push(stray);
        }
        header.extend(body);
        header.extend(footer);
        header
    }

    /// The `table-row` children of a row group, each resolved to its cells.
    fn group_rows(&self, group: NodeId) -> Vec<Vec<NodeId>> {
        self.dom
            .children(group)
            .into_iter()
            .filter(|&r| self.dom.effective_display(r).as_deref() == Some("table-row"))
            .map(|r| self.row_cells(r))
            .collect()
    }

    /// The `table-cell` children of a row.
    fn row_cells(&self, row: NodeId) -> Vec<NodeId> {
        self.dom
            .children(row)
            .into_iter()
            .filter(|&c| self.dom.effective_display(c).as_deref() == Some("table-cell"))
            .collect()
    }

    /// A cell's `colspan`/`rowspan` (the HTML attributes), clamped to ≥1 and a
    /// sane ceiling (a hostile `colspan=100000` can't blow up the grid).
    fn cell_span(&self, id: NodeId, attr: &str) -> usize {
        self.dom
            .attr(id, attr)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000)
    }

    /// A declared `width` on a table/cell — the CSS `width` if set, else the
    /// HTML `width` presentational attribute (HTML §15.3.13 maps it to the
    /// `width` property). `None` for `auto`/unset. A bare number on the
    /// attribute is CSS pixels.
    fn declared_track_width(&self, id: NodeId) -> Option<TrackWidth> {
        let raw = self
            .dom
            .computed_style(id, "width")
            .or_else(|| self.dom.attr(id, "width").map(|s| s.trim().to_string()))?;
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("auto") || raw.is_empty() {
            return None;
        }
        if let Some(p) = parse_percent(raw) {
            return Some(TrackWidth::Pct(p));
        }
        if let Some(em) = css_length_em(raw) {
            return Some(TrackWidth::Px((em * 2.0).round().max(1.0) as usize));
        }
        raw.parse::<f32>()
            .ok()
            .filter(|n| *n > 0.0)
            .map(|n| TrackWidth::Px((n / 8.0).round().max(1.0) as usize))
    }

    /// Horizontal cell spacing (cells): CSS `border-spacing` if set, else the
    /// HTML `cellspacing` attribute (HTML §15.3.13 maps it to `border-spacing`).
    /// Default 0 — a terminal's columns are separated by cellpadding/content.
    fn table_border_spacing(&self, table: NodeId) -> usize {
        let raw = self
            .dom
            .computed_style(table, "border-spacing")
            .or_else(|| {
                self.dom
                    .attr(table, "cellspacing")
                    .map(|s| s.trim().to_string())
            });
        let Some(raw) = raw else { return 0 };
        let first = raw.split_whitespace().next().unwrap_or("0");
        css_length_em(first)
            .map(|em| (em * 2.0).round() as usize)
            .or_else(|| {
                first
                    .parse::<f32>()
                    .ok()
                    .map(|n| (n / 8.0).round() as usize)
            })
            .unwrap_or(0)
    }

    /// Position a table narrower than its band: centered when it has
    /// `margin:0 auto` or sits in a centering (`<center>`/`text-align:center`)
    /// context, right-aligned for `margin-left:auto`, else flush left.
    fn table_lead(&self, id: NodeId, table_w: usize, band: usize) -> usize {
        let slack = band.saturating_sub(table_w);
        if slack == 0 {
            return 0;
        }
        let ml_auto = self.dom.computed_style(id, "margin-left").as_deref() == Some("auto");
        let mr_auto = self.dom.computed_style(id, "margin-right").as_deref() == Some("auto");
        // The HTML `align` attribute on a table centers/right-floats the table
        // itself (not its text — that's why it's resolved here, not via the
        // text-align hint path).
        let attr_align = self
            .dom
            .attr(id, "align")
            .map(|s| s.trim().to_ascii_lowercase());
        match (ml_auto, mr_auto) {
            (true, false) => slack,    // margin-left:auto → right
            (true, true) => slack / 2, // margin:0 auto → centered
            _ if attr_align.as_deref() == Some("center") => slack / 2,
            _ if attr_align.as_deref() == Some("right") => slack,
            _ if self.align == Align::Center => slack / 2,
            _ if self.align == Align::Right => slack,
            _ => 0,
        }
    }

    /// Vertical offset of a cell within its (possibly taller) row band, per the
    /// cell's `valign` attribute / `vertical-align` (top default, middle,
    /// bottom). Baseline is approximated as top in the cell line model.
    fn cell_valign_offset(&self, cell: Option<NodeId>, cell_h: usize, span_h: usize) -> usize {
        let slack = span_h.saturating_sub(cell_h);
        if slack == 0 {
            return 0;
        }
        let Some(id) = cell else { return 0 };
        let v = self
            .dom
            .attr(id, "valign")
            .map(|s| s.trim().to_ascii_lowercase())
            .or_else(|| self.dom.computed_style(id, "vertical-align"));
        match v.as_deref() {
            Some("bottom") => slack,
            Some("middle") => slack / 2,
            _ => 0,
        }
    }

    /// Automatic table layout (CSS 2.1 §17.5.2.2): per-column min/max content
    /// widths from the cells, spanning cells widened across their columns, then
    /// the table width resolved and distributed over the columns. Falls back to
    /// the fixed algorithm when `table-layout:fixed` with a definite width.
    fn table_column_widths(
        &self,
        table: NodeId,
        cells: &[TableCell],
        ncols: usize,
        band: usize,
        bs: usize,
        cheap: bool,
    ) -> Vec<usize> {
        let spacing = bs * ncols.saturating_sub(1);
        let avail = band.saturating_sub(spacing).max(1);

        // The table's own declared width (the column percentages resolve
        // against the used table width; px/% resolve against the band).
        let table_spec = self.declared_track_width(table).map(|w| match w {
            TrackWidth::Px(px) => px.min(band),
            TrackWidth::Pct(p) => ((p * band as f32).round() as usize).min(band),
        });

        // The per-column explicit width preference (a declared `width` on the
        // column's first single-span cell). Always cheap to gather.
        let mut col_w: Vec<Option<TrackWidth>> = vec![None; ncols];
        for cell in cells {
            if cell.colspan == 1 && cell.col < ncols && col_w[cell.col].is_none() {
                col_w[cell.col] = self.declared_track_width(cell.id);
            }
        }

        // Cheap mode (we're inside an ancestor's width measurement): don't
        // re-measure every cell's content — honor declared column widths and
        // divide the rest equally. Bounds the work so nested-table measurement
        // stays linear instead of compounding with depth.
        if cheap {
            let tw = table_spec.unwrap_or(band).min(band);
            return self.fixed_columns(&col_w, ncols, tw, bs);
        }

        // Per-column min/max content widths (the automatic algorithm).
        let mut col_min = vec![1usize; ncols];
        let mut col_max = vec![1usize; ncols];
        // Single-column cells first.
        for cell in cells {
            if cell.colspan != 1 || cell.col >= ncols {
                continue;
            }
            let (mn, mx) = self.cell_min_max(cell.id, avail);
            col_min[cell.col] = col_min[cell.col].max(mn);
            col_max[cell.col] = col_max[cell.col].max(mx);
        }
        // Spanning cells: widen their columns so the span fits (§17.5.2.2 step
        // 3 — widen all spanned columns by approximately the same amount).
        for cell in cells {
            if cell.colspan <= 1 {
                continue;
            }
            let end = (cell.col + cell.colspan).min(ncols);
            let span = end.saturating_sub(cell.col);
            if span == 0 {
                continue;
            }
            let (mn, mx) = self.cell_min_max(cell.id, avail);
            let inner_bs = bs * (span - 1);
            distribute_deficit(&mut col_min[cell.col..end], mn.saturating_sub(inner_bs));
            distribute_deficit(&mut col_max[cell.col..end], mx.saturating_sub(inner_bs));
        }

        // Fixed layout: a definite table width + `table-layout:fixed` ignores
        // content and divides by declared column widths (§17.5.2.1).
        let fixed = self
            .dom
            .computed_style(table, "table-layout")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("fixed"));
        if let Some(tw) = table_spec.filter(|_| fixed) {
            return self.fixed_columns(&col_w, ncols, tw, bs);
        }

        let min_sum: usize = col_min.iter().sum();
        let max_sum: usize = col_max.iter().sum();
        // Used table content width (§17.5.2.2): with a specified width, the
        // greater of it and MIN; auto uses MAX when it fits the band, else the
        // band — always at least MIN.
        let content_avail = avail;
        let used = match table_spec {
            Some(w) => w.saturating_sub(spacing).max(min_sum),
            None => {
                if max_sum <= content_avail {
                    max_sum.max(min_sum)
                } else {
                    content_avail.max(min_sum)
                }
            }
        };

        // Target per column: an explicit width (px, or % of the used table
        // width) clamped to its min; otherwise its max-content.
        let table_used_w = used + spacing;
        let mut target: Vec<usize> = (0..ncols)
            .map(|c| {
                let t = match col_w[c] {
                    Some(TrackWidth::Px(px)) => px,
                    Some(TrackWidth::Pct(p)) => (p * table_used_w as f32).round() as usize,
                    None => col_max[c],
                };
                t.max(col_min[c]).max(1)
            })
            .collect();

        let target_sum: usize = target.iter().sum();
        if target_sum < used {
            // Grow: distribute slack to the auto (no explicit width) columns by
            // their max-content; if none are auto, grow all proportionally.
            let extra = used - target_sum;
            let auto: Vec<usize> = (0..ncols).filter(|&c| col_w[c].is_none()).collect();
            if !auto.is_empty() {
                let weight: usize = auto.iter().map(|&c| col_max[c].max(1)).sum();
                grow_by_weight(&mut target, &auto, extra, |c| col_max[c].max(1), weight);
            } else {
                let all: Vec<usize> = (0..ncols).collect();
                let snapshot: Vec<usize> = target.iter().map(|&w| w.max(1)).collect();
                let weight: usize = snapshot.iter().sum::<usize>().max(1);
                grow_by_weight(&mut target, &all, extra, |c| snapshot[c], weight);
            }
        } else if target_sum > used {
            // Shrink toward each column's min, proportional to the slack above
            // it; if that's not enough (mins overflow the band), scale the mins.
            let mut over = target_sum - used;
            let head: usize = (0..ncols).map(|c| target[c] - col_min[c]).sum();
            for c in 0..ncols {
                let slack_above = target[c] - col_min[c];
                let cut = (over * slack_above)
                    .checked_div(head)
                    .unwrap_or(0)
                    .min(slack_above);
                target[c] -= cut;
            }
            // Any residual (rounding, or min-overflow): trim widest-first.
            over = target.iter().sum::<usize>().saturating_sub(used);
            while over > 0 {
                let widest = (0..ncols).max_by_key(|&c| target[c]).unwrap_or(0);
                if target[widest] <= 1 {
                    break;
                }
                target[widest] -= 1;
                over -= 1;
            }
        }
        target
    }

    /// Fixed table layout column widths (§17.5.2.1): declared column widths are
    /// honored, remaining space is divided equally over the rest.
    fn fixed_columns(
        &self,
        col_w: &[Option<TrackWidth>],
        ncols: usize,
        table_w: usize,
        bs: usize,
    ) -> Vec<usize> {
        let content = table_w.saturating_sub(bs * ncols.saturating_sub(1)).max(1);
        let mut widths = vec![0usize; ncols];
        let mut fixed_total = 0usize;
        let mut autos = Vec::new();
        for c in 0..ncols {
            match col_w[c] {
                Some(TrackWidth::Px(px)) => {
                    widths[c] = px.min(content);
                    fixed_total += widths[c];
                }
                Some(TrackWidth::Pct(p)) => {
                    widths[c] = (p * content as f32).round() as usize;
                    fixed_total += widths[c];
                }
                None => autos.push(c),
            }
        }
        let rest = content.saturating_sub(fixed_total);
        if !autos.is_empty() {
            let each = (rest / autos.len()).max(1);
            for &c in &autos {
                widths[c] = each;
            }
        }
        for w in &mut widths {
            *w = (*w).max(1);
        }
        widths
    }

    /// A cell's min-content and max-content widths (cells): the narrowest its
    /// content wraps to, and its preferred unwrapped width (capped at the
    /// table's available width). A specified `width` raises the minimum (§17.5.2.2
    /// step 1 — "if W is greater than MCW, W is the minimum cell width").
    fn cell_min_max(&self, id: NodeId, avail: usize) -> (usize, usize) {
        let mut mn = self.measure_width(id, 1).max(1);
        let mut mx = self.measure_width(id, avail).max(mn);
        if let Some(TrackWidth::Px(px)) = self.declared_track_width(id) {
            mn = mn.max(px.min(avail));
            mx = mx.max(px.min(avail));
        }
        (mn, mx)
    }

    /// Parse a `grid-template-columns`/`-rows` value into per-track sizing
    /// functions, expanding `repeat()` (`auto-fill`/`auto-fit` counted against
    /// `avail`). Fixed lengths resolve to cells now (against the grid's content
    /// box, `avail`); intrinsic/`fr` tracks resolve during sizing. `None` (the
    /// caller falls back) for `none`/empty or any token we can't parse.
    fn parse_track_list(&self, value: &str, avail: f32, gap: f32) -> Option<Vec<TrackSpec>> {
        let v = value.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("none") {
            return None;
        }
        // Named grid areas / line-name-only templates aren't placed by us.
        let mut tracks = Vec::new();
        for tok in split_track_tokens(v) {
            if let Some(inner) = tok
                .strip_prefix("repeat(")
                .and_then(|r| r.strip_suffix(')'))
            {
                let (count_s, list_s) = inner.split_once(',')?;
                let list = self.parse_track_list(list_s, avail, gap)?;
                let count_s = count_s.trim();
                let count = if count_s.eq_ignore_ascii_case("auto-fill")
                    || count_s.eq_ignore_ascii_case("auto-fit")
                {
                    let unit: f32 = list.iter().map(|t| self.track_repeat_min(t)).sum::<f32>()
                        + gap * (list.len() as f32 - 1.0).max(0.0);
                    if unit <= 0.0 {
                        return None; // no definite repeat width → can't count
                    }
                    (((avail + gap) / (unit + gap)).floor() as usize).clamp(1, 1000)
                } else {
                    count_s.parse::<usize>().ok()?.clamp(1, 1000)
                };
                for _ in 0..count {
                    tracks.extend(list.iter().cloned());
                }
            } else if let Some(spec) = self.parse_one_track(&tok, avail) {
                tracks.push(spec);
            } else {
                return None;
            }
        }
        (!tracks.is_empty()).then_some(tracks)
    }

    /// Parse a single track sizing function (`auto`, `Nfr`, a length,
    /// `minmax()`, `fit-content()`, `min/max-content`). `None` if unparseable.
    fn parse_one_track(&self, tok: &str, avail: f32) -> Option<TrackSpec> {
        let t = tok.trim();
        match t {
            "auto" => return Some(TrackSpec::Auto),
            "min-content" => return Some(TrackSpec::MinContent),
            "max-content" => return Some(TrackSpec::MaxContent),
            _ => {}
        }
        if let Some(inner) = t.strip_prefix("minmax(").and_then(|r| r.strip_suffix(')')) {
            let (a, b) = split_args(inner)
                .split_first()
                .and_then(|(a, rest)| rest.first().map(|b| (a.trim(), b.trim())))?;
            let min = self.parse_one_track(a, avail)?;
            let max = self.parse_one_track(b, avail)?;
            return Some(TrackSpec::Minmax(Box::new(min), Box::new(max)));
        }
        if let Some(inner) = t
            .strip_prefix("fit-content(")
            .and_then(|r| r.strip_suffix(')'))
        {
            let cap = resolve_cells_f32(inner.trim(), avail as usize, self.viewport_w)?;
            return Some(TrackSpec::FitContent(cap.max(0.0)));
        }
        if let Some(num) = t.strip_suffix("fr") {
            let f: f32 = num.trim().parse().ok()?;
            return Some(TrackSpec::Fr(f.max(0.0)));
        }
        resolve_cells_f32(t, avail as usize, self.viewport_w).map(|c| TrackSpec::Fixed(c.max(0.0)))
    }

    /// Whether a track sizes to content (so the content-width pass must measure
    /// its items): `auto`/`min|max-content`/`fit-content`, or a `minmax()` with
    /// an intrinsic bound.
    fn track_is_intrinsic(&self, spec: &TrackSpec) -> bool {
        match spec {
            TrackSpec::Auto
            | TrackSpec::MinContent
            | TrackSpec::MaxContent
            | TrackSpec::FitContent(_) => true,
            TrackSpec::Minmax(min, max) => {
                self.track_is_intrinsic(min) || self.track_is_intrinsic(max)
            }
            _ => false,
        }
    }

    /// A track's definite minimum size in cells — used to count `auto-fill`/
    /// `auto-fit` repetitions. `0` for tracks with no definite floor
    /// (`auto`/`fr`/content), which makes a pure-`fr` `auto-fill` uncountable.
    fn track_repeat_min(&self, spec: &TrackSpec) -> f32 {
        match spec {
            TrackSpec::Fixed(c) => *c,
            TrackSpec::FitContent(cap) => *cap,
            TrackSpec::Minmax(min, _) => self.track_repeat_min(min),
            _ => 0.0,
        }
    }

    /// A track's `(base, fr)` for sizing: its floor in cells and its `fr`
    /// growth weight (0 = inflexible). `content` is the measured content width
    /// for intrinsic tracks.
    fn track_base_fr(&self, spec: &TrackSpec, content: f32) -> (f32, f32) {
        match spec {
            TrackSpec::Fixed(c) => (*c, 0.0),
            TrackSpec::Fr(f) => (0.0, *f),
            TrackSpec::Auto | TrackSpec::MaxContent | TrackSpec::MinContent => (content, 0.0),
            TrackSpec::FitContent(cap) => (content.min(*cap), 0.0),
            TrackSpec::Minmax(min, max) => {
                let (min_base, _) = self.track_base_fr(min, content);
                match max.as_ref() {
                    TrackSpec::Fr(f) => (min_base, *f),
                    TrackSpec::Fixed(c) => (c.max(min_base), 0.0),
                    other => {
                        let (max_base, _) = self.track_base_fr(other, content);
                        (max_base.max(min_base), 0.0)
                    }
                }
            }
        }
    }

    /// Resolve track sizing functions to concrete cell widths: place bases,
    /// hand leftover space to `fr` tracks by weight, and — since a terminal has
    /// no horizontal scroll — proportionally compress an over-wide template to
    /// fit (the common `fr` case never overflows, so fixed tracks keep their
    /// size).
    fn size_grid_tracks(
        &self,
        specs: &[TrackSpec],
        content_w: &[f32],
        avail: usize,
        gap: usize,
    ) -> Vec<usize> {
        let n = specs.len();
        let mut base = vec![0f32; n];
        let mut fr = vec![0f32; n];
        for i in 0..n {
            let (b, f) = self.track_base_fr(&specs[i], content_w[i]);
            base[i] = b.max(0.0);
            fr[i] = f.max(0.0);
        }
        let gaps = gap as f32 * (n as f32 - 1.0).max(0.0);
        let base_sum: f32 = base.iter().sum();
        let fr_sum: f32 = fr.iter().sum();
        let mut size = base.clone();
        let free = avail as f32 - base_sum - gaps;
        if free > 0.0 && fr_sum > 0.0 {
            for i in 0..n {
                size[i] += free * fr[i] / fr_sum;
            }
        }
        let total: f32 = size.iter().sum::<f32>() + gaps;
        if total > avail as f32 {
            let usable = (avail as f32 - gaps).max(n as f32);
            let s: f32 = size.iter().sum();
            if s > 0.0 {
                let k = usable / s;
                for v in &mut size {
                    *v *= k;
                }
            }
        }
        size.into_iter()
            .map(|c| c.round().max(1.0) as usize)
            .collect()
    }

    /// Assign every grid item a `(col, col_span, row, row_span)`: honor an
    /// explicit `grid-column`/`grid-row` (line numbers + `span`), and auto-place
    /// the rest along the `grid-auto-flow` axis into the first free cells,
    /// tracking occupancy so placed and auto items never overlap.
    fn place_grid_items(
        &self,
        items: &[NodeId],
        ncols: usize,
        column_flow: bool,
    ) -> Vec<GridPlace> {
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut out = Vec::with_capacity(items.len());
        let (mut cur_r, mut cur_c) = (0usize, 0usize);
        for &it in items {
            let (cstart, cspan) = self.parse_grid_line(it, "grid-column", ncols);
            let (rstart, rspan) = self.parse_grid_line(it, "grid-row", 0);
            let cspan = cspan.clamp(1, ncols);
            let rspan = rspan.max(1);
            let place = if let Some(c) = cstart {
                let c = c.min(ncols - cspan);
                let mut r = rstart.unwrap_or(0);
                while !grid_cells_free(&occ, r, c, rspan, cspan, ncols) {
                    r += 1;
                }
                GridPlace {
                    col: c,
                    col_span: cspan,
                    row: r,
                    row_span: rspan,
                }
            } else if let Some(r) = rstart {
                let mut c = 0;
                while !grid_cells_free(&occ, r, c, rspan, cspan, ncols) && c + cspan < ncols {
                    c += 1;
                }
                GridPlace {
                    col: c.min(ncols - cspan),
                    col_span: cspan,
                    row: r,
                    row_span: rspan,
                }
            } else {
                let (mut r, mut c) = (cur_r, cur_c);
                if column_flow {
                    while !grid_cells_free(&occ, r, c, rspan, cspan, ncols) {
                        r += 1;
                        if r > occ.len() + items.len() {
                            r = 0;
                            c = (c + 1).min(ncols - cspan);
                        }
                    }
                } else {
                    loop {
                        if c + cspan <= ncols && grid_cells_free(&occ, r, c, rspan, cspan, ncols) {
                            break;
                        }
                        c += 1;
                        if c + cspan > ncols {
                            c = 0;
                            r += 1;
                        }
                    }
                }
                GridPlace {
                    col: c,
                    col_span: cspan,
                    row: r,
                    row_span: rspan,
                }
            };
            grid_mark(
                &mut occ,
                place.row,
                place.col,
                place.row_span,
                place.col_span,
                ncols,
            );
            if column_flow {
                cur_c = place.col;
                cur_r = place.row + place.row_span;
            } else {
                cur_r = place.row;
                cur_c = place.col + place.col_span;
                if cur_c >= ncols {
                    cur_c = 0;
                    cur_r += 1;
                }
            }
            out.push(place);
        }
        out
    }

    /// Parse an item's `grid-column`/`grid-row` into `(start, span)`: `start` is
    /// a zero-based track index (`None` = auto-placed). Supports `N`, `N / M`,
    /// `N / span S`, `span S`, and the `1 / -1` (span all columns) idiom; named
    /// lines/areas fall through to auto. `ntracks` is the column count (for the
    /// `-1` end line), `0` for rows (negatives there → auto).
    fn parse_grid_line(&self, id: NodeId, prop: &str, ntracks: usize) -> (Option<usize>, usize) {
        let Some(v) = self.dom.computed_style(id, prop) else {
            return (None, 1);
        };
        let v = v.trim();
        if v.is_empty() || v == "auto" {
            return (None, 1);
        }
        let parts: Vec<&str> = v.split('/').map(str::trim).collect();
        let span_of = |s: &str| {
            s.strip_prefix("span ")
                .and_then(|n| n.trim().parse::<usize>().ok())
        };
        match parts.as_slice() {
            [a] => {
                if let Some(s) = span_of(a) {
                    (None, s.max(1))
                } else if let Ok(line) = a.parse::<i32>() {
                    ((line > 0).then(|| (line - 1) as usize), 1)
                } else {
                    (None, 1)
                }
            }
            [a, b] => {
                let start = a
                    .parse::<i32>()
                    .ok()
                    .filter(|l| *l > 0)
                    .map(|l| (l - 1) as usize);
                if let Some(s) = span_of(b) {
                    (start, s.max(1))
                } else if let (Some(si), Ok(e)) = (start, b.parse::<i32>()) {
                    let ei = if e > 0 {
                        (e - 1) as usize
                    } else if ntracks > 0 {
                        // `-1` is the line after the last track.
                        (ntracks as i32 + 1 + e).max(0) as usize
                    } else {
                        si + 1
                    };
                    (Some(si), ei.saturating_sub(si).max(1))
                } else {
                    (start, 1)
                }
            }
            _ => (None, 1),
        }
    }

    /// The visible border on each side `[top, right, bottom, left]` (its
    /// rendering weight, or `None`). A side shows iff its `border-*-style` is a
    /// visible style and its width isn't an explicit `0` (CSS: no style = no
    /// border; the default width is the visible `medium`).
    fn border_sides(&self, id: NodeId) -> [Option<BorderWeight>; 4] {
        ["top", "right", "bottom", "left"].map(|side| self.border_one(id, side))
    }

    fn border_one(&self, id: NodeId, side: &str) -> Option<BorderWeight> {
        let style = self
            .dom
            .computed_style(id, &format!("border-{side}-style"))?;
        if style == "none" || style == "hidden" {
            return None;
        }
        let width = self.dom.computed_style(id, &format!("border-{side}-width"));
        if width.as_deref().and_then(border_px) == Some(0.0) {
            return None;
        }
        Some(border_weight(&style, width.as_deref()))
    }

    /// Lay a block-level element that has a border: its interior goes in a
    /// sub-box (margin handled out here, padding kept inside), the bordered
    /// sides are drawn as box-drawing around it, and the framed box is blitted
    /// at the current flow position. Reuses the 2D box primitive, so selection
    /// and scroll keep working.
    fn flow_bordered(&mut self, id: NodeId, sides: [Option<BorderWeight>; 4], ctx: &Ctx) {
        let tag = self.dom.tag_name(id).map(str::to_owned).unwrap_or_default();
        self.flush_block();
        self.clear_floats(id);
        if self.gap_before(id, &tag) {
            self.push_blank();
        }
        let avail = self.width.saturating_sub(self.indent).max(1);
        let frame_w = usize::from(sides[3].is_some()) + usize::from(sides[1].is_some());
        // The framed box fills its container (a block) or its explicit CSS
        // width, so e.g. a `border-bottom` rule spans the whole block.
        let box_w = self
            .css_cells(id, "width")
            .unwrap_or(avail)
            .min(avail)
            .max(frame_w + 1);
        let inner_w = box_w - frame_w;
        // Lay the element's own interior, marking it so the recursion stops
        // and its margin is suppressed (we applied it out here).
        let mut sub = Layout::new(
            self.dom,
            self.base,
            inner_w,
            self.forms,
            self.controls,
            self.images,
            self.borders,
        );
        sub.viewport_w = self.viewport_w;
        sub.tag_all_nodes = self.tag_all_nodes;
        sub.inner_border_box = Some(id);
        // The interior pass must NOT re-float this same element: when `id` is
        // both floated and bordered, `flow_float` lays its box (skipping the
        // float) and that box's `flow_element(id)` routes here for the frame.
        // Without threading the float skip through, this sub's float check
        // fires again → `flow_float` → back here → infinite recursion (the two
        // guards live on different fields, each fresh sub resetting the other).
        sub.float_skip = Some(id);
        sub.flow_node(id, ctx);
        sub.flush_block();
        sub.finish_floats();
        let (rows, carousels, element_tops) = sub.finish();
        let content = LaidBox {
            height: rows.len() as u16,
            width: inner_w as u16,
            rows,
            carousels,
            element_tops,
        };
        let framed = self.frame_box(content, sides);
        if framed.height > 0 {
            let row_base = self.rows.len();
            self.blit(&framed, self.indent as u16, row_base);
        }
        if self.gap_after(id, &tag) {
            self.push_blank();
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// Wrap a laid-out `content` box in the frame for its bordered `sides`:
    /// shift the content in by the present sides, draw top/bottom edge rows
    /// (with corners) and left/right vertical bars, all as `Border` items. A
    /// single weight (the first present side's) styles the whole frame.
    fn frame_box(&self, content: LaidBox, sides: [Option<BorderWeight>; 4]) -> LaidBox {
        let weight = sides
            .iter()
            .flatten()
            .copied()
            .next()
            .unwrap_or(BorderWeight::Light);
        let set = line_set(weight);
        let (tp, rt, bt, lt) = (
            sides[0].is_some(),
            sides[1].is_some(),
            sides[2].is_some(),
            sides[3].is_some(),
        );
        let inner_h = content.height as usize;
        let new_w = content.width as usize + usize::from(lt) + usize::from(rt);
        let new_h = inner_h + usize::from(tp) + usize::from(bt);
        let col_shift = u16::from(lt);
        let row_shift = usize::from(tp);
        let mut rows: Vec<Row> = (0..new_h).map(|_| Row::default()).collect();
        // A border is a hard visual boundary: interior content that overflows
        // the content box (a non-wrapping line, a wide image, a too-wide rail)
        // must be CLIPPED to the inner width, or the renderer — which just
        // concatenates over-wide items — pushes the right bar off the line and
        // the right border vanishes. This mirrors `overflow:hidden`. Clipped
        // per ROW, not per box: a horizontal-scroll carousel's strip rows keep
        // their over-wide content (clipped at render time by `visible_col`, so
        // hard-clipping here would drop the off-screen cards scrolling reveals)
        // while the box's non-scrolling rows still get their borders protected.
        let inner_w = content.width;
        for (r, (target, row)) in rows
            .iter_mut()
            .skip(row_shift)
            .zip(content.rows)
            .enumerate()
        {
            let in_carousel = content.carousels.iter().any(|c| r >= c.start && r < c.end);
            for mut it in row.items {
                if !in_carousel {
                    if it.col >= inner_w {
                        continue;
                    }
                    let max_w = inner_w - it.col;
                    if it.width > max_w {
                        it.width = max_w;
                        it.text = it.text.chars().take(max_w as usize).collect();
                    }
                }
                it.col += col_shift;
                target.items.push(it);
            }
        }
        let glyph = |col: usize, w: usize, text: String| Item {
            col: col as u16,
            width: w as u16,
            height: 1,
            text,
            kind: ItemKind::Border,
            image: None,
            crop: false,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
        };
        if tp {
            let s = edge_string(
                new_w,
                lt.then_some(set.top_left),
                rt.then_some(set.top_right),
                set.horizontal,
            );
            rows[0].items.push(glyph(0, new_w, s));
        }
        if bt {
            let s = edge_string(
                new_w,
                lt.then_some(set.bottom_left),
                rt.then_some(set.bottom_right),
                set.horizontal,
            );
            rows[new_h - 1].items.push(glyph(0, new_w, s));
        }
        for row in rows.iter_mut().skip(row_shift).take(inner_h) {
            if lt {
                row.items.push(glyph(0, 1, set.vertical.to_owned()));
            }
            if rt {
                row.items.push(glyph(new_w - 1, 1, set.vertical.to_owned()));
            }
        }
        for row in &mut rows {
            row.items.sort_by_key(|it| it.col);
        }
        let mut carousels = content.carousels;
        for c in &mut carousels {
            c.start += row_shift;
            c.end += row_shift;
            c.left += col_shift;
            c.right += col_shift;
            // The right frame bar lands at the band's right edge, inside the
            // strip's column span — flag it so the render-time carousel clip
            // (`visible_col`) draws it as static chrome instead of clipping it
            // as off-screen strip content (which dropped the right border).
            if rt {
                c.frame_right = Some(new_w as u16 - 1);
            }
        }
        // The frame shifts the interior in by the present sides, so the
        // recorded empty-element positions move with it.
        let element_tops = content
            .element_tops
            .into_iter()
            .map(|(id, (col, row))| (id, (col + col_shift, row + row_shift as u16)))
            .collect();
        LaidBox {
            rows,
            width: new_w as u16,
            height: new_h as u16,
            carousels,
            element_tops,
        }
    }

    /// A CSS length property in terminal cells (≈ 2 cells/em, 16px=1em),
    /// resolving `%`/`vw`/`calc()` against the current band width and the
    /// viewport. `None` when unset (or `auto`/an unsupported unit). Clamped
    /// to ≥1 cell.
    fn css_cells(&self, id: NodeId, prop: &str) -> Option<usize> {
        let v = self.dom.computed_style(id, prop)?;
        let avail = self.width.saturating_sub(self.indent).max(1);
        resolve_cells_f32(&v, avail, self.viewport_w).map(|c| c.round().max(1.0) as usize)
    }

    /// Like `css_cells`, but FLOORS the result. For a grid column width, `N`
    /// rounded `(100/N)%` widths can sum past the row and drop the last column;
    /// flooring keeps every column (each loses ≤1 cell, absorbed by the slack).
    fn css_cells_floor(&self, id: NodeId, prop: &str) -> Option<usize> {
        let v = self.dom.computed_style(id, prop)?;
        let avail = self.width.saturating_sub(self.indent).max(1);
        resolve_cells_f32(&v, avail, self.viewport_w).map(|c| c.floor().max(1.0) as usize)
    }

    /// Lay an element's subtree out as an independent box at `content_width`,
    /// positioned relative to its own top-left (`col` 0). Shares the DOM,
    /// base URL, form/control maps, and image sizes with the parent. The
    /// recursion that powers grids and (later) columns and floats.
    fn layout_subtree(&self, id: NodeId, content_width: usize, inherit: &Ctx) -> LaidBox {
        // Inherit the parent's measuring state: a subtree laid WHILE measuring an
        // ancestor's intrinsic width (a flex/grid item's box, a stacked column)
        // must keep measuring, so a nested `width:100%` replaced element resolves
        // to its INTRINSIC width (CSS Sizing §5.1 — a percentage is indefinite
        // for a max-content contribution) instead of filling the whole
        // constraint. Without this, measuring a flex item containing a
        // `<img width:100%>` reported the full available width as the item's
        // base size, so every such tile claimed its own shelf at full width
        // (archive.org's Top-Collections grid collapsed to one column once the
        // lazy tile images finished loading). Real (non-measuring) layout passes
        // `false` exactly as before.
        self.layout_subtree_inner(id, content_width, None, false, inherit)
    }

    /// `layout_subtree`, optionally ignoring the float on the root element
    /// (used when laying a float's own box so it doesn't recurse). `measure`
    /// means this is an intrinsic-width measurement (ignore `text-align`).
    /// `inherit` seeds the sub-layout's root context so a child laid in a
    /// separate pass (a flex/grid item) still inherits an enclosing `<a>`'s
    /// link (and emphasis) — otherwise its contents would lose interactivity,
    /// exactly as a bordered box would without `flow_bordered` threading `ctx`.
    fn layout_subtree_inner(
        &self,
        id: NodeId,
        content_width: usize,
        skip_float: Option<NodeId>,
        measure: bool,
        inherit: &Ctx,
    ) -> LaidBox {
        let mut sub = Layout::new(
            self.dom,
            self.base,
            content_width.max(1),
            self.forms,
            self.controls,
            self.images,
            self.borders,
        );
        sub.viewport_w = self.viewport_w;
        sub.float_skip = skip_float;
        sub.subtree_root = Some(id);
        sub.table_depth = self.table_depth;
        sub.measuring = measure;
        // Carry the measurement flag so a sub-layout tags its items with their
        // own nodes and records empty-element flow positions (`element_tops`);
        // `blit` then propagates that geometry back up. The render path keeps
        // this off (no tagging), so it's unaffected.
        sub.tag_all_nodes = self.tag_all_nodes;
        // A box laid within a surfaced modal must know it (so its full-bleed
        // foreground image isn't dropped as a page backdrop, and the modal-root
        // out-of-flow exemption holds in the sub-pass).
        sub.modal_root = self.modal_root;
        sub.flow_node(id, inherit);
        sub.flush_block();
        sub.finish_floats();
        let (rows, carousels, element_tops) = sub.finish();
        let width = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|it| it.col + it.width)
            .max()
            .unwrap_or(0);
        let height = rows.len() as u16;
        LaidBox {
            rows,
            width,
            height,
            carousels,
            element_tops,
        }
    }

    /// Copy a laid-out box into the parent's rows, shifting every item's
    /// `col` by `col_off` and placing box row `r` into parent row
    /// `row_base + r` (creating parent rows as needed). The 2D placement
    /// primitive — items keep their node/link so selection re-anchors and
    /// vertical scroll still index by the parent row grid.
    fn blit(&mut self, b: &LaidBox, col_off: u16, row_base: usize) {
        for (r, row) in b.rows.iter().enumerate() {
            let target = row_base + r;
            while self.rows.len() <= target {
                self.rows.push(Row::default());
            }
            for it in &row.items {
                let mut it = it.clone();
                it.col += col_off;
                self.rows[target].items.push(it);
            }
        }
        // Carousels inside the box move with it: rows by `row_base`, the band
        // by `col_off` (stops/offset are strip-relative, so unchanged).
        for c in &b.carousels {
            let mut c = c.clone();
            c.start += row_base;
            c.end += row_base;
            c.left += col_off;
            c.right += col_off;
            c.frame_right = c.frame_right.map(|fr| fr + col_off);
            self.carousels.push(c);
        }
        // Empty elements recorded in the box (measure pass only) move with it
        // too, so a boxless element nested in this sub-layout keeps its honest
        // flow position in the document's coordinate system.
        for (&id, &(col, row)) in &b.element_tops {
            self.element_tops
                .entry(id)
                .or_insert((col + col_off, row + row_base as u16));
        }
    }

    /// Flow a run of inline text under the active `white-space` mode.
    fn place_text(&mut self, text: &str, ctx: &Ctx) {
        if text.is_empty() {
            return;
        }
        if self.ws.preserves_newlines() {
            // Pre/pre-wrap/pre-line: a literal `\n` is a hard break.
            for (i, seg) in text.split('\n').enumerate() {
                if i > 0 {
                    self.break_line();
                }
                if self.ws.collapses_spaces() {
                    self.place_collapsed(seg, ctx); // pre-line
                } else {
                    self.place_preserved(seg, ctx); // pre / pre-wrap
                }
            }
        } else {
            self.place_collapsed(text, ctx); // normal / nowrap
        }
    }

    /// Flow text that collapses runs of whitespace to a single space. In
    /// `nowrap` mode the words still collapse but never break the line
    /// (the wrap is gated in `place_word`).
    fn place_collapsed(&mut self, text: &str, ctx: &Ctx) {
        if text.is_empty() {
            return;
        }
        let leading = text.starts_with(char::is_whitespace);
        let trailing = text.ends_with(char::is_whitespace);
        let mut any = false;
        if leading {
            self.pending_space = true;
        }
        for (i, word) in text.split_whitespace().enumerate() {
            // Inter-word whitespace within the node collapses to one
            // space; `pending_space` carries it (and any space owed
            // across a node boundary) into the placement.
            if i > 0 {
                self.pending_space = true;
            }
            self.place_word(word, ctx);
            any = true;
        }
        if trailing && any {
            self.pending_space = true;
        } else if !any && !self.line.is_empty() {
            // All-whitespace text node between inline boxes owes a space.
            self.pending_space = true;
        }
    }

    /// Place one word, word-wrapping at the content width. An owed
    /// inter-word space attaches to the *preceding* item (so a link's
    /// own text stays clean at its leading edge).
    fn place_word(&mut self, word: &str, ctx: &Ctx) {
        let transformed = ctx.transform.apply(word);
        let spaced = letter_space(transformed.as_ref(), ctx.letter_spacing);
        let mut text: std::borrow::Cow<str> = std::borrow::Cow::Borrowed(spaced.as_ref());
        let mut wlen = display_width(&text);
        // Clip inside a `nowrap`+`overflow:hidden` box: the line can't wrap, so
        // a word reaching the box's right edge is truncated with `…` and the
        // rest of the (unwrappable) words are dropped. Keeps a long post title
        // from bleeding out of its card into a neighbour.
        if let Some(right) = self.clip_right {
            if self.clip_done {
                self.pending_space = false;
                return;
            }
            let space = self.pending_space && self.col > self.line_left;
            let start = self.col + space as usize;
            if start + wlen > right {
                self.clip_done = true;
                let room = right.saturating_sub(start);
                if room == 0 {
                    self.pending_space = false;
                    return;
                }
                // Leave one cell for the ellipsis (drop it only if the whole
                // box is a single cell wide).
                let mut t = truncate_to_width(&text, room.saturating_sub(1));
                t.push('…');
                wlen = display_width(&t);
                text = std::borrow::Cow::Owned(t);
            }
        }
        let word = text.as_ref();
        let space = self.pending_space && self.col > self.line_left;
        if self.ws.wraps()
            && self.col + space as usize + wlen > self.line_right
            && self.col > self.line_left
        {
            self.break_line();
        }
        let space = self.pending_space && self.col > self.line_left;
        self.pending_space = false;
        if space {
            if let Some(last) = self.line.last_mut() {
                last.text.push(' ');
                last.width += 1;
            }
            self.col += 1;
        }
        if let Some(last) = self.line.last_mut()
            && same_run(last, ctx)
            && last.col as usize + last.width as usize == self.col
        {
            last.text.push_str(word);
            last.width += wlen as u16;
            self.col += wlen;
            return;
        }
        self.push_item(
            word.to_owned(),
            wlen,
            ctx.kind,
            ctx.emph,
            ctx.node,
            ctx.link.clone(),
        );
    }

    /// Place a newline-free segment with its spaces preserved. `pre`
    /// emits it as one unwrapped item; `pre-wrap` breaks it into
    /// width-fitting chunks (spaces kept). Uses `ctx.kind`, so CSS
    /// `white-space:pre` on a non-`<pre>` element keeps its own styling.
    fn place_preserved(&mut self, seg: &str, ctx: &Ctx) {
        if seg.is_empty() {
            return;
        }
        let transformed = ctx.transform.apply(seg);
        let seg = transformed.as_ref();
        if !self.ws.wraps() {
            let len = display_width(seg);
            self.push_preserved_item(seg, len, ctx);
            return;
        }
        // pre-wrap: width-budget wrap within the content box, keeping spaces.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let mut buf = String::new();
        let mut chars = seg.chars().peekable();
        while let Some(c) = chars.next() {
            buf.push(c);
            if display_width(&buf) >= avail && chars.peek().is_some() {
                let len = display_width(&buf);
                self.push_preserved_item(&buf, len, ctx);
                self.break_line();
                buf.clear();
            }
        }
        if !buf.is_empty() {
            let len = display_width(&buf);
            self.push_preserved_item(&buf, len, ctx);
        }
    }

    /// Push one preserved-whitespace run, inheriting the context's kind
    /// and emphasis.
    fn push_preserved_item(&mut self, text: &str, len: usize, ctx: &Ctx) {
        self.push_item(text.to_owned(), len, ctx.kind, ctx.emph, ctx.node, None);
    }

    /// Render a `<video>`/`<audio>` element as a media representation: the
    /// video poster (when present and decoded) as a clickable thumbnail, plus
    /// a labelled link (`▶ Video · 720p HD` / `♪ Audio`). Both carry a link to
    /// the playable source, so following it auto-launches mpv (see
    /// `is_playable_media_url` in app.rs). Audio, or a poster-less / not-yet-
    /// decoded video, falls back to the link alone — fully general (not every
    /// embed has a preview frame).
    fn flow_media(&mut self, id: NodeId, tag: &str, ctx: &Ctx) {
        // The playable URL: the element's own `src`, else the first `<source>`.
        let Some((media_url, src_node)) = self.media_source(id) else {
            return; // no playable source — nothing to represent
        };
        // The representation links to the media; following it launches mpv.
        let mut mctx = ctx.clone();
        mctx.link = Some(crate::http::resolve(self.base, &media_url));
        mctx.kind = ItemKind::Link;
        mctx.node = id;

        self.flush_block();
        self.begin_line();

        // The video poster (its own preview frame), when present AND decoded,
        // renders as a clickable thumbnail. Sized by its decoded box (NOT the
        // `<video>`'s CSS — that carries a `height:0`/`padding-top` 16:9 hack
        // a poster must not inherit), capped to the content width.
        let poster = (tag == "video")
            .then(|| self.dom.attr(id, "poster"))
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|p| match crate::http::resolve(self.base, p) {
                Link::Http(u) => Some(u.to_string()),
                _ => None,
            });
        if let Some(poster) = poster
            && let Some(&(iw, ih)) = self.images.get(&poster)
            && iw > 0
            && ih > 0
        {
            let avail = self.line_right.saturating_sub(self.line_left).max(1) as u16;
            let w = iw.min(avail).max(1);
            // Keep aspect if width-capped.
            let h = ((ih as u32 * w as u32) / iw as u32).max(1) as u16;
            self.line.push(Item {
                col: self.col as u16,
                width: w,
                height: h,
                image: Some(poster),
                crop: false,
                text: String::new(),
                kind: ItemKind::Image,
                emph: Emphasis::default(),
                node: id,
                link: mctx.link.clone(),
            });
            self.col += w as usize;
            self.line_height = self.line_height.max(h);
            self.break_line();
        }

        // The caption / fallback link.
        let label = self.media_label(tag, src_node);
        self.place_text(&label, &mctx);
        self.break_line();
    }

    /// The playable URL of a `<video>`/`<audio>` element and the chosen
    /// `<source>` node (for its quality label): the element's own `src` if set,
    /// else the first `<source>` with an http(s) `src` (browser source order).
    fn media_source(&self, id: NodeId) -> Option<(String, Option<NodeId>)> {
        if let Some(src) = self
            .dom
            .attr(id, "src")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && let Link::Http(u) = crate::http::resolve(self.base, src)
        {
            return Some((u.to_string(), None));
        }
        for c in self.dom.descendants(id) {
            if self.dom.tag_name(c) == Some("source")
                && let Some(src) = self
                    .dom
                    .attr(c, "src")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                && let Link::Http(u) = crate::http::resolve(self.base, src)
            {
                return Some((u.to_string(), Some(c)));
            }
        }
        None
    }

    /// The caption for a media representation: a glyph + kind + optional
    /// quality from the chosen `<source>`'s `res`/`label` (`▶ Video · 720p HD`).
    fn media_label(&self, tag: &str, src_node: Option<NodeId>) -> String {
        let (glyph, kind) = if tag == "audio" {
            ('♪', "Audio")
        } else {
            ('▶', "Video")
        };
        let mut quality = String::new();
        if let Some(sn) = src_node {
            let res = self
                .dom
                .attr(sn, "res")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|r| format!("{r}p"));
            let lab = self
                .dom
                .attr(sn, "label")
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(res.as_deref().unwrap_or("")))
                .map(str::to_owned);
            let parts: Vec<String> = [res, lab].into_iter().flatten().collect();
            if !parts.is_empty() {
                quality = format!(" · {}", parts.join(" "));
            }
        }
        format!("{glyph} {kind}{quality}")
    }

    fn place_image(&mut self, id: NodeId, ctx: &Ctx) {
        // A decoded image (size known) lays out as a real W×H box; an
        // undecoded or failed one falls back to its alt text.
        if let Some(url) = self.image_src(id)
            && let Some(&(w, h)) = self.images.get(&url)
            && w > 0
            && h > 0
        {
            // A full-bleed, out-of-flow, auto-height image is a decorative
            // background layer (a `position:fixed/absolute` `<img
            // min-width:100%;height:auto>` painted behind the page). A terminal
            // can't composite layers, and reserving its full pixel height
            // buries the real content under dozens of blank rows. Drop it — the
            // out-of-flow CONTAINER already collapses to inline; this is the
            // same compaction for its image. (An in-flow full-width hero, or a
            // sized absolute cover with a definite box, still renders.)
            let avail = self.line_right.saturating_sub(self.line_left).max(1) as u16;
            // A framed foreground — an out-of-flow image filling a positioned
            // ancestor that declares a constrained box (`aspect-ratio` or a
            // definite width) — is real content sized to its frame, never a
            // backdrop, so the drops below must spare it (it fills its own
            // small box, which would otherwise read as "full-bleed"). Then drop
            // the layers a terminal can't composite: a full-bleed page
            // background, or a cover/fill backdrop sitting BEHIND an in-flow
            // content sibling (the blurred-duplicate gallery/lightbox idiom).
            // An image inside the surfaced modal (a lightbox/dialog we're
            // showing in place of the page) is the modal's FOREGROUND content,
            // never a page backdrop — a lightbox's slide is typically a
            // full-bleed image whose every wrapper is `position:absolute`, so
            // the background/backdrop heuristics below would wrongly drop the
            // one thing the modal exists to show. Spare it.
            if !self.within_modal(id)
                && !self.framed_foreground(id)
                && (self.is_background_layer_image(id, w, h, avail)
                    || self.is_backdrop_overlay_image(id))
            {
                return;
            }
            self.place_image_box(id, ctx, url, w, h);
            return;
        }
        let alt = self.dom.attr(id, "alt").unwrap_or("").trim().to_owned();
        if alt.is_empty() {
            return;
        }
        // A rating widget draws each star as its own icon image with
        // descriptive alt text ("full star"/"half star"/"empty star"); a
        // (usually SVG, so undecodable) icon then floods the row with verbose
        // phrases. Collapse those to a single glyph so a 5-star row reads
        // "★★★⯨☆" — general to any star-rating markup, keyed on the alt's
        // accessible text, not on a site.
        let text = star_glyph(&alt).unwrap_or(&alt);
        // Flow the text, tagged as an image so the view can mark it and L3
        // can find the node to render pixels in its place.
        let kind = if ctx.link.is_some() {
            ctx.kind
        } else {
            ItemKind::Image
        };
        let img_ctx = Ctx {
            kind,
            emph: ctx.emph,
            transform: ctx.transform,
            letter_spacing: ctx.letter_spacing,
            node: id,
            link: ctx.link.clone(),
        };
        self.place_text(text, &img_ctx);
    }

    /// The text a subtree actually RENDERS — its *visible* label. DOM
    /// `textContent` includes everything: SVG `<title>`/`<desc>` accessibility
    /// metadata (non-rendered per the SVG spec), `display:none` "sr-only"
    /// spans, etc. A visible label must not. We skip the same subtrees the flow
    /// walker never paints (`SKIP` — which includes the whole `<svg>`) and any
    /// hidden element. So a `<button>` whose only content is an `<svg>` icon
    /// has an EMPTY visible label (its accessible name is used instead), not
    /// "User icon An illustration of a person's head and chest."
    fn rendered_text(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_rendered_text(id, &mut out);
        out
    }

    fn collect_rendered_text(&self, id: NodeId, out: &mut String) {
        match &self.dom.node(id).data {
            NodeData::Text(t) => out.push_str(t),
            NodeData::Element { .. } => {
                let tag = self.dom.tag_name(id).unwrap_or("");
                if SKIP.contains(&tag) || self.dom.is_hidden(id) {
                    return;
                }
                for c in self.dom.children(id) {
                    self.collect_rendered_text(c, out);
                }
            }
            _ => {}
        }
    }

    /// The visible handle for an element whose content won't otherwise render
    /// anything (no text, no `<img>`) — an SVG/icon-only link. Prefers the
    /// recognized icon GLYPH (an `<svg>`/sprite Font-Awesome icon — the header
    /// bell/bookmark/gear, the comment ⋯), else the accessible name
    /// (`aria-label`/`title`/`alt`). `None` when it has real content, a pseudo
    /// glyph already draws it, or it's an unnamed disclosure trigger.
    fn icon_only_label(&self, id: NodeId) -> Option<String> {
        if !self.rendered_text(id).trim().is_empty() {
            return None;
        }
        if self
            .dom
            .descendants(id)
            .iter()
            .any(|&d| self.dom.tag_name(d) == Some("img"))
        {
            return None;
        }
        // A `::before`/`::after` Font-Awesome / Nerd-Font glyph (an icon `<i>`)
        // already draws ON SCREEN — don't ALSO surface a glyph or label, or it
        // doubles. (The "Toggle expanded" arrow, a decorative search/close.)
        if std::iter::once(id)
            .chain(self.dom.descendants(id))
            .any(|d| {
                self.pseudo_text(d, crate::dom::PseudoEl::Before).is_some()
                    || self.pseudo_text(d, crate::dom::PseudoEl::After).is_some()
            })
        {
            return None;
        }
        // An SVG/sprite icon renders as its Unicode glyph — the dominant web
        // icon idiom (a terminal can't usefully rasterize an icon-sized SVG).
        // This wins over the disclosure-trigger suppression below so an icon
        // menu (the search cog) shows its glyph rather than vanishing.
        if let Some(g) = self.dom.icon_glyph(id) {
            return Some(g.to_string());
        }
        // A disclosure trigger (`aria-haspopup`) with no icon opens a menu we
        // can't action yet (AJAX on click); its accessible name is a UI
        // affordance, not body text — surfacing it leaks a phantom word (the
        // search bar's settings cog reading "Search"). Drop it.
        if self.dom.attr(id, "aria-haspopup").is_some() {
            return None;
        }
        ["aria-label", "title", "alt"]
            .into_iter()
            .filter_map(|a| self.dom.attr(id, a))
            .map(str::trim)
            .find(|v| !v.is_empty())
            .map(str::to_owned)
    }

    /// The absolute URL of an `<img>`'s `src`, resolved against the base.
    fn image_src(&self, id: NodeId) -> Option<String> {
        let src = self.dom.attr(id, "src")?.trim();
        if src.is_empty() {
            return None;
        }
        // A `data:` image (e.g. a rewritten inline SVG) keys on the URL itself.
        if src.starts_with("data:") {
            return Some(src.to_string());
        }
        match crate::http::resolve(self.base, src) {
            Link::Http(u) => Some(u.to_string()),
            _ => None,
        }
    }

    /// Place a decoded image as a `W×H` box. `<img>` is `display:inline`
    /// by default, so the box FLOWS with surrounding content and wraps to
    /// the next line when it doesn't fit — a row of thumbnails packs
    /// horizontally and wraps into a grid, rather than stacking. CSS
    /// `display:block` (and flex/grid/table/list-item, where the box is a
    /// block-level child) puts it on its own line. The reserved rows for
    /// its height are emitted by `break_line` from `line_height`.
    ///
    /// The box is the CSS replaced-element used size (`image_used_box`): CSS
    /// `width`/`height`/`aspect-ratio`/`object-fit` and the `<img>` width/height
    /// attributes, falling back to the intrinsic decoded box. `w`/`h` are the
    /// intrinsic cell box (the encode's source).
    fn place_image_box(&mut self, id: NodeId, ctx: &Ctx, url: String, w: u16, h: u16) {
        let avail = self.line_right.saturating_sub(self.line_left).max(1) as u16;
        let (w, h, crop) = self.image_used_box(id, w, h, avail);
        // A `display:block` image is block-level — UNLESS it's the content of
        // an atomic inline box (an `inline-flex`/`inline-block` avatar/icon
        // wrapper), in which case it rides the line like any inline image so a
        // row of avatars flows into a grid instead of a vertical tower.
        let block = matches!(
            self.dom.computed_display(id).as_deref(),
            Some("block" | "flex" | "grid" | "table" | "list-item")
        ) && !self.in_atomic_inline_context(id);
        if block {
            self.flush_block();
        } else {
            // Inline: wrap first if the box won't fit the rest of the line.
            let space = self.pending_space && self.col > self.line_left;
            if self.col + space as usize + w as usize > self.line_right && self.col > self.line_left
            {
                self.break_line();
            }
            // An owed inter-item space becomes a one-cell gap (the renderer
            // fills column gaps; no need to pollute a neighbor's text).
            if self.pending_space && self.col > self.line_left {
                self.col += 1;
            }
            self.pending_space = false;
        }
        self.line.push(Item {
            col: self.col as u16,
            width: w,
            height: h,
            image: Some(url),
            crop,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node: id,
            // A linked image follows its anchor on Enter/click.
            link: ctx.link.clone(),
        });
        self.col += w as usize;
        self.line_height = self.line_height.max(h);
        if block {
            self.break_line();
        } else {
            self.pending_space = true; // a trailing gap after the image
        }
    }

    /// Whether an image is a decorative full-bleed background layer — a
    /// `position:fixed`/`absolute` `<img>` sized to FILL its container with no
    /// definite height (`width:100%`/`min-width:100%`, `height:auto`). A real
    /// browser paints these behind the page; a terminal can't composite layers,
    /// and reserving the image's full pixel height (~48 rows) buries the page's
    /// real content under blank space. We already collapse the out-of-flow
    /// CONTAINER to inline; this extends that compaction to its background image.
    ///
    /// Tight on purpose, so it can't swallow real content: the image (or its
    /// parent) must be out of flow, its used width must fill the whole band
    /// (full-bleed), AND its height must fall through to the intrinsic scale —
    /// no CSS `height` length/`%`, no `aspect-ratio`, no `<img width/height>`.
    /// A sized absolute cover (definite height via the box/ancestor) and any
    /// IN-FLOW full-width image keep their real box.
    fn is_background_layer_image(&self, id: NodeId, iw: u16, ih: u16, avail: u16) -> bool {
        if !(self.is_out_of_flow(id) || self.parent_out_of_flow(id)) {
            return false;
        }
        let (used_w, ..) = self.image_used_box(id, iw, ih, avail);
        if used_w < avail {
            return false; // not full-bleed — a positioned thumbnail/badge
        }
        let raw_h = self.dom.computed_style(id, "height");
        raw_h.as_deref().and_then(css_length_rows).is_none()
            && raw_h.as_deref().and_then(parse_percent).is_none()
            && self.css_aspect_ratio(id).is_none()
            && self.img_attr_ratio(id).is_none()
    }

    /// Whether an out-of-flow image is the FOREGROUND of a definite media frame
    /// — the gallery/lightbox idiom `<div style="aspect-ratio:..;
    /// position:relative"><img style="position:absolute;inset:0"></div>`. Such
    /// an image is real content sized to its (constrained) frame, never a page
    /// backdrop, so the background/backdrop drops must spare it even though it
    /// fills its small box (which would otherwise read as "full-bleed").
    fn framed_foreground(&self, id: NodeId) -> bool {
        self.media_frame(id).is_some()
    }

    /// The constrained positioned ancestor an out-of-flow image is framed by —
    /// the containing block (nearest positioned ancestor) when it declares a
    /// definite box: an `aspect-ratio`, or a definite (non-`100%`) width. `None`
    /// for an in-flow image, or when the containing block is an unconstrained
    /// (viewport-/parent-filling) wrapper — that's a backdrop, not a frame.
    fn media_frame(&self, id: NodeId) -> Option<NodeId> {
        if !self.is_out_of_flow(id) {
            return None;
        }
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..4 {
            let p = cur?;
            if matches!(
                self.dom.computed_style(p, "position").as_deref(),
                Some("relative" | "absolute" | "fixed" | "sticky")
            ) {
                return (self.css_aspect_ratio(p).is_some() || self.has_definite_frame_width(p))
                    .then_some(p);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Whether an element declares a definite, non-`100%` width — a fixed media
    /// frame (`width:300px`, `width:min(100%,1043px)`), as opposed to a
    /// box-filling `width:100%`/`auto` wrapper.
    fn has_definite_frame_width(&self, id: NodeId) -> bool {
        match self.dom.computed_style(id, "width") {
            Some(w) => {
                let w = w.trim();
                !w.eq_ignore_ascii_case("auto")
                    && !w.ends_with('%')
                    && !w.eq_ignore_ascii_case("100vw")
                    && self.css_cells(id, "width").is_some()
            }
            None => false,
        }
    }

    /// Whether an image is a non-compositable BACKDROP layer behind real
    /// content — a cover/fill image filling an out-of-flow overlay (`inset:0` /
    /// `100%×100%`) that sits behind an in-flow content sibling. The blurred-
    /// duplicate gallery/lightbox idiom: a `position:absolute` overlay holds an
    /// `object-fit:cover` copy painted under the sharp `position:relative`
    /// image. A terminal can't composite the blur, so we keep the foreground
    /// (the in-flow sibling) and drop this layer.
    fn is_backdrop_overlay_image(&self, id: NodeId) -> bool {
        // The out-of-flow overlay this image fills (itself if positioned, else
        // its positioned parent — the `<div absolute><img cover>` shape).
        let container = if self.is_out_of_flow(id) {
            id
        } else {
            match self.dom.parent_composed(id) {
                Some(p) if self.is_out_of_flow(p) => p,
                _ => return false,
            }
        };
        // A backdrop spans its whole box (`inset:0` / `100%×100%`)...
        if !self.fills_parent(container) {
            return false;
        }
        // ...reads as a cropped fill layer (not a contained foreground)...
        let cover = matches!(
            self.dom.computed_style(id, "object-fit").as_deref(),
            Some("cover" | "fill")
        ) || self.fills_parent(id);
        if !cover {
            return false;
        }
        // ...and sits BEHIND real foreground content: a sibling of the overlay
        // that is in normal flow and not empty (the sharp image / card body).
        let Some(parent) = self.dom.parent_composed(container) else {
            return false;
        };
        self.dom.children(parent).into_iter().any(|s| {
            s != container
                && matches!(self.dom.node(s).data, NodeData::Element { .. })
                && !self.is_out_of_flow(s)
                && !self.dom.is_hidden(s)
                && !self.is_empty_box(s)
        })
    }

    /// Whether an element is sized to fill its containing block on both axes —
    /// `width`/`height` each `100%`/`100vw`/`100vh`, or both opposite offsets
    /// pinned to zero (`inset:0`).
    fn fills_parent(&self, id: NodeId) -> bool {
        let val = |p: &str| {
            self.dom
                .computed_style(id, p)
                .map(|v| v.trim().to_ascii_lowercase())
        };
        let is_zero = |v: Option<String>| v.is_some_and(|v| is_zero_length(&v));
        let fill_w = matches!(val("width").as_deref(), Some("100%" | "100vw"))
            || (is_zero(val("left")) && is_zero(val("right")));
        let fill_h = matches!(val("height").as_deref(), Some("100%" | "100vh"))
            || (is_zero(val("top")) && is_zero(val("bottom")));
        fill_w && fill_h
    }

    /// The CSS replaced-element used box (cells) for an image, plus whether to
    /// `object-fit: cover` (crop). `iw`/`ih` are the intrinsic decoded cell box,
    /// `avail` the content width it may occupy.
    ///
    /// Width = CSS `width` (length/`%` against the containing block), clamped by
    /// `min-width`/`max-width`, capped at `avail`; else intrinsic. Height = CSS
    /// `height` (a length, vertical units); else from `aspect-ratio`; else, for
    /// a `height:100%` image, the nearest ancestor box with an `aspect-ratio`;
    /// else the `<img width/height>` attribute ratio; else the intrinsic box
    /// scaled to the used width (the previous behaviour, so an image with no CSS
    /// sizing is unchanged). A safety cap bounds a pathological CSS height.
    fn image_used_box(&self, id: NodeId, iw: u16, ih: u16, avail: u16) -> (u16, u16, bool) {
        let avail = avail.max(1) as usize;
        let (iw, ih) = (iw.max(1) as usize, ih.max(1) as usize);

        // Used width. A percentage resolves against the CONTAINING BLOCK — the
        // nearest ancestor with a definite (length) width — not the whole flow
        // box. The avatar/thumbnail idiom (`<a style="width:36px"><img
        // style="width:100%">`) depends on this: the image fills the 36px box,
        // not the column it sits in. Falls back to the flow box (`css_cells`)
        // when no ancestor pins a length width (a genuine full-bleed image).
        let raw_w = self.dom.computed_style(id, "width");
        let mut used_w = match raw_w.as_deref().and_then(parse_percent) {
            // Intrinsic sizing (a flex basis / float / table-cell measurement):
            // a percentage width on a replaced element is treated as `auto` and
            // contributes its intrinsic (decoded) width — CSS Sizing §5.1
            // (percentages on auto-sized boxes are indefinite for min/max-content
            // contributions). Resolving the `%` against the row instead made
            // every `width:100%` image measure to the FULL row width, so a flex
            // row of same-size capsules all reported ~`avail` and the shrink pass
            // then split them by their captions' min-content (a discounted
            // two-price capsule got a wider column → wider image). Using the
            // intrinsic width makes those columns equal.
            Some(_) if self.measuring => iw,
            Some(pct) => {
                let basis = self.definite_ancestor_width(id).unwrap_or(avail);
                (pct * basis as f32).round().max(1.0) as usize
            }
            None => self
                .css_cells(id, "width")
                .or_else(|| {
                    self.img_attr_px(id, "width")
                        .map(|px| (px / 8.0).round().max(1.0) as usize)
                })
                .unwrap_or(iw),
        };
        if let Some(mn) = self.css_cells(id, "min-width") {
            used_w = used_w.max(mn);
        }
        if let Some(mx) = self.css_cells(id, "max-width") {
            used_w = used_w.min(mx);
        }
        let used_w = used_w.min(avail).max(1);

        // Used height (rows).
        let raw_h = self.dom.computed_style(id, "height");
        let intrinsic_h = (ih * used_w / iw).max(1);
        let used_h = if let Some(h) = raw_h.as_deref().and_then(css_length_rows) {
            h
        } else if let Some(ar) = self.css_aspect_ratio(id) {
            rows_for_ratio(used_w, ar)
        } else if let Some(pct) = raw_h.as_deref().and_then(parse_percent) {
            // A percentage height resolves against the containing block.
            // Priority: (1) the intrinsic-ratio "aspect box" — a height:0
            // container whose percentage `padding-bottom` (resolved against
            // WIDTH per CSS 2.1 §8.4) establishes the box for an absolutely
            // positioned `height:100%` child to fill (the universal responsive
            // image/thumbnail idiom — Humble Bundle's tiles, padding-bottom
            // hacks everywhere); then (2) a definite-height ancestor (the
            // avatar wrapper's `height:24px`); then (3) a square-tile
            // `aspect-ratio` ancestor; else the intrinsic box.
            if let Some(rows) = self.intrinsic_ratio_container_rows(id, used_w) {
                (pct * rows as f32).round().max(1.0) as usize
            } else if let Some(basis) = self.definite_ancestor_height(id) {
                (pct * basis as f32).round().max(1.0) as usize
            } else {
                self.container_box_rows(id, used_w).unwrap_or(intrinsic_h)
            }
        } else if let Some(ar) = self.img_attr_ratio(id) {
            rows_for_ratio(used_w, ar)
        } else if let Some(px) = self.img_attr_px(id, "height") {
            // `<img height=N>` alone (no matching width attr to form a ratio):
            // the presentation-hint height in rows.
            (px / 16.0).round().max(1.0) as usize
        } else {
            intrinsic_h
        };
        let used_h = used_h.clamp(1, IMG_CSS_MAX_ROWS);

        let object_fit = self.dom.computed_style(id, "object-fit");
        let crop = object_fit.as_deref() == Some("cover");
        // `object-fit: contain` fits the image INSIDE its box preserving
        // aspect, letterboxing the slack. Our renderer never UPSCALES
        // (`Fit`, deliberate), so a box larger than the decoded image just
        // reserves blank rows beneath it — a gap before whatever follows.
        // (archive.org collection tiles set a tall `height` on the cover's
        // box, so the title printed a half-dozen blank rows below the cover.)
        // Reserve exactly what gets drawn: the decoded box scaled to fit,
        // never up — no wasted letterbox in a terminal. `cover` (crop) and
        // the default `fill` keep the author's box.
        if object_fit.as_deref() == Some("contain") {
            let scale = (used_w as f32 / iw as f32)
                .min(used_h as f32 / ih as f32)
                .min(1.0);
            let fit_w = ((iw as f32) * scale).round().max(1.0) as usize;
            let fit_h = ((ih as f32) * scale).round().max(1.0) as usize;
            return (fit_w as u16, fit_h as u16, crop);
        }
        // The renderer always FITS the decoded image into this box (never
        // upscales or stretches), so reserving MORE rows than it actually draws
        // at `used_w` just leaves blank rows beneath it — a gap before the next
        // line. The attr / `aspect-ratio` height is a CSS-pixel ratio that
        // assumes a nominal 2:1 cell, but the DECODED box carries the terminal's
        // real cell aspect: on a non-2:1 font (e.g. foot) a `height:auto`
        // square thumbnail then over-reserves a row, printing a black gap
        // between the image and its caption. Cap to what's drawn — the decoded
        // box scaled to the used width. `cover` (crop) fills its taller box by
        // cropping, so it keeps the author box.
        let used_h = if crop {
            used_h
        } else {
            used_h.min(intrinsic_h)
        };
        (used_w as u16, used_h as u16, crop)
    }

    /// The `aspect-ratio` (width÷height) computed for an element, or `None`
    /// for `auto`/unset/unparseable. Accepts `R`, `W / H`, and the
    /// `auto W / H` form (the `auto` keyword is ignored).
    fn css_aspect_ratio(&self, id: NodeId) -> Option<f32> {
        parse_ratio(&self.dom.computed_style(id, "aspect-ratio")?)
    }

    /// An `<img width=N>` / `<img height=N>` presentation-hint attribute as a
    /// CSS pixel length. The HTML spec maps these unitless content attributes
    /// to the `width`/`height` properties at the lowest cascade priority, so
    /// they size a replaced element only when no CSS (`style`/sheet) sets it.
    /// Honoring them is what gives a bare `<img width="64">` (GitHub's
    /// achievement badge) its 64px box instead of decoding at intrinsic size.
    fn img_attr_px(&self, id: NodeId, attr: &str) -> Option<f32> {
        let v = self.dom.attr(id, attr)?;
        let n: f32 = v.trim().parse().ok()?;
        (n > 0.0).then_some(n)
    }

    /// The intrinsic ratio from the `<img width=… height=…>` presentation
    /// attributes (browsers derive a default `aspect-ratio` from them to avoid
    /// layout shift). Unitless integers only.
    fn img_attr_ratio(&self, id: NodeId) -> Option<f32> {
        let w: f32 = self.dom.attr(id, "width")?.trim().parse().ok()?;
        let h: f32 = self.dom.attr(id, "height")?.trim().parse().ok()?;
        (w > 0.0 && h > 0.0).then_some(w / h)
    }

    /// Height in rows of the nearest ancestor that establishes a definite box
    /// via `aspect-ratio` — for a `height:100%` image filling a sized container
    /// (e.g. a square tile: `aspect-ratio:1` wrapper, `img{width/height:100%}`).
    /// The ancestor's width is the image's used width (it fills the cell).
    fn container_box_rows(&self, id: NodeId, used_w: usize) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..6 {
            let p = cur?;
            if let Some(ar) = self.css_aspect_ratio(p) {
                return Some(rows_for_ratio(used_w, ar));
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Rows of the nearest "intrinsic-ratio" container — the responsive-image
    /// idiom of a `height:0` box whose percentage `padding-bottom` (and/or
    /// `padding-top`) reserves the height. CSS 2.1 §8.4: percentage padding
    /// resolves against the containing block's WIDTH, so `padding-bottom:56.25%`
    /// on a full-width box reserves a 16:9 height. The box collapses the content
    /// height to 0 and an absolutely positioned `width/height:100%` child fills
    /// the padding box. Without this every such thumbnail renders as a 1-row
    /// strip (Humble Bundle's tiles; the technique predates `aspect-ratio` and
    /// is still ubiquitous). Only a height:0/auto container qualifies — a real
    /// fixed-height padded box keeps its own height. `used_w` is the image's
    /// used width (it fills 100% of the container).
    fn intrinsic_ratio_container_rows(&self, id: NodeId, used_w: usize) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..6 {
            let p = cur?;
            let h_zero = match self.dom.computed_style(p, "height").as_deref() {
                Some(h) => css_length_em(h) == Some(0.0) || h.trim() == "auto",
                None => true,
            };
            if h_zero {
                let frac: f32 = ["padding-bottom", "padding-top"]
                    .iter()
                    .filter_map(|prop| first_percent(self.dom.computed_style(p, prop).as_deref()?))
                    .sum();
                if frac > 0.0 {
                    // height_px = frac · width_px, so the box ratio is 1/frac.
                    return Some(rows_for_ratio(used_w, 1.0 / frac));
                }
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Content width (cells) of the nearest ancestor establishing a definite
    /// (length, non-percentage) `width` — the containing block for a
    /// percentage width on `id`. The avatar/thumbnail idiom (`<a
    /// style="width:36px"><img style="width:100%">`) is why this exists: the
    /// image fills the 36px box, not the whole flow column it sits in. `None`
    /// when no ancestor pins a length width — the caller then falls back to the
    /// flow box (the prior behaviour, correct for a genuine full-bleed image).
    fn definite_ancestor_width(&self, id: NodeId) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..8 {
            let p = cur?;
            if let Some(em) = self
                .dom
                .computed_style(p, "width")
                .as_deref()
                .and_then(css_length_em)
            {
                return Some((em * 2.0).round().max(1.0) as usize);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Height (rows) of the nearest ancestor establishing a definite (length,
    /// non-percentage) `height` — the containing block for a percentage height
    /// on `id` (the avatar wrapper's `height:24px`). `None` when none does.
    fn definite_ancestor_height(&self, id: NodeId) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        for _ in 0..8 {
            let p = cur?;
            if let Some(rows) = self
                .dom
                .computed_style(p, "height")
                .as_deref()
                .and_then(css_length_rows)
            {
                return Some(rows);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Flow a form control. A control known to the form extraction (in
    /// `controls`) becomes a selectable `Link::Form` widget showing the
    /// field's current value; anything else falls back to a plain stub.
    fn flow_form_control(&mut self, id: NodeId, tag: &str, ambient: Option<Link>) {
        if let Some(&(form, field)) = self.controls.get(&id) {
            let label = self.field_label(form, field);
            if label.is_empty() {
                return; // hidden control: no widget
            }
            self.place_atom(label, ItemKind::Form, id, Some(Link::Form { form, field }));
            return;
        }
        self.place_form_stub(id, tag, ambient);
    }

    /// The widget label for a `(form, field)` (`Field::row_label`), empty
    /// for hidden fields or out-of-range indices.
    fn field_label(&self, form: usize, field: usize) -> String {
        self.forms
            .get(form)
            .and_then(|f| f.fields.get(field))
            .map(|f| f.row_label())
            .unwrap_or_default()
    }

    /// A stub for a control we couldn't bind to a form (e.g. one outside
    /// any `<form>`), keeping the page readable. When the control is wrapped
    /// in a living-page click marker (`ambient` is its `Link::JsClick`) — a
    /// React/Vue `<button onClick>` is the common case — the stub adopts that
    /// link so it stays selectable and the click reaches the engine. Without
    /// an ambient link it's an inert placeholder, as before.
    fn place_form_stub(&mut self, id: NodeId, tag: &str, ambient: Option<Link>) {
        if self.dom.is_hidden(id) {
            return;
        }
        let kind = self.dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
        if tag == "input" && kind == "hidden" {
            return;
        }
        let stub = match tag {
            "button" => {
                // The label is the button's VISIBLE text — never the SVG
                // `<title>`/`<desc>` screen-reader metadata its `textContent`
                // includes (the archive.org login icon dumped "User icon An
                // illustration of a person's head and chest.").
                let text = self.rendered_text(id);
                let text = text.trim();
                if text.is_empty() {
                    // Icon-only button (its content is an `<svg>`/icon we don't
                    // rasterize): surface its accessible name (`aria-label`/
                    // `title`) so it stays a short clickable token rather than
                    // vanishing. None (an unnamed disclosure trigger) → the
                    // serializer's clickable handle covers it; no stub.
                    match self.icon_only_label(id) {
                        Some(name) => format!("[ {name} ]"),
                        None => return,
                    }
                } else {
                    format!("[ {text} ]")
                }
            }
            "select" => "[ select ▾ ]".to_owned(),
            "textarea" => "[ textarea ]".to_owned(),
            _ if kind == "submit" || kind == "button" => {
                let label = self.dom.attr(id, "value").unwrap_or("Submit");
                format!("[ {label} ]")
            }
            _ if kind == "checkbox" => "[ ]".to_owned(),
            _ if kind == "radio" => "( )".to_owned(),
            _ => {
                let hint = self
                    .dom
                    .attr(id, "placeholder")
                    .or_else(|| self.dom.attr(id, "value"))
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                format!("[{hint}]")
            }
        };
        self.place_atom(stub, ItemKind::Form, id, ambient);
    }

    /// Place a single unbreakable item (a form widget), with the same
    /// inter-item spacing/wrapping rules as a word but kept as one unit.
    fn place_atom(&mut self, text: String, kind: ItemKind, node: NodeId, link: Option<Link>) {
        let len = display_width(&text);
        let space = self.pending_space && self.col > self.line_left;
        if self.col + space as usize + len > self.line_right && self.col > self.line_left {
            self.break_line();
        }
        let space = self.pending_space && self.col > self.line_left;
        self.pending_space = false;
        if space {
            if let Some(last) = self.line.last_mut() {
                last.text.push(' ');
                last.width += 1;
            }
            self.col += 1;
        }
        self.push_item(text, len, kind, Emphasis::default(), node, link);
        self.pending_space = true; // a widget gets a trailing gap
    }

    /// The marker text for a list item: the cascaded `list-style-type`
    /// (inherited from the `<ul>`/`<ol>`, UA-defaulted by tag/depth) rendered
    /// against the current level's counter — a bullet glyph, a formatted
    /// number/letter/roman numeral, or nothing for `none`. `<li value=N>`
    /// resets the counter; the counter always advances so a mixed list still
    /// numbers correctly.
    fn next_list_marker(&mut self, li: NodeId) -> String {
        if let Some(v) = self
            .dom
            .attr(li, "value")
            .and_then(|s| s.trim().parse::<u32>().ok())
            && let Some(c) = self.list_stack.last_mut()
        {
            *c = v;
        }
        let counter = self.list_stack.last().copied().unwrap_or(1);
        if let Some(c) = self.list_stack.last_mut() {
            *c = c.saturating_add(1);
        }
        let kind = self.dom.computed_value(li, "list-style-type");
        format_list_marker(kind.as_deref().unwrap_or("disc"), counter)
    }

    /// Place a deferred list marker in the gutter (`col`) on the item's
    /// first content row — the first row at/after `from` carrying real
    /// (non-spacer) content. A markerless empty item places nothing.
    fn place_list_marker(&mut self, marker: &str, from: usize, col: usize) {
        let Some(row) = (from..self.rows.len()).find(|&r| {
            self.rows[r]
                .items
                .iter()
                .any(|it| !it.text.is_empty() || it.image.is_some())
        }) else {
            return;
        };
        let item = Item {
            col: col as u16,
            width: display_width(marker) as u16,
            height: 1,
            text: marker.to_owned(),
            kind: ItemKind::Text,
            image: None,
            crop: false,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
        };
        let items = &mut self.rows[row].items;
        items.push(item);
        items.sort_by_key(|it| it.col);
    }

    fn push_rule(&mut self) {
        let dashes = "─".repeat(self.line_right.saturating_sub(self.line_left).min(40));
        let len = display_width(&dashes);
        self.push_item(
            dashes,
            len,
            ItemKind::Text,
            Emphasis::default(),
            NO_NODE,
            None,
        );
        self.break_line();
    }

    fn push_item(
        &mut self,
        text: String,
        width: usize,
        kind: ItemKind,
        emph: Emphasis,
        node: NodeId,
        link: Option<Link>,
    ) {
        self.line.push(Item {
            col: self.col as u16,
            width: width as u16,
            height: 1,
            image: None,
            crop: false,
            text,
            kind,
            emph,
            node,
            link,
        });
        self.col += width;
    }

    /// End the current line, pushing it as a row. When an inline image
    /// rode the line (`line_height > 1`), reserve the rest of its vertical
    /// box with zero-width spacer rows so later text flows beneath the
    /// pixels (not through them).
    fn break_line(&mut self) {
        let mut items = std::mem::take(&mut self.line);
        // Preserved-whitespace rows (pre/pre-wrap) keep their own columns;
        // everything else honors the inherited text-align.
        if self.ws.collapses_spaces() {
            self.align_row(&mut items);
        }
        let band = self.line_height;
        let spacer_indent = self.line_left;
        self.rows.push(Row { items });
        self.pending_space = false;
        self.line_height = 1;
        // Spacer rows carry a (non-empty) marker so `finish`'s blank-row
        // collapse leaves them intact; the renderer draws nothing for them
        // (the image overdraws the box) and selection skips them (no link).
        for _ in 1..band {
            self.rows.push(image_spacer_row(spacer_indent));
        }
        // A float we've now scrolled past gets blitted into its rows; then
        // recompute the band/cursor for the next line.
        self.resolve_floats();
        self.begin_line();
    }

    /// Offset a finished row's items to honor center/right `text-align`.
    /// The shift is computed within the current line's content band,
    /// ignoring a trailing space on the last item.
    fn align_row(&self, items: &mut [Item]) {
        // When measuring intrinsic width (for a flex basis / float box), the
        // alignment offset must NOT count — a centered/right-aligned run's
        // content is as wide left-packed, and the offset would inflate the
        // measured width (spreading flex items whose labels are centered).
        if self.measuring || self.align == Align::Left {
            return;
        }
        let Some(last) = items.last() else { return };
        let trailing = last.text.ends_with(' ') as u16;
        let used = (last.col + last.width).saturating_sub(trailing) as usize;
        let free = self.line_right.saturating_sub(used);
        if free == 0 {
            return;
        }
        let offset = match self.align {
            Align::Center => free / 2,
            Align::Right => free,
            Align::Left => 0,
        } as u16;
        for it in items {
            it.col += offset;
        }
    }

    /// End the current line only if it has content (block boundary).
    fn flush_block(&mut self) {
        if !self.line.is_empty() {
            self.break_line();
        }
        self.pending_space = false;
    }

    fn push_blank(&mut self) {
        self.rows.push(Row::default());
    }

    /// Collapse runs of blank rows and trim leading/trailing blanks, and
    /// remap the carousels' row spans through the collapse. Carousels are
    /// recorded with absolute row indices during flow; this is where those
    /// indices get rebased to the final (collapsed) row grid, so a band
    /// stays aligned with its cards no matter how many blank rows above or
    /// inside it were dropped. Without this remap the band drifts off its
    /// cards and the view stops clipping the strip.
    fn finish(mut self) -> (Vec<Row>, Vec<Carousel>, ElementTops) {
        let carousels = std::mem::take(&mut self.carousels);
        let element_tops = std::mem::take(&mut self.element_tops);
        let n = self.rows.len();
        // remap[i] = new index of old row i (for a dropped blank, the index
        // the next kept row takes). remap[n] = total kept rows, for an
        // exclusive `end` that points one past the last row.
        let mut remap = vec![0usize; n + 1];
        let mut out: Vec<Row> = Vec::with_capacity(n);
        for (i, row) in self.rows.into_iter().enumerate() {
            remap[i] = out.len();
            if row.items.is_empty() && out.last().is_none_or(|r| r.items.is_empty()) {
                continue;
            }
            out.push(row);
        }
        remap[n] = out.len();
        while out.last().is_some_and(|r| r.items.is_empty()) {
            out.pop();
        }
        let carousels = carousels
            .into_iter()
            .map(|mut c| {
                c.start = remap[c.start.min(n)];
                c.end = remap[c.end.min(n)].min(out.len());
                c
            })
            .collect();
        // Remap each recorded element top through the SAME blank-row collapse,
        // so a measure pass reads the empty element's row in the kept-row grid
        // (its cell-bearing neighbours live there too). A row at or past the
        // laid content (an out-of-flow box placed beyond the flow — an
        // infinite-scroll sentinel pinned below the loaded tiles) has no blank
        // rows out there to collapse, so keep its overshoot past the last kept
        // row rather than clamping it onto the content bottom (which would drag
        // a far-below sentinel up into the viewport and make it falsely
        // intersect).
        let element_tops = element_tops
            .into_iter()
            .map(|(id, (col, row))| {
                let r_in = row as usize;
                let mapped = if r_in >= n {
                    out.len() + (r_in - n)
                } else {
                    remap[r_in]
                };
                (id, (col, u16::try_from(mapped).unwrap_or(u16::MAX)))
            })
            .collect();
        (out, carousels, element_tops)
    }
}

/// Two inline boxes belong to the same run if they share kind, source
/// node, and link target (so consecutive words coalesce into one item).
/// A zero-width spacer row reserving one cell-row of an image box. Carries
/// a marker item (not empty) so `finish`'s blank-row collapse keeps it,
/// but renders nothing and is never selectable.
fn image_spacer_row(indent: usize) -> Row {
    Row {
        items: vec![Item {
            col: indent as u16,
            width: 0,
            height: 1,
            image: None,
            crop: false,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
        }],
    }
}

fn same_run(item: &Item, ctx: &Ctx) -> bool {
    item.kind == ctx.kind && item.emph == ctx.emph && item.node == ctx.node && item.link == ctx.link
}

/// Pad a flex-grown text/search input's `[…]` widget to fill its allocated
/// box `cw`, so a grown input reads as a wide input field instead of a short
/// placeholder with a long trailing gap (how a browser draws an input that
/// `flex-grow` stretches). Only a lone `[…]` text widget fills — buttons
/// (`[ … ]`) and selects (`… ▾]`) shouldn't stretch, nor multi-item boxes.
fn fill_input_box(b: &mut LaidBox, cw: usize) {
    if b.rows.len() != 1 {
        return;
    }
    let [it] = b.rows[0].items.as_mut_slice() else {
        return;
    };
    if it.kind != ItemKind::Form
        || !it.text.starts_with('[')
        || !it.text.ends_with(']')
        || it.text.starts_with("[ ") // a button "[ Submit ]"
        || it.text.ends_with("▾]")
    // a select "[… ▾]"
    {
        return;
    }
    let cur = it.width as usize;
    if cw <= cur {
        return;
    }
    it.text.insert_str(it.text.len() - 1, &" ".repeat(cw - cur));
    it.width = cw as u16;
    b.width = cw as u16;
}

/// A carousel scroll control's direction: −1 toward the start (prev/left),
/// +1 toward the end (next/right), or `None` when the element shows no
/// (or a contradictory) prev/next signal. Reads the universal carousel
/// vocabulary from `aria-label`/`class`/`id`/`title`/`rel` and from the
/// control's own arrow glyph — the same `prev`/`next` meaning CSS
/// `::scroll-button(left|right)` encodes — so it generalizes across sites.
fn scroll_control_dir(dom: &Dom, id: NodeId) -> Option<i8> {
    let mut attrs = String::new();
    for a in ["aria-label", "class", "id", "title", "rel"] {
        if let Some(v) = dom.attr(id, a) {
            attrs.push_str(&v.to_ascii_lowercase());
            attrs.push(' ');
        }
    }
    let text = dom.text_content(id);
    let prev = attrs.contains("prev") || text.chars().any(is_prev_glyph);
    let next = attrs.contains("next") || text.chars().any(is_next_glyph);
    match (prev, next) {
        (true, false) => Some(-1),
        (false, true) => Some(1),
        _ => None, // no signal, or both (ambiguous)
    }
}

/// Left/back arrow glyphs a prev control commonly renders.
fn is_prev_glyph(c: char) -> bool {
    matches!(c, '«' | '‹' | '❮' | '◀' | '◁' | '←' | '⟨' | '⟪' | '🡠')
}

/// Right/forward arrow glyphs a next control commonly renders.
fn is_next_glyph(c: char) -> bool {
    matches!(c, '»' | '›' | '❯' | '▶' | '▷' | '→' | '⟩' | '⟫' | '🡢')
}

/// A CSS `font-weight` value reads as bold (`bold`/`bolder`/≥600).
fn css_is_bold(value: &str) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "bold" | "bolder" => true,
        "normal" | "lighter" => false,
        n => n.parse::<u32>().is_ok_and(|w| w >= 600),
    }
}

/// A CSS `font-style` value reads as italic (`italic`/`oblique`).
fn css_is_italic(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "italic" | "oblique"
    )
}

/// Safety backstop on a CSS-forced image height (rows). Well-built pages
/// constrain images with a sized box, but a pathological `height: 5000px`
/// shouldn't reserve hundreds of rows and wreck scroll/selection — this is a
/// guard, not a layout cap (cf. `IMG_MAX_CELLS` for intrinsic boxes).
const IMG_CSS_MAX_ROWS: usize = 48;

/// A vertical CSS length as terminal rows. The cell is ~2:1 (col:row), so 1em
/// ≈ 1 row (vs ≈ 2 cols horizontally) and 1px ≈ 1/16 row. `%`/`auto`/`vh`
/// return `None` (a `100%`/`%` height resolves against a container instead).
fn css_length_rows(value: &str) -> Option<usize> {
    css_length_em(value).map(|em| em.round().max(1.0) as usize)
}

/// A CSS percentage (`"75%"`) as a fraction (`0.75`); `None` for any other
/// value. The one place a `%` becomes a multiplier for containing-block sizing.
fn parse_percent(value: &str) -> Option<f32> {
    let n: f32 = value.trim().strip_suffix('%')?.trim().parse().ok()?;
    Some(n / 100.0)
}

/// Build a table's cell grid (CSS 2.1 §17.5 / the HTML cell-placement
/// algorithm): walk each row's cells left→right, skipping slots already
/// claimed by a `rowspan` from above, and record each cell's resolved
/// top-left coordinate and span. Returns the placed cells and the column
/// count. `rows` is one inner vec of cell ids per table row, in visual order.
fn build_table_grid(layout: &Layout, rows: &[Vec<NodeId>]) -> (Vec<TableCell>, usize) {
    let mut cells = Vec::new();
    let mut ncols = 0usize;
    // Slots occupied by a rowspan reaching down from an earlier row.
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for (r, row) in rows.iter().enumerate() {
        let mut c = 0usize;
        for &cell in row {
            while occupied.contains(&(r, c)) {
                c += 1;
            }
            let colspan = layout.cell_span(cell, "colspan");
            let rowspan = layout.cell_span(cell, "rowspan");
            for rr in r..r + rowspan {
                for cc in c..c + colspan {
                    occupied.insert((rr, cc));
                }
            }
            cells.push(TableCell {
                id: cell,
                row: r,
                col: c,
                rowspan,
                colspan,
            });
            ncols = ncols.max(c + colspan);
            c += colspan;
        }
    }
    (cells, ncols)
}

/// Raise the widths in `slice` so their sum is at least `need`, adding the
/// deficit in approximately equal parts (CSS 2.1 §17.5.2.2 step 3 — widen all
/// spanned columns by roughly the same amount).
fn distribute_deficit(slice: &mut [usize], need: usize) {
    let n = slice.len();
    if n == 0 {
        return;
    }
    let have: usize = slice.iter().sum();
    if need <= have {
        return;
    }
    let extra = need - have;
    let base = extra / n;
    let mut rem = extra % n;
    for w in slice.iter_mut() {
        *w += base + usize::from(rem > 0);
        rem = rem.saturating_sub(1);
    }
}

/// Grow the listed `cols` of `target` by `extra` cells total, in proportion to
/// each column's `weight` (the largest residual takes the rounding remainder).
fn grow_by_weight(
    target: &mut [usize],
    cols: &[usize],
    extra: usize,
    weight: impl Fn(usize) -> usize,
    total_weight: usize,
) {
    if extra == 0 || cols.is_empty() || total_weight == 0 {
        return;
    }
    let mut given = 0usize;
    for &c in cols {
        let share = extra * weight(c) / total_weight;
        target[c] += share;
        given += share;
    }
    // Hand the rounding remainder to the widest-weighted column.
    let mut left = extra - given;
    while left > 0 {
        if let Some(&c) = cols.iter().max_by_key(|&&c| weight(c)) {
            target[c] += 1;
        }
        left -= 1;
    }
}

/// A positioning offset/size as a fraction of the containing block: a `%` →
/// fraction, a zero length → `0.0`, anything else (a non-zero length, `auto`,
/// `calc(…)`) → `None`. Used by `axis_visible_fraction` to resolve an
/// out-of-flow box's rect without pixel geometry; `None` keeps the box (we
/// never drop on a span we can't pin down). See `is_clipped_offscreen`.
fn css_axis_fraction(value: &str) -> Option<f32> {
    parse_percent(value).or_else(|| is_zero_length(value).then_some(0.0))
}

/// Below this visible fraction (i.e. ≥75% clipped by its containing block) an
/// out-of-flow box is treated as off-screen and dropped — a browser would paint
/// only a useless sliver of the next carousel page / off-canvas panel, which a
/// line model can't render as a fractional slice anyway. See
/// `is_clipped_offscreen`.
const OFFSCREEN_VISIBLE_MIN: f32 = 0.25;

/// The first percentage anywhere in a value, as a fraction — including inside
/// `calc(…)`. The aspect-box padding is frequently authored as
/// `calc(57.305% + 0em)` (a `% + 0` to defeat some preprocessor), so a strict
/// `strip_suffix('%')` misses it. Scans for a `%` and reads the number before
/// it. Used only for the responsive-padding ratio, where the value is a single
/// length whose `%` term is the ratio.
fn first_percent(value: &str) -> Option<f32> {
    let pct = value.find('%')?;
    let start = value[..pct]
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map_or(0, |i| i + 1);
    let n: f32 = value[start..pct].parse().ok()?;
    Some(n / 100.0)
}

/// Parse a CSS `aspect-ratio` to width÷height. `R`, `W / H`, and `auto W / H`
/// (the `auto` keyword ignored); `auto`/unset/zero/unparseable → `None`.
fn parse_ratio(value: &str) -> Option<f32> {
    let v = value.trim().trim_start_matches("auto").trim();
    if v.is_empty() {
        return None;
    }
    let ratio = if let Some((a, b)) = v.split_once('/') {
        a.trim().parse::<f32>().ok()? / b.trim().parse::<f32>().ok()?
    } else {
        v.parse::<f32>().ok()?
    };
    (ratio.is_finite() && ratio > 0.0).then_some(ratio)
}

/// Rows for a box `width_cols` wide at pixel aspect `ratio` (width÷height).
/// With the nominal 8×16 cell: `height_px = width_px / ratio`, so
/// `rows = cols / (2·ratio)` (a 1:1 box is half as many rows as columns).
fn rows_for_ratio(width_cols: usize, ratio: f32) -> usize {
    (width_cols as f32 / (2.0 * ratio)).round().max(1.0) as usize
}

/// A context-free CSS length as an em-equivalent (≈ one em ≈ 2 text cells).
/// `16px≈1em`, `12pt≈1em`, `1ch≈half an em` (one cell), unitless treated as
/// px; the single place absolute units are understood. Context-dependent
/// values (`%`/`vw`/`calc()`/`auto`) → `None` here — they go through
/// `resolve_cells`, which knows the containing block and the viewport.
fn css_length_em(value: &str) -> Option<f32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    match v[split..].trim() {
        "em" | "rem" => Some(n),
        "px" | "" => Some(n / 16.0),
        "pt" => Some(n / 12.0),
        // 1ch is the advance of "0" — a single cell in a monospace terminal,
        // i.e. half our 2-cells-per-em density. The natural terminal unit.
        "ch" => Some(n / 2.0),
        _ => None,
    }
}

/// A CSS horizontal length, percentage, `vw`, or `calc()` as a count of
/// cells (f32, for `calc` arithmetic). `%` resolves against `avail` (the
/// containing block's content width), `vw` against `viewport` (the full
/// terminal width), absolute units via `css_length_em` (≈ 2 cells/em).
/// `None` for `auto` and units we don't resolve here (`vh`/`vmin`/… need a
/// viewport height the terminal layout doesn't carry). This is the single
/// contextual length resolver.
/// Which CSS math comparison function to fold over its arguments.
enum Fold {
    Min,
    Max,
    Clamp,
}

/// Split a comma-separated argument list, respecting nested parentheses (so a
/// `min(100%, calc(50% + 2px))` keeps the inner comma-free calc intact). Used
/// for the `min()`/`max()`/`clamp()` argument lists.
fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Split a `grid-template-*` value into its whitespace-separated track tokens,
/// keeping a parenthesised group (`minmax(a, b)`, `repeat(2, 1fr)`,
/// `fit-content(20%)`) intact and dropping `[line-name]` groups (we don't
/// place by named lines).
fn split_track_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_names = false;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '[' => in_names = true,
            ']' => in_names = false,
            _ if in_names => {}
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether the `rspan × cspan` block of grid cells at `(r, c)` is unoccupied
/// (and fits within `ncols`). Rows beyond the current occupancy are empty.
fn grid_cells_free(
    occ: &[Vec<bool>],
    r: usize,
    c: usize,
    rspan: usize,
    cspan: usize,
    ncols: usize,
) -> bool {
    if c + cspan > ncols {
        return false;
    }
    for row in occ.iter().skip(r).take(rspan) {
        if row[c..c + cspan].iter().any(|&b| b) {
            return false;
        }
    }
    true
}

/// Mark the `rspan × cspan` block of grid cells at `(r, c)` occupied, growing
/// the occupancy grid as needed.
fn grid_mark(
    occ: &mut Vec<Vec<bool>>,
    r: usize,
    c: usize,
    rspan: usize,
    cspan: usize,
    ncols: usize,
) {
    for rr in r..r + rspan {
        while occ.len() <= rr {
            occ.push(vec![false; ncols]);
        }
        for cell in occ[rr][c..(c + cspan).min(ncols)].iter_mut() {
            *cell = true;
        }
    }
}

fn resolve_cells_f32(value: &str, avail: usize, viewport: usize) -> Option<f32> {
    let v = value.trim();
    // `var(--name, fallback)`: stylesheets are dropped before layout, so a
    // referenced custom property is (almost always) undefined here — the
    // spec-correct result is then the fallback, which is also the common case
    // for sizing (`min-width: var(--cell, 16rem)`). Resolve the fallback;
    // a `var()` with no fallback is unresolvable. Nested `var()`/`calc()` in
    // the fallback resolve recursively.
    if let Some(inner) = v
        .strip_prefix("var(")
        .or_else(|| v.strip_prefix("VAR("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let fallback = inner.split_once(',')?.1.trim();
        return resolve_cells_f32(fallback, avail, viewport);
    }
    if let Some(inner) = v
        .strip_prefix("calc(")
        .or_else(|| v.strip_prefix("CALC("))
        .and_then(|r| r.strip_suffix(')'))
    {
        return resolve_calc(inner, avail, viewport);
    }
    // `min()`/`max()`/`clamp()` — the modern responsive sizing functions
    // (`width: min(100%, 1043px)` = "fill the container but cap at 1043px").
    // Comma-separated arguments, each a length/percentage/calc resolved by us;
    // an unresolvable argument is skipped (so a `min(100% - 5px, 200px)` whose
    // bare-math arg we can't parse still yields the other bound, rather than
    // dropping the whole value as it did before these were understood at all).
    let lower = v.to_ascii_lowercase();
    for (name, fold) in [
        ("min(", Fold::Min),
        ("max(", Fold::Max),
        ("clamp(", Fold::Clamp),
    ] {
        if lower.starts_with(name)
            && let Some(inner) = v.get(name.len()..).and_then(|r| r.strip_suffix(')'))
        {
            let args: Vec<f32> = split_args(inner)
                .into_iter()
                .filter_map(|a| resolve_cells_f32(a.trim(), avail, viewport))
                .collect();
            return match fold {
                Fold::Min => args.into_iter().reduce(f32::min),
                Fold::Max => args.into_iter().reduce(f32::max),
                // clamp(MIN, VAL, MAX) = max(MIN, min(VAL, MAX)); degrade
                // gracefully if a bound was unresolvable.
                Fold::Clamp => match args.as_slice() {
                    [lo, val, hi] => Some(val.clamp(*lo, *hi)),
                    [a, b] => Some(a.max(*b)),
                    [a] => Some(*a),
                    _ => None,
                },
            };
        }
    }
    if let Some(p) = v.strip_suffix('%') {
        let pct: f32 = p.trim().parse().ok()?;
        return Some((pct / 100.0) * avail as f32);
    }
    if let Some(n) = v
        .strip_suffix("vw")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some((n / 100.0) * viewport as f32);
    }
    css_length_em(v).map(|em| em * 2.0)
}

/// `resolve_cells_f32` rounded to whole cells (never negative).
fn resolve_cells(value: &str, avail: usize, viewport: usize) -> Option<usize> {
    resolve_cells_f32(value, avail, viewport).map(|c| c.round().max(0.0) as usize)
}

/// A `calc()` body as cells. A real expression evaluator: `+ - * /` with the
/// correct precedence (`* /` bind tighter than `+ -`) and parenthesised
/// grouping. A value carrying a unit/`%`/`vw` resolves to cells via
/// `resolve_cells_f32`; a UNITLESS number is a scalar (per the CSS calc
/// grammar — `3` is a number, only `3px` is a length), so
/// `calc((100% - 3.75em) / 3)` is a third of the container, the ubiquitous
/// 3-column flex/grid item width (Humble Bundle's bundle item grid). Returns
/// `None` on a parse failure or an unresolvable term, so the caller ignores
/// the value as it did before `calc` was understood at all.
fn resolve_calc(body: &str, avail: usize, viewport: usize) -> Option<f32> {
    let mut p = CalcParser {
        s: body.as_bytes(),
        pos: 0,
        avail,
        viewport,
    };
    let v = p.sum()?;
    p.skip_ws();
    (p.pos == p.s.len()).then_some(v)
}

/// Recursive-descent evaluator for a `calc()` body (`sum := product ((+|-)
/// product)*`, `product := value ((*|/) value)*`, `value := (sum) | number |
/// length`). CSS requires whitespace around `+`/`-` (to disambiguate signed
/// numbers); `*`/`/` need none, so a value token ends at a top-level space,
/// `*`, `/`, or `)`.
struct CalcParser<'a> {
    s: &'a [u8],
    pos: usize,
    avail: usize,
    viewport: usize,
}

impl CalcParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos] == b' ' {
            self.pos += 1;
        }
    }

    fn sum(&mut self) -> Option<f32> {
        let mut acc = self.product()?;
        loop {
            self.skip_ws();
            match self.s.get(self.pos) {
                Some(&op @ (b'+' | b'-')) => {
                    self.pos += 1;
                    let rhs = self.product()?;
                    acc = if op == b'+' { acc + rhs } else { acc - rhs };
                }
                _ => return Some(acc),
            }
        }
    }

    fn product(&mut self) -> Option<f32> {
        let mut acc = self.value()?;
        loop {
            self.skip_ws();
            match self.s.get(self.pos) {
                Some(&op @ (b'*' | b'/')) => {
                    self.pos += 1;
                    let rhs = self.value()?;
                    if op == b'/' {
                        if rhs == 0.0 {
                            return None;
                        }
                        acc /= rhs;
                    } else {
                        acc *= rhs;
                    }
                }
                _ => return Some(acc),
            }
        }
    }

    fn value(&mut self) -> Option<f32> {
        self.skip_ws();
        // Parenthesised sub-expression: resolve the balanced group.
        if self.s.get(self.pos) == Some(&b'(') {
            let start = self.pos + 1;
            let mut depth = 0usize;
            let mut i = self.pos;
            while i < self.s.len() {
                match self.s[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            if depth != 0 {
                return None;
            }
            let inner = std::str::from_utf8(&self.s[start..i]).ok()?;
            self.pos = i + 1;
            return resolve_calc(inner, self.avail, self.viewport);
        }
        // A value token runs to the next top-level space / `*` / `/` / `)`,
        // but a nested function call (`min(...)`, `var(...)`) carries balanced
        // parens whose internal commas/spaces stay part of the token.
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(&c) = self.s.get(self.pos) {
            match c {
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b' ' | b'*' | b'/' if depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        let tok = std::str::from_utf8(&self.s[start..self.pos]).ok()?.trim();
        if tok.is_empty() {
            return None;
        }
        // A unitless number is a scalar (calc multiplier/divisor); anything with
        // a unit or `%` is a length resolved against the containing block.
        if let Ok(n) = tok.parse::<f32>() {
            Some(n)
        } else {
            resolve_cells_f32(tok, self.avail, self.viewport)
        }
    }
}

/// Whether a vertical length is big enough to warrant a blank spacer row.
/// A terminal row is precious (~1em of height), so we spend one only when the
/// gap EXCEEDS half a line: an exactly-half-row gap — `8px`/`0.5em`/`1ch`, the
/// web's ubiquitous "tight" spacing (a thumbnail-to-caption tab, an icon-row
/// pad) — no longer costs a whole blank line. Gaps over half a row still do.
fn vertical_space(value: &str) -> bool {
    css_length_em(value).is_some_and(|em| em > 0.5)
}

/// A horizontal length as an indent in cells (≈ 2 cells per em).
fn indent_cells(value: Option<&str>) -> usize {
    value
        .and_then(css_length_em)
        .map(|em| (em * 2.0).round().max(0.0) as usize)
        .unwrap_or(0)
}

/// Glyphs a star-rating icon's alt text collapses to (swappable in one
/// place if a terminal font lacks the half-star).
const STAR_FULL: &str = "★";
const STAR_HALF: &str = "⯨";
const STAR_EMPTY: &str = "☆";

/// Map a star-rating icon image's alt text to a compact glyph, or `None`
/// when it isn't a star. Keyed on the accessible phrasing rating widgets
/// share ("full/filled", "half", "empty/blank/unfilled" + "star"), so any
/// site's image-based stars read as glyphs instead of repeated phrases.
fn star_glyph(alt: &str) -> Option<&'static str> {
    let a = alt.to_ascii_lowercase();
    if !a.contains("star") {
        return None;
    }
    if a.contains("half") {
        Some(STAR_HALF)
    } else if a.contains("empty") || a.contains("blank") || a.contains("unfilled") {
        Some(STAR_EMPTY)
    } else if a.contains("full") || a.contains("filled") {
        Some(STAR_FULL)
    } else {
        None
    }
}

/// `h1`..`h6` → the level; anything else → `None`.
fn heading_level(tag: &str) -> Option<u8> {
    let bytes = tag.as_bytes();
    if bytes.len() == 2 && bytes[0] == b'h' && (b'1'..=b'6').contains(&bytes[1]) {
        Some(bytes[1] - b'0')
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay(html: &str, width: usize) -> Vec<Row> {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        lay_out(
            &dom,
            &base,
            width,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        )
    }

    /// Lay out with borders ON (off is the production default). The border
    /// tests opt in explicitly so they stay isolated from the session flag.
    fn lay_b(html: &str, width: usize) -> Vec<Row> {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        lay_out(
            &dom,
            &base,
            width,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            true,
        )
    }

    fn lay_with_images(html: &str, width: usize, images: &ImageSizes) -> Vec<Row> {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        lay_out(&dom, &base, width, &[], &ControlMap::new(), images, false)
    }

    fn measure(html: &str, width: usize) -> (Dom, HashMap<NodeId, PxRect>) {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let boxes = measure_boxes(&dom, &base, width, &[], &ControlMap::new(), (8, 16), false);
        (dom, boxes)
    }

    #[test]
    fn measure_boxes_reports_cell_boxes_and_unions_into_ancestors() {
        let (dom, m) = measure(
            r#"<body><div id="wrap"><span id="a">AB</span><span id="b">CD</span></div></body>"#,
            40,
        );
        let by_id = |id: &str| {
            let n = dom
                .descendants(DOCUMENT)
                .into_iter()
                .find(|&n| dom.attr(n, "id") == Some(id))
                .unwrap();
            *m.get(&n)
                .unwrap_or_else(|| panic!("{id} should have a laid-out box"))
        };
        let a = by_id("a");
        let b = by_id("b");
        let wrap = by_id("wrap");
        // Each "AB"/"CD" is 2 cells wide; cell_px is 8x16 → 16px wide, 16px tall.
        assert_eq!(a.width, 16.0);
        assert_eq!(a.height, 16.0);
        assert_eq!(b.width, 16.0);
        // The two inline spans sit side by side on the same row.
        assert_eq!(a.top, b.top);
        assert_eq!(b.left, a.left + a.width);
        // The wrapping block's box spans both children — the ancestor union, so
        // a block reports the bounding box of its content (what gBCR requires).
        assert_eq!(wrap.left, a.left);
        assert_eq!(wrap.top, a.top);
        assert_eq!(wrap.width, 32.0);
        assert_eq!(wrap.height, 16.0);
    }

    #[test]
    fn measure_boxes_stacks_blocks_vertically() {
        // Two stacked blocks: the second's box sits strictly below the first's,
        // proving the y coordinate tracks document rows (cell_px height 16).
        let (dom, m) = measure(
            r#"<body><div id="top">one</div><div id="bot">two</div></body>"#,
            40,
        );
        let by_id = |id: &str| {
            let n = dom
                .descendants(DOCUMENT)
                .into_iter()
                .find(|&n| dom.attr(n, "id") == Some(id))
                .unwrap();
            *m.get(&n).unwrap()
        };
        let top = by_id("top");
        let bot = by_id("bot");
        assert!(
            bot.top >= top.top + top.height,
            "second block ({}) should start at or below the first's bottom ({})",
            bot.top,
            top.top + top.height
        );
    }

    fn box_by_id(dom: &Dom, m: &HashMap<NodeId, PxRect>, id: &str) -> PxRect {
        let n = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap();
        *m.get(&n)
            .unwrap_or_else(|| panic!("{id} should have a laid-out box"))
    }

    #[test]
    fn declared_block_width_floors_geometry() {
        // Geometry Phase 2: a sized block reports its declared CSS box even when
        // its content paints fewer cells. "240px" = 30 cells; cell_px 8 → 240px.
        let (dom, m) = measure(
            r#"<body><div id="card" style="width:240px">x</div></body>"#,
            80,
        );
        let card = box_by_id(&dom, &m, "card");
        assert_eq!(card.width, 240.0, "declared width is the geometry floor");
    }

    #[test]
    fn declared_box_floors_when_content_lives_in_a_child_element() {
        // A sized block whose content is a child element (not direct text) has
        // no item tagged to itself — its text is tagged to the child. The floor
        // must still apply once the block absorbs the child's box (the bug the
        // live smoke caught: reported 10x20 instead of 240x160).
        let (dom, m) = measure(
            r#"<body><div id="sz" style="width:240px;height:160px"><span>.</span></div></body>"#,
            80,
        );
        let sz = box_by_id(&dom, &m, "sz");
        assert_eq!(sz.width, 240.0, "declared width floors a child-only block");
        assert_eq!(
            sz.height, 160.0,
            "declared height floors a child-only block"
        );
    }

    #[test]
    fn declared_block_height_floors_geometry_but_not_render() {
        // Declared height floors the REPORTED box ("160px" = 10 rows × 16 =
        // 160px) but is NOT reserved on screen — the next block sits right below
        // the content, not 160px down (terminal rows are precious; height stays
        // geometry-only, the documented deviation).
        let (dom, m) = measure(
            r#"<body><div id="box" style="height:160px">x</div><div id="after">y</div></body>"#,
            80,
        );
        let bx = box_by_id(&dom, &m, "box");
        let after = box_by_id(&dom, &m, "after");
        assert_eq!(bx.height, 160.0, "declared height is the geometry floor");
        assert!(
            after.top < bx.height,
            "following block ({}) must not be pushed down to the reserved height ({})",
            after.top,
            bx.height
        );
    }

    #[test]
    fn definite_block_width_narrows_the_band() {
        // Part 2 (render): a definite block width narrows the content band, so
        // text wraps within it. "80px" = 10 cells; the same text flows on one
        // row at full width.
        let words = "aaa bbb ccc ddd eee fff";
        let (dom, narrow) = measure(
            &format!(r#"<body><div id="n" style="width:80px">{words}</div></body>"#),
            80,
        );
        let (dom2, wide) = measure(&format!(r#"<body><div id="n">{words}</div></body>"#), 80);
        let n = box_by_id(&dom, &narrow, "n");
        let w = box_by_id(&dom2, &wide, "n");
        assert_eq!(w.height, 16.0, "unconstrained text flows on one row");
        assert!(
            n.height > w.height,
            "a definite width should wrap the text taller ({} vs {})",
            n.height,
            w.height
        );
    }

    #[test]
    fn percent_height_stays_indefinite() {
        // `%` height resolves against an indefinite (auto-height) container, so
        // it computes to auto — no floor. The block reports its content extent.
        let (dom, m) = measure(r#"<body><div id="p" style="height:50%">x</div></body>"#, 80);
        let p = box_by_id(&dom, &m, "p");
        assert_eq!(p.height, 16.0, "percent height is indefinite → no floor");
    }

    #[test]
    fn auto_and_overwide_width_do_not_cramp() {
        // `width:auto` and a width that meets/exceeds the band both flow wide —
        // the band is never narrowed below the available width.
        let words = "one two three four five";
        let (da, auto) = measure(
            &format!(r#"<body><div id="d" style="width:auto">{words}</div></body>"#),
            80,
        );
        let (df, full) = measure(
            &format!(r#"<body><div id="d" style="width:100%">{words}</div></body>"#),
            80,
        );
        let (dp, plain) = measure(&format!(r#"<body><div id="d">{words}</div></body>"#), 80);
        let h = |d: &Dom, m: &HashMap<NodeId, PxRect>| box_by_id(d, m, "d").height;
        let plain_h = h(&dp, &plain);
        assert_eq!(h(&da, &auto), plain_h, "width:auto flows like no width");
        assert_eq!(h(&df, &full), plain_h, "width:100% flows like no width");
        assert_eq!(plain_h, 16.0, "the text fits on one row at width 80");
    }

    #[test]
    fn bare_max_width_does_not_narrow_the_band() {
        // A bare `max-width` (no `width`, no auto margin) is only a ceiling — an
        // auto-width block already fills the band, so it must NOT be narrowed.
        // (This narrowed Steam's `.sale_capsule{max-width:50%}` flex capsules
        // and shrank their `width:100%` thumbnails to a sliver.) The text fits
        // on one row at full width; an explicit `width:50%` of the same text
        // wraps taller.
        let words = "one two three four five six seven eight nine ten";
        let (dm, mw) = measure(
            &format!(r#"<body><div id="d" style="max-width:50%">{words}</div></body>"#),
            80,
        );
        let (dw, wd) = measure(
            &format!(r#"<body><div id="d" style="width:50%">{words}</div></body>"#),
            80,
        );
        let max_h = box_by_id(&dm, &mw, "d").height;
        let wid_h = box_by_id(&dw, &wd, "d").height;
        assert_eq!(max_h, 16.0, "a bare max-width must not narrow → one row");
        assert!(
            wid_h > max_h,
            "an explicit width:50% narrows and wraps ({wid_h} vs {max_h})"
        );
    }

    #[test]
    #[ignore = "manual diagnostic: TRUST_LAYOUT_DIAG=<file> TRUST_LAYOUT_W=<cols>"]
    fn layout_diag() {
        let Ok(path) = std::env::var("TRUST_LAYOUT_DIAG") else {
            return;
        };
        let html = std::fs::read_to_string(&path).unwrap();
        let w: usize = std::env::var("TRUST_LAYOUT_W")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(88);
        let dom = Dom::parse_document(&html);
        let base = Url::parse("https://marketplace.secondlife.com/").unwrap();
        // Fake every <img> a small box so image flow exercises.
        let mut images = ImageSizes::new();
        for id in dom.descendants(DOCUMENT) {
            if dom.tag_name(id) == Some("img")
                && let Some(src) = dom.attr(id, "src")
                && let Link::Http(u) = crate::http::resolve(&base, src)
            {
                let (fw, fh) = std::env::var("TRUST_FAKE_IMG")
                    .ok()
                    .and_then(|s| {
                        let (w, h) = s.split_once('x')?;
                        Some((w.parse().ok()?, h.parse().ok()?))
                    })
                    .unwrap_or((10, 4));
                images.insert(u.to_string(), (fw, fh));
            }
        }
        let (rows, carousels) =
            lay_out_with_carousels(&dom, &base, w, &[], &ControlMap::new(), &images, true);
        for c in &carousels {
            println!(
                "CAROUSEL rows {}..{} band [{},{}] width {} stops {}",
                c.start,
                c.end,
                c.left,
                c.right,
                c.width,
                c.stops.len()
            );
        }
        for (i, row) in rows.iter().enumerate() {
            let mut s = String::new();
            let mut col = 0usize;
            for it in &row.items {
                while col < it.col as usize {
                    s.push(' ');
                    col += 1;
                }
                let t = if it.image.is_some() {
                    format!("[img {}x{}]", it.width, it.height)
                } else {
                    it.text.clone()
                };
                s.push_str(&t);
                col = it.col as usize + t.chars().count();
            }
            println!("{i:3}|{s}");
        }
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(render_row).collect()
    }

    fn all_text(rows: &[Row]) -> String {
        texts(rows).join("\n")
    }

    #[test]
    fn offscreen_clipped_absolute_box_is_dropped_onscreen_sibling_kept() {
        // A carousel's next slide: `position:absolute;left:90%;width:90%` parked
        // off-canvas in an `overflow:hidden` positioned container is clipped to
        // invisible in a browser (Steam's spotlight slide 2). The active slide
        // (`left:0`) stays. We render neither the off-screen box nor its subtree.
        let html = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:relative;left:0;width:90%\">ACTIVE</div>\
            <div style=\"position:absolute;top:0;left:90%;width:90%\">OFFSCREEN</div>\
            </div></body>";
        let out = all_text(&lay(html, 100));
        assert!(out.contains("ACTIVE"), "active slide kept: {out:?}");
        assert!(
            !out.contains("OFFSCREEN"),
            "off-screen slide dropped: {out:?}"
        );
    }

    #[test]
    fn offscreen_clip_needs_a_clipping_containing_block() {
        // The SAME off-canvas box, but the containing block does NOT clip
        // (`overflow:visible`): a browser paints it overflowing, so we keep it
        // (e.g. a dropdown/tooltip that escapes its parent). Standards: an
        // abspos box is clipped only by a CLIPPING containing block.
        let html = "<body><div style=\"position:relative;overflow:visible\">\
            <div style=\"position:absolute;top:0;left:90%;width:90%\">ESCAPES</div>\
            </div></body>";
        assert!(
            all_text(&lay(html, 100)).contains("ESCAPES"),
            "non-clipping parent keeps it"
        );
    }

    #[test]
    fn onscreen_and_indeterminate_absolute_boxes_are_kept() {
        // On-screen (left:0), a corner overlay (right:0, size unknown), and a
        // length offset we can't resolve to a fraction all stay — we only drop
        // on a provably off-screen %-resolved span.
        let onscreen = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:absolute;top:0;left:0;width:50%\">INFLOWISH</div></div></body>";
        let corner = "<body><div style=\"position:relative;overflow:hidden\">\
            <span style=\"position:absolute;top:0;right:0\">BADGE</span>x</div></body>";
        let modal = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:absolute;top:0;left:0;right:0;bottom:0\">MODAL</div></div></body>";
        let px = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:absolute;left:1200px;width:90%\">PXOFFSET</div></div></body>";
        assert!(all_text(&lay(onscreen, 100)).contains("INFLOWISH"));
        assert!(all_text(&lay(corner, 100)).contains("BADGE"));
        assert!(all_text(&lay(modal, 100)).contains("MODAL"));
        // A non-zero length offset is indeterminate without pixel geometry → kept.
        assert!(all_text(&lay(px, 100)).contains("PXOFFSET"));
    }

    #[test]
    fn wraps_paragraph_into_rows() {
        let rows = lay("<body><p>one two three four five six</p></body>", 14);
        let lines = texts(&rows);
        // Word-wrapped at 14 columns, no row exceeds the width.
        assert!(lines.iter().all(|l| l.chars().count() <= 14), "{lines:?}");
        assert_eq!(lines.join(" ").split_whitespace().count(), 6);
    }

    #[test]
    fn multi_link_row_keeps_each_anchor_a_separate_item() {
        let rows = lay(
            r#"<body><p>see <a href="/a">foo</a> and <a href="/b">bar</a> ok</p></body>"#,
            60,
        );
        // The whole sentence fits on one row.
        assert_eq!(rows.len(), 1, "{:?}", texts(&rows));
        let links: Vec<&Item> = rows[0].items.iter().filter(|i| i.link.is_some()).collect();
        assert_eq!(links.len(), 2, "two links on one row");
        assert_eq!(links[0].text.trim(), "foo");
        assert_eq!(links[1].text.trim(), "bar");
        // Distinct source nodes so L2 can tell them apart.
        assert_ne!(links[0].node, links[1].node);
        assert_eq!(render_row(&rows[0]), "see foo and bar ok");
        // Each link resolved against the base URL.
        assert!(matches!(&links[0].link, Some(Link::Http(u)) if u.as_str().ends_with("/a")));
    }

    #[test]
    fn headings_carry_their_level() {
        let rows = lay("<body><h2>Title</h2><p>body</p></body>", 40);
        let heading = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| matches!(i.kind, ItemKind::Heading(_)))
            .expect("a heading item");
        assert_eq!(heading.kind, ItemKind::Heading(2));
        assert_eq!(heading.text, "Title");
    }

    #[test]
    fn list_items_get_markers_and_indent() {
        let rows = lay("<body><ul><li>alpha</li><li>beta</li></ul></body>", 40);
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("• alpha")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("• beta")), "{lines:?}");
        // Indented under the list.
        let alpha = lines.iter().find(|l| l.contains("alpha")).unwrap();
        assert!(alpha.starts_with("  •"), "indented: {alpha:?}");
    }

    #[test]
    fn ordered_list_numbers_items() {
        let rows = lay("<body><ol><li>first</li><li>second</li></ol></body>", 40);
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("1. first")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("2. second")), "{lines:?}");
    }

    #[test]
    fn list_style_none_removes_markers() {
        // The nav-menu idiom: a <ul> with list-style:none shows no bullets.
        let rows = lay(
            r#"<body><ul style="list-style:none"><li>Home</li><li>About</li></ul></body>"#,
            40,
        );
        let all = texts(&rows).join("\n");
        assert!(all.contains("Home") && all.contains("About"), "{all:?}");
        assert!(
            !all.contains('•'),
            "no bullets with list-style:none: {all:?}"
        );
    }

    #[test]
    fn list_style_type_alpha_and_roman() {
        let rows = lay(
            r#"<body><ol style="list-style-type:lower-alpha"><li>x</li><li>y</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("a. x")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("b. y")), "{lines:?}");
        let rows = lay(
            r#"<body><ol style="list-style-type:lower-roman"><li>x</li><li>y</li><li>z</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("i. x")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("ii. y")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("iii. z")), "{lines:?}");
    }

    #[test]
    fn nested_ul_markers_vary_by_depth() {
        let rows = lay(
            "<body><ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li></ul></body>",
            40,
        );
        let all = texts(&rows).join("\n");
        assert!(all.contains("• a"), "depth 1 disc: {all:?}");
        assert!(all.contains("◦ b"), "depth 2 circle: {all:?}");
        assert!(all.contains("▪ c"), "depth 3 square: {all:?}");
    }

    #[test]
    fn ol_start_and_li_value_set_the_counter() {
        let rows = lay(
            r#"<body><ol start="5"><li>five</li><li value="9">nine</li><li>ten</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("5. five")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("9. nine")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("10. ten")), "{lines:?}");
    }

    #[test]
    fn pre_preserves_whitespace_and_newlines() {
        let rows = lay("<body><pre>a   b\n  c</pre></body>", 80);
        let lines = texts(&rows);
        assert_eq!(lines, vec!["a   b".to_owned(), "  c".to_owned()]);
        assert!(rows[0].items.iter().all(|i| i.kind == ItemKind::Pre));
    }

    #[test]
    fn hidden_subtree_is_skipped() {
        let rows = lay(
            r#"<body><p>shown</p><p style="display:none">secret</p><p hidden>also</p></body>"#,
            40,
        );
        let all = texts(&rows).join("\n");
        assert!(all.contains("shown"));
        assert!(!all.contains("secret"), "display:none skipped");
        assert!(!all.contains("also"), "hidden attr skipped");
    }

    #[test]
    fn blocks_are_separated_but_blanks_collapse() {
        let rows = lay("<body><p>one</p><p>two</p></body>", 40);
        let lines = texts(&rows);
        assert_eq!(
            lines,
            vec!["one".to_owned(), String::new(), "two".to_owned()]
        );
        // No leading or trailing blank rows.
        assert!(!rows.first().unwrap().items.is_empty());
        assert!(!rows.last().unwrap().items.is_empty());
    }

    #[test]
    fn inline_display_flows_list_horizontally() {
        // reddit's subreddit bar: a <ul> of <li> set to display:inline by
        // CSS. They must flow across one row, not stack with bullets.
        let rows = lay(
            r#"<html><head><style>li{display:inline}</style></head>
               <body><ul><li><a href="/a">one</a></li> <li><a href="/b">two</a></li>
               <li><a href="/c">three</a></li></ul></body></html>"#,
            60,
        );
        let link_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.items.iter().any(|i| i.link.is_some()))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            link_rows.len(),
            1,
            "inline <li>s share one row: {:?}",
            texts(&rows)
        );
        let all = texts(&rows).join("\n");
        assert!(
            !all.contains('•'),
            "no bullet markers for inline lis: {all:?}"
        );
        assert!(all.contains("one") && all.contains("two") && all.contains("three"));
    }

    #[test]
    fn adjacent_links_get_a_separating_space() {
        // Nav markup with no whitespace between anchors must not fuse.
        let rows = lay(
            r#"<html><head><style>a{display:inline}</style></head>
               <body><a href="/a">one</a><a href="/b">two</a><a href="/c">three</a></body></html>"#,
            60,
        );
        assert_eq!(render_row(&rows[0]), "one two three");
        // But a link's own clean leading edge is preserved (no space
        // injected before the first link on the line).
        assert!(!render_row(&rows[0]).starts_with(' '));
    }

    #[test]
    fn css_margin_drives_block_gaps() {
        // A reset zeroing a paragraph's margin removes its default gap.
        let rows = lay(
            "<html><head><style>p{margin:0}</style></head>\
             <body><p>one</p><p>two</p></body></html>",
            40,
        );
        assert_eq!(
            texts(&rows),
            vec!["one".to_owned(), "two".to_owned()],
            "margin:0 collapses the inter-paragraph gap"
        );
        // A <div> (no default gap) given a top margin gains one.
        let rows = lay(
            "<html><head><style>div{margin-top:1em}</style></head>\
             <body><div>a</div><div>b</div></body></html>",
            40,
        );
        assert!(
            texts(&rows).contains(&String::new()),
            "a CSS margin opens a gap on an otherwise-tight div: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn css_left_indent_from_margin_and_padding() {
        let rows = lay(
            "<html><head><style>div{margin-left:2em;padding-left:1em}</style></head>\
             <body><div>x</div></body></html>",
            40,
        );
        // 2em·2 + 1em·2 = 6 cells of indent.
        assert_eq!(render_row(&rows[0]), "      x");
    }

    #[test]
    fn block_display_breaks_inline_span() {
        // Conversely, CSS can make a normally-inline <span> a block.
        let rows = lay(
            r#"<html><head><style>span{display:block}</style></head>
               <body><span>one</span><span>two</span></body></html>"#,
            60,
        );
        let lines: Vec<String> = texts(&rows).into_iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["one".to_owned(), "two".to_owned()]);
    }

    #[test]
    fn text_align_center_and_right_shift_rows() {
        // Centered: "hi" (2 cells) in width 10 → free 8, offset 4.
        let rows = lay(
            r#"<html><head><style>p{text-align:center}</style></head>
               <body><p>hi</p></body></html>"#,
            10,
        );
        assert_eq!(render_row(&rows[0]), "    hi");
        // Right: offset = free = 8.
        let rows = lay(
            r#"<html><head><style>p{text-align:right}</style></head>
               <body><p>hi</p></body></html>"#,
            10,
        );
        assert_eq!(render_row(&rows[0]), "        hi");
    }

    #[test]
    fn text_align_inherits_to_child_blocks() {
        // A centered container centers a child block that sets no align.
        let rows = lay(
            r#"<html><head><style>div{text-align:center}</style></head>
               <body><div><p>hi</p></div></body></html>"#,
            10,
        );
        assert_eq!(render_row(&rows[0]), "    hi");
    }

    fn find<'a>(rows: &'a [Row], needle: &str) -> &'a Item {
        rows.iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains(needle))
            .unwrap_or_else(|| panic!("no item containing {needle:?}"))
    }

    #[test]
    fn tags_drive_bold_and_italic() {
        let rows = lay(
            "<body><p>a <b>bee</b> <i>cee</i> <em>dee</em> <strong>eee</strong></p></body>",
            60,
        );
        assert!(find(&rows, "bee").emph.bold);
        assert!(find(&rows, "cee").emph.italic);
        assert!(find(&rows, "dee").emph.italic);
        assert!(find(&rows, "eee").emph.bold);
        let plain = find(&rows, "a ");
        assert!(!plain.emph.bold && !plain.emph.italic);
    }

    #[test]
    fn length_resolver_units_ch_vw_and_calc() {
        // ch is the natural terminal unit (1ch = 1 cell); % is the
        // containing block, vw the viewport; calc folds a +/- term chain.
        assert_eq!(resolve_cells("10ch", 100, 80), Some(10), "1ch = 1 cell");
        assert_eq!(resolve_cells("50%", 40, 80), Some(20), "% of avail");
        assert_eq!(resolve_cells("50vw", 40, 80), Some(40), "vw of viewport");
        assert_eq!(
            resolve_cells("calc(100% - 4ch)", 40, 80),
            Some(36),
            "calc subtracts a ch length from a percentage"
        );
        assert_eq!(
            resolve_cells("calc(50% + 2ch)", 40, 80),
            Some(22),
            "calc adds across unit kinds"
        );
        // Unsupported values are ignored (None), exactly as before.
        assert_eq!(resolve_cells("auto", 40, 80), None);
        assert_eq!(resolve_cells("12vh", 40, 80), None, "no viewport height");
        // calc multiplication/division (a unitless number is a scalar).
        assert_eq!(
            resolve_cells("calc(100% * 2)", 40, 80),
            Some(80),
            "calc multiplies a percentage by a scalar"
        );
        assert_eq!(
            resolve_cells("calc((100% - 4ch) / 3)", 40, 80),
            Some(12),
            "calc divides a grouped sub-expression — the 3-column item width"
        );
        assert_eq!(
            resolve_cells("calc(100% / 3)", 60, 80),
            Some(20),
            "calc divides a percentage by a scalar"
        );
        // ch also flows through the absolute-unit path (indents).
        assert_eq!(indent_cells(Some("3ch")), 3);
    }

    #[test]
    fn css_font_weight_and_style_apply_and_override() {
        // CSS sets emphasis on otherwise-plain elements...
        let rows = lay(
            r#"<html><head><style>.b{font-weight:700}.i{font-style:italic}</style></head>
               <body><p class="b">heavy</p><p class="i">slanted</p></body></html>"#,
            60,
        );
        assert!(find(&rows, "heavy").emph.bold);
        assert!(find(&rows, "slanted").emph.italic);
        // ...and can turn a tag's emphasis back OFF.
        let rows = lay(
            r#"<html><head><style>strong{font-weight:normal}</style></head>
               <body><p><strong>quiet</strong></p></body></html>"#,
            60,
        );
        assert!(
            !find(&rows, "quiet").emph.bold,
            "CSS font-weight:normal wins"
        );
    }

    #[test]
    fn emphasis_is_orthogonal_to_links_and_inherits() {
        // A bold link keeps BOTH its link target and bold flag.
        let rows = lay(r#"<body><p><a href="/x"><b>go</b></a></p></body>"#, 60);
        let link = find(&rows, "go");
        assert_eq!(link.kind, ItemKind::Link);
        assert!(link.emph.bold);
        assert!(link.link.is_some());
        // font-weight inherits to descendants.
        let rows = lay(
            r#"<html><head><style>div{font-weight:bold}</style></head>
               <body><div><span>child</span></div></body></html>"#,
            60,
        );
        assert!(find(&rows, "child").emph.bold, "font-weight inherits");
    }

    #[test]
    fn white_space_nowrap_keeps_one_row() {
        // Long text that would wrap at 14 cols stays on one row under nowrap.
        let rows = lay(
            r#"<html><head><style>p{white-space:nowrap}</style></head>
               <body><p>one two three four five six</p></body></html>"#,
            14,
        );
        // Whitespace still collapses (single spaces), but no wrapping.
        let content: Vec<&Row> = rows.iter().filter(|r| !r.items.is_empty()).collect();
        assert_eq!(
            content.len(),
            1,
            "nowrap stays on one row: {:?}",
            texts(&rows)
        );
        assert_eq!(render_row(content[0]), "one two three four five six");
    }

    #[test]
    fn white_space_pre_line_keeps_newlines_collapses_spaces() {
        let rows = lay(
            "<html><head><style>p{white-space:pre-line}</style></head>\
             <body><p>a    b\nc   d</p></body></html>",
            40,
        );
        let lines: Vec<String> = texts(&rows).into_iter().filter(|l| !l.is_empty()).collect();
        // Newline preserved into two rows; runs of spaces collapsed to one.
        assert_eq!(lines, vec!["a b".to_owned(), "c d".to_owned()]);
    }

    #[test]
    fn white_space_pre_via_css_preserves_spaces_without_pre_tag() {
        // A <div> (not <pre>) set to white-space:pre keeps its spaces, and
        // keeps Text styling (not the green Pre kind).
        let rows = lay(
            "<html><head><style>div{white-space:pre}</style></head>\
             <body><div>a   b\n  c</div></body></html>",
            40,
        );
        assert_eq!(texts(&rows), vec!["a   b".to_owned(), "  c".to_owned()]);
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.kind == ItemKind::Text),
            "CSS pre on a div keeps Text kind, not Pre"
        );
    }

    #[test]
    fn css_pre_wrap_wraps_long_lines() {
        // pre-wrap preserves spacing but still breaks lines wider than the box.
        let rows = lay(
            "<html><head><style>div{white-space:pre-wrap}</style></head>\
             <body><div>aaaaaaaaaaaaaaaaaaaa</div></body></html>",
            10,
        );
        let content: Vec<&Row> = rows.iter().filter(|r| !r.items.is_empty()).collect();
        assert!(
            content.len() >= 2,
            "20 chars wrap at width 10: {:?}",
            texts(&rows)
        );
        assert!(content.iter().all(|r| render_row(r).chars().count() <= 10));
    }

    #[test]
    fn text_transform_changes_rendered_case() {
        let rows = lay(
            r#"<html><head><style>.u{text-transform:uppercase}.c{text-transform:capitalize}</style></head>
               <body><p class="u">hello world</p><p class="c">hello world</p></body></html>"#,
            60,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("HELLO WORLD")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Hello World")), "{lines:?}");
    }

    #[test]
    fn text_decoration_and_tags_set_underline_strike() {
        // Tags: <u> underlines, <s>/<del> strike through.
        let rows = lay(
            "<body><p><u>under</u> <s>gone</s> <del>old</del></p></body>",
            60,
        );
        assert!(find(&rows, "under").emph.underline);
        assert!(find(&rows, "gone").emph.strike);
        assert!(find(&rows, "old").emph.strike);
        // CSS text-decoration propagates to descendants; `none` clears it.
        let rows = lay(
            r#"<html><head><style>.d{text-decoration:underline}a{text-decoration:none}</style></head>
               <body><p class="d">deco <a href="/x">link</a></p></body></html>"#,
            60,
        );
        assert!(find(&rows, "deco").emph.underline);
        assert!(
            !find(&rows, "link").emph.underline,
            "a{{text-decoration:none}} clears the inherited underline"
        );
    }

    #[test]
    fn decoded_image_lays_out_as_a_sized_box() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cat.png".to_owned(), (10, 4));
        let rows = lay_with_images(
            r#"<body><p>before</p><img src="/cat.png" alt="cat"><p>after</p></body>"#,
            40,
            &images,
        );
        // The image is an Image item carrying its URL and full box height.
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("an image item");
        assert_eq!(img.kind, ItemKind::Image);
        assert_eq!((img.width, img.height), (10, 4));
        assert_eq!(img.image.as_deref(), Some("https://example.com/cat.png"));
        // It reserves its full vertical box: the image row + 3 spacer rows.
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .unwrap();
        let spacers = rows[img_row + 1..img_row + 4]
            .iter()
            .all(|r| r.items.iter().all(|i| i.image.is_none() && i.width == 0));
        assert!(spacers, "3 reserved spacer rows follow the image");
    }

    #[test]
    fn a_float_columns_percentage_width_is_not_applied_twice_to_its_content() {
        // erome's /explore thumbnail grid: Bootstrap float columns
        // (`float:left;width:16.66%`) each holding a `width:100%` image. The
        // float pass already sizes the column box to 16.66% of the row; laying
        // its subtree must NOT re-resolve the column's own `width:16.66%`
        // against that already-narrowed box (16.66% of the column → a sliver),
        // which collapsed every thumbnail to a few cells. The image should fill
        // its column, not a fraction of it.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/t.jpg".to_owned(), (30, 14));
        // Six 16.66% float columns across a 120-cell row → ~20-cell columns.
        let cols: String = (0..6)
            .map(|_| {
                r#"<div style="float:left;width:16.66666667%">
                     <img src="/t.jpg" style="width:100%;height:auto" alt="x">
                   </div>"#
                    .to_owned()
            })
            .collect();
        let rows = lay_with_images(&format!("<body><div>{cols}</div></body>"), 120, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("a thumbnail image");
        // 16.66% of 120 ≈ 20 cells. The image should fill (near) the whole
        // column, not collapse to a sliver (the regression rendered ~3 cells).
        assert!(
            img.width >= 16,
            "the thumbnail fills its float column (got {} cells, expected ~20)",
            img.width
        );
    }

    /// The grid row index a text first appears on (for table layout tests).
    fn row_index_of(rows: &[Row], needle: &str) -> usize {
        rows.iter()
            .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
            .unwrap_or_else(|| panic!("no row containing {needle:?}"))
    }

    #[test]
    fn table_cells_lay_side_by_side_not_stacked() {
        // The core of CSS table layout: cells in one row share the SAME grid
        // rows, in distinct columns — not each `<td>` on its own line (the old
        // block-stacking behavior, which broke every table-as-layout page).
        let rows = lay(
            "<body><table><tr><td>LeftCell</td><td>RightCell</td></tr></table></body>",
            60,
        );
        assert_eq!(
            row_index_of(&rows, "LeftCell"),
            row_index_of(&rows, "RightCell"),
            "both cells are on the same row"
        );
        assert!(
            find(&rows, "RightCell").col > find(&rows, "LeftCell").col,
            "the second cell is to the RIGHT of the first"
        );
    }

    #[test]
    fn table_rows_stack_vertically() {
        // Rows stack; their cells align into shared columns.
        let rows = lay(
            "<body><table>\
             <tr><td>r1a</td><td>r1b</td></tr>\
             <tr><td>r2a</td><td>r2b</td></tr></table></body>",
            60,
        );
        assert!(row_index_of(&rows, "r2a") > row_index_of(&rows, "r1a"));
        // Same column for cells stacked in a column.
        assert_eq!(find(&rows, "r1a").col, find(&rows, "r2a").col);
        assert_eq!(find(&rows, "r1b").col, find(&rows, "r2b").col);
    }

    #[test]
    fn a_layout_table_puts_a_narrow_menu_beside_a_wide_content_column() {
        // The slackware.com pattern: a `width:10%` menu cell beside an
        // auto-width content cell. The menu column stays narrow and the content
        // column takes the rest, both on the same rows (side by side).
        let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do";
        let rows = lay(
            &format!(
                "<body><table width=\"100%\"><tr valign=\"top\">\
                 <td width=\"10%\">Menu</td>\
                 <td>{words}</td></tr></table></body>"
            ),
            80,
        );
        assert_eq!(
            row_index_of(&rows, "Menu"),
            row_index_of(&rows, "lorem"),
            "the menu sits beside the content, not above it"
        );
        let content_col = find(&rows, "lorem").col;
        assert!(
            content_col >= 8,
            "the content column starts past the narrow menu (col {content_col})"
        );
    }

    #[test]
    fn a_colspan_cell_spans_its_columns() {
        // A header spanning both columns sits above two cells that share its
        // width — its content starts at the table's left edge, the second-row
        // cells in two columns beneath it.
        let rows = lay(
            "<body><table>\
             <tr><td colspan=\"2\">Header</td></tr>\
             <tr><td>colA</td><td>colB</td></tr></table></body>",
            60,
        );
        assert!(row_index_of(&rows, "Header") < row_index_of(&rows, "colA"));
        assert_eq!(
            row_index_of(&rows, "colA"),
            row_index_of(&rows, "colB"),
            "the two spanned cells are on one row"
        );
        assert!(find(&rows, "colB").col > find(&rows, "colA").col);
    }

    #[test]
    fn a_nested_table_lays_out_inside_its_cell() {
        // slackware nests a table inside every cell (the bgcolor border trick);
        // the inner table must lay out within its cell's column, not collapse.
        let rows = lay(
            "<body><table><tr>\
             <td><table><tr><td>InnerL</td><td>InnerR</td></tr></table></td>\
             <td>Outer</td></tr></table></body>",
            60,
        );
        // The inner cells lay side by side, and the outer second cell is right
        // of both of them on the same row.
        assert_eq!(row_index_of(&rows, "InnerL"), row_index_of(&rows, "Outer"));
        assert!(find(&rows, "InnerR").col > find(&rows, "InnerL").col);
        assert!(find(&rows, "Outer").col > find(&rows, "InnerR").col);
    }

    #[test]
    fn deeply_nested_tables_lay_out_without_overflowing() {
        // The per-cell content measurement re-descends each cell's subtree, so
        // an arbitrarily deep table tree could overflow the layout stack.
        // `MAX_TABLE_DEPTH` degrades a table nested past the lid to block-stacked
        // content — this still terminates and surfaces the innermost content.
        let mut html = String::from("DEEPEST");
        for i in 0..40 {
            html = format!("<table><tr><td>L{i} {html}</td><td>x</td></tr></table>");
        }
        let rows = lay(&format!("<body>{html}</body>"), 80);
        assert!(
            all_text(&rows).contains("DEEPEST"),
            "the innermost cell content still renders past the depth lid"
        );
    }

    #[test]
    fn a_video_renders_as_poster_plus_caption_linking_to_the_source() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/poster.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<body><video poster="/poster.jpg"><source src="/clip_720p.mp4" type="video/mp4" res="720" label="HD"></video></body>"#,
            80,
            &images,
        );
        let poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("a poster image item");
        assert_eq!(
            poster.image.as_deref(),
            Some("https://example.com/poster.jpg")
        );
        assert!(
            matches!(&poster.link, Some(Link::Http(u)) if u.as_str().ends_with("clip_720p.mp4")),
            "poster links to the media source"
        );
        assert!(
            shows(&rows, "▶ Video · 720p HD"),
            "caption present: {:?}",
            texts(&rows)
        );
        // The caption is itself a link to the media.
        let cap = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("▶ Video"))
            .expect("a caption item");
        assert!(matches!(&cap.link, Some(Link::Http(u)) if u.as_str().ends_with("clip_720p.mp4")));
    }

    #[test]
    fn a_videojs_transformed_player_still_renders_poster_and_caption() {
        // The real shape after video.js initialises: the <video> is renamed,
        // class vjs-tech, position:absolute, src set directly, inside a
        // video-js wrapper alongside control divs.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/poster.jpg".to_owned(), (80, 23));
        let rows = lay_with_images(
            r#"<body><div class="video-js vjs-16-9" style="position:relative">
                 <video class="vjs-tech" poster="/poster.jpg" src="/clip_720p.mp4"
                        style="display:inline-block;width:100%;height:100%;position:absolute;top:0;left:0"></video>
                 <div class="vjs-poster" style="background-image:url(/poster.jpg)"></div>
                 <button class="vjs-big-play-button"><span class="vjs-control-text">Play Video</span></button>
               </div></body>"#,
            120,
            &images,
        );
        let has_poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .any(|i| i.image.as_deref() == Some("https://example.com/poster.jpg"));
        assert!(has_poster, "poster renders: {:?}", texts(&rows));
        assert!(
            shows(&rows, "▶ Video"),
            "caption renders: {:?}",
            texts(&rows)
        );
        // The player chrome (big-play button etc.) is suppressed — only the
        // media representation renders, no leaked control labels.
        assert!(
            !shows(&rows, "Play Video"),
            "player chrome suppressed: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn an_svg_icon_link_renders_its_glyph_not_its_label() {
        // The logged-in header idiom: `<a><svg class="...fa-bell"></svg></a>`.
        // An icon-only anchor renders the icon GLYPH (the web's dominant icon
        // form) rather than vanishing or dumping its aria-label.
        let rows = lay(
            r##"<body><a href="/n" aria-label="Notifications"><svg class="svg-fa svg-fas-fa-bell"><use href="#fas-fa-bell"></use></svg></a></body>"##,
            80,
        );
        assert!(shows(&rows, "🔔"), "bell glyph: {:?}", texts(&rows));
        assert!(!shows(&rows, "Notifications"), "{:?}", texts(&rows));
    }

    #[test]
    fn an_audio_with_no_poster_renders_a_link_only() {
        let rows = lay_with_images(
            r#"<body><audio><source src="/track.mp3" type="audio/mpeg"></audio></body>"#,
            80,
            &ImageSizes::new(),
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none())
        );
        assert!(shows(&rows, "♪ Audio"), "{:?}", texts(&rows));
    }

    /// Whether any laid item's text contains `needle`.
    fn shows(rows: &[Row], needle: &str) -> bool {
        rows.iter()
            .flat_map(|r| &r.items)
            .any(|i| i.text.contains(needle))
    }

    #[test]
    fn a_fullviewport_modal_surfaces_and_defers_the_page() {
        // erome's pattern: a full-viewport position:fixed age gate sits in the
        // DOM beside the page. A browser paints it ON TOP (the page is hidden
        // behind it). We can't composite, so we surface only the overlay and
        // defer the page — not stack the gate below the login it should cover.
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <div style="position:fixed;width:100%;height:100%">
                   <p>AgeGate</p><a href="/enter">EnterButton</a>
                 </div>
               </body>"#,
            80,
        );
        assert!(shows(&rows, "AgeGate"), "the overlay content renders");
        assert!(shows(&rows, "EnterButton"), "the overlay control renders");
        assert!(
            !shows(&rows, "LoginLink"),
            "the page behind the overlay is deferred"
        );
    }

    #[test]
    fn a_bare_backdrop_overlay_does_not_surface() {
        // A full-viewport position:fixed layer holding only a decorative
        // backdrop image is NOT a modal — it has no content. The page must
        // still render (the `.bg` divs erome stacks behind its gate).
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <div style="position:fixed;width:100%;height:100%">
                   <img src="/bg.jpg" style="position:absolute;width:auto;min-width:100%;height:auto">
                 </div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "LoginLink"),
            "a bare backdrop doesn't defer the page"
        );
    }

    #[test]
    fn a_full_viewport_background_layer_is_not_a_modal() {
        // pixiv's logged-out top: a `position:fixed; inset:0` background
        // slideshow (z-index:auto) sits BEHIND the real page content (a
        // `position:relative; z-index:1` signup card). The slide matches the
        // modal GEOMETRY test, but a modal paints ON TOP — here the card paints
        // ABOVE the slide (positioned, higher z), so the slide is a background,
        // not a modal. Treating it as a modal deferred the entire login page
        // (only the slide's caption rendered). Both must render.
        let rows = lay(
            r#"<body>
                 <div style="position:fixed;top:0;right:0;bottom:0;left:0">
                   <p>SlideCaption</p>
                 </div>
                 <div style="position:relative;z-index:1">
                   <a href="/signup">CreateAccount</a><a href="/login">LoginButton</a>
                 </div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "CreateAccount"),
            "the in-flow signup card is NOT deferred behind the background slide"
        );
        assert!(shows(&rows, "LoginButton"), "the login control renders");
    }

    #[test]
    fn a_small_absolute_overlay_does_not_defer_the_page() {
        // Regression: a small absolutely-positioned badge/control is chrome,
        // not a page-covering modal — it stays inline and never hides content.
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <span style="position:absolute"><a href="/x">Badge</a></span>
               </body>"#,
            80,
        );
        assert!(shows(&rows, "LoginLink"), "the page still renders");
    }

    #[test]
    fn a_surfaced_modal_keeps_its_foreground_image() {
        // A lightbox: a position:fixed overlay whose content is a full-bleed
        // image wrapped in absolutely-positioned layers (every ancestor is
        // out-of-flow). The image is the modal's FOREGROUND — render it, don't
        // drop it as a page backdrop. (`Close` makes the overlay qualify as a
        // modal; the image alone has no "content".)
        let mut images = ImageSizes::new();
        images.insert("https://example.com/slide.jpg".to_string(), (80, 60));
        let dom = Dom::parse_document(
            r##"<body>
                 <div id="page"><a href="/in">PageLink</a></div>
                 <div style="position:fixed;width:100%;height:100%">
                   <div style="position:absolute;top:0;right:0;bottom:0;left:0">
                     <img src="/slide.jpg" style="width:auto;max-width:100%;height:auto">
                   </div>
                   <a href="#close">Close</a>
                 </div>
               </body>"##,
        );
        let base = Url::parse("https://example.com/").unwrap();
        let rows = lay_out(&dom, &base, 80, &[], &ControlMap::new(), &images, false);
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.image.as_deref() == Some("https://example.com/slide.jpg")),
            "the lightbox slide image renders: {:?}",
            texts(&rows)
        );
        assert!(
            !shows(&rows, "PageLink"),
            "the page behind the lightbox is deferred"
        );
    }

    #[test]
    fn a_top_right_corner_overlay_rides_the_first_line() {
        // erome's comment options idiom: a `position:relative` block holds its
        // content plus a `position:absolute;top:0;right:0` `⋯` menu. The overlay
        // is right-aligned ON the first content line, not dropped to its own row
        // below it.
        let rows = lay(
            r#"<body><div style="position:relative">
                 <div>comment body</div>
                 <div style="position:absolute;top:0;right:0"><a href="/opts">X</a></div>
               </div></body>"#,
            40,
        );
        let first = &rows[0];
        let link = first
            .items
            .iter()
            .find(|i| i.link.is_some())
            .expect("the overlay link is on the FIRST row");
        assert_eq!(link.text.trim(), "X");
        let text = first
            .items
            .iter()
            .find(|i| i.text.contains("comment"))
            .expect("the comment text is on the first row too");
        // Right-aligned: to the right of the content, near the band's right edge.
        assert!(
            link.col > text.col && (link.col as usize) >= 40 - 3,
            "overlay right-aligned: link@{} text@{}",
            link.col,
            text.col
        );
    }

    #[test]
    fn a_corner_overlay_wrapped_in_a_link_rides_the_first_line() {
        // The live serializer wraps a clickable overlay in an `<a>` (so it
        // becomes a JsClick link). That wrapper is not itself positioned, so the
        // corner-overlay test must see THROUGH a sole-child wrapper to the
        // abspos overlay it holds — else the overlay claims its own row below the
        // content. This is the homepage search bar: a `position:absolute`
        // clear/submit button over the `display:block` input, wrapped in the
        // serializer's anchor, was pushing the whole search field a row above
        // the nav.
        let rows = lay(
            r#"<body><div style="position:relative">
                 <div>comment body</div>
                 <a href="/opts"><button style="position:absolute;top:0;right:0;bottom:0;left:auto">X</button></a>
               </div></body>"#,
            40,
        );
        let first = &rows[0];
        let link = first
            .items
            .iter()
            .find(|i| i.link.is_some())
            .expect("the wrapped overlay link is on the FIRST row");
        assert!(
            link.text.contains('X'),
            "the overlay is the link, got {:?}",
            link.text
        );
        assert!(
            first.items.iter().any(|i| i.text.contains("comment")),
            "the content is on the first row too"
        );
        assert!(link.col as usize > 10, "the overlay is right-aligned");
    }

    #[test]
    fn positioned_overlays_land_by_coordinate_top_or_bottom() {
        // A bottom-anchored badge lands at the BOTTOM of its containing block; a
        // top:0;left:0;right:0 full-bleed layer lands on the FIRST row spanning
        // the width — each where its own offsets place it (CSS 2.1 §10.6.4),
        // not collapsed to a compact inline run. The CB has a real height so the
        // bottom anchor is observable (content alone is one line).
        let rows = lay(
            r#"<body><div style="position:relative;height:6rem">
                 <div>line one</div>
                 <div style="position:absolute;bottom:0;right:0"><a href="/b">BOT</a></div>
                 <div style="position:absolute;top:0;left:0;right:0"><a href="/f">FULL</a></div>
               </div></body>"#,
            40,
        );
        let (r_bot, _) = pos_of(&rows, "BOT");
        let (r_full, c_full) = pos_of(&rows, "FULL");
        // FULL (top:0) rides the first row at the left edge.
        assert_eq!(
            r_full,
            0,
            "full-bleed layer on the first row: {:?}",
            texts(&rows)
        );
        assert_eq!(
            c_full,
            0,
            "full-bleed layer at the left edge: {:?}",
            texts(&rows)
        );
        // BOT (bottom:0) sits below it, near the bottom of the 6rem box.
        assert!(
            r_bot > r_full,
            "the bottom badge lands below the top layer (BOT@{r_bot} FULL@{r_full}): {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn a_semantic_dialog_overlay_surfaces_without_full_geometry() {
        // A role=dialog overlay surfaces on semantics alone (need not cover the
        // viewport geometrically), and defers the page behind it.
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <div role="dialog" style="position:fixed">
                   <p>DialogBody</p><button>Okay</button>
                 </div>
               </body>"#,
            80,
        );
        assert!(shows(&rows, "DialogBody"), "the dialog renders");
        assert!(!shows(&rows, "LoginLink"), "the page is deferred");
    }

    #[test]
    fn an_absolute_full_percent_box_in_a_positioned_ancestor_is_not_a_modal() {
        // `width:100%;height:100%` on an absolute element resolves against its
        // positioned ancestor, NOT the viewport — a cover image inside a sized
        // relative box must not be mistaken for a page-covering modal.
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <div style="position:relative;width:80px;height:48px">
                   <div style="position:absolute;width:100%;height:100%"><a href="/x">Cover</a></div>
                 </div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "LoginLink"),
            "an ancestor-relative full box isn't a modal"
        );
    }

    #[test]
    fn the_topmost_overlay_wins_by_z_index() {
        // Two full-viewport modals: the higher z-index is painted on top, so
        // it's the one we surface (despite coming first in document order).
        let rows = lay(
            r#"<body>
                 <div style="position:fixed;width:100%;height:100%;z-index:50">
                   <a href="/a">TopModal</a>
                 </div>
                 <div style="position:fixed;width:100%;height:100%;z-index:10">
                   <a href="/b">LowModal</a>
                 </div>
               </body>"#,
            80,
        );
        assert!(shows(&rows, "TopModal"), "the higher-z overlay surfaces");
        assert!(!shows(&rows, "LowModal"), "the lower-z overlay is deferred");
    }

    fn image_item(rows: &[Row]) -> &Item {
        rows.iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("an image item")
    }

    #[test]
    fn flex_children_inherit_an_ancestor_anchor_link() {
        // archive's tile: an <a> wraps a flex container holding the image and
        // label. Flex items are laid in a separate sub-pass (layout_subtree),
        // so without threading the inherited Ctx they'd lose the anchor's link
        // — no hover, no click. Both the image and the label must stay links.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/t.png".to_owned(), (8, 8));
        let rows = lay_with_images(
            r#"<body><a href="/dest"><div style="display:flex"><img src="/t.png"><span>Label</span></div></a></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        assert!(
            img.link.is_some(),
            "image inside flex-in-anchor stays a link"
        );
        let label = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Label"))
            .expect("label");
        assert!(
            label.link.is_some(),
            "label inside flex-in-anchor stays a link"
        );
    }

    #[test]
    fn css_box_overrides_intrinsic_image_with_object_fit_cover() {
        // archive.org's collection tile: width:100%; height:160px;
        // object-fit:cover on a small source. The used box is the CSS box
        // (uniform across tiles), cropped — not the intrinsic size.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/t.png".to_owned(), (8, 8));
        let rows = lay_with_images(
            r#"<body><img src="/t.png" style="width:100%;height:160px;object-fit:cover"></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        assert_eq!(img.width, 40, "width:100% fills the content width");
        assert_eq!(img.height, 10, "160px is 10 rows");
        assert!(img.crop, "object-fit:cover crops");
    }

    #[test]
    fn percentage_image_resolves_against_its_definite_ancestor_not_the_flow_box() {
        // The avatar/thumbnail idiom, universal on forums/social sites: a
        // fixed-size wrapper (`width/height:36px`) holding `<img
        // style="width:100%;height:100%">`. The image must fill the 36px box
        // (≈ 5 cols, ≈ 2 rows), NOT the 80-col flow box it sits in — else a
        // tiny avatar reserves a screen-wide, 16-row-tall block and shoves the
        // whole page apart (the firesofheaven.org / XenForo forum-index bug).
        let mut images = ImageSizes::new();
        // Decoded square (the encode source); the used box ignores this scale.
        images.insert("https://example.com/a.png".to_owned(), (6, 3));
        let rows = lay_with_images(
            r#"<body><a style="display:inline-flex;width:36px;height:36px"><img src="/a.png" style="width:100%;height:100%"></a></body>"#,
            80,
            &images,
        );
        let img = image_item(&rows);
        // 36px → 2.25em → ≈5 cols / ≈2 rows; far below the 80-col flow box.
        assert_eq!(
            (img.width, img.height),
            (5, 2),
            "avatar img fills its 36px wrapper, not the flow box"
        );
    }

    #[test]
    fn fullbleed_out_of_flow_background_image_is_dropped() {
        // erome.com's full-page backdrop: `<div class="bg" position:fixed>`
        // holding `<img min-width:100%;height:auto;position:absolute>`. A real
        // browser paints it BEHIND the page; a terminal can't composite layers,
        // and reserving its ~48-row pixel box buries the login box under blank
        // space. Drop it — a decorative background layer renders nothing.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/bg.jpg".to_owned(), (40, 30));
        let rows = lay_with_images(
            r#"<body>
                 <div style="position:fixed;width:100%;height:100%">
                   <img src="/bg.jpg" style="width:auto;min-width:100%;height:auto;position:absolute">
                 </div>
                 <p>Hello</p>
               </body>"#,
            80,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none()),
            "the full-bleed background image is dropped, not laid out"
        );
        // The real content sits at the top, not 48 blank rows down.
        assert!(
            rows.iter().take(2).any(|r| render_row(r).contains("Hello")),
            "real content rides to the top: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn an_in_flow_full_width_image_is_not_treated_as_a_background() {
        // The narrowing guard: an IN-FLOW full-width hero (`<img width:100%>`,
        // no positioning) is real content and must still render its box — only
        // an OUT-OF-FLOW full-bleed backdrop is dropped.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/hero.jpg".to_owned(), (40, 10));
        let rows = lay_with_images(
            r#"<body><img src="/hero.jpg" style="width:100%;height:auto"></body>"#,
            80,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.image.is_some()),
            "an in-flow full-width image still lays out"
        );
    }

    #[test]
    fn a_sized_absolute_cover_image_keeps_its_box() {
        // The other guard: a `position:absolute` cover with a DEFINITE height
        // (`height:100%` against a sized box) is a real thumbnail/cover, not a
        // background — it keeps its box.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cover.jpg".to_owned(), (20, 20));
        let rows = lay_with_images(
            r#"<body><div style="width:80px;height:48px;position:relative"><img src="/cover.jpg" style="position:absolute;width:100%;height:100%"></div></body>"#,
            80,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.image.is_some()),
            "a sized absolute cover keeps its box"
        );
    }

    #[test]
    fn block_images_in_inline_atomic_boxes_flow_into_a_grid() {
        // XenForo's "most reactions" block: a <ul> of inline-block <li>s, each
        // an inline-flex avatar wrapper around a `display:block` <img>. The
        // block images must NOT each break the line into a vertical tower —
        // they ride the line and wrap into a grid, like any inline image.
        let mut images = ImageSizes::new();
        for n in 0..6 {
            images.insert(format!("https://example.com/a{n}.png"), (4, 2));
        }
        let lis: String = (0..6)
            .map(|n| {
                format!(
                    r#"<li style="display:inline-block"><a style="display:inline-flex;width:32px;height:32px"><img src="/a{n}.png" style="display:block;width:100%;height:100%"></a></li>"#
                )
            })
            .collect();
        let html = format!("<body><ul style=\"list-style-type:none\">{lis}</ul></body>");
        let rows = lay_with_images(&html, 24, &images);
        let img_rows = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| i.image.is_some()))
            .count();
        let total: usize = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .count();
        assert_eq!(total, 6, "all six avatars present");
        // 6 avatars at ~4 cells wrap into a couple of shelves — a grid, NOT
        // one image per row (which was the bug: 6 separate rows).
        assert!(
            (2..=3).contains(&img_rows),
            "block avatars wrap into a grid, got {img_rows} image rows"
        );
    }

    #[test]
    fn inline_block_tiles_lay_in_a_row_not_a_column() {
        // archive.org's hero media-count tiles: a block of `inline-block` tiles,
        // each an icon block over a count block. The line model towers each onto
        // its own row (a 9-tall column); `flow_inline_box_grid` lays each as a
        // sub-box and packs them onto one shelf — a row.
        let mut images = ImageSizes::new();
        for n in 0..9 {
            images.insert(format!("https://example.com/i{n}.svg"), (4, 2));
        }
        let tiles: String = (0..9)
            .map(|n| {
                format!(
                    r#"<a style="display:inline-block;width:6ch"><div style="display:block"><img src="/i{n}.svg"></div><div>{n}M</div></a>"#
                )
            })
            .collect();
        let rows = lay_with_images(&format!("<body><div>{tiles}</div></body>"), 100, &images);
        let imgs: usize = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .count();
        let img_rows = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| i.image.is_some()))
            .count();
        assert_eq!(imgs, 9, "all nine icons present");
        assert_eq!(
            img_rows, 1,
            "the nine tiles share one shelf, got {img_rows}"
        );
    }

    #[test]
    fn single_row_inline_block_links_stay_inline_and_spaced() {
        // archive.org's footer nav: a `<ul>` of single-row `<li
        // display:inline-block>` text links. These must NOT route through the box
        // grid (it spaces by margins, so padding-spaced links fuse —
        // "ABOUTBLOGEVENTS"); the line model keeps them on one row, separated.
        let rows = lay(
            r#"<body><ul><li style="display:inline-block;padding-left:15px"><a href="/a">ABOUT</a></li><li style="display:inline-block;padding-left:15px"><a href="/b">BLOG</a></li><li style="display:inline-block;padding-left:15px"><a href="/h">HELP</a></li></ul></body>"#,
            80,
        );
        // The three links share ONE row (a row, not a vertical column)...
        let link_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.items.iter().any(|i| i.link.is_some()))
            .map(|(y, _)| y)
            .collect();
        assert_eq!(
            link_rows,
            vec![0],
            "nav links share one row, got {link_rows:?}"
        );
        // ...and they stay SEPARATED, not fused: reconstructing the row keeps a
        // space between each (the grid path would butt them: "ABOUTBLOGHELP").
        let mut line = vec![' '; 80];
        for it in &rows[0].items {
            for (k, ch) in it.text.chars().enumerate() {
                if let Some(slot) = line.get_mut(it.col as usize + k) {
                    *slot = ch;
                }
            }
        }
        let text: String = line.into_iter().collect();
        assert!(
            text.contains("ABOUT BLOG HELP"),
            "nav links stay spaced, not fused: {:?}",
            text.trim()
        );
    }

    #[test]
    fn a_stacked_column_beside_a_left_float_clears_it() {
        // XenForo's latest-post block: a left-floated avatar, then a column of
        // title/date. The column must lay to the float's RIGHT, not paint over
        // it at column 0 (flex/stack layout used the block box, ignoring the
        // float-narrowed band).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/av.png".to_owned(), (5, 2));
        let rows = lay_with_images(
            r#"<body><div style="display:inline-flex"><div style="float:left"><img src="/av.png" style="display:block"></div><div style="display:flex;flex-direction:column"><div>Title</div><div>Date</div></div></div></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        let title = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Title"))
            .expect("title");
        assert!(
            title.col >= img.col + img.width,
            "title clears the floated avatar (title col {}, avatar ends {})",
            title.col,
            img.col + img.width
        );
    }

    #[test]
    fn nowrap_overflow_hidden_truncates_with_an_ellipsis() {
        // The single-line-ellipsis card idiom (`white-space:nowrap;
        // overflow:hidden`): a too-long line is clipped at the box edge with an
        // ellipsis instead of overflowing it (a forum post title bleeding into
        // the sidebar). Laid in a 10-cell box.
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow:hidden">Permanently banned forever</div></body>"#,
            10,
        );
        let line = texts(&rows)[0].clone();
        assert!(
            display_width(&line) <= 10,
            "clipped to the box width: {line:?}"
        );
        assert!(
            line.ends_with('…'),
            "truncation marked with an ellipsis: {line:?}"
        );
        assert!(
            line.starts_with("Perman"),
            "keeps the leading text: {line:?}"
        );
    }

    #[test]
    fn absolutely_positioned_sibling_rows_flow_as_rows_not_one_pile() {
        // A browser places `position:absolute` children at their `top`. When a
        // positioned container holds several such siblings at DISTINCT tops (a
        // positioned layout — any virtual scroller's rows), we can't pixel-place
        // them, but they must each take their own row in document order, NOT
        // collapse onto one inline line. (The single-overlay/badge case keeps
        // the compact inline path — see the overlay tests.)
        let rows = lay(
            r#"<body><div style="position:relative;height:60px">
                 <div style="position:absolute;top:0px">alpha</div>
                 <div style="position:absolute;top:20px">beta</div>
                 <div style="position:absolute;top:40px">gamma</div>
               </div></body>"#,
            40,
        );
        let t = texts(&rows);
        let pos = |s: &str| t.iter().position(|l| l.contains(s)).unwrap();
        assert!(
            pos("alpha") < pos("beta") && pos("beta") < pos("gamma"),
            "rows in document order: {t:?}"
        );
        assert!(
            !t.iter().any(|l| l.contains("alpha") && l.contains("beta")),
            "rows are not piled onto one line: {t:?}"
        );
    }

    #[test]
    fn a_lone_absolute_overlay_still_flows_inline_not_as_its_own_block() {
        // The general rule must NOT disturb a single positioned overlay/badge:
        // with no distinct-top sibling it stays on the compact inline path, so
        // the content it overlays still leads.
        let rows = lay(
            r#"<body><div style="position:relative">label
                 <span style="position:absolute;top:0;right:0">x</span>
               </div></body>"#,
            40,
        );
        let t = texts(&rows);
        assert!(
            t.first().is_some_and(|l| l.contains("label")),
            "the lone overlay didn't push content into its own stack: {t:?}"
        );
    }

    #[test]
    fn a_nowrap_box_without_overflow_clip_is_not_truncated() {
        // `nowrap` alone (no overflow clip) still overflows, as before — only
        // the clip context truncates.
        let rows = lay(
            r#"<body><div style="white-space:nowrap">Permanently banned forever</div></body>"#,
            10,
        );
        let line = texts(&rows)[0].clone();
        assert!(
            line.contains("forever"),
            "uncut without overflow clip: {line:?}"
        );
        assert!(!line.contains('…'), "no ellipsis without a clip: {line:?}");
    }

    #[test]
    fn an_inline_horizontal_margin_separates_an_icon_from_its_label() {
        // An icon `<i style="margin-right:…">` abutting its label: the margin is
        // the only separator, dropped before — a terminal renders it as one
        // cell of gap. (Subforum icons crammed against their text.)
        let rows = lay(
            r#"<body><span><i style="margin-right:1em">X</i>Label</span></body>"#,
            40,
        );
        let line = texts(&rows)[0].clone();
        assert!(
            line.contains("X Label"),
            "the icon's right margin separates it from the label: {line:?}"
        );
    }

    #[test]
    fn a_grid_of_percentage_image_tiles_lays_uniform_columns() {
        // archive.org's Top-Collections grid: `display:grid` with
        // `repeat(auto-fill, minmax(16rem,1fr))` tracks, each cell a `width:100%`
        // image over a caption. The grid TRACKS size the cells (not the cells'
        // content), so every `width:100%` image is exactly its track width —
        // a uniform grid, not content-sized columns. (This is the path
        // `@supports (display:grid)` selects over the flex fallback.)
        let mut images = ImageSizes::new();
        for n in ["a", "b", "c", "d", "e", "f"] {
            images.insert(format!("https://example.com/img/{n}"), (22, 11));
        }
        let tile = |n: &str| {
            format!(
                r#"<article>
                <a href="/x" style="display:block;">
                  <div style="display:flex;width:100%;flex-direction:column;">
                    <div style="display:block;">
                      <img src="/img/{n}" style="width:100%;height:160px;object-fit:cover;">
                    </div>
                    <h3>Collection {n}</h3>
                  </div>
                </a>
              </article>"#
            )
        };
        let body = format!(
            r#"<body><section style="display:grid;grid-template-columns:repeat(auto-fill,minmax(16rem,1fr));column-gap:1.7rem;">{}{}{}{}{}{}</section></body>"#,
            tile("a"),
            tile("b"),
            tile("c"),
            tile("d"),
            tile("e"),
            tile("f")
        );
        let rows = lay_with_images(&body, 100, &images);
        let img_widths: Vec<u16> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| matches!(i.kind, ItemKind::Image) && i.width > 0)
            .map(|i| i.width)
            .collect();
        assert_eq!(img_widths.len(), 6, "all six tile images laid out");
        // Every image is the SAME width (uniform tracks), and not the full row.
        let first = img_widths[0];
        for w in &img_widths {
            assert_eq!(*w, first, "uniform track widths: {img_widths:?}");
        }
        assert!(first < 60, "a track, not the full 100-cell row: {first}");
        let top_row_imgs = rows[0]
            .items
            .iter()
            .filter(|i| matches!(i.kind, ItemKind::Image))
            .count();
        assert!(
            top_row_imgs >= 2,
            "tiles pack into columns ({top_row_imgs} on the first row)"
        );
    }

    #[test]
    fn svg_title_desc_do_not_leak_into_a_button_label() {
        // SVG `<title>`/`<desc>` are non-rendered accessibility metadata (SVG
        // spec); a browser never paints them. An icon-only `<button>` must
        // surface its accessible name (`aria-label`), NOT the screen-reader
        // description — archive.org's login icon dumped "User icon An
        // illustration of a person's head and chest." as a 60-cell label.
        let rows = lay(
            r#"<body><button aria-label="Toggle login menu"><svg><title>User icon</title><desc>An illustration of a person's head and chest.</desc><path d="m20"/></svg></button></body>"#,
            60,
        );
        let line = texts(&rows).join(" ");
        assert!(!line.contains("User icon"), "title not rendered: {line:?}");
        assert!(
            !line.contains("illustration"),
            "desc not rendered: {line:?}"
        );
        assert!(
            line.contains("Toggle login menu"),
            "the accessible name is the visible label: {line:?}"
        );
    }

    #[test]
    fn float_is_dropped_on_a_block_level_flex_item() {
        // CSS ignores `float` on a flex item. A `display:flex` row holding
        // `display:block;float:right` items lays them as flex columns (packed,
        // on-canvas), not floated to the page edge — archive.org's `.upload`
        // (`display:block;float:right` in a `display:flex` section) shot
        // off-canvas right. An `inline-flex` parent is the carve-out (next
        // test) — there a block child still floats.
        let rows = lay(
            r#"<body><div style="display:flex"><div style="display:block;float:right">AAA</div><div style="display:block;float:right">BBB</div></div></body>"#,
            40,
        );
        let aaa = rows
            .iter()
            .enumerate()
            .find_map(|(ri, r)| {
                r.items
                    .iter()
                    .find(|i| i.text.contains("AAA"))
                    .map(|i| (ri, i.col))
            })
            .expect("AAA laid out");
        let bbb = rows
            .iter()
            .enumerate()
            .find_map(|(ri, r)| {
                r.items
                    .iter()
                    .find(|i| i.text.contains("BBB"))
                    .map(|i| (ri, i.col))
            })
            .expect("BBB laid out");
        assert_eq!(aaa.0, bbb.0, "both items share one row (flex columns)");
        assert!(
            aaa.1 < bbb.1 && aaa.1 < 4,
            "items pack from the left, not floated right (AAA col {}, BBB col {})",
            aaa.1,
            bbb.1
        );
    }

    #[test]
    fn a_block_float_in_an_inline_flex_parent_still_floats() {
        // The carve-out for the block-level float drop above: an `inline-flex`
        // container is laid by INLINE recursion (not real flex columns), so a
        // BLOCK-level floated child there still needs its float to sit beside
        // its siblings — XenForo's latest-post avatar. (Companion to
        // `a_stacked_column_beside_a_left_float_clears_it`.)
        let mut images = ImageSizes::new();
        images.insert("https://example.com/av.png".to_owned(), (5, 2));
        let rows = lay_with_images(
            r#"<body><div style="display:inline-flex"><div style="float:left"><img src="/av.png" style="display:block"></div><span>beside</span></div></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        let beside = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("beside"))
            .expect("text");
        assert!(
            beside.col >= img.col + img.width,
            "the text clears the still-floated avatar (text col {}, avatar ends {})",
            beside.col,
            img.col + img.width
        );
    }

    #[test]
    fn an_icon_glyph_control_does_not_leak_its_aria_label() {
        // A disclosure trigger whose visible content is a `::before` glyph (the
        // "Toggle expanded" arrow) must NOT also dump its aria-label as body
        // text — the glyph IS the affordance.
        let rows = lay(
            r#"<body><a href="/x" aria-label="Toggle expanded"><i data-trust-before="▾"></i></a></body>"#,
            40,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Toggle expanded"),
            "aria-label suppressed when a glyph shows: {line:?}"
        );
        assert!(line.contains('▾'), "the glyph still renders: {line:?}");
    }

    #[test]
    fn an_aria_haspopup_trigger_does_not_leak_its_label() {
        // An empty disclosure trigger (`aria-haspopup`) opens an AJAX panel we
        // can't action yet; its accessible name is a UI affordance, not body
        // text. The search bar's settings cog (`aria-label="Search"`) must not
        // render a phantom "Search".
        let rows = lay(
            r#"<body><a href="/x" aria-haspopup="true" aria-label="Search"></a></body>"#,
            40,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Search"),
            "an empty haspopup trigger's label is suppressed: {line:?}"
        );
    }

    #[test]
    fn float_on_an_inline_flex_item_flows_inline_not_its_own_row() {
        // CSS ignores `float` on a flex item. An inline-flex split-trigger
        // (`float:left`) inside an inline-flex nav group must flow inline with
        // the label, not drop to its own row (the "Members ▾" toggle).
        let rows = lay(
            r#"<body><div style="display:inline-flex"><span>Members</span><a href="/x" style="display:inline-flex;float:left" data-trust-after="▾"></a></div></body>"#,
            40,
        );
        let members_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("Members")))
            .expect("Members laid out");
        let arrow_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains('▾')))
            .expect("arrow laid out");
        assert_eq!(
            arrow_row, members_row,
            "the floated trigger shares the label's row, not its own"
        );
    }

    #[test]
    fn float_inside_an_absolute_overlay_flows_inline() {
        // A float inside an absolutely-positioned overlay (which we render as a
        // compact inline run) must not break the line — a search bar's
        // magnifier `<i float:left>` in an `position:absolute` icon span.
        let rows = lay(
            r#"<body><span>text</span><span style="position:absolute"><i style="float:left" data-trust-before="🔍"></i></span><span>after</span></body>"#,
            40,
        );
        // Everything stays on one row (the float doesn't flush).
        let real_rows = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| !i.text.is_empty()))
            .count();
        assert_eq!(
            real_rows, 1,
            "the absolute overlay's float doesn't break the line"
        );
    }

    #[test]
    fn block_floats_in_an_absolute_container_pack_horizontally() {
        // Steam's `#global_header` nav: a `position:absolute` `.supernav_container`
        // holding `display:block;float:left` menu items. The abspos box is the
        // floats' formatting context, so they pack into one horizontal row — not
        // the vertical stack we'd get if the abspos parent dropped its BLOCK
        // floats (inline-level floats there still drop — see the overlay test
        // above). Regression for the Steam header stacking vertically.
        let rows = lay(
            r#"<body><div style="position:absolute">
               <a style="display:block;float:left">STORE</a>
               <a style="display:block;float:left">COMMUNITY</a>
               <a style="display:block;float:left">ABOUT</a>
               </div></body>"#,
            80,
        );
        let row_of = |label: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(label)))
                .unwrap_or_else(|| panic!("{label} laid out: {:?}", texts(&rows)))
        };
        let (s, c, a) = (row_of("STORE"), row_of("COMMUNITY"), row_of("ABOUT"));
        assert!(
            s == c && c == a,
            "the three block floats share one row (got STORE@{s} COMMUNITY@{c} ABOUT@{a}): {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn uniform_flex_capsules_shrink_to_equal_columns() {
        // Steam's "Special Offers" rows: a `display:flex` row of same-size image
        // capsules (`width:100%` thumb + a price caption), each
        // `flex-grow:0;flex-shrink:1` (content-sized). A DISCOUNTED capsule shows
        // two prices ("59,99€ 17,99€"), so its caption is wider — and TRust used
        // to size each column to its content's min-content, handing that capsule
        // a wider column and therefore a wider image. With (a) intrinsic-width
        // measurement of `width:100%` images and (b) the CSS flex-shrink
        // algorithm, equal-basis items shrink to EQUAL widths regardless of
        // caption length.
        let mut images = ImageSizes::new();
        for f in ["a", "b", "c"] {
            images.insert(format!("https://example.com/{f}.jpg"), (40, 20));
        }
        let rows = lay_with_images(
            r#"<body><div style="display:flex;width:100%;gap:1px">
                 <a style="display:block;flex-grow:0;flex-shrink:1"><img src="/a.jpg" style="width:100%"><div>9,99€</div></a>
                 <a style="display:block;flex-grow:0;flex-shrink:1"><img src="/b.jpg" style="width:100%"><div>59,99€ 17,99€</div></a>
                 <a style="display:block;flex-grow:0;flex-shrink:1"><img src="/c.jpg" style="width:100%"><div>4,99€</div></a>
               </div></body>"#,
            60,
            &images,
        );
        // The three capsule images fill their (equal) columns: equal widths.
        let widths: Vec<u16> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.width)
            .collect();
        assert_eq!(widths.len(), 3, "three capsule images: {widths:?}");
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "same-size capsules get equal columns regardless of caption width: {widths:?}"
        );
    }

    #[test]
    fn percentage_image_without_a_definite_ancestor_still_fills_the_flow_box() {
        // A genuine full-bleed image (`width:100%` with no sized ancestor)
        // must keep filling the content width — the fallback when no ancestor
        // pins a length width, preserving the prior behaviour.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/hero.png".to_owned(), (8, 4));
        let rows = lay_with_images(
            r#"<body><div><img src="/hero.png" style="width:100%"></div></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        assert_eq!(img.width, 40, "width:100% still fills the flow box");
    }

    #[test]
    fn object_fit_contain_reserves_the_fitted_box_not_a_letterbox() {
        // archive.org collection tile: a small cover with width:100% and a
        // tall box, object-fit:contain. The box would upscale to fill the
        // width (40 wide → 20 tall here), but the renderer never upscales,
        // so it would draw the 20×10 cover and leave ~10 blank rows beneath
        // — the title floated a half-screen below the image. `contain` must
        // reserve what's actually drawn (the fitted, never-upscaled box) so
        // the next content sits directly under the image.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cover.png".to_owned(), (20, 10));
        let rows = lay_with_images(
            r#"<body><img src="/cover.png" style="width:100%;height:100%;object-fit:contain"><p>title</p></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        // The box is the fitted decoded size (20×10), NOT the upscaled
        // width:100% box (40×20) that would reserve ~10 blank rows below.
        assert_eq!(
            (img.width, img.height),
            (20, 10),
            "contain reserves the fitted decoded box, not the upscaled width"
        );
        assert!(!img.crop, "contain does not crop");
        // The title follows the image's (fitted) box + the single block
        // spacer — no half-screen of reserved letterbox rows between them.
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .unwrap();
        let title_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("title")))
            .unwrap();
        assert!(
            title_row - img_row <= img.height as usize + 1,
            "title sits just under the image (row {title_row} vs image box at {img_row}+{})",
            img.height
        );
    }

    #[test]
    fn auto_height_image_reserves_no_letterbox_row_on_a_non_2to1_font() {
        // erome thumbnail: <img width=250 height=250 style="width:100%;
        // height:auto"> in a column. The attr ratio is 1:1, so the height comes
        // from `rows_for_ratio` — which assumes a nominal 2:1 cell (→ used_w/2
        // = 16 rows). But the DECODED box carries the real cell aspect: on a
        // taller-than-2:1 font (foot) the 250×250 square decodes to 32×14, so
        // the renderer (Fit, no upscale) draws only 14 rows and leaves a blank
        // 15th/16th — a black gap between the thumbnail and its count caption.
        // The used box must reserve what's actually drawn: 14 rows, no gap.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/t.jpg".to_owned(), (32, 14));
        // The caption is a margin-less span (like erome's overlay count), so any
        // gap below the image is a letterbox row, not a block margin.
        let rows = lay_with_images(
            r#"<body><div><img width="250" height="250" src="/t.jpg" style="display:block;width:100%;height:auto"></div><span>9 265</span></body>"#,
            32,
            &images,
        );
        let img = image_item(&rows);
        assert_eq!(
            (img.width, img.height),
            (32, 14),
            "auto-height image reserves the drawn box (32x14), not the 2:1 \
             ratio box (32x16) that would letterbox a blank row beneath it"
        );
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .unwrap();
        let caption_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("9 265")))
            .unwrap();
        // The caption sits on the row immediately after the image's drawn box —
        // no reserved letterbox row between them.
        assert_eq!(
            caption_row - img_row,
            img.height as usize,
            "caption sits directly under the image box (row {caption_row} vs \
             image at {img_row} + {} drawn rows)",
            img.height
        );
    }

    #[test]
    fn aspect_ratio_sets_image_height_from_width() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.png".to_owned(), (30, 30));
        let rows = lay_with_images(
            r#"<body><img src="/a.png" style="width:20em;aspect-ratio:2 / 1"></body>"#,
            80,
            &images,
        );
        let img = image_item(&rows);
        // 20em = 40 cols; 2:1 ratio => 40 / (2*2) = 10 rows.
        assert_eq!((img.width, img.height), (40, 10));
        assert!(!img.crop);
    }

    #[test]
    fn img_width_height_attrs_give_default_ratio() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/b.png".to_owned(), (50, 50));
        let rows = lay_with_images(
            r#"<body><img src="/b.png" width="200" height="100" style="width:100%"></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        // width:100% => 40 cols; attr ratio 200:100 = 2:1 => 40/(2*2) = 10 rows.
        assert_eq!((img.width, img.height), (40, 10));
    }

    #[test]
    fn intrinsic_ratio_padding_box_sizes_a_full_height_image() {
        // The responsive-image idiom: a `height:0` container whose percentage
        // `padding-bottom` reserves the box height (CSS 2.1 §8.4: percentage
        // padding resolves against the containing block's WIDTH), with an
        // absolutely-positioned `width/height:100%` image filling it. This is
        // Humble Bundle's bundle tiles and the ubiquitous pre-`aspect-ratio`
        // technique; without it the image collapsed to a 1-row strip (the
        // container's `height:0`).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/tile.jpg".to_owned(), (40, 23));
        let rows = lay_with_images(
            r#"<body><div style="padding-bottom:calc(57.30519481% + 0em);height:0;position:relative;"><img src="/tile.jpg" width="616" height="353" style="display:block;width:100%;height:100%;object-fit:cover;position:absolute"></div></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        // width:100% => 40 cols; padding-bottom 57.305% of the width gives
        // height_px = .573·width, so rows = 40·.573/2 ≈ 11 — not 1.
        assert_eq!(img.width, 40);
        assert_eq!(
            img.height, 11,
            "aspect-box padding must reserve real height"
        );
        assert!(img.crop);
    }

    #[test]
    fn container_aspect_ratio_sizes_a_full_height_image() {
        // Square tile: aspect-ratio:1 wrapper, img width/height:100%.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/c.png".to_owned(), (5, 20));
        let rows = lay_with_images(
            r#"<body><div style="aspect-ratio:1 / 1"><img src="/c.png" style="width:100%;height:100%;object-fit:cover"></div></body>"#,
            40,
            &images,
        );
        let img = image_item(&rows);
        // width 40; height:100% of a 1:1 container of width 40 => 40/2 = 20 rows.
        assert_eq!((img.width, img.height), (40, 20));
        assert!(img.crop);
    }

    #[test]
    fn inline_images_pack_horizontally_and_wrap() {
        let mut images = ImageSizes::new();
        for n in ["a", "b", "c", "d"] {
            images.insert(format!("https://example.com/{n}.png"), (4, 2));
        }
        // Four 4-wide boxes (1-cell gaps) in a width-14 row: three fit
        // (4+1+4+1+4 = 14), the fourth wraps.
        let rows = lay_with_images(
            r#"<body><img src="/a.png"><img src="/b.png"><img src="/c.png"><img src="/d.png"></body>"#,
            14,
            &images,
        );
        let img_rows: Vec<&Row> = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| i.image.is_some()))
            .collect();
        assert_eq!(img_rows.len(), 2, "images wrap onto two rows");
        assert_eq!(
            img_rows[0]
                .items
                .iter()
                .filter(|i| i.image.is_some())
                .count(),
            3,
            "three images pack on the first row"
        );
        assert_eq!(
            img_rows[1]
                .items
                .iter()
                .filter(|i| i.image.is_some())
                .count(),
            1,
            "the fourth wrapped to the next row"
        );
        // Each image row reserves one spacer row beneath (box height 2).
        let first = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .unwrap();
        assert!(
            rows[first + 1]
                .items
                .iter()
                .all(|i| i.width == 0 && i.image.is_none()),
            "a spacer row follows the packed image row"
        );
    }

    #[test]
    fn display_block_image_gets_its_own_line() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.png".to_owned(), (4, 2));
        images.insert("https://example.com/b.png".to_owned(), (4, 2));
        let rows = lay_with_images(
            r#"<html><head><style>img{display:block}</style></head>
               <body><img src="/a.png"><img src="/b.png"></body></html>"#,
            40,
            &images,
        );
        let img_rows: Vec<&Row> = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| i.image.is_some()))
            .collect();
        assert_eq!(img_rows.len(), 2, "display:block stacks each image");
        assert!(
            img_rows
                .iter()
                .all(|r| { r.items.iter().filter(|i| i.image.is_some()).count() == 1 })
        );
    }

    #[test]
    fn wide_image_clamps_width_and_rescales_height() {
        let mut images = ImageSizes::new();
        // 40×20 box in a width-20 viewport → clamp to 20 wide, 10 tall.
        images.insert("https://example.com/big.png".to_owned(), (40, 20));
        let rows = lay_with_images(r#"<body><img src="/big.png"></body>"#, 20, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .unwrap();
        assert_eq!(
            (img.width, img.height),
            (20, 10),
            "aspect preserved on clamp"
        );
    }

    #[test]
    fn pseudo_before_after_inject_generated_content() {
        // A nav-separator pattern plus a `::before` glyph via a CSS hex
        // escape (\00ab = «). Static path: the cascade reads the sheet.
        let rows = lay(
            r#"<html><head><style>
                 .sep::after { content: "|" }
                 .q::before { content: "\00ab" }
               </style></head>
               <body><span class="sep">A</span><span class="sep">B</span>
               <span class="q">quote</span></body></html>"#,
            60,
        );
        let all = texts(&rows).join(" ");
        assert!(all.contains("A|"), "::after separator after A: {all:?}");
        assert!(all.contains("B|"), "::after separator after B: {all:?}");
        // \00ab decodes to « and its trailing whitespace is the escape
        // delimiter (consumed), so the glyph abuts the element's text.
        assert!(all.contains("«quote"), "::before hex-escape glyph: {all:?}");
    }

    #[test]
    fn pseudo_content_attr_function_reads_attribute() {
        // `content: attr(href)` resolves to the element's attribute.
        let rows = lay(
            r#"<html><head><style>a::after{content:attr(href)}</style></head>
               <body><a href="/page">link</a></body></html>"#,
            60,
        );
        let all = texts(&rows).join(" ");
        assert!(all.contains("/page"), "attr(href) injected: {all:?}");
    }

    #[test]
    fn pseudo_rule_does_not_style_the_element_itself() {
        // `div::before{display:none}` must NOT hide the div.
        let rows = lay(
            r#"<html><head><style>div::before{display:none;content:"x"}</style></head>
               <body><div>visible</div></body></html>"#,
            60,
        );
        assert!(
            texts(&rows).join(" ").contains("visible"),
            "the element itself is unaffected by its ::before rule"
        );
    }

    /// The (row, col) of the first item whose text contains `needle`.
    fn pos_of(rows: &[Row], needle: &str) -> (usize, u16) {
        for (r, row) in rows.iter().enumerate() {
            for it in &row.items {
                if it.text.contains(needle) {
                    return (r, it.col);
                }
            }
        }
        panic!("no item containing {needle:?}");
    }

    #[test]
    fn flex_wrap_grid_packs_children_side_by_side_and_wraps() {
        // Three 10-cell boxes (5em·2) in a wrapping flex container at
        // width 24: two fit per shelf (10 + 1 gap + 10 = 21 ≤ 24), the
        // third wraps onto a new band.
        let rows = lay(
            r#"<html><head><style>
                 .grid{display:flex;flex-wrap:wrap}
                 .cell{width:5em}
               </style></head>
               <body><div class="grid">
                 <div class="cell">one</div>
                 <div class="cell">two</div>
                 <div class="cell">three</div>
               </div></body></html>"#,
            24,
        );
        let (r1, c1) = pos_of(&rows, "one");
        let (r2, c2) = pos_of(&rows, "two");
        let (r3, c3) = pos_of(&rows, "three");
        // one|two share a row, side by side (two is one box-width + gap right).
        assert_eq!(r1, r2, "first two cells share a shelf: {:?}", texts(&rows));
        assert_eq!(c1, 0);
        assert_eq!(c2, 11, "second cell sits past the first box + gap");
        // three wrapped to a lower row, back at the left edge.
        assert!(r3 > r1, "third cell wrapped to a new shelf");
        assert_eq!(c3, 0);
    }

    #[test]
    fn flex_shorthand_wins_over_a_preceding_longhand_by_source_order() {
        // `flex-grow:0;flex:1` must resolve to grow:1 — the `flex` shorthand,
        // declared LATER, wins (it's expanded into the longhands in the cascade,
        // so source order decides). Steam's capsules carry exactly this. Two such
        // items (flex base 0, grow 1) SPLIT the row equally, so the second starts
        // near the half. If the longhand wrongly won (grow 0) both would collapse
        // to their content and the second would sit right after the first.
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex}
                 .a,.b{flex-grow:0;flex:1}
               </style></head><body><div class="row">
                 <div class="a">AA</div>
                 <div class="b">BB</div>
               </div></body></html>"#,
            60,
        );
        let (ra, ca) = pos_of(&rows, "AA");
        let (rb, cb) = pos_of(&rows, "BB");
        assert_eq!(ra, rb, "both on one row: {:?}", texts(&rows));
        assert_eq!(ca, 0);
        assert!(
            cb >= 25,
            "grow:1 split the row, so the second item starts near the half (col {cb})"
        );
    }

    #[test]
    fn flex_wrap_breaks_lines_by_flex_base_size_not_content() {
        // Steam's "Featured tag" 2-up capsule grid: `flex:1` (base 0) cells with
        // `min-width:40%`/`max-width:50%` and content WIDER than 50%. A browser
        // breaks flex lines by the flex BASE size (0, clamped up to the 40% min),
        // so 40% + gap + 40% fits TWO per row; the old code packed by the
        // content/max-width (~50%+), breaking one-per-row and collapsing the grid
        // to a single column. The third cell still wraps to the next line.
        let wide = "X".repeat(55); // wider than 50% of the 100-cell row
        let html = format!(
            r#"<html><head><style>
                 .row{{display:flex;flex-wrap:wrap;gap:2px}}
                 .cap{{flex:1;min-width:40%;max-width:50%;white-space:nowrap}}
               </style></head><body><div class="row">
                 <div class="cap">A{wide}</div>
                 <div class="cap">B{wide}</div>
                 <div class="cap">C{wide}</div>
               </div></body></html>"#
        );
        let rows = lay(&html, 100);
        let (ra, _) = pos_of(&rows, "AXXXXX");
        let (rb, cb) = pos_of(&rows, "BXXXXX");
        let (rc, _) = pos_of(&rows, "CXXXXX");
        assert_eq!(ra, rb, "first two capsules share a row: {:?}", texts(&rows));
        assert!(cb >= 40, "second capsule sits past the 40% first column");
        assert!(rc > ra, "the third capsule wraps to the next row");
    }

    #[test]
    fn flex_wrap_min_width_var_cells_pack_into_a_grid() {
        // archive.org's "Top Collections": a flex-wrap container whose cells
        // size via `min-width: var(--x, 8em)` + `max-width: var(--y, 1fr)` and
        // hold `width:100%` content. Two general behaviours under test:
        //  (1) `var(--name, fallback)` resolves to its fallback (stylesheets are
        //      gone by layout time, so the custom prop is undefined → fallback);
        //  (2) `flow_flex_wrap` honours `min-width`, so the cell lays out to its
        //      floor instead of letting its `width:100%` content balloon to the
        //      whole row. Without either, the cells packed one-per-row.
        let rows = lay(
            r#"<html><body>
               <section style="display:flex;flex-wrap:wrap">
                 <article style="min-width:var(--w, 8em);max-width:var(--m, 1fr)"><div style="width:100%">one</div></article>
                 <article style="min-width:var(--w, 8em);max-width:var(--m, 1fr)"><div style="width:100%">two</div></article>
                 <article style="min-width:var(--w, 8em);max-width:var(--m, 1fr)"><div style="width:100%">three</div></article>
               </section></body></html>"#,
            40,
        );
        let (r1, c1) = pos_of(&rows, "one");
        let (r2, c2) = pos_of(&rows, "two");
        let (r3, _) = pos_of(&rows, "three");
        // 8em = 16 cells; at width 40, two cells + a gap fit (16+1+16 ≤ 40).
        assert_eq!(
            r1,
            r2,
            "min-width var() cells share a shelf: {:?}",
            texts(&rows)
        );
        assert_eq!(c1, 0);
        assert!(
            c2 >= 16,
            "second cell reserves the first's min-width column"
        );
        assert!(r3 > r1, "third cell wraps to a new shelf");
    }

    #[test]
    fn flex_column_gap_controls_spacing() {
        // 5em (10-cell) boxes; an explicit 2em (4-cell) column-gap puts the
        // second at 14 instead of the default-gap 11.
        let rows = lay(
            r#"<html><head><style>.r{display:flex;column-gap:2em}.c{width:5em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&rows, "a").1, 0);
        assert_eq!(pos_of(&rows, "b").1, 14, "10-cell box + 4-cell gap");
        // The `gap` shorthand's column component (2nd value) works too.
        let rows = lay(
            r#"<html><head><style>.r{display:flex;gap:1em 3em}.c{width:5em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(
            pos_of(&rows, "b").1,
            16,
            "gap shorthand column = 3em = 6 cells"
        );
    }

    #[test]
    fn text_indent_offsets_only_the_first_line() {
        // 2em (4-cell) indent: the first line starts at col 4; the wrapped
        // continuation returns to the left edge.
        let rows = lay(
            r#"<html><head><style>p{text-indent:2em}</style></head>
               <body><p>aaaa bbbb cccc dddd</p></body></html>"#,
            12,
        );
        let lines: Vec<String> = texts(&rows).into_iter().filter(|l| !l.is_empty()).collect();
        assert!(
            lines[0].starts_with("    aaaa"),
            "first line indented 4 cells: {lines:?}"
        );
        assert!(
            !lines[1].starts_with(' '),
            "wrapped line not indented: {lines:?}"
        );
    }

    #[test]
    fn full_border_frames_the_content() {
        let rows = lay_b(
            r#"<body><div style="border:1px solid">Carded</div></body>"#,
            40,
        );
        let all = texts(&rows);
        assert!(
            all.iter().any(|l| l.contains('┌') && l.contains('┐')),
            "top corners: {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains('│') && l.contains("Carded")),
            "content flanked by bars: {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains('└') && l.contains('┘')),
            "bottom corners: {all:?}"
        );
    }

    #[test]
    fn border_bottom_is_a_rule_with_no_sides() {
        let rows = lay_b(
            r#"<body><h2 style="border-bottom:1px solid">Section</h2></body>"#,
            40,
        );
        let all = texts(&rows);
        assert!(all.iter().any(|l| l.contains("Section")), "{all:?}");
        assert!(
            all.iter().any(|l| l.trim_start().starts_with('─')),
            "a horizontal rule under the heading: {all:?}"
        );
        assert!(
            !all.iter().any(|l| l.contains('┌') || l.contains('│')),
            "no frame corners or vertical bars: {all:?}"
        );
    }

    #[test]
    fn border_left_is_a_gutter_bar() {
        let rows = lay_b(
            r#"<body><blockquote style="border-left:1px solid">Quote</blockquote></body>"#,
            40,
        );
        let all = texts(&rows);
        assert!(
            all.iter().any(|l| l.contains('│') && l.contains("Quote")),
            "left bar beside the quote: {all:?}"
        );
        assert!(
            !all.iter().any(|l| l.contains('┌') || l.contains('─')),
            "no top/bottom edges: {all:?}"
        );
    }

    #[test]
    fn bordered_box_clips_overflow_keeping_the_right_border() {
        // Interior content wider than the box used to paint over the right
        // border column (the renderer just concatenates over-wide items), so
        // the right bar vanished. A border is a hard boundary: content clips.
        let rows = lay_b(
            r#"<body><div style="overflow:hidden;border:1px solid;width:20ch">SUPERCALIFRAGILISTICEXPIALIDOCIOUS_overflow</div></body>"#,
            40,
        );
        let all = texts(&rows);
        // Every framed row (top edge, content, bottom edge) ends at the same
        // right border: the content row must still carry its right `│`.
        assert!(
            all.iter().any(|l| l.contains('┐')),
            "top-right corner present: {all:?}"
        );
        assert!(
            all.iter()
                .any(|l| l.contains('│') && l.contains("SUPERCALI") && l.trim_end().ends_with('│')),
            "content row keeps its right bar (overflow clipped): {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains('┘')),
            "bottom-right corner present: {all:?}"
        );
    }

    #[test]
    fn bordered_element_inside_a_link_stays_clickable() {
        // A clickable ancestor (`<a>`) wrapping a bordered block (a tab
        // `<div>` with `border-bottom`) must keep its interior a link: the
        // bordered sub-pass inherits the enclosing context instead of rooting
        // it. Regression — borders used to drop the link, so SL Marketplace's
        // tab labels stopped being clickable once they gained a border.
        let rows = lay_b(
            r#"<body><a href="x-trust-js:6:#"><div style="border-bottom:4px solid">Items</div></a></body>"#,
            40,
        );
        let item = find(&rows, "Items");
        assert!(
            item.is_interactive(),
            "the bordered tab label is still a link"
        );
        assert!(
            matches!(item.link, Some(crate::doc::Link::JsClick { .. })),
            "it routes through the live click marker: {:?}",
            item.link
        );
    }

    #[test]
    fn bordered_carousel_keeps_its_right_frame_bar() {
        // A horizontal-scroll carousel inside a right-bordered box: the right
        // frame bar lands at the band edge, inside the strip span. It must be
        // flagged as static chrome so the render-time clip (`visible_col`)
        // draws it, not clips it as off-screen strip content (the live SL
        // Marketplace bug: the carousel's right border vanished on strip rows).
        let dom = Dom::parse_document(
            r#"<body><div style="overflow:hidden;border:1px solid">
                 <div style="width:100000px">
                   <div style="float:left;width:18ch;border:1px solid">one</div>
                   <div style="float:left;width:18ch;border:1px solid">two</div>
                   <div style="float:left;width:18ch;border:1px solid">three</div>
                 </div>
               </div></body>"#,
        );
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            50,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            true,
        );
        assert!(
            !carousels.is_empty(),
            "an overflowing strip forms a carousel"
        );
        let c = &carousels[0];
        let fr = c.frame_right.expect("right frame bar column recorded");
        let row = c.start; // a strip row
        let bar = rows[row]
            .items
            .iter()
            .find(|it| it.kind == ItemKind::Border && it.col == fr)
            .expect("right frame bar item present on a strip row");
        assert_eq!(
            visible_col(&carousels, row, bar),
            Some(fr),
            "the frame bar renders at its fixed column, not clipped away"
        );
    }

    #[test]
    fn floated_bordered_element_does_not_recurse_forever() {
        // A floated element that ALSO has a border used to ping-pong between
        // `flow_float` (sets `float_skip`, clears `inner_border_box`) and
        // `flow_bordered` (the reverse) → stack overflow. It must terminate and
        // produce a framed, floated box.
        let rows = lay_b(
            r#"<body><div style="float:left;border:1px solid;width:10ch">Tile</div><p>After the float runs alongside.</p></body>"#,
            40,
        );
        let all = texts(&rows);
        assert!(
            all.iter().any(|l| l.contains('┌') && l.contains('┐')),
            "floated box is framed: {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains("Tile")),
            "float content present: {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains("After the float")),
            "following content laid out: {all:?}"
        );
    }

    #[test]
    fn border_style_picks_the_glyph_weight() {
        // double → double-line, thick → heavy box-drawing.
        let dbl = lay_b(r#"<body><div style="border:3px double">D</div></body>"#, 30);
        assert!(
            texts(&dbl)
                .iter()
                .any(|l| l.contains('╔') && l.contains('╗')),
            "double: {:?}",
            texts(&dbl)
        );
        let thick = lay_b(
            r#"<body><div style="border:thick solid">T</div></body>"#,
            30,
        );
        assert!(
            texts(&thick)
                .iter()
                .any(|l| l.contains('┏') && l.contains('┓')),
            "thick: {:?}",
            texts(&thick)
        );
    }

    #[test]
    fn borders_off_by_default_draw_no_chrome() {
        // The production default (`lay`, borders off): a bordered box renders
        // its content with NO box-drawing — terminal vertical space is saved.
        // `set borders on` (`lay_b`) is the opt-in, covered by the tests above.
        let rows = lay(
            r#"<body><div style="border:1px solid">Carded</div></body>"#,
            40,
        );
        let all = texts(&rows);
        assert!(all.iter().any(|l| l.contains("Carded")), "content: {all:?}");
        assert!(
            !all.iter()
                .any(|l| l.contains('┌') || l.contains('┐') || l.contains('│') || l.contains('└')),
            "no border glyphs when borders are off: {all:?}"
        );
    }

    #[test]
    fn justify_content_distributes_free_space() {
        // 3em (6-cell) boxes, default gap 1 → 13 used of 40, free 27.
        let center = lay(
            r#"<html><head><style>.r{display:flex;justify-content:center}.c{width:3em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&center, "a").1, 13, "centered: leading = free/2");
        let end = lay(
            r#"<html><head><style>.r{display:flex;justify-content:flex-end}.c{width:3em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&end, "a").1, 27, "flex-end: leading = free");
        let between = lay(
            r#"<html><head><style>.r{display:flex;justify-content:space-between}.c{width:3em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&between, "a").1, 0, "space-between: first at left");
        assert_eq!(
            pos_of(&between, "b").1,
            34,
            "space-between: second pushed right"
        );
    }

    #[test]
    fn grid_justify_content_centers_a_shelf() {
        // A `display:grid` (shelf-packed) container honours justify-content
        // per shelf, just like a flex row: 5em·2 = 10-cell boxes, gap 1 →
        // used 21 of 40, free 19, centred leading = 9.
        let rows = lay(
            r#"<html><head><style>.g{display:grid;justify-content:center}.c{width:5em}</style></head>
               <body><div class="g"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&rows, "a").1, 9, "{:?}", texts(&rows));
    }

    #[test]
    fn grid_template_columns_places_a_fixed_sidebar_beside_a_flexible_main() {
        // GitHub's profile `Layout`: a two-track grid, a fixed sidebar (column
        // 1) beside a flexible main column (column 2, `minmax(0,1fr)`). The
        // page's own `grid-template-columns` drives this — without honoring it
        // the main column collapsed to min-content (one word per line).
        let rows = lay(
            r#"<html><head><style>
                 .g{display:grid;grid-template-columns:20em minmax(0,1fr)}
                 .side{grid-column:1}
                 .main{grid-column:2}
               </style></head>
               <body><div class="g">
                 <div class="side">sidebar</div>
                 <div class="main">the main column has plenty of room to breathe here</div>
               </div></body></html>"#,
            80,
        );
        let (sr, sc) = pos_of(&rows, "sidebar");
        let (mr, mc) = pos_of(&rows, "plenty");
        assert_eq!(sr, mr, "sidebar and main share a row: {:?}", texts(&rows));
        assert_eq!(sc, 0, "sidebar at the left edge");
        // 20em = 40 cells; main starts past the sidebar track (+gap).
        assert!(
            mc >= 40,
            "main column sits right of the 40-cell sidebar: {mc}"
        );
        // Main is wide, not one-word-per-line: several words share its first row.
        let main_row = render_row(&rows[mr]);
        assert!(
            main_row.contains("plenty of room"),
            "main text flows wide, not per-word: {main_row:?}"
        );
    }

    #[test]
    fn grid_template_columns_fr_tracks_split_space_evenly() {
        // `1fr 1fr 1fr` makes three equal columns sharing the free space.
        let rows = lay(
            r#"<html><head><style>.g{display:grid;grid-template-columns:1fr 1fr 1fr}</style></head>
               <body><div class="g"><div>aaa</div><div>bbb</div><div>ccc</div></div></body></html>"#,
            62,
        );
        let (ra, ca) = pos_of(&rows, "aaa");
        let (rb, cb) = pos_of(&rows, "bbb");
        let (rc, cc) = pos_of(&rows, "ccc");
        assert_eq!(ra, rb, "fr columns share a row: {:?}", texts(&rows));
        assert_eq!(ra, rc);
        assert_eq!(ca, 0);
        // 62 − 2 gaps = 60 / 3 ≈ 20 per track → columns at 0, ~21, ~42.
        assert!((20..=22).contains(&cb), "second fr column ~21: {cb}");
        assert!(cc >= 40, "third fr column to the right: {cc}");
    }

    #[test]
    fn grid_column_span_lays_an_item_across_tracks() {
        // `grid-column: 1 / -1` (span all) is the full-width banner idiom; the
        // following auto-placed items flow onto the next row.
        let rows = lay(
            r#"<html><head><style>.g{display:grid;grid-template-columns:1fr 1fr}
                 .full{grid-column:1 / -1}</style></head>
               <body><div class="g">
                 <div class="full">banner</div>
                 <div>left</div><div>right</div>
               </div></body></html>"#,
            40,
        );
        let (br, _) = pos_of(&rows, "banner");
        let (lr, lc) = pos_of(&rows, "left");
        let (rr, rc) = pos_of(&rows, "right");
        assert!(
            lr > br,
            "banner spans row 1; left drops below: {:?}",
            texts(&rows)
        );
        assert_eq!(lr, rr, "left and right share row 2");
        assert_eq!(lc, 0);
        assert!(rc >= 19, "right column in the second half: {rc}");
    }

    #[test]
    fn grid_without_a_template_falls_back_to_shelf_pack() {
        // A `display:grid` with no resolvable `grid-template-columns` keeps the
        // shelf-packed behaviour (danbooru's post grid) — grid-template support
        // is additive, never a regression for templateless grids.
        let rows = lay(
            r#"<html><head><style>.g{display:grid}.c{width:5em}</style></head>
               <body><div class="g"><div class="c">one</div><div class="c">two</div></div></body></html>"#,
            40,
        );
        let (r1, _) = pos_of(&rows, "one");
        let (r2, c2) = pos_of(&rows, "two");
        assert_eq!(r1, r2, "shelf-packed side by side: {:?}", texts(&rows));
        assert_eq!(c2, 11, "5em box (10 cells) + 1 gap");
    }

    #[test]
    fn grid_repeat_auto_fill_counts_columns_against_the_container() {
        // `repeat(auto-fill, 10em)` at width 64: 20-cell tracks, gap 1 →
        // floor((64+1)/(20+1)) = 3 columns; the fourth item wraps.
        let rows = lay(
            r#"<html><head><style>.g{display:grid;grid-template-columns:repeat(auto-fill,10em)}</style></head>
               <body><div class="g"><div>a</div><div>b</div><div>c</div><div>d</div></div></body></html>"#,
            64,
        );
        let (ra, _) = pos_of(&rows, "a");
        let (rc, _) = pos_of(&rows, "c");
        let (rd, _) = pos_of(&rows, "d");
        assert_eq!(
            ra,
            rc,
            "three columns share the first row: {:?}",
            texts(&rows)
        );
        assert!(rd > ra, "the fourth item wraps to a new row");
    }

    #[test]
    fn img_width_attribute_sizes_a_replaced_element() {
        // A bare `<img width="64">` (no CSS width) is an HTML presentation hint:
        // 64px → 8 cells, height from the decoded aspect ratio. GitHub's
        // achievement badge relies on this — without it the badge decoded at its
        // intrinsic size (~22 cells) and ballooned.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/badge.png".to_owned(), (22, 11));
        let rows = lay_with_images(
            r#"<body><img src="/badge.png" width="64"></body>"#,
            80,
            &images,
        );
        let img = image_item(&rows);
        assert_eq!(img.width, 8, "64px width attr → 8 cells");
        assert_eq!(img.height, 4, "height scales with the decoded box: 11·8/22");
    }

    #[test]
    fn align_items_center_offsets_a_short_column() {
        // Two side-by-side columns of unequal height; align-items:center
        // drops the short column down within the tall column's height.
        let rows = lay(
            r#"<html><head><style>.r{display:flex;align-items:center}.c{width:10em}</style></head>
               <body><div class="r"><div class="c">one<br>two<br>three</div>
               <div class="c">solo</div></div></body></html>"#,
            60,
        );
        let tall_top = pos_of(&rows, "one").0;
        let solo = pos_of(&rows, "solo").0;
        assert!(
            solo > tall_top,
            "short column centred below the tall one's top: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn flex_order_reorders_items_visually() {
        // `order:-1` lays the second item out before the first; the source
        // DOM (and selection nodes) are untouched — only columns move.
        let rows = lay(
            r#"<html><head><style>.r{display:flex}.first{order:1}</style></head>
               <body><div class="r"><div class="first">alpha</div><div>beta</div></div></body></html>"#,
            40,
        );
        assert!(
            pos_of(&rows, "beta").1 < pos_of(&rows, "alpha").1,
            "order:1 pushes alpha after beta: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn letter_spacing_tracks_characters() {
        // 0.5em ≈ 8px ≈ one cell of gap between each character.
        let rows = lay(
            r#"<body><p style="letter-spacing:0.5em">abc</p></body>"#,
            40,
        );
        assert!(
            texts(&rows).iter().any(|l| l.contains("a b c")),
            "tracked: {:?}",
            texts(&rows)
        );
        // Sub-cell tracking rounds to nothing — no terminal half-cells.
        let rows = lay(
            r#"<body><p style="letter-spacing:0.1em">abc</p></body>"#,
            40,
        );
        assert!(
            texts(&rows)
                .iter()
                .any(|l| l.contains("abc") && !l.contains("a b c")),
            "subtle tracking is a no-op: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn wide_glyphs_advance_two_terminal_cells() {
        // A CJK glyph renders two cells wide; the following inline item must
        // start at col 2, not col 1. `chars().count()` mis-measured this and
        // drifted aligned/`pre` columns (the wttr.in-class bug) — we now use
        // the same `unicode-width` ratatui renders with.
        let rows = lay("<body><pre>中<span>X</span></pre></body>", 40);
        assert_eq!(
            pos_of(&rows, "X").1,
            2,
            "wide glyph = 2 cells: {:?}",
            texts(&rows)
        );
        // A combining mark adds no width (zero cells).
        let rows = lay("<body><pre>e\u{0301}<span>Y</span></pre></body>", 40);
        assert_eq!(
            pos_of(&rows, "Y").1,
            1,
            "base+combining = 1 cell: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn list_style_position_inside_aligns_wrap_under_the_marker() {
        // outside (default): the content hangs to the right of the marker, so
        // a wrapped/second line indents under the content. inside: the marker
        // joins the content flow, so the second line returns to the marker.
        let outside = lay("<body><ul><li>one<br>two</li></ul></body>", 40);
        let inside = lay(
            r#"<body><ul style="list-style-position:inside"><li>one<br>two</li></ul></body>"#,
            40,
        );
        // First line identical in both; the marker sits at the list margin.
        assert_eq!(pos_of(&outside, "one").1, pos_of(&inside, "one").1);
        // Second line: outside under the content, inside under the marker.
        assert!(
            pos_of(&inside, "two").1 < pos_of(&outside, "two").1,
            "inside returns the wrap to the marker margin: out={} in={}",
            pos_of(&outside, "two").1,
            pos_of(&inside, "two").1
        );
    }

    #[test]
    fn flex_wrap_thumbnail_grid_lays_images_in_a_grid() {
        // safebooru's shape: an `.image-list` flex-wrap container of
        // `.thumb` boxes, each a fixed-width column holding an image and a
        // caption. The thumbs must pack into a grid, not stack vertically.
        let mut images = ImageSizes::new();
        for n in ["a", "b", "c"] {
            images.insert(format!("https://example.com/{n}.png"), (10, 3));
        }
        let rows = lay_with_images(
            r#"<html><head><style>
                 .image-list{display:flex;flex-flow:wrap}
                 .thumb{display:flex;flex-direction:column;width:6em}
               </style></head>
               <body><div class="image-list">
                 <div class="thumb"><a href="/a"><img src="/a.png"></a><span>cap a</span></div>
                 <div class="thumb"><a href="/b"><img src="/b.png"></a><span>cap b</span></div>
                 <div class="thumb"><a href="/c"><img src="/c.png"></a><span>cap c</span></div>
               </div></body></html>"#,
            40,
            &images,
        );
        // 6em·2 = 12 cells per thumb; 12+1+12+1+12 = 38 ≤ 40 → all three
        // pack onto the first band, side by side.
        let (ra, ca) = pos_of(&rows, "cap a");
        let (rb, cb) = pos_of(&rows, "cap b");
        let (rc, cc) = pos_of(&rows, "cap c");
        assert_eq!(ra, rb, "captions on the same band: {:?}", texts(&rows));
        assert_eq!(rb, rc);
        assert!(ca < cb && cb < cc, "thumbs ordered left to right");
        // Each thumb's image landed at the thumb's left edge, on its own
        // band above the caption; three images, distinct columns.
        let img_cols: Vec<u16> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.col)
            .collect();
        assert_eq!(img_cols.len(), 3, "three thumbnail images placed");
        assert_eq!(img_cols, vec![ca, cb, cc], "images align with captions");
    }

    #[test]
    fn flex_column_stacks_block_children() {
        // A `flex-direction:column` card stacks its children vertically.
        let rows = lay(
            r#"<html><head><style>.card{display:flex;flex-direction:column}</style></head>
               <body><div class="card"><div>top</div><div>bottom</div></div></body></html>"#,
            40,
        );
        let (rt, _) = pos_of(&rows, "top");
        let (rb, _) = pos_of(&rows, "bottom");
        assert!(rb > rt, "column flex stacks: {:?}", texts(&rows));
    }

    #[test]
    fn flex_column_blockifies_inline_children() {
        // The thumbnail-card shape: an anchor and a caption span (both
        // inline) must STACK under flex-direction:column, not fuse on one
        // line — each flex item is block-level.
        let rows = lay(
            r#"<html><head><style>.thumb{display:flex;flex-direction:column}</style></head>
               <body><div class="thumb"><a href="/x">LINK</a><span>CAPTION</span></div></body></html>"#,
            40,
        );
        let (rl, _) = pos_of(&rows, "LINK");
        let (rc, _) = pos_of(&rows, "CAPTION");
        assert!(
            rc > rl,
            "inline flex items stack as blocks: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn flex_row_lays_children_as_columns() {
        // A fixed sidebar (width 10em = 20 cells) beside a growing content
        // column (`flex:1`) at width 60: side by side, content gets the rest
        // (a flex item only grows with flex-grow — that's real flexbox).
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex;flex-direction:row}
                 .side{width:10em}
                 .main{flex:1}
               </style></head>
               <body><div class="row">
                 <div class="side">SIDEBAR</div>
                 <div class="main">CONTENT</div>
               </div></body></html>"#,
            60,
        );
        let (rs, cs) = pos_of(&rows, "SIDEBAR");
        let (rc, cc) = pos_of(&rows, "CONTENT");
        assert_eq!(rs, rc, "columns share a row: {:?}", texts(&rows));
        assert_eq!(cs, 0, "sidebar at the left edge");
        assert_eq!(cc, 21, "content past the 20-cell sidebar + 1-cell gap");
    }

    #[test]
    fn a_flex_container_renders_its_anonymous_text_item() {
        // A contiguous text run directly inside a flex container forms an
        // anonymous flex item (CSS Flexbox §4) and must render. Steam's
        // `<div class=discount_pct style=display:flex>-70%</div>` discount
        // badge was dropped entirely because `flex_items` collected only
        // element children — the "-70%" vanished. Mixed text + element
        // content keeps BOTH (the text run is its own anonymous item).
        let rows = lay(
            r#"<body>
                 <div style="display:flex">-70%</div>
                 <div style="display:flex">Save <span>now</span></div>
               </body>"#,
            60,
        );
        assert_eq!(
            find(&rows, "-70%").text,
            "-70%",
            "pure-text flex item renders"
        );
        find(&rows, "Save"); // text run beside an element child still renders
        find(&rows, "now"); // (panics if either is missing)
    }

    #[test]
    fn flex_row_flexible_column_wraps_its_own_content() {
        // A flexible content column gets the remaining width and wraps its
        // text WITHIN that column (not across the whole viewport). The fixed
        // sidebar holds its width (flex-shrink:0) so the content starts past
        // it and wraps in the remainder.
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex;flex-direction:row}
                 .side{width:6em;flex-shrink:0}
               </style></head>
               <body><div class="row">
                 <div class="side">menu</div>
                 <div class="main">alpha beta gamma delta epsilon zeta eta theta</div>
               </div></body></html>"#,
            40,
        );
        // The content column starts at 12 (6em·2) + 1 gap = 13, so every
        // content word sits at col ≥ 13 and the column wraps onto >1 row.
        let content_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.items.iter().any(|i| i.col >= 13))
            .map(|(i, _)| i)
            .collect();
        assert!(
            content_rows.len() >= 2,
            "flexible column wraps within its width: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn flex_row_empty_flexible_sibling_collapses() {
        // safebooru's #post-list has THREE flex children: a fixed sidebar, a
        // flexible content column, and a trailing EMPTY <span>. The empty
        // span must collapse to zero so the content column gets the FULL
        // remaining width (else it'd split the width and the grid would
        // pack half as many thumbnails, leaving the page half blank).
        let mut images = ImageSizes::new();
        for n in 0..6 {
            images.insert(format!("https://example.com/{n}.png"), (10, 5));
        }
        let html = r#"<html><head><style>
             #post-list{display:flex;flex-direction:row;flex-wrap:nowrap}
             .sidebar{max-width:10em}
             .image-list{display:flex;flex-flow:wrap}
             .thumb{display:flex;flex-direction:column;width:10em}
           </style></head>
           <body><div id="post-list">
             <div class="sidebar">tags</div>
             <div class="content"><div class="image-list">
               <span class="thumb"><img src="/0.png"></span><span class="thumb"><img src="/1.png"></span>
               <span class="thumb"><img src="/2.png"></span><span class="thumb"><img src="/3.png"></span>
               <span class="thumb"><img src="/4.png"></span><span class="thumb"><img src="/5.png"></span>
             </div></div>
             <span></span>
           </div></body></html>"#;
        // width 80: sidebar 20 cells, gap 1 → content gets ~59 cells; each
        // thumb is 10em=20 cells, so 2 fit per band (20+1+20=41 ≤ 59). With
        // the empty span stealing half, content would be ~29 → only 1/band.
        let rows = lay_with_images(html, 80, &images);
        let first_band: usize = rows
            .iter()
            .find(|r| r.items.iter().any(|i| i.image.is_some()))
            .map(|r| r.items.iter().filter(|i| i.image.is_some()).count())
            .unwrap_or(0);
        assert!(
            first_band >= 2,
            "content column got the full width (≥2 thumbs/band), not half: {first_band}"
        );
    }

    #[test]
    fn flex_row_stacks_when_too_narrow() {
        // Two un-shrinkable 10em (=20 cell) columns can't both fit in width
        // 30 even at their minimum, so the row falls back to stacking them
        // vertically (the terminal has no horizontal scroll).
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex;flex-direction:row}
                 .col{width:10em;flex-shrink:0}
               </style></head>
               <body><div class="row">
                 <div class="col">LEFT</div>
                 <div class="col">RIGHT</div>
               </div></body></html>"#,
            30,
        );
        let (rl, cl) = pos_of(&rows, "LEFT");
        let (rr, cr) = pos_of(&rows, "RIGHT");
        assert!(rr > rl, "narrow row stacks: {:?}", texts(&rows));
        assert_eq!((cl, cr), (0, 0), "stacked columns are full-width");
    }

    #[test]
    fn a_clipping_flex_row_keeps_its_gutter_beside_unbreakable_content() {
        // GitHub's blob/code view: a `display:flex` pane that CLIPS its
        // overflow (`overflow:auto`) holding a fixed line-number gutter
        // (`min-width:72px`) and a code column of `white-space:pre` lines wider
        // than the row. The row overflows even at min-content, but a
        // scroll-context container must keep the gutter BESIDE the code
        // (clipped at the box edge), not reflow it ABOVE the code (the
        // responsive stack fallback — right for an overflow:visible nav, wrong
        // for a scroll pane: it dropped every line number above the file).
        let long = "X".repeat(120);
        let html = format!(
            r#"<body><div style="display:flex;overflow:auto">
                 <div style="min-width:72px">1</div>
                 <div style="white-space:pre;width:100%">{long}</div>
               </div></body>"#
        );
        let rows = lay(&html, 40);
        let (rn, cn) = pos_of(&rows, "1");
        let (rc, cc) = pos_of(&rows, &long);
        assert_eq!(
            rn,
            rc,
            "gutter rides the code's row, not stacked above it: {:?}",
            texts(&rows)
        );
        assert!(
            cc >= cn + 9,
            "code clears the 72px (=9 cell) min-width gutter (gutter col {cn}, code col {cc})"
        );
    }

    #[test]
    fn flex_width_100_percent_shrinks_a_content_sibling() {
        // The SL toolbar shape: a small logo beside a `width:100%` box. The
        // 100% box should take almost all the width (shrinking to fit beside
        // the logo), not split 50/50 with it.
        let rows = lay(
            r#"<html><head><style>
                 .bar{display:flex}
                 .grow{width:100%}
               </style></head>
               <body><div class="bar">
                 <div class="logo">L</div>
                 <div class="grow">SEARCHBAR</div>
               </div></body></html>"#,
            60,
        );
        let (_, cl) = pos_of(&rows, "L");
        let (_, cs) = pos_of(&rows, "SEARCHBAR");
        assert_eq!(cl, 0, "logo at the left edge");
        assert!(
            cs <= 4,
            "search box starts right after the tiny logo, not mid-row: col {cs} ({:?})",
            texts(&rows)
        );
    }

    #[test]
    fn flex_percent_widths_share_one_row() {
        // Percent widths resolve against the row, so a content column plus two
        // 25% columns lay out on a single line.
        let rows = lay(
            r#"<html><head><style>.r{display:flex}.q{width:25%}</style></head>
               <body><div class="r">
                 <div class="a">AAA</div>
                 <div class="q">BBB</div>
                 <div class="q">CCC</div>
               </div></body></html>"#,
            80,
        );
        let (ra, _) = pos_of(&rows, "AAA");
        let (rb, cb) = pos_of(&rows, "BBB");
        let (rc, cc) = pos_of(&rows, "CCC");
        assert_eq!(ra, rb, "all columns on one row: {:?}", texts(&rows));
        assert_eq!(rb, rc);
        assert!(cb < cc, "ordered left to right");
    }

    #[test]
    fn centered_labels_do_not_inflate_flex_basis() {
        // A flex item whose content is `text-align:center` must size to its
        // content width, not the centered offset within the measure width —
        // else centered nav tabs spread their flex row apart (the SL
        // Marketplace toolbar bug). Measurement now ignores alignment.
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex}
                 .tab{text-align:center}
               </style></head>
               <body><div class="row">
                 <div class="tab">AA</div><div class="tab">BB</div>
               </div></body></html>"#,
            80,
        );
        let (ra, ca) = pos_of(&rows, "AA");
        let (rb, cb) = pos_of(&rows, "BB");
        assert_eq!(ra, rb, "tabs share one row: {:?}", texts(&rows));
        assert!(
            cb - ca <= 4,
            "centered tabs pack adjacent, not spread across the row: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn grown_text_input_fills_its_box() {
        // A `flex-grow` text input fills its allocated width (a wide search
        // bar) instead of leaving a long gap after its short placeholder —
        // how a browser draws a stretched input.
        let rows = lay(
            r#"<html><head><style>
                 .row{display:flex}
                 .grow{flex-grow:1}
               </style></head>
               <body><div class="row">
                 <input class="grow" type="text" placeholder="find">
                 <button>Go</button>
               </div></body></html>"#,
            40,
        );
        let input = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("find"))
            .expect("the input widget");
        assert!(
            input.width >= 20,
            "grown input fills its box (not just '[find]'): width {}",
            input.width
        );
        // The Go button is NOT stretched (only text inputs fill).
        let go = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("Go"))
            .expect("the button");
        assert!(go.width <= 6, "button keeps its size: width {}", go.width);
    }

    #[test]
    fn block_width_with_margin_auto_centers_content() {
        // A fixed-width block with `margin:0 auto` constrains its content and
        // centers it (the centered-page-wrapper idiom — the SL Marketplace's
        // `#body-shadow-repeating{width:1082px;margin:0 auto}`), instead of
        // spanning the full terminal.
        let rows = lay(
            r#"<html><head><style>
                 .page{width:20em;margin:0 auto}
               </style></head>
               <body><div class="page"><p>HELLO</p></div></body></html>"#,
            80,
        );
        let (_, c) = pos_of(&rows, "HELLO");
        // 20em = 40 cells, centered in 80 → left pad (80-40)/2 = 20.
        assert!(
            (18..=22).contains(&(c as usize)),
            "centered near col 20, got {c}: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn block_bare_width_without_auto_margin_is_not_constrained() {
        // A width with NO auto margin keeps full-width flow — we don't cramp
        // content on a bare pixel width; only auto margins signal "position me".
        let rows = lay(
            r#"<html><head><style>.x{width:20em}</style></head>
               <body><div class="x"><p>HELLO</p></div></body></html>"#,
            80,
        );
        let (_, c) = pos_of(&rows, "HELLO");
        assert_eq!(
            c,
            0,
            "bare-width block flows at the left, unconstrained: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn carousel_lays_a_scrollable_strip_that_snaps() {
        // An overflow-x container with an over-wide track of ≥3 cards is a
        // carousel: a strip wider than the viewport, scrolled card-by-card.
        let html = r#"<html><head><style>
             .scroller{overflow:hidden}
             .track{width:500em}
             .card{width:6em;float:left}
           </style></head>
           <body><div class="scroller"><div class="track">
             <div class="card">one</div><div class="card">two</div>
             <div class="card">three</div><div class="card">four</div>
             <div class="card">five</div>
           </div></div></body></html>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (_, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            20,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert_eq!(carousels.len(), 1, "one carousel detected");
        let c = &carousels[0];
        assert_eq!(c.stops.len(), 5, "a snap stop per card");
        assert!(c.width as usize > 20, "the strip overflows the viewport");
        assert_eq!(c.offset, 0, "starts at the first card");
        // A clearfix wrapping ONE wide column is NOT a carousel.
        let plain = r#"<html><head><style>.wrap{overflow:hidden}.col{width:50em}</style></head>
            <body><div class="wrap"><div class="col">just one wide column</div></div></body></html>"#;
        let dom = Dom::parse_document(plain);
        let (_, none) = lay_out_with_carousels(
            &dom,
            &base,
            20,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert!(none.is_empty(), "a single wide column isn't a carousel");
        // Scrolling snaps to the next/prev card edge and clamps at the ends.
        let mut c = c.clone();
        c.scroll_cards(1);
        assert!(
            c.offset > 0 && c.stops.contains(&c.offset),
            "→ snaps forward"
        );
        c.scroll_cards(-1);
        assert_eq!(c.offset, 0, "← snaps back to the start");
    }

    #[test]
    fn carousel_full_bleed_cards_show_several_across_not_one() {
        // A slick.js-style rail: an over-wide track of `width:100%` slides
        // (slick also sets a JS-computed per-slide pixel width sized against a
        // fictional viewport, equally unusable). Laying each at the full band
        // shows ONE slide — and a fixed-aspect tile image then fills the whole
        // screen (Humble Bundle's Featured carousel). A rail card is narrower
        // than the rail, so a full-bleed/over-band card is sized to show a few
        // across instead.
        let html = r#"<html><head><style>
             .scroller{overflow:hidden}
             .track{width:500em}
             .card{width:100%;float:left}
           </style></head>
           <body><div class="scroller"><div class="track">
             <div class="card">one</div><div class="card">two</div>
             <div class="card">three</div><div class="card">four</div>
           </div></div></body></html>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (_, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            60,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert_eq!(carousels.len(), 1, "still a carousel");
        let c = &carousels[0];
        assert_eq!(c.stops.len(), 4, "a snap stop per card");
        // Card pitch (stop spacing) is ~a third of the 60-cell band, so ~3
        // cards show — NOT the full band (one full-bleed slide). The card width
        // subtracts the two inter-card gaps so 3 cards + 2 gaps fit EXACTLY:
        // (60 - 2)/3 = 19, + 1 gap = pitch 20.
        let pitch = c.stops[1] - c.stops[0];
        assert_eq!(pitch, 20, "full-bleed cards are sized to show several");
        // The crux: the THIRD card fits fully inside the band, so
        // `Carousel::shows` draws its (band-wide) image instead of clipping the
        // whole card — `band_w/3` overshot by the gaps and dropped it (Humble's
        // 3rd Featured card showed its title but not its image).
        let card_w = pitch - 1;
        assert!(
            c.shows(c.left + c.stops[2], card_w),
            "the 3rd card (and its image) is fully visible, not clipped"
        );
    }

    #[test]
    fn visual_columns_append_overlapping_items_so_paint_and_hit_test_agree() {
        use crate::doc::Link;
        // A terminal can't overlay text, so the renderer appends an item whose
        // column falls inside an earlier one — a clickable overlay placed over an
        // input (the homepage search bar's clear button). The hit-test reads the
        // SAME placement, so the overlay is clickable exactly where it's drawn.
        let mk = |col, width, text: &str, link: Option<Link>| Item {
            col,
            width,
            height: 1,
            text: text.into(),
            kind: ItemKind::Text,
            image: None,
            emph: Emphasis::default(),
            node: NO_NODE,
            link,
            crop: false,
        };
        let row = Row {
            items: vec![
                mk(10, 8, "[Search]", None), // input: cols 10..18
                mk(11, 7, "[Clear]", Some(Link::External("x".into()))), // overlaps
                mk(20, 6, "Log In", Some(Link::External("y".into()))),
            ],
        };
        // The overlay is appended after the input (at 18, not its raw col 11),
        // and the trailing nav item closes up behind it (no phantom gap).
        assert_eq!(
            visual_columns(&row, &[], 0),
            vec![(0, 10), (1, 18), (2, 25)]
        );
    }

    #[test]
    fn carousel_band_stays_aligned_after_blank_row_collapse() {
        // The bug: carousels are recorded with absolute row indices during
        // flow, but `finish` later collapses/trims blank rows. A heading with
        // top margin emits a leading blank that gets trimmed, shifting every
        // row below it up by one — including the cards — while the recorded
        // band stayed put. The band then no longer covered its cards, so the
        // view stopped clipping the strip and every card showed (the SL
        // Marketplace "Featured Items" wide-strip bug). `finish` now remaps
        // the band through the collapse; here the band must still contain
        // every row that holds a card.
        let html = r#"<html><head><style>
             .scroller{overflow:hidden}
             .track{width:500em}
             .card{width:6em;float:left}
           </style></head>
           <body>
             <h2>Featured</h2>
             <div class="scroller"><div class="track">
               <div class="card">one</div><div class="card">two</div>
               <div class="card">three</div><div class="card">four</div>
               <div class="card">five</div>
             </div></div>
           </body></html>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            20,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert_eq!(carousels.len(), 1, "one carousel");
        let c = &carousels[0];
        // Every row that holds a card's text must fall inside the band, or
        // the view won't clip it (off-band cards leak as a wide strip).
        for (r, row) in rows.iter().enumerate() {
            let is_card = row
                .items
                .iter()
                .any(|it| matches!(it.text.as_str(), "one" | "two" | "three" | "four" | "five"));
            if is_card {
                assert!(
                    c.contains_row(r),
                    "card row {r} must be inside the band [{}, {}): {:?}",
                    c.start,
                    c.end,
                    texts(&rows)
                );
            }
        }
        // And the band's top edge is exactly the first card row (not drifted
        // past it onto blank rows below).
        let first_card_row = rows
            .iter()
            .position(|row| row.items.iter().any(|it| it.text == "one"))
            .expect("a card row");
        assert_eq!(c.start, first_card_row, "band starts on the card row");
    }

    #[test]
    fn carousel_generates_glyph_scroll_buttons_and_hides_author_controls() {
        use crate::doc::Link;
        // The SL Marketplace shape: a wrapper div holds BOTH the page's own
        // prev/next buttons and the scroll container. We follow the CSS
        // `::scroll-button` model — generate our own `‹`/`›` glyph controls
        // and suppress the page's author-supplied ones (so no duplicate).
        let html = r##"<html><head><style>
             .scroller{overflow:hidden}
             .track{width:500em}
             .card{width:6em;float:left}
           </style></head>
           <body><div class="featured">
             <div class="controls">
               <a class="next" href="#"><span>Next &raquo;</span></a>
             </div>
             <div class="scroller"><div class="track">
               <div class="card">one</div><div class="card">two</div>
               <div class="card">three</div><div class="card">four</div>
               <div class="card">five</div>
             </div></div>
           </div></body></html>"##;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            20,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert_eq!(carousels.len(), 1, "one carousel");
        // The page's authored "Next »" control is suppressed...
        assert!(
            !rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("Next")),
            "author-supplied control hidden: {:?}",
            texts(&rows)
        );
        // ...and replaced with our own generated prev/next glyph controls.
        let buttons: Vec<i8> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter_map(|it| match it.link {
                Some(Link::CarouselScroll(d)) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(buttons.len(), 2, "a prev and a next button generated");
        assert!(
            buttons.contains(&-1) && buttons.contains(&1),
            "both directions"
        );
        // The glyphs sit on the row just above the band, flanking it.
        let c = &carousels[0];
        let prev = rows
            .iter()
            .enumerate()
            .flat_map(|(r, row)| row.items.iter().map(move |it| (r, it)))
            .find(|(_, it)| it.link == Some(Link::CarouselScroll(-1)))
            .expect("prev button");
        assert_eq!(prev.0 + 1, c.start, "button row sits just above the band");
        assert_eq!(prev.1.col, c.left, "‹ flanks the band's left edge");

        // The disabled state mirrors the spec's `:disabled`: at the start you
        // can't go back but can go forward; at the end, the reverse.
        let mut c = c.clone();
        assert!(
            !c.can_scroll(-1) && c.can_scroll(1),
            "start: prev off, next on"
        );
        c.scroll_page(1);
        assert!(
            c.offset > 0 && c.stops.contains(&c.offset),
            "→ pages to a card"
        );
        for _ in 0..20 {
            c.scroll_page(1);
        }
        let pinned = c.offset;
        c.scroll_page(1);
        assert_eq!(c.offset, pinned, "→ clamps at the end");
        assert!(
            c.can_scroll(-1) && !c.can_scroll(1),
            "end: prev on, next off"
        );
        c.scroll_page(-1);
        assert!(c.offset < pinned, "← pages back");
    }

    #[test]
    fn slideshow_shows_the_active_slide_and_drops_the_hidden_ones() {
        // A deck of stacked, absolutely-positioned slides, one revealed by
        // opacity. Pure standards (her call): a browser paints the VISIBLE slide
        // and the `opacity:0` ones contribute nothing — so we render the active
        // slide and drop the rest. No paging carousel (the imminent JS-unfreeze
        // step lets the page's own timer advance slides). The slides overlap at
        // the same coordinates, so there is nothing to page between anyway.
        use crate::doc::Link;
        let html = r##"<html><head><style>
             .slide { position: absolute; opacity: 0 }
             .slide.active { opacity: 1 }
           </style></head>
           <body>
             <h2>Banner</h2>
             <div class="show" style="position:relative">
               <div class="slide active">ALPHA</div>
               <div class="slide">BETA</div>
               <div class="slide">GAMMA</div>
             </div>
           </body></html>"##;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, carousels) = lay_out_with_carousels(
            &dom,
            &base,
            40,
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        // No paging carousel and no generated scroll buttons.
        assert!(carousels.is_empty(), "no carousel: {:?}", texts(&rows));
        let buttons = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| matches!(it.link, Some(Link::CarouselScroll(_))))
            .count();
        assert_eq!(buttons, 0, "no paging controls: {:?}", texts(&rows));
        // The active slide renders; the hidden ones do not.
        assert!(
            shows(&rows, "ALPHA"),
            "active slide shows: {:?}",
            texts(&rows)
        );
        assert!(
            !shows(&rows, "BETA"),
            "hidden slide dropped: {:?}",
            texts(&rows)
        );
        assert!(
            !shows(&rows, "GAMMA"),
            "hidden slide dropped: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn absolute_columns_lay_side_by_side_not_as_a_carousel() {
        // eggramen's threecol_v2: a `position:relative` container with three
        // `position:absolute` column boxes — a left sidebar (default left edge),
        // a middle column (`margin-left`), and a right sidebar (`right:0`). They
        // tile horizontally, so they lay SIDE BY SIDE at their computed columns
        // (CSS 2.1 §10.3.7), not as a one-at-a-time carousel.
        use crate::doc::Link;
        let rows = lay(
            r#"<body><div style="position:relative;width:60ch">
                 <div style="position:absolute;width:20%">LEFT</div>
                 <div style="position:absolute;width:55%;margin-left:22%">MIDDLE</div>
                 <div style="position:absolute;width:20%;right:0">RIGHT</div>
               </div></body>"#,
            80,
        );
        // No carousel paging — they are columns, not slides.
        let buttons = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| matches!(it.link, Some(Link::CarouselScroll(_))))
            .count();
        assert_eq!(buttons, 0, "columns are not a carousel: {:?}", texts(&rows));
        let (lr, lc) = pos_of(&rows, "LEFT");
        let (mr, mc) = pos_of(&rows, "MIDDLE");
        let (rr, rc) = pos_of(&rows, "RIGHT");
        // Ascending columns, all on the same top row.
        assert!(
            lc < mc && mc < rc,
            "left < middle < right cols: {lc} {mc} {rc}"
        );
        assert!(
            lr == mr && mr == rr,
            "all three share the top row: {lr} {mr} {rr}"
        );
        // Left rides the left edge; right is pinned toward the right edge.
        assert_eq!(lc, 0, "left column at the container's left edge");
        assert!(
            (rc as usize) >= 40,
            "right column anchored toward the right: {rc}"
        );
    }

    #[test]
    fn over_wide_absolute_columns_compress_to_fit() {
        // No horizontal scroll: a positioned multi-column layout computed wider
        // than the band is scaled down so every column stays visible (her call,
        // mirrors grid-track over-wide handling). Two columns declared 60% each
        // (120% total) still both render, the right one within the band.
        let rows = lay(
            r#"<body><div style="position:relative;width:200ch">
                 <div style="position:absolute;width:60%;left:0">AYE</div>
                 <div style="position:absolute;width:60%;left:60%">BEE</div>
               </div></body>"#,
            40,
        );
        let (_, ac) = pos_of(&rows, "AYE");
        let (_, bc) = pos_of(&rows, "BEE");
        assert_eq!(ac, 0, "first column at the left");
        assert!(bc > ac, "second column to its right: {ac} {bc}");
        assert!(
            (bc as usize) < 40,
            "compressed within the 40-col band (col {bc}): {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn image_with_absolute_corner_badges_is_not_a_carousel() {
        use crate::doc::Link;
        // The thumbnail-card idiom (erome.com album, every thumbnail grid): an
        // anchor holding a fill `<img>` plus two `position:absolute` `<span>`
        // corner badges (a view count, a photo/video count). All three children
        // are absolute, so the old "≥2 all-absolute children = deck" heuristic
        // misread it as a 3-slide carousel and painted dead prev/next arrows
        // over every card. The badges are CHROME, not slides: no carousel, no
        // scroll buttons — just the image and its overlay text.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/t.png".to_owned(), (20, 20));
        let rows = lay_with_images(
            r#"<body><div style="position:relative">
                 <a href="/a" style="position:absolute;width:100%;height:100%">
                   <img src="/t.png" width="250" height="250" style="display:block;width:100%;height:auto;position:absolute">
                   <span style="position:absolute">181</span>
                   <span style="position:absolute">14</span>
                 </a>
               </div></body>"#,
            40,
            &images,
        );
        // No generated carousel paging controls.
        let scroll_buttons = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| matches!(it.link, Some(Link::CarouselScroll(_))))
            .count();
        assert_eq!(scroll_buttons, 0, "no carousel arrows: {:?}", texts(&rows));
        // The thumbnail still renders as a real image box.
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.image.is_some()),
            "the thumbnail image renders: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn an_overlay_on_an_image_is_lifted_above_it() {
        // A store-capsule corner badge (a "MIDWEEK DEAL" ribbon,
        // position:absolute top:-12px) sits ON TOP of the capsule's fill image.
        // A terminal can't composite a glyph over a (sixel) image, so the
        // overlay is lifted onto its own row ABOVE the image rather than
        // garbling it or shoving it off to the side. (Steam's Discounts
        // carousel — the "MI"/"MIDWEEK DEAL" debris.)
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cap.png".to_owned(), (92, 43));
        let rows = lay_with_images(
            r#"<body><div style="position:relative">
                 <a href="/g" style="position:relative;display:block;width:40ch">
                   <img src="/cap.png" style="display:block;width:100%;height:auto;position:relative">
                   <div style="position:absolute;top:-12px;left:0">MIDWEEK DEAL</div>
                 </a>
               </div></body>"#,
            60,
            &images,
        );
        let (badge_row, _) = pos_of(&rows, "MIDWEEK DEAL");
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|it| matches!(it.kind, ItemKind::Image)))
            .expect("the capsule image renders");
        // The badge reads ABOVE the image, on its own row…
        assert!(
            badge_row < img_row,
            "badge lifted above image (badge r{badge_row}, image r{img_row}): {:?}",
            texts(&rows)
        );
        // …and that row carries no image cells (it didn't paint over the image).
        assert!(
            !rows[badge_row]
                .items
                .iter()
                .any(|it| matches!(it.kind, ItemKind::Image)),
            "badge row is clear of the image: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn positioned_overlay_controls_land_at_their_offsets() {
        // A slideshow's prev/next arrows are `position:absolute` pinned to
        // opposite edges of their containing block; both sit on the same row at
        // their computed columns — prev at the left, next at the right — over
        // the slide, exactly where their CSS offsets place them.
        let rows = lay(
            r##"<html><head><style>
                 .show{position:relative;height:3rem}
                 .arrow{position:absolute;top:0}
                 .prev{left:0}
                 .next{right:0}
               </style></head>
               <body>
                 <div class="show">
                   <div class="slide">IMG</div>
                   <a class="arrow prev" href="#p">PREV</a>
                   <a class="arrow next" href="#n">NEXT</a>
                 </div>
               </body></html>"##,
            80,
        );
        let (r_prev, c_prev) = pos_of(&rows, "PREV");
        let (r_next, c_next) = pos_of(&rows, "NEXT");
        assert_eq!(r_prev, r_next, "both arrows on one row: {:?}", texts(&rows));
        assert_eq!(
            c_prev,
            0,
            "prev pinned to the left edge: {:?}",
            texts(&rows)
        );
        assert!(
            (c_next as usize) >= 80 - 5,
            "next pinned to the right edge (col {c_next}): {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn float_left_wraps_text_beside_then_full_width_below() {
        // A 12-wide, 2-row float at the left edge; the long paragraph flows
        // in the narrowed band beside it, then returns to full width below.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/f.png".to_owned(), (12, 2));
        let words = "aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp qq rr ss tt uu vv ww xx";
        let html = format!(
            r#"<html><head><style>img{{float:left}}</style></head>
               <body><img src="/f.png"><p>{words}</p></body></html>"#
        );
        let rows = lay_with_images(&html, 40, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("float image");
        assert_eq!((img.col, img.width, img.height), (0, 12, 2));
        // Some text rides beside the float (col ≥ 13 = 12 + 1 gap)...
        let beside = rows
            .iter()
            .flat_map(|r| &r.items)
            .any(|i| i.image.is_none() && !i.text.trim().is_empty() && i.col >= 13);
        assert!(beside, "text wraps beside the float: {:?}", texts(&rows));
        // ...and some text below the float returns to the left edge (col 0).
        let below_full_width = rows
            .iter()
            .enumerate()
            .filter(|(r, _)| *r >= img.height as usize)
            .any(|(_, row)| {
                row.items
                    .iter()
                    .any(|i| !i.text.trim().is_empty() && i.col == 0)
            });
        assert!(
            below_full_width,
            "text returns to full width below the float: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn float_persists_across_following_blocks() {
        // Her call: floats wrap content ACROSS sibling blocks (BFC), not just
        // their own. A tall float beside two separate <p>s wraps both.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/f.png".to_owned(), (12, 6));
        let rows = lay_with_images(
            r#"<html><head><style>img{float:left}</style></head>
               <body><img src="/f.png"><p>one two</p><p>four five</p></body></html>"#,
            40,
            &images,
        );
        let one = pos_of(&rows, "one");
        let four = pos_of(&rows, "four");
        assert!(one.1 >= 13, "first block flows beside the float");
        assert!(
            four.1 >= 13,
            "second block ALSO flows beside the float (across blocks): {:?}",
            texts(&rows)
        );
        assert!(four.0 < 6, "both blocks within the float's height");
    }

    #[test]
    fn float_right_pins_to_the_right_edge() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/f.png".to_owned(), (10, 3));
        let rows = lay_with_images(
            r#"<html><head><style>img{float:right}</style></head>
               <body><img src="/f.png"><p>alpha beta gamma delta epsilon zeta eta</p></body></html>"#,
            40,
            &images,
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("float image");
        assert_eq!(img.col, 30, "float:right pinned to the right edge (40-10)");
        // No text overlaps the floated box (everything stays left of col 30).
        let max_text_right = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_none())
            .map(|i| i.col + i.width)
            .max()
            .unwrap_or(0);
        assert!(
            max_text_right <= 30,
            "text stays left of the right float: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn clear_drops_below_the_float() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/f.png".to_owned(), (12, 5));
        let rows = lay_with_images(
            r#"<html><head><style>img{float:left}.below{clear:both}</style></head>
               <body><img src="/f.png"><p>beside</p><p class="below">cleared</p></body></html>"#,
            40,
            &images,
        );
        let beside = pos_of(&rows, "beside");
        let cleared = pos_of(&rows, "cleared");
        assert!(beside.1 >= 13, "first para sits beside the float");
        assert_eq!(cleared.1, 0, "cleared para is full-width");
        assert!(
            cleared.0 >= 5,
            "clear:both drops the para below the 5-row float: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn list_marker_shares_row_with_block_child() {
        // A list item whose content is block-level (a flex row / nested
        // <div>) must keep its bullet on the content's first row, not
        // stranded on a row of its own above it.
        let rows = lay(
            r#"<html><head><style>li>div{display:flex}</style></head>
               <body><ul><li><div><a href="/a">Animals</a><span>90,831</span></div></li></ul></body></html>"#,
            60,
        );
        let (rm, cm) = pos_of(&rows, "•");
        let (ra, ca) = pos_of(&rows, "Animals");
        assert_eq!(
            rm,
            ra,
            "bullet shares the content's first row: {:?}",
            texts(&rows)
        );
        assert!(cm < ca, "bullet sits in the gutter left of the content");
    }

    #[test]
    fn star_rating_images_become_glyphs() {
        // Image-based star ratings collapse their verbose alt text to glyphs.
        let rows = lay(
            r#"<body><span><img alt="full star"><img alt="full star"><img alt="half star"><img alt="empty star"></span></body>"#,
            40,
        );
        let line = texts(&rows).join("");
        assert!(line.contains('★'), "full→★: {line:?}");
        assert!(line.contains('⯨'), "half→⯨: {line:?}");
        assert!(line.contains('☆'), "empty→☆: {line:?}");
        assert!(
            !line.to_lowercase().contains("star"),
            "no verbose phrases left: {line:?}"
        );
        // A non-star icon keeps its alt text.
        assert_eq!(star_glyph("shopping cart"), None);
    }

    #[test]
    fn bfc_overflow_hidden_contains_floats() {
        // A float inside an `overflow:hidden` wrapper (the ubiquitous
        // clearfix) must NOT leak past it: the following block renders
        // full-width below, not flowed into the float's narrowed band. This
        // is what keeps a page's footer off its floated main column.
        let rows = lay(
            r#"<html><head><style>
                 .wrap{overflow:hidden}
                 .col{float:left;width:6em}
               </style></head>
               <body><div class="wrap"><div class="col">SIDEBAR</div></div>
               <p>FOLLOWING</p></body></html>"#,
            40,
        );
        let (rs, _) = pos_of(&rows, "SIDEBAR");
        let (rf, cf) = pos_of(&rows, "FOLLOWING");
        assert!(
            rf > rs,
            "following block clears the contained float: {:?}",
            texts(&rows)
        );
        assert_eq!(
            cf,
            0,
            "following block is full-width, not in the float's band: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn wide_float_stacks_below_following_content() {
        // A float as wide as the viewport leaves no usable band beside it,
        // so it drops in-flow as a block and the next block stacks below —
        // never painted over (the bug that put a page footer on top of its
        // sidebar/main column at terminal widths).
        let rows = lay(
            r#"<html><head><style>.main{float:left;width:60em}</style></head>
               <body><div class="main">MAIN COLUMN</div><p>FOOTER</p></body></html>"#,
            40,
        );
        let (rm, cm) = pos_of(&rows, "MAIN COLUMN");
        let (rf, cf) = pos_of(&rows, "FOOTER");
        assert_eq!(cm, 0, "wide float starts at the left edge");
        assert!(
            rf > rm,
            "footer stacks below the wide float, not over it: {:?}",
            texts(&rows)
        );
        assert_eq!(cf, 0, "footer is full-width");
    }

    #[test]
    fn float_grid_wraps_into_aligned_rows() {
        // Equal-width left floats (a 25%, i.e. 4-up, grid) pack four across,
        // then WRAP to a fresh row aligned at the left edge below — the 5th
        // float must NOT tuck under the 4th (the erome.com /explore album-grid
        // jank: "6 across, then one underneath the 6th"). Wider tracks them
        // packing tight (no inter-float gap) so a browser's column count holds.
        let html = r#"<html><head><style>.c{float:left;width:25%}</style></head>
            <body><div>
              <div class="c">AA</div><div class="c">BB</div>
              <div class="c">CC</div><div class="c">DD</div>
              <div class="c">EE</div><div class="c">FF</div>
            </div></body></html>"#;
        let rows = lay(html, 40);
        let (ra, ca) = pos_of(&rows, "AA");
        let (rb, cb) = pos_of(&rows, "BB");
        let (rc, cc) = pos_of(&rows, "CC");
        let (rd, cd) = pos_of(&rows, "DD");
        let (re, ce) = pos_of(&rows, "EE");
        let (rf, cf) = pos_of(&rows, "FF");
        // AA BB CC DD share the first row, left→right.
        assert_eq!((ra, rb, rc), (rb, rc, rd), "first four share a row");
        assert!(
            ca < cb && cb < cc && cc < cd,
            "left→right: {:?}",
            texts(&rows)
        );
        // EE FF wrap to a NEW row below, EE aligned back under AA — not tucked
        // under DD.
        assert!(re > ra, "fifth float wraps below: {:?}", texts(&rows));
        assert_eq!(ce, ca, "wrapped row aligns at the left edge");
        assert_eq!(rf, re, "EE and FF share the wrapped row");
        assert!(cf > ce);
    }

    #[test]
    fn clearfix_pseudo_contains_floats() {
        // A `.row`/`.clearfix` (`::after{clear:both}`) contains its floats like
        // a BFC, so content after it clears BELOW the float grid instead of
        // being painted over it (the erome.com pagination + "suggested users"
        // bleeding onto the thumbnails).
        let html = r#"<html><head><style>
            .row::after{content:"";display:table;clear:both}
            .c{float:left;width:50%}
          </style></head>
          <body>
            <div class="row"><div class="c">LEFT</div><div class="c">RIGHT</div></div>
            <p>BELOW</p>
          </body></html>"#;
        let rows = lay(html, 40);
        let (rl, _) = pos_of(&rows, "LEFT");
        let (rbelow, cbelow) = pos_of(&rows, "BELOW");
        assert!(
            rbelow > rl,
            "content after a clearfix row clears below its floats: {:?}",
            texts(&rows)
        );
        assert_eq!(cbelow, 0, "and returns to the full-width left edge");
    }

    #[test]
    fn percentage_float_columns_floor_so_all_fit() {
        // Four 25% floats in a row whose width makes 25% fractional (38 cells →
        // 9.5). Rounding each to 10 sums to 40 > 38 and drops the 4th to a new
        // row; flooring to 9 keeps all four across (the erome /explore narrow
        // 4-up grid that was rendering only 3). Wider/markup-free so the count
        // is unambiguous.
        let html = r#"<html><head><style>.c{float:left;width:25%}</style></head>
            <body><div>
              <div class="c">AA</div><div class="c">BB</div>
              <div class="c">CC</div><div class="c">DD</div>
            </div></body></html>"#;
        let rows = lay(html, 38);
        let (ra, _) = pos_of(&rows, "AA");
        let (rd, _) = pos_of(&rows, "DD");
        assert_eq!(ra, rd, "all four columns share one row: {:?}", texts(&rows));
    }

    #[test]
    fn standalone_clearfix_div_clears_a_float_grid() {
        // erome /explore's exact shape: a float grid, a STANDALONE
        // `<div class="clearfix">` (bootstrap's single-colon `:after{clear:both}`
        // in a big comma list), then a full-width `float:left` section
        // ("suggested users"). The section must land BELOW the whole grid, not
        // float up onto the first row over the thumbnails.
        let html = r#"<html><head><style>
            .clearfix:after, .row:after { clear: both }
            .col { float:left; width:25% }
            .full { float:left; width:100% }
          </style></head>
          <body>
            <div class="row">
              <div class="col">AA</div><div class="col">BB</div>
              <div class="col">CC</div><div class="col">DD</div>
              <div class="col">EE</div>
            </div>
            <div class="clearfix"></div>
            <div class="full">SUGGESTED</div>
          </body></html>"#;
        let rows = lay(html, 40);
        let (re, _) = pos_of(&rows, "EE"); // the wrapped 2nd-row cell
        let (rs, cs) = pos_of(&rows, "SUGGESTED");
        assert!(
            rs > re,
            "the suggested section clears below the whole grid: {:?}",
            texts(&rows)
        );
        assert_eq!(cs, 0, "and is full-width at the left edge");
    }

    #[test]
    fn image_renders_alt_text() {
        let rows = lay(
            r#"<body><p>x <img src="/a.png" alt="a cat"> y</p></body>"#,
            60,
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.kind == ItemKind::Image)
            .expect("an image item");
        assert!(img.text.contains("cat"));
    }
}
