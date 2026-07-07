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
pub(crate) fn truncate_to_width(s: &str, max: usize) -> String {
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

/// Incremental-layout boundary capture (INCREMENTAL_LAYOUT_PLAN.md §14), keyed
/// by the boundary's PARSE `NodeId`: the live actor id (`data-trust-node`), the
/// outer band it fills (`content_width`, `origin_col`), and its row span
/// (`start_row..end_row`, this layout's coordinate space — `blit` translates and
/// `finish` remaps through the blank-row collapse, exactly like `ElementTops`).
/// Only populated when `capture_boundaries` is set (the live full render).
#[derive(Clone, Copy)]
struct BoundaryRec {
    node: usize,
    /// The width to re-lay the boundary's content at (the band it fills, for a
    /// block-filling box; the track width it was given, for a sub-box).
    content_width: u16,
    /// The boundary's painted extent (cells). For a block-filling box this is the
    /// band; for a sub-box (flex/grid item, inline-block) it is the content's
    /// actual width — the column span the owns-rows check + width verify use.
    width: u16,
    origin_col: u16,
    start_row: usize,
    end_row: usize,
    /// Laid as a SUB-BOX (`layout_subtree_inner`+`blit`: a flex/grid item, float,
    /// inline-box-grid cell) rather than in-flow. A sub-box re-lays with
    /// `subtree_root` set (its own `constrain`/width was applied by the parent,
    /// not itself) and is verified for STRICT width-stability (its width is
    /// content-dependent, so a width change reflows its siblings → resync). A
    /// block-filling box (in-flow) re-lays without `subtree_root` and fills its
    /// band regardless of content, so it needs no width verify.
    sub_box: bool,
}

type BoundaryRecs = HashMap<NodeId, BoundaryRec>;

/// The CLIP box `(live_node, client_h_rows, client_w_cells)` of every
/// definite-height scroll-y box flowed (region or fitting). See `Doc.scroll_clips`.
type ScrollClips = Vec<(usize, u16, u16)>;

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

/// The terminal's real cell HEIGHT in px (the picker's font size). Every
/// vertical px→rows conversion runs through it (via `Units`), so a definite
/// CSS pixel height maps to the same physical extent a browser gives it AND
/// round-trips the JS geometry (`getBoundingClientRect` = rows × cell_px).
/// Session-global (set once from the picker, like `BORDERS_ENABLED`) rather
/// than threaded through every `lay_out`/`parse_seeded`/test signature;
/// defaults to 16 so tests and the 8×16 measurement fixtures round-trip
/// exactly. Read into `Layout.cell_px_h` at construction; `measure_boxes`
/// overrides it with its explicit `cell_px.1`.
static CELL_PX_H: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(16);

pub fn set_cell_px_h(px: u16) {
    CELL_PX_H.store(px.max(1), std::sync::atomic::Ordering::Relaxed);
}

pub fn cell_px_h() -> u16 {
    CELL_PX_H.load(std::sync::atomic::Ordering::Relaxed)
}

/// The terminal's real cell WIDTH in px, completing what `CELL_PX_H` started:
/// with both axes real, a CSS pixel length maps to the same physical extent
/// the browser gives it on ANY terminal font, and `rows_for_ratio` keeps true
/// aspect instead of assuming 2:1 cells. Same session-global pattern as
/// `CELL_PX_H`; defaults to 8 (the nominal cell) so tests stay deterministic.
static CELL_PX_W: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(8);

pub fn set_cell_px_w(px: u16) {
    CELL_PX_W.store(px.max(1), std::sync::atomic::Ordering::Relaxed);
}

pub fn cell_px_w() -> u16 {
    CELL_PX_W.load(std::sync::atomic::Ordering::Relaxed)
}

/// The context a CSS length resolves in: the element's computed font-size
/// (`em`), the root's (`rem`), and the terminal's real cell box (px → cells).
/// Built per element by `Layout::units` / `Units::of`; the default is the
/// nominal test fixture (16px font, 8×16 cells) under which 1em = 2 cells =
/// 1 row, the engine's historical constants.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Units {
    /// The element's computed font-size, CSS px.
    pub fs: f32,
    /// The root element's computed font-size, CSS px (the `rem` basis).
    pub root: f32,
    /// Terminal cell width, px.
    pub cell_w: f32,
    /// Terminal cell height, px.
    pub cell_h: f32,
}

impl Default for Units {
    fn default() -> Self {
        Units {
            fs: crate::dom::FONT_SIZE_INITIAL,
            root: crate::dom::FONT_SIZE_INITIAL,
            cell_w: 8.0,
            cell_h: 16.0,
        }
    }
}

impl Units {
    /// The resolution context for `id` in `dom`, with the session's real cell
    /// box — for callers outside a `Layout` (dom.rs's clip checks).
    pub(crate) fn of(dom: &Dom, id: NodeId) -> Units {
        Units {
            fs: dom.font_px(id),
            root: dom.root_font_px(),
            cell_w: f32::from(cell_px_w()),
            cell_h: f32::from(cell_px_h()),
        }
    }
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
    /// `image-rendering: pixelated`/`crisp-edges` on an `Image` item (CSS
    /// Images 3 §5.4): the encoder scales with NEAREST-NEIGHBOR instead of
    /// Lanczos, keeping upscaled blocks hard-edged — a smoothing filter turns
    /// Steam's 41px QR GIF into an unscannable blur. False for non-images.
    pub pixelated: bool,
    /// Paint suppression (`opacity:0` — CSS Color/Compositing): the item is
    /// fully laid out (its `col`/`width`/`height` reserve its real box, so
    /// `measure_boxes`/`getBoundingClientRect` are unaffected) but the renderer
    /// writes BLANK cells for it (spaces for text, no pixels for an image) —
    /// exactly like a browser painting the element transparent. Set from the
    /// inline formatting context (`Ctx.invisible`), so a whole `opacity:0`
    /// subtree paints blank while still occupying space. This is what makes
    /// React virtualized-list placeholders (`opacity:0` + cached height) report
    /// their real height instead of collapsing.
    pub invisible: bool,
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

/// A `position:fixed` element captured into the PINNED overlay layer: its laid
/// content, plus the viewport position it pins at. The document scrolls
/// underneath; the renderer draws this on top at a fixed screen position — the
/// one place a terminal composites (see the CSS-cascade fixed-layer deviation).
/// Only all-insets-`auto` (static-position), non-viewport-covering fixed boxes
/// are captured here (Mastodon's side rails); covering ones stay modals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixedItem {
    /// Column of the box's top-left in the pinned viewport (0-based cells).
    pub col: u16,
    /// Row of the box's top-left in the pinned viewport (clamped into view).
    pub row: u16,
    /// The laid content rows (position-independent, like a scroll-region buffer).
    pub rows: Vec<Row>,
    /// Paint order — higher draws last (over lower). From `z-index`.
    pub z: i32,
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
    /// Whether scrolling SNAPS to the card stops (CSS Scroll Snap 1): true only
    /// when the page declares `scroll-snap-type` on the container. Otherwise the
    /// strip scrolls FREELY (by a fraction of the band), never forcing an
    /// alignment the page didn't ask for.
    pub snap: bool,
}

/// One image layer of an alpha-composited overlap group (LAYOUT_OVERHAUL_PLAN.md
/// P8). When image fragments overlap and an upper one has real transparency,
/// the paint compositor emits a SINGLE synthetic `x-trust-composite:` image item
/// over the union box and records the group's layers here (in `Doc.composites`,
/// keyed by that synthetic URL). The app encodes them by alpha-compositing each
/// layer's decoded RGBA onto a union-sized canvas in PAINT ORDER (bottom first),
/// so a lower image shows through an upper image's transparent pixels — the one
/// place a terminal can honor image-over-image alpha (it happens before encode,
/// since two already-encoded opaque cell protocols can't be blended at draw
/// time). Offsets/sizes are in terminal cells, relative to the union top-left.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompositeLayer {
    /// Absolute image URL / data-url — the key into the app's decoded cache.
    pub url: String,
    /// Column offset of this layer within the union box (cells).
    pub dcol: u16,
    /// Row offset of this layer within the union box (cells).
    pub drow: u16,
    /// This layer's own used cell box.
    pub w: u16,
    pub h: u16,
    /// `object-fit: cover` for this layer (else contain).
    pub crop: bool,
    /// `image-rendering: pixelated` for this layer (nearest-neighbour scaling).
    pub pixelated: bool,
}

impl Carousel {
    /// The band's visible width in cells.
    pub fn view_width(&self) -> u16 {
        self.right.saturating_sub(self.left)
    }

    /// The furthest FREE scroll offset (whole strip minus the visible band) —
    /// the clamp for a non-snapping (`scroll-snap-type: none`) strip.
    pub fn max_offset(&self) -> u16 {
        self.width.saturating_sub(self.view_width())
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

    /// Advance the scroll by one step (`dir` ±1): when the strip SNAPS, to the
    /// next/prev card edge (never past the last card); otherwise a free scroll
    /// by ~half the band, clamped to `[0, max_offset]`.
    pub fn scroll_cards(&mut self, dir: i32) {
        if !self.snap {
            let step = (self.view_width() / 2).max(1);
            self.offset = if dir > 0 {
                self.offset.saturating_add(step).min(self.max_offset())
            } else {
                self.offset.saturating_sub(step)
            };
            return;
        }
        let max_stop = self.max_stop();
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

    /// Whether the strip can still scroll in `dir` (±1) — drives the
    /// `:disabled`/greyed state of any scroll control at the ends.
    pub fn can_scroll(&self, dir: i32) -> bool {
        let far = if self.snap {
            self.max_stop()
        } else {
            self.max_offset()
        };
        if dir > 0 {
            self.offset < far
        } else {
            self.offset > 0
        }
    }

    /// Page the strip by ~one visible width (`dir` ±1). When it SNAPS, to a
    /// card edge (the CSS carousel model — page, then scroll-snap pulls to the
    /// nearest item); otherwise a free page clamped to `[0, max_offset]`.
    pub fn scroll_page(&mut self, dir: i32) {
        let view = self.view_width();
        if !self.snap {
            self.offset = if dir > 0 {
                self.offset.saturating_add(view).min(self.max_offset())
            } else {
                self.offset.saturating_sub(view)
            };
            return;
        }
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

/// A VERTICAL inner-scroll viewport (CSS `overflow-y: auto|scroll` on a
/// definite-height box — a scroll container per CSS Overflow L3). Unlike a
/// `Carousel` (which scrolls a NON-indexing axis and keeps all its items in
/// real doc rows), a vertical region scrolls the same axis the document is
/// indexed by and must show FEWER rows than its content holds. So it cannot
/// keep its content inline — the layout reserves exactly `height` BLANK doc
/// rows at the region's flow position (the document stays flat, so the
/// scroll/selection INDEX MATH is untouched, exactly the property the carousel
/// relies on) and stashes the full content in `buffer` (its own laid rows).
/// The renderer draws `buffer[voffset + local]` clipped to the band
/// `[left, left+width)` for each screen row inside the region's band; scrolling
/// only changes `voffset` and re-blits the retained buffer — it never re-runs
/// layout. The scrollport is the box's padding box (CSS Overflow L3 §2); the
/// scroll origin is the top, so a fresh region's `voffset` is 0 (CSSOM View).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    /// The scroll-container element, for re-anchoring `voffset` across the
    /// chat's per-message re-layout and (Phase 3) `element.scrollTop`.
    pub node: NodeId,
    /// First reserved doc row — the top of the scrollport band.
    pub start_row: usize,
    /// On-screen left column of the scrollport (cells).
    pub left: u16,
    /// Scrollport width (cells) — the band the buffer is clipped to.
    pub width: u16,
    /// Reserved doc rows / the scrollport's visible height (`clientHeight`).
    pub height: u16,
    /// The full scrollable content, laid at `width`. `buffer.len()` is the
    /// scrollable overflow height (`scrollHeight`).
    pub buffer: Vec<Row>,
    /// Current vertical scroll position in rows (CSSOM `scrollTop`, clamped to
    /// `[0, max_voffset()]`). Seeded from the page's baked `data-trust-scroll-top`
    /// signal (Phase 3) when present, else 0 (the top — CSSOM scroll origin).
    pub voffset: usize,
    /// The resident page actor's node id for this scroll container (from the
    /// baked `data-trust-node`), so the app can correlate this re-parsed region
    /// with the live element for the geometry round-trip + wheel write-back
    /// (Phase 3). `None` for a static (no-engine) page's region.
    pub live_node: Option<usize>,
    /// Whether `voffset` came from the page's own `element.scrollTop` signal this
    /// layout (the baked `data-trust-scroll-top`). When true, `carry_region_
    /// offsets` keeps it — the page dictated the position (a chat pinning to the
    /// bottom); when false, the user's wheel offset is restored across re-layout.
    pub voffset_from_page: bool,
    /// Whether this is the page's PRINCIPAL scroll region — the one a locked
    /// viewport delegates document scrolling to (`Dom::is_principal_scroller`).
    /// The terminal presents it as "the page": the main scrollbar reflects its
    /// position, the page-level scroll gestures (wheel off a nested region,
    /// PgUp/PgDn, Home/End) drive it, and `carry_region_offsets` keeps its
    /// offset user-locked across live re-renders — never overridden by the
    /// page's own `scrollTop` signal (it is the reader who scrolls "the page",
    /// not the page, so a lagging signal must not snap it back). At most one
    /// region on a page is principal; nested regions are never principal.
    pub principal: bool,
    /// Horizontal scroll strips nested inside this region (buffer-relative
    /// coords: `start`/`left` are indices into `buffer`). The renderer windows
    /// them within this region's window (a shelf inside a scrolling feed).
    pub carousels: Vec<Carousel>,
    /// Vertical scroll regions nested inside this region (buffer-relative). Each
    /// is independently scrollable — the renderer windows it within this
    /// region's window, and wheel/keys route to the deepest one under the
    /// cursor (a scroll container inside a scroll container, CSS Overflow L3).
    pub regions: Vec<Region>,
    /// Absolute http(s)/`data:` URLs of EVERY `<img>` in this region's subtree —
    /// decoded or not — collected from the DOM at layout time (an undecoded image
    /// is alt text, absent from the laid `buffer`, so this is read off the
    /// CONTENT, not the rendered items). This is what lets an image-decode reflow
    /// be ROUTED: a URL here means the image's box is contained by this scroll
    /// region's independent formatting context, so its intrinsic-size reflow
    /// re-lays only this region — never the whole document (the inner-scroll
    /// de-lag, INCREMENTAL_LAYOUT_PLAN.md §14). Populated on every full render; a
    /// region patch refreshes it from the patch fragment, so it survives the
    /// per-message re-parse and stays current as chat grows.
    pub image_urls: Vec<String>,
}

impl Region {
    /// Whether a doc row index falls inside this region's reserved band.
    pub fn contains_row(&self, row: usize) -> bool {
        row >= self.start_row && row < self.start_row + self.height as usize
    }

    /// Whether on-screen content-column `col` (content-area-relative — the same
    /// space as `left`) falls inside the scrollport band, i.e. the cursor is
    /// over this region.
    pub fn contains_col(&self, col: u16) -> bool {
        col >= self.left && col < self.left + self.width
    }

    /// The furthest the content can scroll: `scrollHeight − clientHeight`
    /// (CSSOM View — the `scrollTop`/`voffset` upper clamp bound).
    pub fn max_voffset(&self) -> usize {
        self.buffer.len().saturating_sub(self.height as usize)
    }

    /// Scroll the window by `delta` rows, clamped to `[0, max_voffset]`.
    /// Returns whether `voffset` actually moved (`false` = already at that
    /// boundary). The wheel/page handlers TRAP a scroll inside the hovered
    /// region regardless, so a boundary scroll is simply absorbed (never chains
    /// to the page) — her call, `overscroll-behavior: contain`.
    pub fn scroll_by(&mut self, delta: i64) -> bool {
        let next = (self.voffset as i64 + delta).clamp(0, self.max_voffset() as i64) as usize;
        let moved = next != self.voffset;
        self.voffset = next;
        moved
    }
}

/// An independent-formatting-context boundary that lays its content INLINE in
/// `Doc.rows` — the cache entry for incremental layout's general subtree splice
/// (INCREMENTAL_LAYOUT_PLAN.md §14). Captured during a full render so a live
/// `Patched{node}` whose boundary matches can re-lay ONLY that subtree and
/// splice it back in place (Tier 1) or splice+shift+scroll-anchor (Tier 2),
/// leaving the rest of the document identity. Two kinds qualify: BLOCK-FILLING
/// IFC containers (`display:flow-root`/`flex`/`grid`, in-flow) whose outer width
/// is their containing block's; and SUB-BOXES (a flex/grid item, inline-block
/// cell) that OWN their rows (no sibling shares a row — proven geometrically at
/// harvest) and are width-stable (verified on patch). A box that shares its rows
/// with siblings is excluded → full path (always correct).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryBox {
    /// The live actor node id (baked as `data-trust-node`) — the app maps a
    /// `Patched{node}` to this cached box.
    pub node: usize,
    /// The rows in `Doc.rows` this boundary's content occupies (`start..end`).
    pub row_range: std::ops::Range<usize>,
    /// The left edge (cells) where the re-laid fragment's column 0 maps when
    /// spliced back.
    pub origin_col: u16,
    /// The width fed to the fragment re-lay (the band, for a block-filling box;
    /// the track width, for a sub-box) so it wraps identically.
    pub content_width: u16,
    /// The boundary's painted extent (cells) — the column span the splice
    /// occupies and (for a sub-box) the width the patch verifies stays stable.
    pub width: u16,
    /// Laid as a SUB-BOX (flex/grid item, inline-block) — re-laid with
    /// `subtree_root` set and verified for strict width-stability. `false` = a
    /// block-filling in-flow box (fills its band, no width verify).
    pub sub_box: bool,
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

/// The effective items for document row `row_idx`, merging any scroll-`Region`
/// buffer window over the row's reserved band. Returns the BORROWED doc row
/// when no region covers it (the overwhelming common case — no allocation),
/// else an OWNED row = the doc row's own items (page content beside the region,
/// e.g. the video player left of the chat) PLUS the region's windowed buffer
/// row (`buffer[voffset + local]`), each item clipped to the scrollport `width`
/// and shifted right into the band by `left`. The renderer and (Phase 2) the
/// hit-test share this so region content draws — and becomes selectable —
/// exactly where it lands. The reserved doc rows are blank in the region's
/// band, so the merge never collides with page content there.
pub fn effective_row<'a>(
    rows: &'a [Row],
    regions: &'a [Region],
    row_idx: usize,
) -> std::borrow::Cow<'a, Row> {
    use std::borrow::Cow;
    let row = &rows[row_idx];
    if !regions.iter().any(|rg| rg.contains_row(row_idx)) {
        return Cow::Borrowed(row);
    }
    let mut merged = row.clone();
    for rg in regions.iter().filter(|rg| rg.contains_row(row_idx)) {
        let buf_idx = rg.voffset + (row_idx - rg.start_row);
        if buf_idx >= rg.buffer.len() {
            continue; // past the content tail: the band shows blank here
        }
        // Resolve the region's buffer row through its OWN nested regions
        // (recursion) so a scroll container inside this one draws its own
        // windowed content — each independently scrolled (CSS Overflow L3).
        let brow = effective_row(&rg.buffer, &rg.regions, buf_idx);
        for it in &brow.items {
            // Window through this region's nested carousels first (buffer-
            // relative column shift/clip); no carousels ⇒ the item's own col.
            let Some(bcol) = visible_col(&rg.carousels, buf_idx, it) else {
                continue; // a nested strip clipped this item out of its band
            };
            if bcol >= rg.width {
                continue; // beyond the scrollport's right edge: clipped away
            }
            let mut it = it.clone();
            it.col = bcol;
            let max_w = rg.width - it.col;
            if it.width > max_w {
                if it.text.is_empty() {
                    // An image box: clip the reserved box to the scrollport.
                    it.width = max_w;
                } else {
                    // Truncate by DISPLAY width (the binding width rule): a
                    // char-count cut keeps up to 2× the cells of CJK/emoji
                    // text, painting past the band and desyncing hit-tests.
                    it.text = truncate_to_width(&it.text, max_w as usize);
                    it.width = display_width(&it.text) as u16;
                }
            }
            it.col += rg.left;
            merged.items.push(it);
        }
    }
    Cow::Owned(merged)
}

/// Where item `i` of `effective_row(rows, regions, row_idx)` came from: the
/// doc row itself, or a scroll region's buffer (which buffer row and item).
/// MUST mirror `effective_row`'s merge exactly — same region iteration
/// order, same right-edge clip filter — so a merged index translates back
/// to stable buffer coordinates (the find highlighter keys matches on
/// them; a divergence highlights the wrong item).
pub enum ItemOrigin {
    Doc,
    Region {
        region: usize,
        brow: usize,
        bitem: usize,
    },
}

pub fn item_origin(rows: &[Row], regions: &[Region], row_idx: usize, i: usize) -> ItemOrigin {
    let doc_n = rows[row_idx].items.len();
    if i < doc_n {
        return ItemOrigin::Doc;
    }
    let mut next = doc_n;
    for (ri, rg) in regions.iter().enumerate() {
        if !rg.contains_row(row_idx) {
            continue;
        }
        let brow = rg.voffset + (row_idx - rg.start_row);
        let Some(b) = rg.buffer.get(brow) else {
            continue;
        };
        for (bi, it) in b.items.iter().enumerate() {
            if it.col >= rg.width {
                continue;
            }
            if next == i {
                return ItemOrigin::Region {
                    region: ri,
                    brow,
                    bitem: bi,
                };
            }
            next += 1;
        }
    }
    ItemOrigin::Doc
}

/// How far above the scroll top to look for an image whose box reaches down
/// into the viewport (a tall banner scrolled partly off the top). Bounds the
/// per-frame back-scan; an image taller than this many cells (~5000px) is not
/// realistic.
pub const MAX_IMAGE_LOOKBACK: usize = 256;

/// Pass-wide memo for `measure_width`: `(node, constraint, table_depth)` → cells.
/// `Rc` so every sub-layout in a pass shares the one cache.
type MeasureCache = std::rc::Rc<std::cell::RefCell<HashMap<(NodeId, usize, usize), usize>>>;

/// The `measure_boxes` geometry maps, shared across sub-layouts like
/// `MeasureCache` — their values are position-independent sizes keyed by
/// NodeId, so unlike `element_tops` they need no blit offset remapping.
type DeclaredBoxes = std::rc::Rc<std::cell::RefCell<HashMap<NodeId, (usize, usize)>>>;
type ClipHeights = std::rc::Rc<std::cell::RefCell<HashMap<NodeId, usize>>>;

/// Per-region memoization of the laid rows of a scroll container's block
/// children, keyed by `Dom::subtree_layout_hash` (INCREMENTAL_LAYOUT_PLAN.md
/// §14 — region child caching). The whole point of the inner-scroll de-lag:
/// re-laying a 150-message chat boundary from scratch on every appended message
/// is O(N) per message = O(N²) over a session (measured: 25→157ms and growing).
/// With this, an unchanged message reuses its cached rows and only a NEW message
/// is laid — O(1 message). `container` is the BFC whose block children are the
/// cacheable units; `old` is last pass's cache (read), `new` is built this pass
/// (every child, hit or laid) to become next pass's `old`. Shared across the
/// sub-layouts a region fragment spins up (like `MeasureCache`), so the children
/// flowed inside `layout_subtree_inner` see it.
struct RegionChildCache {
    container: NodeId,
    old: HashMap<u64, std::rc::Rc<Vec<Row>>>,
    new: HashMap<u64, std::rc::Rc<Vec<Row>>>,
}
type RegionCache = std::rc::Rc<std::cell::RefCell<RegionChildCache>>;

/// The serializable, cross-pass form of a region's child-row cache: the laid
/// rows keyed by content hash, plus the band `width` they were laid at (a width
/// change invalidates them). Held app-side per region (`live_node`) so it
/// survives the per-message re-parse AND the occasional full re-render.
#[derive(Default, Clone)]
pub struct RegionRowCache {
    pub width: usize,
    pub children: HashMap<u64, std::rc::Rc<Vec<Row>>>,
}

/// An element subtree laid out as an independent box, positioned relative
/// to its own top-left. `width` is the widest used column and `height` is
/// `rows.len()`. `blit` places it into a parent at a `(col, row)` offset —
/// the primitive under flex-wrap grids (and later columns and floats).
#[derive(Clone)]
struct LaidBox {
    rows: Vec<Row>,
    width: u16,
    height: u16,
    /// Carousels found inside this box (relative to its top-left); `blit`
    /// translates and propagates them so a carousel inside a float/flex
    /// column still reaches the document.
    carousels: Vec<Carousel>,
    /// `position:fixed` boxes captured inside this box (`col`/`row` relative to
    /// its top-left); `blit` translates and propagates them up so a fixed rail
    /// nested in a flex column reaches the document with its true static
    /// position. See `FixedItem`.
    fixed: Vec<FixedItem>,
    /// Scroll regions found inside this box (`start_row`/`left` relative to its
    /// top-left; the buffer is position-independent); `blit` translates and
    /// propagates them so a region inside a float/flex/abspos sub-layout (the
    /// chat column) still reaches the document.
    regions: Vec<Region>,
    /// Clip boxes (`Doc.scroll_clips` entries) recorded inside this box.
    /// Position-independent (`(live_node, rows, cells)`), so `blit` propagates
    /// them verbatim. Without this, a definite-height scroll-y box nested in a
    /// flex/grid/float/abspos sub-layout never reached `Doc.scroll_clips`, and
    /// the app never pushed its `clientHeight` to the live page — the one
    /// harvest channel that didn't ride the box.
    scroll_clips: ScrollClips,
    /// Recorded flow positions of EMPTY elements inside this box (measure
    /// pass only — `tag_all_nodes`), relative to its top-left; `blit`
    /// translates and propagates them so a boxless element nested in a
    /// float/flex/grid/abspos sub-layout still gets honest geometry (an
    /// IntersectionObserver sentinel hidden in a web component's positioned
    /// shadow subtree). Empty for the render path.
    element_tops: ElementTops,
    /// Incremental-layout boundaries found inside this box (relative to its
    /// top-left); `blit` translates and propagates them so a boundary nested in
    /// a float/flex/grid/abspos sub-layout reaches the document. Empty unless
    /// `capture_boundaries` is set.
    boundary_boxes: BoundaryRecs,
    /// The element this box was laid for (`layout_subtree_inner`'s root) and the
    /// width it was laid at — so `blit` can record this box itself as an
    /// incremental-layout SUB-BOX boundary (a flex/grid item, inline-block cell)
    /// when capturing. `None` for boxes not rooted on an element.
    root: Option<NodeId>,
    lay_width: u16,
    /// Out-of-flow (`position:absolute`/`fixed`) boxes discovered inside this
    /// box, at coordinates relative to its top-left. They are NOT part of
    /// `rows` — an out-of-flow box has no effect on the layout of its
    /// containing block's siblings (CSS 2.1 §9.3.1) and does not change any
    /// box's used height (CSS Overflow 3 §4.1) — so they ride here as a side
    /// channel that `blit` translates and propagates upward, exactly like
    /// `fixed`/`carousels`, and only the document ROOT composites them over the
    /// flow (`composite_positioned`). Each entry's own `b.positioned` is empty
    /// (already flattened into this list at collection time).
    positioned: Vec<PositionedBox>,
}

/// An out-of-flow (`position:absolute`/`fixed`) box awaiting its final
/// composite over the in-flow document (CSS 2.1 §9.6 / CSS Overflow 3 §2.2 —
/// it contributes to scrollable overflow but never to flow height). Collected
/// by `place_positioned_children` at its containing block's placed origin,
/// propagated up through sub-layouts by `blit`, and painted last by
/// `composite_positioned`. `col`/`row` are in the CURRENT buffer's coordinate
/// space until `blit`/`finish` translate them to the document.
#[derive(Clone)]
struct PositionedBox {
    /// Left column of the box's top-left in the current buffer. SIGNED, and
    /// clamped into a real band only at the final composite: a translated box
    /// can legitimately sit LEFT of its (collapsed, fit-content) wrapper's
    /// origin in a sub-layout's coordinate space — Twitch's chat column is at
    /// `-34rem` relative to its 1-cell wrapper — and each `blit` offset
    /// rebases it toward its true document column.
    col: i32,
    /// Top row of the box's top-left in the current buffer (may exceed
    /// `rows.len()` — an out-of-flow box placed below all in-flow content
    /// extends the scrollable region there). Boxes composite in row order so
    /// the image-lift row inserts only shift boxes below them; the source
    /// element and `z-index` are already consumed at collection (the per-CB
    /// occlusion collapse), and each box's items carry their own node id.
    row: usize,
    /// The laid subtree (its `positioned` is empty — nested out-of-flow boxes
    /// are flattened into the owning list at collection time).
    b: LaidBox,
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
    // Test helper: viewport height defaults to 0 (unknown ⇒ `vh` unresolved),
    // so existing width-only tests are unaffected. Tests exercising `vh`/inner
    // scroll call `lay_out_with_carousels` directly with a real height.
    lay_out_with_carousels(dom, base, (width, 0), forms, controls, images, borders).0
}

/// Lay a document out, also returning the horizontally-scrollable strips
/// (carousels), the vertical inner-scroll viewports (regions), the scroll-clip
/// boxes, and the incremental-layout boundaries found so the view can clip/
/// scroll them and a live page can patch a boundary's subtree. The carousel
/// strip items are already in the returned rows; a region instead reserves
/// blank rows in `rows` and carries its content in its own `buffer` (the view
/// windows it).
#[allow(clippy::type_complexity)]
pub fn lay_out_with_carousels(
    dom: &Dom,
    base: &Url,
    viewport: (usize, usize),
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    borders: bool,
) -> (
    Vec<Row>,
    Vec<Carousel>,
    Vec<Region>,
    ScrollClips,
    Vec<BoundaryBox>,
    Vec<FixedItem>,
    HashMap<String, usize>,
) {
    let mut layout = Layout::new(
        dom,
        base,
        viewport.0.max(10),
        forms,
        controls,
        images,
        borders,
    );
    layout.viewport_h = viewport.1;
    // Capture incremental-layout boundaries (INCREMENTAL_LAYOUT_PLAN.md §14).
    // Near-free for non-live pages (no `data-trust-node` baked ⇒ the capture
    // hook short-circuits on the attr gate); bounded for live pages (the cascade
    // checks run only on the sparse baked boundary set).
    layout.capture_boundaries = true;
    layout.flow_all();
    // `flow_all` has composited the out-of-flow boxes over the flow, so
    // `finish` reports none left over (the field is drained at the root).
    let (rows, carousels, fixed, regions, element_tops, scroll_clips, boundary_recs, _positioned) =
        layout.finish();
    let boundaries = harvest_boundaries(boundary_recs, &rows, &regions, &carousels);
    let anchor_rows = anchor_rows_from(dom, &element_tops);
    (
        rows,
        carousels,
        regions,
        scroll_clips,
        boundaries,
        fixed,
        anchor_rows,
    )
}

/// Build the `id`/`<a name>` → first-row map for fragment scrolling from the
/// remapped element flow positions. Keyed by the anchor string (a duplicate id
/// keeps its topmost row).
fn anchor_rows_from(dom: &Dom, tops: &ElementTops) -> HashMap<String, usize> {
    let mut map: HashMap<String, usize> = HashMap::new();
    let mut note = |name: &str, row: usize| {
        map.entry(name.to_string())
            .and_modify(|r| *r = (*r).min(row))
            .or_insert(row);
    };
    for (&node, &(_, row)) in tops {
        let row = row as usize;
        if let Some(id) = dom.attr(node, "id").filter(|v| !v.is_empty()) {
            note(id, row);
        }
        if dom.tag_name(node) == Some("a")
            && let Some(name) = dom.attr(node, "name").filter(|v| !v.is_empty())
        {
            note(name, row);
        }
    }
    map
}

/// Turn the raw per-pass boundary records into `Doc.boundaries`, keeping only the
/// ones the inline splice can apply correctly (INCREMENTAL_LAYOUT_PLAN.md §14).
/// Dropped:
/// - boxes whose row span overlaps a scroll region / carousel — those hold
///   content OUTSIDE `Doc.rows` (a region buffer / a scrolled strip), so the
///   `Doc.rows` splice can't operate on them;
/// - a SUB-BOX (flex/grid item, inline-block) that SHARES a row with a sibling —
///   replacing its rows would drop the sibling. A box OWNS its rows when no item
///   on any of its rows falls outside its `[origin_col, origin_col+width)` span;
///   only then is the row splice safe. A block-filling box owns its (full-width)
///   rows by construction, so the check is skipped for it.
///
/// Anything dropped takes the always-correct full path.
fn harvest_boundaries(
    recs: BoundaryRecs,
    rows: &[Row],
    regions: &[Region],
    carousels: &[Carousel],
) -> Vec<BoundaryBox> {
    let overlaps = |start: usize, end: usize, a: usize, b: usize| start < b && a < end;
    let owns_rows = |rec: &BoundaryRec| -> bool {
        if !rec.sub_box {
            return true; // a block-filling box fills its rows
        }
        let (lo, hi) = (rec.origin_col, rec.origin_col.saturating_add(rec.width));
        !rows[rec.start_row.min(rows.len())..rec.end_row.min(rows.len())]
            .iter()
            .flat_map(|r| &r.items)
            .any(|it| it.col < lo || it.col >= hi)
    };
    recs.into_values()
        .filter(|rec| rec.end_row > rec.start_row)
        .filter(|rec| {
            !regions.iter().any(|rg| {
                overlaps(
                    rec.start_row,
                    rec.end_row,
                    rg.start_row,
                    rg.start_row + rg.height as usize,
                )
            }) && !carousels
                .iter()
                .any(|c| overlaps(rec.start_row, rec.end_row, c.start, c.end))
        })
        .filter(owns_rows)
        .map(|rec| BoundaryBox {
            node: rec.node,
            row_range: rec.start_row..rec.end_row,
            origin_col: rec.origin_col,
            content_width: rec.content_width,
            width: rec.width,
            sub_box: rec.sub_box,
        })
        .collect()
}

/// Lay out a single relayout-boundary subtree (a scroll region) into its buffer
/// rows, for an incremental layout patch (INCREMENTAL_LAYOUT_PLAN.md), MEMOIZING
/// the laid rows of the scroll container's block children (§14 — the inner-scroll
/// de-lag). This mirrors EXACTLY what `flow_region` does to build a
/// `Region.buffer` — lay the boundary's interior at `width` with the region
/// recursion guard set — so the result is byte-identical to the same region
/// produced by a full `lay_out` (the §9 differential guarantee), but reuses
/// `cache.children` for every child whose content hash is unchanged and lays only
/// the new/changed ones: an appended chat message is O(1 message) instead of
/// O(all messages). `boundary` is the scroll-container element in `dom`; the
/// inherited styling context arrives materialized on the fragment (so
/// `computed_value` over `dom` resolves it); an anchor-wrapped boundary is
/// excluded upstream (the actor falls back to full), so the root `Ctx` carries no
/// link. Returns the laid buffer/carousels, the clip boxes of every definite-
/// height scroll box nested in the fragment (so the app can refresh their
/// `clientHeight` — `Doc.scroll_clips` — without a full re-lay), plus the
/// refreshed cache (every child this pass, to seed the next). When the region's
/// structure doesn't fit the cacheable shape (a single-child-descended block
/// BFC with ≥2 block children), it transparently lays the region in full —
/// always correct. Guarded by `region_incremental_layout_matches_full`.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn lay_out_region_fragment_cached(
    dom: &Dom,
    base: &Url,
    content_width: usize,
    viewport: (usize, usize),
    controls: &ControlMap,
    images: &ImageSizes,
    boundary: NodeId,
    cache: &RegionRowCache,
) -> (
    Vec<Row>,
    Vec<Carousel>,
    Vec<(usize, u16, u16)>,
    RegionRowCache,
) {
    let content_width = content_width.max(1);
    let mut layout = Layout::new(
        dom,
        base,
        content_width,
        &[],
        controls,
        images,
        borders_enabled(),
    );
    layout.viewport_w = viewport.0;
    layout.viewport_h = viewport.1;
    layout.region_inner = Some(boundary);
    // The cacheable container (a block BFC with ≥2 block children); `None` ⇒ no
    // memoization, a plain full layout.
    let rc = layout.region_cache_container(boundary).map(|container| {
        // Reuse last pass's rows only if they were laid at this same width.
        let old = if cache.width == content_width {
            cache.children.clone()
        } else {
            HashMap::new()
        };
        std::rc::Rc::new(std::cell::RefCell::new(RegionChildCache {
            container,
            old,
            new: HashMap::new(),
        }))
    });
    layout.region_child_cache = rc.clone();
    let mut buffer =
        layout.layout_subtree_inner(boundary, content_width, None, false, &Ctx::root());
    // A scroll region is a self-contained buffer the app windows on its own, so
    // its out-of-flow boxes can't escape to the document — composite them into
    // the region's rows here (the document root does this in `flow_all`),
    // keeping every side channel (nested scroll clips included) on the buffer.
    // Rare (a region holding an abspos hovercard/badge), and free when empty.
    layout.composite_box_positioned(&mut buffer);
    let new_cache = match rc {
        Some(rc) => RegionRowCache {
            width: content_width,
            children: std::mem::take(&mut rc.borrow_mut().new),
        },
        None => RegionRowCache::default(),
    };
    (
        buffer.rows,
        buffer.carousels,
        buffer.scroll_clips,
        new_cache,
    )
}

/// The result of laying one INLINE relayout-boundary fragment (a block-filling
/// IFC box, NOT a scroll region) for the general incremental splice
/// (INCREMENTAL_LAYOUT_PLAN.md §14). `rows` are in the fragment's own coordinate
/// space (cols from 0); the app shifts them by the cached `origin_col` and
/// splices them into `Doc.rows`. `regions`/`carousels` non-empty means the box
/// now contains content outside pure `Doc.rows` (it grew a scroll viewport /
/// strip since capture) → the app resyncs to the full path.
pub struct SubtreeFragment {
    pub rows: Vec<Row>,
    pub height: usize,
    pub width: u16,
    pub carousels: Vec<Carousel>,
    pub regions: Vec<Region>,
    pub scroll_clips: ScrollClips,
}

/// Lay an INLINE relayout-boundary subtree for the general incremental splice.
/// Two re-lay modes, matching how the full document laid the box:
/// - A BLOCK-FILLING box (`sub_box == false`) went through `flow_element` in
///   flow, so it re-lays through the NORMAL block path (NO `subtree_root`) and
///   re-applies its own `block_indent`/`constrain`/`width`. `content_width` is
///   the OUTER band it fills (captured before its own indent); the re-applied
///   indent/constrain land the content where the full pass did.
/// - A SUB-BOX (`sub_box == true`: a flex/grid item) was laid by the parent's
///   formatting pass via `layout_subtree_inner` with `subtree_root` SET (its own
///   `constrain`/width already applied by the parent), so it re-lays the same way
///   — `subtree_root` set, constrain skipped — at `content_width` = the track
///   width it was given. (Unlike `lay_out_region_fragment`, no region guard.)
///
/// The §9 differential test guards both. An IFC box never overlaps an external
/// float, so no float context is needed; the app adds `origin_col` after.
#[allow(clippy::too_many_arguments)] // a layout entry point with genuinely many inputs
pub fn lay_out_subtree_fragment(
    dom: &Dom,
    base: &Url,
    content_width: usize,
    viewport: (usize, usize),
    controls: &ControlMap,
    images: &ImageSizes,
    boundary: NodeId,
    sub_box: bool,
) -> SubtreeFragment {
    let content_width = content_width.max(1);
    let mut layout = Layout::new(
        dom,
        base,
        content_width,
        &[],
        controls,
        images,
        borders_enabled(),
    );
    layout.viewport_w = viewport.0;
    layout.viewport_h = viewport.1;
    if sub_box {
        // A flex/grid item: the parent already sized it, so skip its own
        // `constrain` (`subtree_root`) — exactly as the full layout's
        // `layout_subtree_inner` did when the parent laid this item.
        layout.subtree_root = Some(boundary);
    }
    layout.flow_node(boundary, &Ctx::root());
    layout.flush_block();
    layout.finish_floats();
    // A fragment is its own document root for its out-of-flow descendants:
    // composite them over its flow before finishing (a full document does this
    // in `flow_all`).
    layout.composite_positioned();
    let (rows, carousels, _fixed, regions, _tops, scroll_clips, _bnd, _pos) = layout.finish();
    let width = rows
        .iter()
        .flat_map(|r| &r.items)
        .map(|it| it.col + it.width)
        .max()
        .unwrap_or(0);
    SubtreeFragment {
        height: rows.len(),
        width,
        rows,
        carousels,
        regions,
        scroll_clips,
    }
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
/// host (the walk follows the light tree). `images` carries the decoded
/// intrinsic dimensions the app's render uses (the actor receives them via
/// `PageCmd::ImageSizes`), so measured boxes match the rendered page — CSSOM
/// View geometry must report the actual layout, and a virtualized feed
/// (Mastodon) caches these heights and feeds them back as placeholder sizes;
/// a divergence there reshapes the document under the reader. Before the
/// first decode arrives the map is simply sparse (CSS/attribute sizing only).
///
/// The box is the element's RENDERED content extent — deliberately, so a page
/// that measures and then renders sees the geometry it will really get (the
/// binding rule: report what we paint). A plain block's declared CSS `width`/
/// `height` is therefore NOT reflected unless the layout actually reserves it
/// (as it does for flex/grid tracks, sized images, etc.); making blocks reserve
/// their declared box is a layout change that geometry would then follow for
/// free — see the geometry notes in CLAUDE.md.
#[allow(clippy::too_many_arguments)] // a layout entry point with genuinely many inputs
pub fn measure_boxes(
    dom: &Dom,
    base: &Url,
    viewport: (usize, usize),
    forms: &[Form],
    controls: &ControlMap,
    cell_px: (u16, u16),
    borders: bool,
    images: &ImageSizes,
) -> HashMap<NodeId, PxRect> {
    let mut layout = Layout::new(
        dom,
        base,
        viewport.0.max(10),
        forms,
        controls,
        images,
        borders,
    );
    layout.viewport_h = viewport.1;
    // Unit resolution converts px→cells against the real cell box, so a
    // measured geometry round-trips to the cells the visible box rendered.
    layout.cell_px_w = cell_px.0.max(1);
    layout.cell_px_h = cell_px.1.max(1);
    layout.tag_all_nodes = true;
    layout.flow_all();
    let declared = layout.declared_boxes.take();
    let clip_heights = layout.clip_heights.take();
    // `finish` returns `element_tops` already remapped through its blank-row
    // collapse (and accumulated from every sub-layout via `blit`), so an
    // empty element's recorded row matches the kept-row grid the cells use.
    let (rows, _carousels, _fixed, _regions, element_tops, _scroll_clips, _boundaries, _positioned) =
        layout.finish();

    // DIAG (TRUST_DIAG_MEASURE): total measured height + the tallest item
    // boxes, to localize an app-render vs engine-measure divergence. Healthy =
    // rows here ≈ the app's doc_rows (DIAGFRAME); a large gap means the two
    // coordinate systems diverged (see the Steam hidden-carousel-page fix in
    // place_positioned_children).
    if std::env::var_os("TRUST_DIAG_MEASURE").is_some() {
        let mut tall: Vec<(u16, usize, NodeId, String)> = Vec::new();
        for (y, r) in rows.iter().enumerate() {
            for it in &r.items {
                if it.height > 8 && it.node != NO_NODE {
                    let cls = dom.attr(it.node, "class").unwrap_or("").to_owned();
                    tall.push((
                        it.height,
                        y,
                        it.node,
                        format!(
                            "{} {}",
                            &it.text.chars().take(24).collect::<String>(),
                            cls.chars().take(48).collect::<String>()
                        ),
                    ));
                }
            }
        }
        tall.sort_by_key(|t| std::cmp::Reverse(t.0));
        eprintln!("DIAGMEASURE rows={} width={}", rows.len(), viewport.0);
        for (h, y, n, d) in tall.iter().take(10) {
            eprintln!("DIAGMEASURE   h={h} row={y} node={n} {d}");
        }
    }
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
        // The counterpart to the floor: `overflow:hidden|clip` + a definite
        // `height` CLIPS the content, so the box is EXACTLY that height. Applied
        // AFTER the floor (so a shorter content is raised to it and a taller one
        // is capped to it → exactly the declared height) and BEFORE the parent
        // unions it (so the parent sees the clipped extent, not the unclipped
        // content). This is what makes a React virtualized-list placeholder
        // (`height:320px;overflow:hidden;opacity:0`) report 320px.
        if let (Some(a), Some(&clip_h)) = (acc.as_mut(), clip_heights.get(&id)) {
            let clip_h = u16::try_from(clip_h).unwrap_or(u16::MAX);
            a.y1 = a.y0.saturating_add(clip_h);
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

/// Inner-scroll GATE diagnostic (gated by `TRUST_DIAG_SCROLL_BOXES` in the
/// one-shot `js::transform` path): list every vertical scroll-container candidate
/// (`overflow-y` ∈ {auto, scroll}) on the LIVE post-JS arena, with its
/// `definite_height` and the height chain that produced it — so we can confirm a
/// real page's scroll area (Twitch chat) resolves a definite H before building
/// the Region primitive. MUST run on the live arena (full cascade), NOT the baked
/// re-parse, which drops `overflow-y`/`min-height`/`max-height` (see dom `PROPS`).
/// Walks the COMPOSED tree so a scroll area inside a shadow root is found too.
pub(crate) fn scroll_box_report(dom: &Dom, base: &Url, viewport: (usize, usize)) -> String {
    let ctrls = ControlMap::new();
    let imgs = ImageSizes::new();
    let mut l = Layout::new(dom, base, viewport.0.max(10), &[], &ctrls, &imgs, false);
    l.viewport_h = viewport.1;
    let desc = |id: NodeId| -> String {
        let tag = dom.tag_name(id).unwrap_or("?");
        let idr = dom
            .attr(id, "id")
            .map(|s| format!("#{s}"))
            .unwrap_or_default();
        let cls = dom
            .attr(id, "class")
            .map(|s| {
                s.split_whitespace()
                    .take(2)
                    .map(|c| format!(".{c}"))
                    .collect::<String>()
            })
            .unwrap_or_default();
        format!("{tag}{idr}{cls}")
    };
    let mut out = String::new();
    let mut n = 0;
    for id in dom.composed_descendants(DOCUMENT) {
        let oy = l.axis_overflow(id, true);
        if !matches!(oy.as_deref(), Some("auto" | "scroll")) {
            continue;
        }
        n += 1;
        if n > 50 {
            out.push_str("\n… (more than 50 scroll boxes; truncated)\n");
            break;
        }
        let dh = l.definite_height(id);
        out.push_str(&format!(
            "\n[{n}] {} overflow-y={} height={:?} -> definite_height={:?}  {}\n",
            desc(id),
            oy.as_deref().unwrap_or("?"),
            dom.computed_style(id, "height"),
            dh,
            if dh.is_some() {
                "✓ REGION-CAPABLE"
            } else {
                "✗ indefinite"
            }
        ));
        // The height chain up to the viewport — shows WHERE a chain breaks
        // (an `auto` ancestor that isn't a stretched row-flex item).
        let mut cur = Some(id);
        let mut depth = 0;
        while let Some(c) = cur {
            if depth > 40 {
                out.push_str("      … (chain deeper than 40; truncated)\n");
                break;
            }
            // Per-node dump: the height inputs (explicit `height`, flex-grow,
            // position + `top`/`bottom`) plus the resolved `definite_height`, so
            // a `dh=None` chain shows exactly which ancestor breaks definiteness.
            out.push_str(&format!(
                "    {}{}  h={:?} disp={:?} flex-dir={:?} grow={:?} pos={:?} top={:?} bot={:?} -> dh={:?}\n",
                "  ".repeat(depth),
                desc(c),
                dom.computed_style(c, "height"),
                dom.computed_display(c),
                dom.computed_style(c, "flex-direction"),
                dom.computed_style(c, "flex-grow"),
                dom.computed_style(c, "position"),
                dom.computed_style(c, "top"),
                dom.computed_style(c, "bottom"),
                l.definite_height(c),
            ));
            cur = dom
                .parent_composed(c)
                .filter(|&p| dom.tag_name(p).is_some());
            depth += 1;
        }
    }
    format!(
        "=== scroll-box report: {n} vertical overflow:auto/scroll element(s) @ viewport {viewport:?} ===\n{out}"
    )
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

/// Hostile-input lid on a single cell's occupancy footprint: `colspan` and
/// `rowspan` are each clamped to 1000 (`cell_span`), but their PRODUCT drives
/// the grid inserts in `build_table_grid` — a page of `rowspan=1000
/// colspan=1000` cells did 10^6 inserts PER CELL. The colspan (the visually
/// meaningful axis) is kept; the rowspan is clamped to fit the area.
const MAX_CELL_SPAN_AREA: usize = 10_000;

/// Recursion lid on nested bordered boxes (the `flow_bordered` analogue of
/// `MAX_TABLE_DEPTH`): each level is a fresh sub-layout on the native stack,
/// so hostile nesting could overflow it. Past the lid an element's border is
/// dropped and its interior flows as a plain block. 32 frames already eat 64
/// columns — beyond any legible terminal rendering.
const MAX_BORDER_DEPTH: usize = 32;

/// The containing block a percentage `height` resolves against (`Layout::
/// height_cb`): a block-level ancestor element, or the viewport (the initial
/// containing block) at the root.
enum CbHeight {
    Element(NodeId),
    Viewport,
}

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
/// the old `<pre>` bool it generalizes). Shared with layout2 (the one
/// white-space model — it moves there when this engine is deleted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WhiteSpace {
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
    pub(crate) fn from_css(value: &str) -> Option<WhiteSpace> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(WhiteSpace::Normal),
            "nowrap" => Some(WhiteSpace::Nowrap),
            "pre" => Some(WhiteSpace::Pre),
            // `break-spaces` (CSS Text 4) = preserve + wrap, differing from
            // pre-wrap only in trailing-space breaking — at terminal cell
            // resolution the pre-wrap chunker already breaks anywhere, so
            // they coincide (documented approximation).
            "pre-wrap" | "break-spaces" => Some(WhiteSpace::PreWrap),
            "pre-line" => Some(WhiteSpace::PreLine),
            _ => None,
        }
    }

    /// Fold the CSS Text 4 longhands over this (shorthand-derived) mode:
    /// `white-space-collapse` replaces the collapse half, `text-wrap-mode`
    /// the wrap half — §2: `white-space` is now their shorthand. Approximations
    /// (terminal scale): `break-spaces`/`preserve-spaces` act as `preserve`,
    /// and preserve-breaks + nowrap has no variant here (stays `PreLine`).
    pub(crate) fn with_longhands(self, collapse: Option<&str>, nowrap: Option<bool>) -> WhiteSpace {
        // Decompose to (collapse: 0 collapse / 1 preserve / 2 preserve-breaks,
        // nowrap), override the declared half, recompose.
        let (mut c, mut nw) = match self {
            WhiteSpace::Normal => (0u8, false),
            WhiteSpace::Nowrap => (0, true),
            WhiteSpace::Pre => (1, true),
            WhiteSpace::PreWrap => (1, false),
            WhiteSpace::PreLine => (2, false),
        };
        if let Some(v) = collapse {
            match v.trim().to_ascii_lowercase().as_str() {
                "collapse" => c = 0,
                "preserve" | "break-spaces" | "preserve-spaces" => c = 1,
                "preserve-breaks" => c = 2,
                _ => {}
            }
        }
        if let Some(n) = nowrap {
            nw = n;
        }
        match (c, nw) {
            (0, false) => WhiteSpace::Normal,
            (0, true) => WhiteSpace::Nowrap,
            (1, false) => WhiteSpace::PreWrap,
            (1, true) => WhiteSpace::Pre,
            _ => WhiteSpace::PreLine,
        }
    }
    /// Whether runs of spaces collapse to a single space.
    pub(crate) fn collapses_spaces(self) -> bool {
        matches!(
            self,
            WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
        )
    }
    /// Whether literal `\n` forces a line break.
    pub(crate) fn preserves_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }
    /// Whether lines wrap at the content width.
    pub(crate) fn wraps(self) -> bool {
        !matches!(self, WhiteSpace::Nowrap | WhiteSpace::Pre)
    }
}

/// CSS `text-transform`: alters the rendered text of a run. Inherits, so
/// it rides on the inline `Ctx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextTransform {
    None,
    Upper,
    Lower,
    Capitalize,
}

impl TextTransform {
    pub(crate) fn from_css(value: &str) -> Option<TextTransform> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(TextTransform::None),
            "uppercase" => Some(TextTransform::Upper),
            "lowercase" => Some(TextTransform::Lower),
            "capitalize" => Some(TextTransform::Capitalize),
            _ => None,
        }
    }
    /// Apply the transform to a text run (borrowing unchanged when `None`).
    pub(crate) fn apply<'t>(self, s: &'t str) -> std::borrow::Cow<'t, str> {
        use std::borrow::Cow;
        match self {
            TextTransform::None => Cow::Borrowed(s),
            TextTransform::Upper => Cow::Owned(s.to_uppercase()),
            TextTransform::Lower => Cow::Owned(s.to_lowercase()),
            TextTransform::Capitalize => Cow::Owned(capitalize_words(s)),
        }
    }
}

/// CSS "document white space" (CSS Text 3 §4.1.1): the ONLY characters that
/// collapse and offer soft-wrap opportunities in the collapsing `white-space`
/// modes. Deliberately NOT `char::is_whitespace`: U+00A0 NO-BREAK SPACE (and
/// U+202F) have Unicode `White_Space=Yes` but are non-collapsible, NON-BREAKING
/// glue — `10&nbsp;000` must neither wrap between its halves nor collapse its
/// run (`&nbsp;&nbsp;&nbsp;` indentation is real spacing on old table-layout
/// pages). Other Unicode spaces (em/ideographic) likewise render as themselves;
/// treating them as word glue costs only a rare break opportunity.
pub(crate) fn is_collapsible_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
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
pub(crate) fn format_list_marker(kind: &str, n: i64) -> String {
    // css-counter-styles-3 §3: a <counter-style-name> that doesn't name a
    // style we implement falls back to DECIMAL (the `_` arm — it was a
    // bullet, which numbered nothing). Alphabetic/roman systems are defined
    // for n ≥ 1 only; outside their range the marker also falls back to
    // decimal (a `<ol reversed>` can count through zero into negatives).
    let alpha = |n: i64, upper: bool| match u32::try_from(n) {
        Ok(v) if v >= 1 => format!("{}. ", alpha_marker(v, upper)),
        _ => format!("{n}. "),
    };
    let roman = |n: i64, upper: bool| match u32::try_from(n) {
        Ok(v) if v >= 1 => format!("{}. ", roman_marker(v, upper)),
        _ => format!("{n}. "),
    };
    match kind {
        "none" => String::new(),
        "disc" => "• ".to_owned(),
        "circle" => "◦ ".to_owned(),
        "square" => "▪ ".to_owned(),
        "decimal" => format!("{n}. "),
        "decimal-leading-zero" => format!("{n:02}. "),
        "lower-alpha" | "lower-latin" => alpha(n, false),
        "upper-alpha" | "upper-latin" => alpha(n, true),
        "lower-roman" => roman(n, false),
        "upper-roman" => roman(n, true),
        _ => format!("{n}. "),
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
    /// CSS `font-size:0` collapses text to zero cells (the copyable-but-unseen
    /// idiom — Mastodon's `.invisible` URL scheme/tail). Inherits down the
    /// formatting context; an absolute font-size reset re-shows a descendant.
    font_zero: bool,
    /// Paint suppression for TEXT/emission in this formatting context: items are
    /// laid out normally but tagged `invisible` so the renderer writes blank
    /// cells (see `Item.invisible`). Unlike `font_zero`, the box is RESERVED,
    /// not collapsed. This is the element's FULL suppression — the sticky opacity
    /// chain (`opacity_hidden`) OR its own `visibility:hidden` — so a text child,
    /// which inherits its parent's visibility, paints blank correctly. The
    /// authority for the CURRENT layer's emission is the `Layout.invisible` field
    /// (set from this on entering each element); `Ctx.invisible` carries it
    /// across sub-layout boundaries (where a fresh `Layout` re-derives it).
    invisible: bool,
    /// The STICKY `opacity:0` (`paint_suppressed`) chain ONLY — accumulated down
    /// the tree because opacity applies to a subtree as a group (a descendant
    /// cannot re-reveal it). Kept SEPARATE from `invisible` because `visibility`
    /// is re-clearable: an element child derives its own suppression from THIS
    /// (opacity, sticky) plus its own computed `visibility` — never from the
    /// parent's `visibility`, which would wrongly stick. (`invisible` folds in
    /// the current element's visibility for its text; element children ignore
    /// that part and re-read `visibility` via the cascade.)
    opacity_hidden: bool,
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
            font_zero: false,
            invisible: false,
            opacity_hidden: false,
            node: NO_NODE,
            link: None,
        }
    }
}

/// Insert `cells` spaces between each pair of characters (CSS `letter-spacing`
/// rendered as whole-cell tracking). A no-op for `cells == 0` or a single
/// character, so it borrows in the common case.
pub(crate) fn letter_space(word: &str, cells: usize) -> std::borrow::Cow<'_, str> {
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

/// The page's standard preview image (Open Graph `og:image`, else Twitter's
/// `twitter:image`), resolved to an absolute http(s) URL. This is the
/// cross-site convention for "a still frame of this page's media" — used to
/// give an unplayable streaming `<video>` a poster. Host-agnostic: no site
/// knows it's being read this way.
pub(crate) fn page_preview_image(dom: &Dom, base: &Url) -> Option<String> {
    for key in ["og:image", "twitter:image", "og:image:secure_url"] {
        if let Some(src) = dom.meta_content(key)
            && let Link::Http(u) = crate::http::resolve(base, src)
        {
            return Some(u.to_string());
        }
    }
    None
}

/// Whether the page declares ITSELF a video page via the standard Open Graph
/// convention: an `og:video`(/`:secure_url`) resource, or an `og:type` in the
/// `video.*` hierarchy (ogp.me — `video.movie`/`.episode`/`.tv_show`/
/// `.other`, what every watch page ships for embeds/shares). The cross-site
/// signal — never a host check — shared by the page-level mpv fallback, the
/// streaming-`<video>` play-the-page representation, and the preview-image
/// decode gate: "play this page in mpv" is only an honest offer on a page
/// whose canonical content IS a video (a homepage with an autoplaying hero
/// is not one, and yt-dlp finds nothing there).
pub(crate) fn page_declares_video(dom: &Dom) -> bool {
    ["og:video", "og:video:secure_url"]
        .into_iter()
        .any(|k| dom.meta_content(k).is_some())
        || dom
            .meta_content("og:type")
            .is_some_and(|t| t.trim().to_ascii_lowercase().starts_with("video"))
}

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
    /// The full terminal HEIGHT in cells — the CSS viewport for `vh`/`vmin`/
    /// `vmax` units and the basis a `height:100%` chain terminates against. `0`
    /// when a caller didn't thread it (legacy/test paths), which leaves `vh`
    /// and viewport-relative heights unresolved rather than zero. The
    /// foundation for inner scroll regions (a definite region height) — see
    /// INNER_SCROLL_PLAN.md.
    viewport_h: usize,
    rows: Vec<Row>,
    /// The widest flex/grid row USED width seen in this (sub-)layout, in cells —
    /// the rightmost allocated column, which can exceed the rightmost PAINTED
    /// cell because a flex item occupies its full main size even when its
    /// content is narrower (CSS Flexbox §9.9.1). `layout_subtree_inner` floors
    /// the box's reported width at this so a container reports its true used
    /// width (else an ancestor flex row under-measures it and wrongly stacks —
    /// Steam's `hero_capsule` spotlight carousel). `0` when no flex/grid row was
    /// laid. Fresh per sub-layout (not copied by `make_sub`).
    flex_min_width: usize,
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
    /// One `(counter, step)` per open list level: the NEXT item's number and
    /// the per-item increment — `+1`, or `-1` for `<ol reversed>` (HTML
    /// §4.4.5: reversed lists count DOWN, and may pass zero into negatives).
    list_stack: Vec<(i64, i64)>,
    /// Horizontally-scrollable strips discovered during the pass.
    carousels: Vec<Carousel>,
    /// `position:fixed` boxes captured into the pinned overlay layer during the
    /// pass (viewport-relative once translated up by `blit`). See `FixedItem`.
    fixed: Vec<FixedItem>,
    /// Vertical inner-scroll viewports (`overflow-y:auto|scroll` on a
    /// definite-height box) discovered during the pass — each reserves `height`
    /// blank doc rows and holds its content in a separate `buffer`. See `Region`.
    regions: Vec<Region>,
    /// Out-of-flow (`position:absolute`/`fixed`) boxes collected during the
    /// pass, at their containing block's placed coordinates. They are kept OUT
    /// of `rows` so they never inflate an ancestor's height or push its
    /// siblings (CSS 2.1 §9.3.1); `blit` propagates them upward and only the
    /// document root composites them over the flow (`composite_positioned`),
    /// which is what extends the scrollable region to reach them (CSS Overflow
    /// 3 §2.2) without disturbing in-flow layout. See `PositionedBox`.
    positioned: Vec<PositionedBox>,
    /// The CLIP box `(live_node, client_h_rows, client_w_cells)` of EVERY
    /// definite-height scroll-y box flowed — region OR currently-fitting — keyed
    /// by its baked actor node id. The app pushes these as `clientHeight`/
    /// `clientWidth` so a chat's `atBottom` is right BEFORE its content overflows
    /// into a region (Phase 3 inner scroll). Only boxes with a `data-trust-node`.
    scroll_clips: Vec<(usize, u16, u16)>,
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
    /// When laying a scroll region's own content into its `buffer`
    /// (`flow_region`'s sub-pass), this is that element: its region routing is
    /// skipped so the sub-layout flows its children normally instead of
    /// re-entering `flow_region` on the same node forever (mirrors
    /// `inner_border_box`).
    region_inner: Option<NodeId>,
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
    /// How many bordered boxes enclose this pass (`flow_bordered` sub-layouts,
    /// carried through `make_sub`) — the `MAX_BORDER_DEPTH` recursion lid.
    border_depth: usize,
    /// Whether the active nowrap clip marks its truncation with `…`
    /// (`text-overflow: ellipsis` or a custom-string value — CSS Overflow 3
    /// §5.1) or cuts silently (the initial `clip`).
    clip_ellipsis: bool,
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
    /// left out. Populated under `tag_all_nodes` (never while `measuring` —
    /// a probe band would clamp the recorded width), so the render path never
    /// touches it. SHARED across sub-layouts via the `Rc` (like
    /// `measure_cache`): the values are position-independent SIZES keyed by
    /// NodeId, so unlike `element_tops` they need no blit offset remapping —
    /// a floor recorded inside a flex item's sub-layout used to be silently
    /// dropped with that sub's map. See [[js-geometry-real-boxes]] Phase 2.
    declared_boxes: DeclaredBoxes,
    /// Measurement pass only: every element's flow position `(col, row)` at the
    /// moment the flow enters it — captured for EVERY element, including empty
    /// ones that paint no cells. `measure_boxes` uses it to give a boxless
    /// element (an infinite-scroll sentinel — often empty, often in a web
    /// component's shadow root) a real ZERO-HEIGHT box at its true position, so
    /// `getBoundingClientRect`/IntersectionObserver have honest geometry instead
    /// of a viewport-fallback lie. Populated under `tag_all_nodes`; the render
    /// path never touches it.
    element_tops: ElementTops,
    /// Live full render only (INCREMENTAL_LAYOUT_PLAN.md §14): capture each
    /// block-filling IFC boundary's outer band + row span into `boundary_boxes`,
    /// for the general incremental subtree splice. OFF for tests / non-live
    /// renders / measurement, so they pay nothing.
    capture_boundaries: bool,
    /// Incremental-layout boundaries recorded this pass (keyed by parse NodeId);
    /// `blit` translates them up + `finish` remaps their rows. See `BoundaryRec`.
    boundary_boxes: BoundaryRecs,
    /// Memoizes the intrinsic-width measurement `measure_width(id, constraint)`
    /// for the WHOLE lay-out pass (SHARED across sub-layouts via the `Rc`). The
    /// measurement lays out the entire subtree, and a flex/grid container
    /// measures each item (min- AND max-content) and THEN lays it out for real —
    /// each pass re-creating a sub-layout — so a nested flex/grid tree re-measures
    /// the same subtrees EXPONENTIALLY in depth. A styled-components SPA (Twitch)
    /// is ~all `display:flex`, which made a ~700KB live re-render take ~2s and
    /// peg a core. The measurement is a pure function of `(id, constraint,
    /// table_depth)` — `measure_width` always uses `Ctx::root()`, `measure=true`,
    /// and `subtree_root=id`; the only other input that varies mid-pass is the
    /// table nesting depth (`flow_table` ±1's it). The DOM is immutable during a
    /// pass, so caching collapses the blow-up to linear. Fresh per pass.
    measure_cache: MeasureCache,
    /// Region child-row memoization (INCREMENTAL_LAYOUT_PLAN.md §14). `Some`
    /// only on the region-patch path (`lay_out_region_fragment_cached`): when the
    /// block flow reaches `container`, each child is reused from `old` (by content
    /// hash) instead of re-laid, and every child is recorded into `new`. `None`
    /// everywhere else (the full document layout, measurement, tests) — zero cost.
    region_child_cache: Option<RegionCache>,
    /// `<video>`/`<audio>` nodes this Layout has emitted as a media
    /// representation (`flow_media`). Per-Layout (NOT shared with sub-layouts):
    /// an abspos media element's in-flow wrapper dispatch and the coordinate
    /// model (`place_positioned_children`) both run inside the Layout of the
    /// media's nearest positioned ancestor, so a local set connects them — and,
    /// unlike a shared set, it can't dedupe across the redundant subtree re-lays
    /// nested positioning spawns (which landed the lone render in a discarded
    /// re-lay). See `flow_media` and `place_positioned_children`.
    media_emitted: std::collections::HashSet<NodeId>,
    /// Set while laying a SHRINK-TO-FIT box (an abspos `width:auto` card sizing
    /// to content). Makes a `width:%` replaced child with no definite-width
    /// ancestor contribute its intrinsic width rather than stretching to the
    /// flow box, so the box wraps to its content. Propagated to sub-layouts (the
    /// image can be nested). See `image_used_box` and `place_positioned_children`.
    shrink_wrap: bool,
    /// Paint suppression for the item currently being emitted: `true` while the
    /// flow is inside an `opacity:0` (`paint_suppressed`) element. Every emitted
    /// `Item` copies this into `Item.invisible` so the renderer writes blank
    /// cells while the box still reserves its space/geometry. It's the CURRENT
    /// layer's authority (set from `ctx.invisible || paint_suppressed(id)` at
    /// each element and refreshed before the block-tail emitters, since the
    /// child recursion overwrites it); `Ctx.invisible` carries it across
    /// sub-layout boundaries, where a fresh `Layout` re-derives its own value.
    invisible: bool,
    /// Measurement pass only (`measure_boxes`): the definite CONTENT height (in
    /// rows) of each block that clips its vertical overflow (`overflow`/
    /// `overflow-y` ∈ {hidden, clip} + a definite `height`). `measure_boxes`
    /// CAPS the block's measured box to exactly this — the counterpart to the
    /// `declared_boxes` floor — so a `height:N;overflow:hidden` placeholder
    /// reports exactly N (its cached height), not the taller unclipped content
    /// (React virtualized lists cache the measured height then clip). Populated
    /// under `tag_all_nodes` (never while `measuring`); the render path never
    /// touches it. SHARED across sub-layouts via the `Rc` like
    /// `declared_boxes` (row counts are position-independent) — a VISIBLE
    /// clipped box nested in a flex item's sub-layout used to report its
    /// unclipped extent because the entry died with the sub's map.
    clip_heights: ClipHeights,
    /// The terminal's real cell box in px (from the `CELL_PX_W`/`CELL_PX_H`
    /// globals, or set explicitly by `measure_boxes`). Every absolute CSS
    /// length converts to cells through these via `Units`, so a px length
    /// occupies the same physical extent a browser gives it on any terminal
    /// font. Copied onto sub-layouts.
    cell_px_w: u16,
    cell_px_h: u16,
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
            viewport_h: 0,
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
            flex_min_width: 0,
            carousels: Vec::new(),
            fixed: Vec::new(),
            regions: Vec::new(),
            positioned: Vec::new(),
            scroll_clips: Vec::new(),
            suppressed_controls: std::collections::HashSet::new(),
            measuring: false,
            table_depth: 0,
            subtree_root: None,
            inner_border_box: None,
            region_inner: None,
            borders,
            clip_right: None,
            clip_done: false,
            border_depth: 0,
            clip_ellipsis: false,
            modal_root: None,
            tag_all_nodes: false,
            declared_boxes: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
            element_tops: HashMap::new(),
            capture_boundaries: false,
            boundary_boxes: HashMap::new(),
            measure_cache: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
            region_child_cache: None,
            media_emitted: std::collections::HashSet::new(),
            shrink_wrap: false,
            invisible: false,
            clip_heights: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
            cell_px_w: cell_px_w(),
            cell_px_h: cell_px_h(),
        }
    }

    /// The unit-resolution context for `id`: its computed font-size (`em`),
    /// the root's (`rem`), and this layout's cell box (px → cells).
    fn units(&self, id: NodeId) -> Units {
        Units {
            fs: self.dom.font_px(id),
            root: self.dom.root_font_px(),
            cell_w: f32::from(self.cell_px_w.max(1)),
            cell_h: f32::from(self.cell_px_h.max(1)),
        }
    }

    /// Rows a box RESERVES for its clipped vertical overflow — the counterpart
    /// to `clip_heights` on the render side. `Some(rows)` when the box clips its
    /// vertical overflow (`clips_overflow_y`) AND has a definite absolute
    /// `height`; `None` otherwise (or a `%`/`vh`/`auto` height). The px→rows
    /// conversion uses the real cell height so the reserved rows match what the
    /// visible (unclipped) version rendered.
    fn clip_reserve_rows(&self, id: NodeId) -> Option<usize> {
        if !self.clips_overflow_y(id) {
            return None;
        }
        let h = self.dom.computed_style(id, "height")?;
        css_length_rows(&h, self.units(id))
    }

    /// Whether an element clips its VERTICAL overflow with `hidden`/`clip`
    /// (NOT `auto`/`scroll` — those establish a scroll region, sized
    /// elsewhere). A definite `height` + this ⇒ the box is exactly that
    /// height, so `measure_boxes` caps the measured geometry to it (a React
    /// virtualized-list placeholder's `height:N;overflow:hidden`). Per-axis
    /// via `axis_overflow`, so the `overflow-y` longhand wins over the
    /// shorthand and a two-value `overflow: auto hidden` (y = hidden) clips.
    fn clips_overflow_y(&self, id: NodeId) -> bool {
        matches!(
            self.axis_overflow(id, true).as_deref(),
            Some("hidden" | "clip")
        )
    }

    /// Whether an element clips/scrolls its HORIZONTAL axis — used
    /// `overflow-x` ∈ {`auto`, `scroll`, `hidden`, `clip`}, resolved per-axis
    /// via `axis_overflow` (the longhand wins over the shorthand's x
    /// component). Two consumers: a non-wrapping flex row inside such a
    /// container keeps its items side by side (clipped) instead of reflowing
    /// into a vertical stack (see `flow_flex_row`), and a
    /// `white-space:nowrap` block that clips truncates its single line at its
    /// box edge (the single-line-ellipsis card idiom).
    fn clips_x(&self, id: NodeId) -> bool {
        matches!(
            self.axis_overflow(id, false).as_deref(),
            Some("hidden" | "clip" | "auto" | "scroll")
        )
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
            // Emitted FIRST (before the body's own content) — a video/audio
            // page's player is normally near the top of the page, so a
            // synthesized fallback belongs there too, not buried at the very
            // bottom of a long channel/chat page where nobody would find it.
            self.flow_page_level_media_fallback();
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
        // Paint every collected out-of-flow box over the finished in-flow
        // document (CSS 2.1 §9.6 painting / CSS Overflow 3 §2.2 scrollable
        // region). This is the ONE place the document composites its positioned
        // layer; sub-layouts only propagate their boxes up here (via `blit`),
        // never inflating an ancestor's flow (§9.3.1).
        self.composite_positioned();
    }

    /// A page-level "▶ Watch in mpv" fallback for a video page whose player
    /// never inserts an actual `<video>`/`<audio>` ELEMENT into the DOM at
    /// all — so `flow_media`'s per-element dispatch (which already handles a
    /// PRESENT-but-sourceless `<video>`, e.g. an MSE/blob streaming player)
    /// never gets a chance to run. Modern streaming players increasingly
    /// mount their playback surface only after negotiating a manifest/token
    /// (WebCodecs/MSE-in-workers, low-latency canvases, …); when that
    /// negotiation is walled off (Twitch's Kasada bot-check), the player can
    /// give up before ever creating the `<video>` tag, silently removing the
    /// ONLY hook our engine has for a mpv affordance — the exact "engine got
    /// advanced enough to try loading the player, and that ate the mpv link"
    /// regression she flagged. General, host-agnostic: gated on the standard
    /// Open Graph video protocol (`og:video`/`og:video:secure_url`, the same
    /// cross-site convention `page_preview_image` already reads for a
    /// poster), never on a specific host. Plays the PAGE url via yt-dlp/
    /// streamlink, exactly like the sourceless-`<video>` streaming case
    /// (`og:video`'s own URL is usually an iframe-embed player, not a
    /// yt-dlp-playable target). Called BEFORE the body's own content flows
    /// (so it lands near the top, where a real player would sit), gated
    /// purely on "no `<video>`/`<audio>` tag exists ANYWHERE in the DOM" — a
    /// present tag always gets its own per-element representation via
    /// `flow_media` instead, so the two can never double up.
    fn flow_page_level_media_fallback(&mut self) {
        if self
            .dom
            .descendants(DOCUMENT)
            .any(|id| matches!(self.dom.tag_name(id), Some("video" | "audio")))
            || !page_declares_video(self.dom)
        {
            return;
        }
        let mut mctx = Ctx::root();
        mctx.link = Some(Link::Media(self.base.clone()));
        mctx.kind = ItemKind::Link;
        self.invisible = false;
        self.flush_block();
        self.begin_line();
        let mut preview_drawn = false;
        if let Some(poster) = page_preview_image(self.dom, self.base)
            && let Some(&(iw, ih)) = self.images.get(&poster)
            && iw > 0
            && ih > 0
        {
            let avail = self.width.max(1) as u16;
            let w = iw.min(avail).max(1);
            let h = ((ih as u32 * w as u32) / iw as u32).max(1) as u16;
            self.line.push(Item {
                col: self.col as u16,
                width: w,
                height: h,
                image: Some(poster),
                crop: false,
                pixelated: false,
                text: String::new(),
                kind: ItemKind::Image,
                emph: Emphasis::default(),
                node: NO_NODE,
                link: mctx.link.clone(),
                invisible: false,
            });
            self.col += w as usize;
            self.line_height = self.line_height.max(h);
            self.break_line();
            preview_drawn = true;
        }
        // The drawn preview IS the mpv link (her call 2026-07-04); the text
        // affordance stands in only until/unless a preview frame renders.
        if !preview_drawn {
            self.place_text("▶ Watch in mpv", &mctx);
            self.break_line();
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
        // A closed `<details>` renders ONLY its first `<summary>` child (HTML
        // rendering, "the details element": without `open` the second slot —
        // everything else, including bare text nodes — is not rendered). The
        // dialog/popover analogues live in `Dom::is_hidden`, but the details
        // rule hides the CONTENT, not an element with a display to override,
        // so the flow's child chokepoint is the faithful place for it.
        if self.dom.tag_name(id) == Some("details") && self.dom.attr(id, "open").is_none() {
            return self
                .dom
                .children(id)
                .into_iter()
                .find(|&c| self.dom.tag_name(c) == Some("summary"))
                .into_iter()
                .collect();
        }
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
        crate::dom::casc_note_flow_visit();
        let Some(tag) = self.dom.tag_name(id).map(str::to_owned) else {
            return;
        };
        if SKIP.contains(&tag.as_str())
            || self.dom.is_hidden(id)
            || self.suppressed_controls.contains(&id)
            || self.is_clipped_offscreen(id)
            || self.is_clip_collapsed(id)
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
        // Fragment scroll targets: record the flow row where every id-bearing
        // element (and legacy `<a name>`) is entered, so a `#id` link / URL /
        // live hash-change can scroll it to the top. Reuses the geometry
        // `element_tops` map — remapped through the blank-row collapse in
        // `finish` and merged across sub-layouts — so nested (flex/grid/
        // bordered) anchors resolve too. Captured HERE, ahead of the early
        // dispatches below, so replaced/control targets (`<img id>`, form
        // controls, media) and floated ones anchor at their entry position; an
        // out-of-flow target is captured inside its own placement sub-pass
        // (the guard above returns before this) and `blit` merges it at its
        // PLACED position. Cheap: one attr probe per element, id'd elements
        // only. Under `tag_all_nodes` every element is recorded further down,
        // so skip it here.
        if !self.tag_all_nodes
            && (self.dom.attr(id, "id").is_some_and(|v| !v.is_empty())
                || (tag == "a" && self.dom.attr(id, "name").is_some_and(|v| !v.is_empty())))
        {
            self.element_tops.entry(id).or_insert((
                u16::try_from(self.col).unwrap_or(u16::MAX),
                u16::try_from(self.rows.len()).unwrap_or(u16::MAX),
            ));
        }
        // Paint suppression rides the formatting context (like `font_zero`): a
        // suppressed element and its subtree are laid out normally but painted
        // BLANK. Two CSS mechanisms, combined here:
        //   - `opacity:0` (Phase 1) — STICKY: opacity applies to the subtree as a
        //     group, so a descendant can't re-reveal it. Accumulated in the
        //     `opacity_hidden` chain.
        //   - `visibility:hidden` (Phase 2) — INHERITED but RE-CLEARABLE: a
        //     `visibility:visible` descendant of a hidden ancestor IS painted.
        //     The cascade (`computed_value`, via `visibility_hidden`) resolves
        //     the inheritance/override PER ELEMENT, so it's read fresh here and
        //     never propagated as sticky.
        // Set here (before every emission path — media/float/`<img>`/form
        // controls/`<hr>`, and the block dispatch below) and copied onto `cctx`
        // so it flows to children and across sub-layout boundaries. NOT
        // save/restored across the whole function: the block-tail emitters
        // refresh it from `cctx` (the child recursion overwrites `self.invisible`),
        // and after this element returns the caller re-sets it before its own
        // next emission — so a stale value is never read.
        let opacity_hidden = ctx.opacity_hidden || self.dom.paint_suppressed(id);
        self.invisible = opacity_hidden || self.dom.visibility_hidden(id);
        // Capture this element's pinned `position:fixed` children into the
        // overlay layer at the parent's content origin (their STATIC position —
        // the reserved flex column for Mastodon's side rails); `blit` carries
        // them up to the document. Done here — after the out-of-flow guard, so a
        // skipped fixed box can't re-enter — because a fixed child of a FLEX
        // container is not a flex item and the flex layout never visits it.
        self.capture_fixed_children(id);
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
            // …but only when the wrapper really is player CHROME. A wrapper
            // whose subtree holds a visible IN-FLOW <img> is the other idiom:
            // a content image with a video overlaid on top (Steam's sale
            // capsules mount an abspos microtrailer <video> beside the capsule
            // <img> on first hover — treating that as a player ate the image
            // and the capsule collapsed to its price). Player chrome (video.js
            // / Plyr / JW) draws its poster as a background-image div, never
            // an in-flow <img>, so this test keeps the skip for real players
            // while an image-bearing wrapper flows normally (its <video>
            // child then renders — or suppresses — via its own dispatch).
            let has_content_img = self.dom.descendants(id).any(|d| {
                self.dom.tag_name(d) == Some("img")
                    && !self.is_out_of_flow(d)
                    && !self.dom.is_hidden(d)
            });
            if !has_content_img {
                let mtag = self.dom.tag_name(media).unwrap_or("video").to_owned();
                self.flow_media(media, &mtag, ctx);
                return;
            }
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
            "input" | "textarea" | "select" => {
                self.flow_form_control(id, &tag, ctx.link.clone());
                return;
            }
            // A `<button>` is normally an atomic widget stub (`[ label ]`). But an
            // ICON-ONLY button (no visible text, an `<svg>`→`<img>` icon inside)
            // renders its ICON like an `<a>`/`<div>` does — the document made it an
            // icon, so we draw the icon (the stub path threw it away and fell back
            // to the `aria-label`, which is how YouTube's masthead chrome came out
            // as long German smears). Such a button is NOT matched here: it falls
            // through to normal inline flow (its `<img>` renders). This holds even
            // for a form-bound icon button (a magnifier submit) — its submit link
            // is threaded onto the icon below, so it stays clickable. Text/labeled
            // buttons keep the readable stub.
            "button" if !self.button_is_icon_only(id) => {
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
        // An icon-only `<button>` that fell through the match above keeps its
        // click semantics: if it's a form control with no ambient (live-page)
        // link of its own, its icon carries the form's submit `Link` so a click
        // still submits — the stub path's behaviour, minus the stub.
        if tag == "button"
            && cctx.link.is_none()
            && let Some(&(form, field)) = self.controls.get(&id)
        {
            cctx.link = Some(Link::Form { form, field });
        }
        // A hover host (the live serializer's `data-trust-hover` marker): tag
        // the items flowed beneath it with this element, so the app's hover
        // hit-test can resolve a cell back to the actor node via the
        // parse-time `Doc.hover_ids` map — the render pass otherwise only
        // attributes items to anchors. A deeper anchor/host still overrides
        // (nearest marker wins), and unmarked pages pay one attr miss.
        if self.dom.attr(id, "data-trust-hover").is_some() {
            cctx.node = id;
        }
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
            .and_then(|v| resolve_cells(v, 1, (self.viewport_w, self.viewport_h), self.units(id)))
            .unwrap_or(0);
        // `font-size:0` collapses this element's text to nothing. Definitive on
        // the element wins; otherwise inherit the parent's answer (relative
        // sizes scale zero to zero, an absolute reset re-shows a descendant).
        cctx.font_zero = self.dom.font_size_zero(id).unwrap_or(ctx.font_zero);
        // Paint suppression flows to children/sub-layouts through the context:
        // `invisible` (the element's FULL state) is what a text child inherits;
        // `opacity_hidden` (the sticky opacity chain) is what an element child
        // accumulates — it re-derives its own `visibility` from the cascade, so
        // the parent's visibility must NOT ride along as sticky.
        cctx.invisible = self.invisible;
        cctx.opacity_hidden = opacity_hidden;

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
        // Never while `measuring`: the maps are SHARED across sub-layouts now,
        // and an intrinsic-width probe's clamped band would poison the
        // recorded floor.
        if self.tag_all_nodes && !self.measuring && block_like {
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
                .filter_map(|v| css_length_rows(&v, self.units(id)))
                .max();
            if floor_w.is_some() || floor_h.is_some() {
                self.declared_boxes
                    .borrow_mut()
                    .insert(id, (floor_w.unwrap_or(0), floor_h.unwrap_or(0)));
            }
            // The COUNTERPART cap: a definite `height` with `overflow`/
            // `overflow-y` ∈ {hidden, clip} clips its content vertically, so the
            // box is EXACTLY that height even when its (unclipped) content is
            // taller. Record it so `measure_boxes` caps the measured box — a
            // React virtualized-list placeholder (`height:320px;overflow:hidden;
            // opacity:0` holding a full clipped article) then reports 320px, its
            // cached height, not the article's real extent. Only a DEFINITE
            // `height` clips to a known box; `min-height`/`auto` don't cap.
            if let Some(h) = self.clip_reserve_rows(id) {
                self.clip_heights.borrow_mut().insert(id, h);
            }
        }
        // A `display:table` element establishes a table formatting context: its
        // rows lay their cells side by side into computed columns (CSS 2.1 §17),
        // instead of stacking every `<td>` as its own block. Routed before the
        // border/flex/block dispatch so the whole table subtree is laid by the
        // table algorithm. `flow_table` does its own block framing.
        //
        // `establishes_anonymous_table` catches the §17.2.1 "generate missing
        // parents" case: a `<table>` whose `display` was overridden to `block`
        // (GitHub/markdown CSS does this for horizontal scroll) still wraps its
        // row-group children in an anonymous table, so it lays as a table too.
        if block_like
            && (matches!(
                self.dom.effective_display(id).as_deref(),
                Some("table" | "inline-table")
            ) || self.dom.establishes_anonymous_table(id))
        {
            self.flow_table(id, ctx);
            return;
        }
        // A block-level element with a visible border is laid as its own
        // framed sub-box: lay its interior, draw the bordered sides as
        // box-drawing, blit. `inner_border_box` guards the recursion (the
        // interior pass lays this same element without re-entering here).
        if self.borders
            && block_like
            && self.inner_border_box != Some(id)
            // Past the recursion lid the border is dropped and the interior
            // flows as a plain block (hostile deep nesting; see the const).
            && self.border_depth < MAX_BORDER_DEPTH
        {
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
        // A paint-suppressed block that CLIPS its vertical overflow to a definite
        // height is a virtualized-list placeholder: React/Mastodon render an
        // off-screen row as `height:<cachedPx>px; overflow:hidden; opacity:0`
        // holding hidden content. Its subtree is invisible AND clipped, so it
        // contributes nothing but its reserved height — reserve exactly that many
        // rows (spacer rows that survive `finish`'s blank-row collapse) instead of
        // laying the full invisible subtree. The cached px was measured from OUR
        // OWN `getBoundingClientRect` (rows × cell_px), so it converts back to the
        // SAME row count the visible version rendered, which keeps the DOCUMENT
        // HEIGHT STABLE as the list virtualizes on scroll. Without it a placeholder
        // rendered its full unclipped invisible extent and every intersection swap
        // thrashed the doc height, teleporting the reader onto a different post on a
        // one-line scroll.
        //
        // Applied in the GEOMETRY pass too (`tag_all_nodes`), NOT just the render:
        // `measure_boxes` backs `getBoundingClientRect`/`offset*` and — critically —
        // the IntersectionObserver the page uses to decide WHICH rows to reveal as
        // it scrolls. If geometry flowed the placeholder at its full (unclipped)
        // extent while the render reserved the shorter box, every row below it would
        // sit at a DIFFERENT document position in the engine than on screen: the app
        // scrolls in the rendered coordinate system, the engine reveals rows in the
        // full-extent one, so scrolling reveals the wrong posts, leaves blank
        // reserved rows on screen, and lands you on a different post when you scroll
        // back up. Reserving in BOTH passes keeps the two coordinate systems (and
        // `documentElement.scrollHeight`) identical. In geometry we still record the
        // placeholder's OWN box — a node-tagged, `clip_h`-tall reserved run — so the
        // observer sees it at its real reserved position; the invisible subtree is
        // skipped (its children aren't observed and don't paint). The intrinsic-width
        // pass (`measuring`) still flows inline — reserving wouldn't change a
        // full-band block's width. Scoped to `opacity_hidden` (the sticky, whole-
        // subtree suppression): a `visibility:hidden` box can be re-revealed by a
        // `visibility:visible` child, so it is NOT blanked here.
        if block_like
            && opacity_hidden
            && !self.measuring
            && let Some(clip_h) = self.clip_reserve_rows(id)
        {
            self.flush_block();
            if self.gap_before(id, &tag) {
                self.push_blank();
            }
            // Geometry pass: tag the reserved run with the placeholder's node + box
            // width so `measure_boxes` records the reserved box. Render pass: an
            // inert 0-width, node-less spacer (paints nothing, stays uninteractive).
            let (node, w) = if self.tag_all_nodes {
                let avail = self.width.saturating_sub(self.indent).max(1);
                let bw = self
                    .css_cells(id, "width")
                    .unwrap_or(avail)
                    .min(avail)
                    .max(1);
                self.element_tops
                    .entry(id)
                    .or_insert((self.indent as u16, self.rows.len() as u16));
                (id, bw as u16)
            } else {
                (NO_NODE, 0)
            };
            for _ in 0..clip_h {
                self.rows.push(reserved_clip_row(self.indent, w, node));
            }
            if self.gap_after(id, &tag) {
                self.push_blank();
            }
            self.col = self.indent;
            self.pending_space = false;
            return;
        }
        // A flex container lays its children out as boxes: a wrapping one
        // as a 2D grid, a row one as side-by-side columns, a column one as
        // stacked block-level items. Everything else flows normally.
        let flex = if block_like { self.flex_mode(id) } else { None };
        // A horizontal-scroll container (an `overflow-x` box with content
        // wider than the viewport — a carousel) lays its content as one wide
        // strip, clipped to the band and scrolled by the view.
        let hscroll = block_like && flex.is_none() && self.is_hscroll(id);
        // A vertical inner-scroll viewport (`overflow-y:auto|scroll` on a
        // definite-height box — CSS Overflow L3). Unlike `hscroll` this is NOT
        // mutually exclusive with flex: a scroll container is commonly
        // `display:flex; flex-direction:column` (the chat column), and its
        // buffer sub-layout flows that flex interior itself. So it dispatches
        // ahead of the flex match, and the flex check here is omitted.
        let region = block_like && !hscroll && self.scroll_region_height(id).is_some();
        // Incremental-layout boundary capture (INCREMENTAL_LAYOUT_PLAN.md §14):
        // a block-filling IFC container (`display:flow-root`/`flex`/`grid`,
        // in-flow, not a flex/grid item, not a scroll/clip region) lays its
        // content as pure `Doc.rows` and its outer width IS its containing
        // block's (width-stable). Record the OUTER band (before this block's own
        // indent/constrain — the sub-layout re-applies those) so a live patch can
        // re-lay only this subtree at the same width and splice it back. The band
        // is `self.width − self.indent`; because an IFC box never overlaps an
        // external float (CSS 2.1 §9.4.1), its left edge is `self.indent`, not a
        // float-narrowed `line_left`. `is_out_of_flow`/`parent_is_flex_container`
        // exclude the content-width-dependent cases; the `data-trust-node` gate
        // keeps the predicate cheap (most blocks aren't IFC boundaries).
        let capture_band = if self.capture_boundaries
            && !self.measuring
            && block_like
            && !region
            && !hscroll
            // Not the root of this sub-layout: a box laid as a SUB-BOX (a flex/
            // grid item) is captured by `blit` instead (with `subtree_root` set),
            // so capturing it here too would double-record it.
            && self.subtree_root != Some(id)
            // The cheap `data-trust-node` gate FIRST: only IFC boundaries carry
            // it, so the cascade queries below run on the sparse boundary set,
            // not every block.
            && let Some(node) = self
                .dom
                .attr(id, "data-trust-node")
                .and_then(|s| s.parse::<usize>().ok())
            && !self.is_out_of_flow(id)
            && !self.parent_is_flex_container(id)
            && matches!(
                self.dom.effective_display(id).as_deref(),
                Some("flex" | "grid" | "flow-root")
            ) {
            Some((
                node,
                u16::try_from(self.width.saturating_sub(self.indent)).unwrap_or(u16::MAX),
                u16::try_from(self.indent).unwrap_or(u16::MAX),
            ))
        } else {
            None
        };
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
        let saved_floats =
            if block_like && flex.is_none() && !hscroll && !region && self.establishes_bfc(id) {
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
        // CSS Text 4 longhands: a declared `text-wrap`/`text-wrap-mode`
        // overrides the WRAP half of the mode (Tailwind emits
        // `text-wrap:nowrap`; `balance`/`pretty`/`stable` are wrap-with-
        // aesthetics = wrap), `white-space-collapse` the COLLAPSE half.
        // Inherited via the registry, so a wrapper's longhand reaches its
        // descendants. A longhand beats the shorthand regardless of cascade
        // order — the cascade doesn't expand `white-space` (same documented
        // simplification as `axis_overflow`).
        let nowrap_half = self
            .dom
            .computed_value(id, "text-wrap-mode")
            .or_else(|| self.dom.computed_value(id, "text-wrap"))
            .and_then(|v| {
                v.split_whitespace()
                    .find_map(|t| match t.to_ascii_lowercase().as_str() {
                        "nowrap" => Some(true),
                        "wrap" | "balance" | "pretty" | "stable" => Some(false),
                        _ => None,
                    })
            });
        let collapse_half = self.dom.computed_value(id, "white-space-collapse");
        if nowrap_half.is_some() || collapse_half.is_some() {
            self.ws = self
                .ws
                .with_longhands(collapse_half.as_deref(), nowrap_half);
        }
        // A `white-space:nowrap` box that clips its x axis (`overflow` or the
        // bare `overflow-x` longhand) truncates its single line at its content
        // edge (the single-line-ellipsis card idiom). The band is already set
        // (`begin_line` above), so `line_right` is this box's right; clip
        // there. Saved/restored so siblings/children that inherit `nowrap` but
        // DON'T clip overflow lay normally.
        let saved_clip = (self.clip_right, self.clip_done, self.clip_ellipsis);
        if block_like && self.ws == WhiteSpace::Nowrap && self.clips_x(id) {
            self.clip_right = Some(self.line_right);
            self.clip_done = false;
            // `text-overflow` (CSS Overflow 3 §5.1) picks the truncation
            // style: `ellipsis` (or a custom string) marks the cut with `…`;
            // the initial `clip` cuts silently — what the wild's card CSS
            // declares is honored, instead of `…` unconditionally. Not
            // inherited: read on the clipping box itself.
            self.clip_ellipsis = self
                .dom
                .computed_style(id, "text-overflow")
                .is_some_and(|v| {
                    let v = v.trim();
                    v.eq_ignore_ascii_case("ellipsis") || v.starts_with('"') || v.starts_with('\'')
                });
        }
        let pushed_list = match tag.as_str() {
            "ul" => {
                self.list_stack.push((1, 1));
                true
            }
            "ol" => {
                // `<ol start=N>` seeds the counter (default 1; negative is
                // valid HTML). `<ol reversed>` counts DOWN (HTML §4.4.5),
                // starting at the number of `<li>` children when no `start`
                // is given.
                let start = self
                    .dom
                    .attr(id, "start")
                    .and_then(|s| s.trim().parse::<i64>().ok());
                if self.dom.attr(id, "reversed").is_some() {
                    let items = self
                        .dom
                        .children(id)
                        .into_iter()
                        .filter(|&c| self.dom.tag_name(c) == Some("li"))
                        .count() as i64;
                    self.list_stack.push((start.unwrap_or(items), -1));
                } else {
                    self.list_stack.push((start.unwrap_or(1), 1));
                }
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
                (self.viewport_w, self.viewport_h),
                self.units(id),
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
        } else if region {
            // A vertical inner-scroll viewport: lay the content into a separate
            // buffer, reserve exactly H blank rows here, and let the view window
            // it. `flow_region` handles a flex/grid/block interior itself (its
            // buffer sub-layout re-enters the normal dispatch under the
            // recursion guard), so it sits ahead of the flex match.
            self.flow_region(id, &cctx);
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
                    } else if self.is_region_cache_container(id) {
                        // Region de-lag (INCREMENTAL_LAYOUT_PLAN.md §14): reuse the
                        // cached rows of every unchanged block child, lay only the
                        // new/changed ones.
                        self.flow_cached_children(id, &cctx);
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
        // The child recursion above overwrote `self.invisible` with the last
        // child's value; restore this element's own so the block-tail emitters
        // (`::after`, the form-submit stub, the list marker) tag their items
        // with THIS element's paint-suppression. `place_positioned_children`
        // re-derives it per placed box, so it needs no refresh.
        self.invisible = cctx.invisible;

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
            // CSS 2.1 §8.4: PERCENTAGE vertical padding resolves against the
            // containing block's WIDTH — which makes an (often empty) padded
            // box the web's aspect-ratio spacer: `padding-bottom:56.25%`
            // reserves a 16:9 frame that an `inset:0` thumbnail then fills
            // (Twitch's ScAspectSpacer sibling, the classic responsive-embed
            // hack). Reserve that geometry in flow, FLOORING the box at its
            // padded height — never adding, so a box whose content already
            // fills the ratio (the padding-on-the-image-wrapper variant, which
            // the declared-image placeholder machinery sizes) doesn't double.
            // Without this the frame is 0 rows tall and, out-of-flow boxes no
            // longer inflating their ancestors (§9.3.1), every card thumbnail
            // composited over the content BELOW its card (Twitch's whole front
            // page piled its feed into the hero). Reserved rows carry the
            // zero-width image-spacer marker so `finish`'s blank-row collapse
            // keeps them for the composite to fill. ABSOLUTE (px/em) vertical
            // padding stays unreserved — that's whitespace, not geometry, and
            // terminal rows are precious (the borders-off philosophy).
            let vpad_frac: f32 = ["padding-top", "padding-bottom"]
                .iter()
                .filter_map(|p| first_percent(self.dom.computed_style(id, p).as_deref()?))
                .filter(|f| *f > 0.0)
                .sum();
            if vpad_frac > 0.0 {
                let band_w = corner_band.1.saturating_sub(corner_band.0);
                let want = rows_for_ratio(band_w.max(1), 1.0 / vpad_frac, self.units(id));
                let have = self.rows.len().saturating_sub(corner_start_row);
                for _ in have..want {
                    self.rows.push(image_spacer_row(self.indent));
                }
            }
            // Incremental-layout boundary capture (INCREMENTAL_LAYOUT_PLAN.md §14):
            // the in-flow content + contained floats are now laid, so the box's
            // row span is `[corner_start_row, rows.len())`. Recorded with the
            // outer band captured before this block's own indent.
            if let Some((node, content_width, origin_col)) = capture_band {
                self.boundary_boxes.insert(
                    id,
                    BoundaryRec {
                        node,
                        content_width,
                        // A block-filling box fills its band, so its width IS the
                        // band — the owns-rows check is skipped for it.
                        width: content_width,
                        origin_col,
                        start_row: corner_start_row,
                        end_row: self.rows.len(),
                        sub_box: false,
                    },
                );
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
                    .and_then(|v| css_length_rows(&v, self.units(id)))
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
        (self.clip_right, self.clip_done, self.clip_ellipsis) = saved_clip;
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
                    (self.viewport_w, self.viewport_h),
                    self.units(id),
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
            return indent_cells(
                self.dom.computed_style(id, "padding-left").as_deref(),
                self.units(id),
            )
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
                indent_cells(ml.as_deref(), self.units(id))
            };
            (margin + indent_cells(pl.as_deref(), self.units(id))).min(self.width / 4)
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
                .is_some_and(|v| vertical_space(&v, self.units(id)));
        }
        let mt = self.dom.computed_style(id, "margin-top");
        let pt = self.dom.computed_style(id, "padding-top");
        if mt.is_some() || pt.is_some() {
            [mt, pt]
                .into_iter()
                .flatten()
                .any(|v| vertical_space(&v, self.units(id)))
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
                .is_some_and(|v| vertical_space(&v, self.units(id)));
        }
        let mb = self.dom.computed_style(id, "margin-bottom");
        let pb = self.dom.computed_style(id, "padding-bottom");
        if mb.is_some() || pb.is_some() {
            [mb, pb]
                .into_iter()
                .flatten()
                .any(|v| vertical_space(&v, self.units(id)))
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
        // Unbounded, like every containing-block walk (the Twitch deep-wrapper
        // lesson — see `definite_ancestor_width`): only transparent inline
        // ancestors continue the loop, any block-level box returns, and the
        // tree has no cycles, so an arbitrary cap could only stop SHORT of
        // the real formatting context.
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
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

    /// Whether `id` is an incremental-layout SUB-BOX boundary candidate — a flex/
    /// grid ITEM, or an atomic inline box (`inline-block`/`-flex`/`-grid`) — laid
    /// as its own box via `layout_subtree_inner`+`blit` (INCREMENTAL_LAYOUT_PLAN.md
    /// §14, the widening for styled-components items / animated counters).
    /// Excludes out-of-flow (abspos/fixed) and floated boxes — they position
    /// specially, aren't the high-churn targets, and a wrong splice would be
    /// worse than a full render. The harvest's owns-rows check + the patch's
    /// width verify are the real safety, so this is only a cheap pre-filter.
    fn is_sub_box_boundary(&self, id: NodeId) -> bool {
        if self.is_out_of_flow(id) {
            return false;
        }
        if matches!(
            self.dom
                .computed_style(id, "float")
                .as_deref()
                .map(str::trim),
            Some("left" | "right" | "inline-start" | "inline-end")
        ) {
            return false;
        }
        self.parent_is_flex_container(id)
            || matches!(
                self.dom.effective_display(id).as_deref(),
                Some("inline-block" | "inline-flex" | "inline-grid")
            )
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

    /// Whether `id` is a `position:fixed` box we PIN into the overlay layer: it
    /// has NO box insets (so it paints at its static position — the reserved
    /// flex column, for a sidebar/header) and does not cover the viewport (a
    /// full-bleed fixed overlay stays a modal). A terminal has no compositing
    /// layer, so such a box draws pinned over the scrolling document at its
    /// static viewport position (see the fixed-layer deviation). Offset-
    /// positioned fixed boxes and `absolute` boxes keep the normal out-of-flow
    /// placement.
    fn is_fixed_pinned(&self, id: NodeId) -> bool {
        self.subtree_root != Some(id)
            && Some(id) != self.modal_root
            && self.dom.computed_style(id, "position").as_deref() == Some("fixed")
            && self.all_insets_auto(id)
            && !self.covers_viewport(id)
    }

    /// Whether every box inset (`top`/`right`/`bottom`/`left`) on `id` is `auto`
    /// or absent — so a positioned box uses its static position on both axes.
    fn all_insets_auto(&self, id: NodeId) -> bool {
        ["top", "right", "bottom", "left"].iter().all(|&side| {
            self.dom
                .computed_style(id, side)
                .is_none_or(|v| v.trim().eq_ignore_ascii_case("auto"))
        })
    }

    /// Capture `id`'s pinned `position:fixed` children (`is_fixed_pinned`) into
    /// the overlay layer at the parent's current content origin — their static
    /// position, which `blit` translates up to the document coordinate space.
    /// A no-op while measuring (the overlay is a render-pass product) and for a
    /// parent with no such children. Each child's subtree is laid at its used
    /// width (CB = viewport: explicit `width`, else shrink-to-fit).
    fn capture_fixed_children(&mut self, id: NodeId) {
        if self.measuring {
            return;
        }
        let col = self.indent as u16;
        let row = self.rows.len() as u16;
        for child in self.flow_children(id) {
            if !self.is_fixed_pinned(child) {
                continue;
            }
            let cb_w = self.viewport_w.max(1);
            let explicit_w = self.abs_used_width(child, cb_w);
            let lay_w = explicit_w.unwrap_or(cb_w).clamp(1, cb_w);
            let inherit = self.ancestor_link_ctx(child);
            let prev_sw = self.shrink_wrap;
            self.shrink_wrap = explicit_w.is_none();
            let mut b = self.layout_subtree_inner(child, lay_w, Some(child), false, &inherit);
            self.shrink_wrap = prev_sw;
            // The rail's rows become an overlay layer (`FixedItem.rows`) the
            // document-root composite can never paint into — so its abspos
            // descendants (badges, dropdown carets, unread dots) composite into
            // the box HERE; the merged `b.fixed`/`b.scroll_clips` then ride the
            // existing propagation below.
            self.composite_box_positioned(&mut b);
            // The pinned box's own subtree can hold FURTHER pinned descendants
            // (captured into its sub-layout's overlay) — carry them up
            // translated to this box's static position, like `blit` does.
            for f in &b.fixed {
                let mut f = f.clone();
                f.col += col;
                f.row += row;
                self.fixed.push(f);
            }
            // A definite-height scroll box inside the rail keeps its client
            // box honest even though the rail's Region support is deferred.
            self.scroll_clips.extend(b.scroll_clips.iter().copied());
            if b.height == 0 {
                continue;
            }
            self.fixed.push(FixedItem {
                col,
                row,
                rows: b.rows,
                z: self.z_index(child),
            });
        }
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
            .enumerate()
            .filter(|&(_, id)| self.is_modal_overlay(id))
            .max_by_key(|&(order, id)| (self.z_index(id), order))
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
        let oz = self.z_index(overlay);
        // Exclude the overlay, its subtree (its own content) and its ancestors
        // (whose stacking context carries the overlay along).
        let mut excluded: HashSet<NodeId> = self.dom.descendants(overlay).collect();
        excluded.insert(overlay);
        let mut up = self.dom.parent_composed(overlay);
        while let Some(p) = up {
            excluded.insert(p);
            up = self.dom.parent_composed(p);
        }
        let root = body_or_document(self.dom);
        let order: Vec<crate::dom::NodeId> = self.dom.descendants(root).collect();
        // Document-order index of the overlay, for the equal-z paint-order
        // tiebreak below (CSS 2.1 Appendix E).
        let overlay_pos = order.iter().position(|&id| id == overlay);
        order.iter().enumerate().any(|(pos, &id)| {
            !excluded.contains(&id)
                && matches!(
                    self.dom.computed_style(id, "position").as_deref(),
                    Some("relative" | "sticky")
                )
                // A strictly-higher z-index paints above; equal z-index paints
                // above when the content comes LATER in document order. Per CSS
                // 2.1 Appendix E painting order, positioned boxes sharing a
                // z-index (the common `auto`/`0` case) paint in tree order, so a
                // full-viewport BACKGROUND layer that sits FIRST in the DOM (the
                // solar battery meter, a hero slideshow) is painted under the
                // page's positioned content that follows it — not a modal.
                && {
                    let cz = self.z_index(id);
                    cz > oz || (cz == oz && overlay_pos.is_some_and(|op| pos > op))
                }
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
            // A PAINT-SUPPRESSED (`opacity:0`/`visibility:hidden`) overlay paints
            // blank, so the page behind it shows through — it is NOT covering
            // anything. Surfacing it would defer the whole page behind a blank
            // screen (a fade-in dialog/lightbox that hasn't animated in yet, or a
            // `visibility:hidden` menu until a class toggles). Since neither is
            // folded into `is_hidden` anymore (both keep their box), these guards
            // are what stop that.
            && !self.dom.paint_suppressed(id)
            && !self.dom.visibility_hidden(id)
            && (self.covers_viewport(id) || self.is_semantic_dialog(id))
            && self.overlay_has_content(id)
    }

    /// Whether `id`'s used box covers the whole viewport — width and height
    /// each either fill it (`100%`/`100vw`/`100vh`) or pin both opposite
    /// offsets to zero (`inset:0`). Viewport units (`vw`/`vh`) are an
    /// unambiguous author signal regardless of containing block. The `%`/
    /// inset-derived fill is only trusted for `position:fixed`: a fixed box's
    /// containing block IS the viewport by definition (CSS 2.1 §10.1),
    /// unconditionally, so `inset:0` there reliably means "spans the visible
    /// window regardless of scroll" — a real modal signal. A `position:
    /// absolute` box with NO positioned ancestor is ALSO placed against the
    /// initial containing block, but that box still scrolls away with the
    /// document (unlike `fixed`) and can be clipped/constrained by ordinary
    /// (non-positioned) ancestors our pre-layout heuristic can't see — so an
    /// `absolute` box merely stretching to fill an indefinite/unknown-size CB
    /// is a weak, easily-false-positive signal: an ordinary small in-page
    /// widget (e.g. a video "play" scrim) that just happens to fill its own
    /// small container this way is NOT a page-blocking modal, and treating it
    /// as one wiped the whole surrounding page (nav/chat/sidebar) down to that
    /// widget's own subtree. Only `100vw`/`100vh` (the unconditional fast path
    /// above) still qualifies an `absolute` box geometrically; anything else
    /// wanting modal treatment needs real dialog semantics (`is_modal_overlay`'s
    /// `is_semantic_dialog` OR-arm).
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
        if w_vw && h_vh {
            return true;
        }
        let w_pct = w.as_deref() == Some("100%") || (is_zero(val("left")) && is_zero(val("right")));
        let h_pct = h.as_deref() == Some("100%") || (is_zero(val("top")) && is_zero(val("bottom")));
        w_pct && h_pct && self.dom.computed_style(id, "position").as_deref() == Some("fixed")
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
        // A translate MOVES the painted box (CSS Transforms 1), and overflow
        // clips painted content — the fraction model below judges the
        // UN-transformed position, so a translated box is indeterminate here.
        // Kept, and judged instead by the translate-aware laid-geometry clip
        // test at placement. This is the off-canvas slide-in idiom: a
        // `left:100%` panel with `translateX(-100%)` is fully VISIBLE.
        if self.has_translate(id) {
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
    /// THE single overflow authority: every overflow consumer (`clips_x`,
    /// `clips_overflow_y`, `is_hscroll`, `establishes_bfc`,
    /// `node_clips_overflow`, `scroll_region_height`, `is_clip_collapsed`)
    /// resolves through it, so longhand-vs-shorthand precedence can't
    /// disagree between them. Two documented simplifications: a longhand
    /// always beats the shorthand regardless of cascade order (the cascade
    /// doesn't expand the shorthand into the longhands), and CSS Overflow 3
    /// §3.1's computed-value coercion (`visible` beside a scrolling axis
    /// computes to `auto`) is not applied.
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

    /// A box that CLIPS overflow on an axis and is sized to less than one cell
    /// on that axis paints none of its content: the CSS clip (CSS Overflow §3)
    /// leaves no room for it at the terminal's cell resolution. This is the
    /// standard visually-hidden / "sr-only" idiom — `width:1px;height:1px;
    /// overflow:hidden` (the `clip` accessibility-label pattern); a browser
    /// renders a ~1px speck, so we faithfully render nothing rather than a stray
    /// glyph. (Twitch's side-nav cards carry two per card: a clipped "Live" and
    /// "<n> viewers" sr-only label.) Only a DEFINITE absolute/font length under
    /// a cell collapses — `%`/`vw`/`auto` don't (`css_length_em` returns `None`)
    /// — and clipping a box TALLER than a cell to a definite height is the
    /// inner-scroll feature (deferred), not this.
    fn is_clip_collapsed(&self, id: NodeId) -> bool {
        let collapsed = |prop: &str, pad: [&str; 2], vertical: bool| {
            // The axis must actually clip: a 0-size `overflow:visible` box still
            // paints its overflowing content, so it is NOT collapsed.
            if matches!(
                self.axis_overflow(id, vertical).as_deref(),
                None | Some("visible")
            ) {
                return false;
            }
            // Sub-cell on the axis: narrower than one column, or shorter
            // than the half-row that rounds to zero rows (a definite height
            // past that paints a real strip a browser would show).
            let u = self.units(id);
            let sub_cell = |px: f32| px < if vertical { u.cell_h / 2.0 } else { u.cell_w };
            let size_zero = self
                .dom
                .computed_style(id, prop)
                .and_then(|v| css_length_px(&v, u))
                .is_some_and(sub_cell);
            if !size_zero {
                return false;
            }
            // Padding on this axis makes the PADDING box non-zero, and
            // `overflow:hidden`/`clip` clips to the PADDING box (CSS Overflow §3),
            // so the box isn't collapsed even though its CONTENT box is 0. This is
            // the universal responsive-image aspect-ratio placeholder —
            // `height:0; padding-bottom:56.25%; overflow:hidden` with an
            // absolutely-positioned `width/height:100%` child filling the padding
            // (percentage padding resolves against the containing block's WIDTH,
            // CSS 2.1 §8.4). A percentage or a ≥1-cell length keeps the box.
            let has_axis_padding = pad.iter().any(|p| {
                self.dom.computed_style(id, p).is_some_and(|v| {
                    parse_percent(&v).is_some_and(|f| f > 0.0)
                        || css_length_px(&v, u).is_some_and(|px| !sub_cell(px))
                })
            });
            !has_axis_padding
        };
        collapsed("width", ["padding-left", "padding-right"], false)
            || collapsed("height", ["padding-top", "padding-bottom"], true)
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

    /// Whether the container's flex MAIN axis runs reversed —
    /// `flex-direction: row-reverse | column-reverse`, directly or via
    /// `flex-flow` (Flexbox §5.1: main-start and main-end swap). Items then
    /// lay out in reverse order and `justify_offsets` packs toward the
    /// swapped main-start. Flex containers only: `flex-direction` does not
    /// apply to grid, so the `display:grid` fallback through the shared wrap
    /// path is untouched.
    fn flex_main_reversed(&self, id: NodeId) -> bool {
        if !matches!(
            self.dom.computed_display(id).as_deref(),
            Some("flex" | "inline-flex")
        ) {
            return false;
        }
        let has = |prop: Option<String>| {
            prop.is_some_and(|v| {
                v.split_whitespace()
                    .any(|t| matches!(t, "row-reverse" | "column-reverse"))
            })
        };
        has(self.dom.computed_style(id, "flex-direction"))
            || has(self.dom.computed_style(id, "flex-flow"))
    }

    /// Whether the container's flex CROSS axis runs reversed —
    /// `flex-wrap: wrap-reverse`, directly or via `flex-flow` (Flexbox §5.2:
    /// cross-start and cross-end swap). The flex LINES then stack
    /// bottom-to-top. Flex containers only (grid has no `flex-wrap`).
    fn flex_cross_reversed(&self, id: NodeId) -> bool {
        if !matches!(
            self.dom.computed_display(id).as_deref(),
            Some("flex" | "inline-flex")
        ) {
            return false;
        }
        let has = |prop: Option<String>| {
            prop.is_some_and(|v| v.split_whitespace().any(|t| t == "wrap-reverse"))
        };
        has(self.dom.computed_style(id, "flex-wrap"))
            || has(self.dom.computed_style(id, "flex-flow"))
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
        // `row-reverse` (Flexbox §5.1): the main axis runs right-to-left, so
        // the items lay out in reverse order (after the `order` sort — the
        // direction reverses the AXIS, composing on top of order-modified
        // document order) and `justify_offsets` packs toward the right edge.
        let main_rev = self.flex_main_reversed(id);
        let mut kids = self.flex_items(id);
        if main_rev {
            kids.reverse();
        }
        let mut nodes = Vec::new();
        let mut basis = Vec::new();
        let mut grow = Vec::new();
        let mut shrink = Vec::new();
        for k in kids {
            let (b_css, g, s) = self.flex_props(k, avail);
            let b = match b_css {
                Some(w) => {
                    let base = w.min(avail);
                    if self.measuring {
                        // Intrinsic sizing (CSS Flexbox §9.9.1): an item's
                        // max-content contribution is its content max-content —
                        // flex-grow does NOT expand it (no free space exists
                        // yet), and a flex-basis BELOW the content (the common
                        // `flex-basis:0` grow item — every Material/Polymer
                        // button) must still contribute its content. Floor the
                        // declared basis by the measured content.
                        base.max(self.measure_width(k, avail))
                    } else {
                        base
                    }
                }
                None => {
                    // `flex-basis:auto`/`width:auto`: size to content. An empty,
                    // non-growing item takes no column — UNLESS it reserves space
                    // via `min-width` (it's a width-reserving spacer, not empty:
                    // Mastodon's side panes, whose only child is a `position:fixed`
                    // rail captured into the overlay — laying the pane is also what
                    // runs `capture_fixed_children`).
                    if g == 0.0 && self.is_empty_box(k) && self.css_cells(k, "min-width").is_none()
                    {
                        continue;
                    }
                    self.measure_width(k, avail)
                }
            };
            // `max-width` CAPS the hypothetical main size (CSS Flexbox §9.2.3:
            // the flex base size is clamped by the item's min/max main size).
            // `flex_props` already caps a DECLARED basis; an AUTO (content-
            // sized) basis must be capped too — Steam's tab rows wrap a
            // `width:100%` @2x capsule image (intrinsic ~2× the box) in a
            // `max-width:231px` cell, which otherwise contributes its full
            // intrinsic width and opens a dead gap between image and text.
            let b = match self.len_or_pct(k, "max-width", avail) {
                Some(mx) => b.min(mx),
                None => b,
            };
            // `min-width` is a HARD floor on a flex item's used main size — even
            // when the row has room (CSS Flexbox §4.5/§9.7: the hypothetical
            // main size clamps the flex base size to `min-width`). Without this
            // a `min-width:285px` item whose content measures narrower collapses
            // to that content (or to 0) once the row "fits", instead of holding
            // its floor: Mastodon's centered 3-column layout has two side panes
            // at `min-width:285px` flanking a `max-width:600px` main; they were
            // basis=0 so only main laid out, centered alone. (`max-width` already
            // caps an explicit basis in `flex_props`.) Skipped while measuring so
            // intrinsic width still reflects content, not the declared floor.
            let b = if self.measuring {
                b
            } else {
                match self.css_cells(k, "min-width") {
                    Some(mw) => b.max(mw.min(avail)),
                    // `min-width:auto` (CSS Flexbox §4.5): with no explicit
                    // min-width a flex item's automatic minimum is its
                    // content-based minimum — the min-content size, capped by
                    // a definite `width` (the specified size suggestion).
                    // Floor the hypothetical size for NON-GROWING items only:
                    // a grow item ends at/above min-content via the grow
                    // distribution, and inflating its base would skew that
                    // distribution. Steam's login QR pane is `flex:0` (basis
                    // 0, grow 0) beside a `flex:1` form — without the
                    // automatic minimum it collapsed to zero width and the
                    // whole QR column vanished.
                    None if g == 0.0 => {
                        let mut auto_min = self
                            .measure_width(k, 1)
                            .max(self.definite_width_floor(k).unwrap_or(0));
                        if let Some(w) = self.css_cells(k, "width") {
                            auto_min = auto_min.min(w);
                        }
                        // "In either case, the size is clamped by the maximum
                        // main size if it's definite" (§4.5) — without this, a
                        // `width:100%; max-width:600px` feed column holding one
                        // long unbreakable token (a URL pasted as plain text)
                        // floors at its min-content instead of its max-width,
                        // overflows the row at minimum, and the whole 3-column
                        // layout falls back to a stack (Mastodon).
                        if let Some(mx) = self.len_or_pct(k, "max-width", avail) {
                            auto_min = auto_min.min(mx);
                        }
                        b.max(auto_min.min(avail))
                    }
                    None => b,
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
            // Free space is distributed to the grow items by their flex-grow —
            // but ONLY during real layout. While measuring intrinsic width there
            // is no free space to hand out (CSS Flexbox §9.9.1): flex-grow must
            // not inflate an item past its content, or every measured flex
            // container holding a grow child reports ~the whole constraint
            // (which froze YouTube's `flex-shrink:0` masthead end-cap at ~the
            // viewport width and starved the search box to one column).
            let free = avail - total_basis - gaps;
            let total_grow: f32 = grow.iter().sum();
            if !self.measuring && total_grow > 0.0 && free > 0 {
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
        // A flex item OCCUPIES its allocated main size even when its own content
        // is narrower, so a flex row's max-content contribution is the sum of
        // its items' flex base sizes plus gaps (CSS Flexbox §9.9.1) — which can
        // exceed the rightmost painted cell (a spotlight capsule whose art is a
        // `width:28vw` box but whose only in-flow content is a short title).
        // Record it so `layout_subtree_inner` reports the row's TRUE measured
        // width; without this the container measured only to its last item's
        // content, and an ancestor flex row then handed this row too little
        // space and stacked it (Steam's featured `hero_capsule` carousel
        // collapsed to a vertical column). MEASURE-ONLY deliberately: on the
        // render pass grow has been applied (`used` ≈ the whole band), and
        // flooring the reported width there made every shrink-to-fit consumer
        // see a full-band box — a `right:0` abspos flyout holding a `flex:1`
        // row lost its right anchor (left = cb_w − used_w = 0) and the
        // occlusion collapse saw phantom full-width overlaps.
        if self.measuring {
            self.flex_min_width = self.flex_min_width.max(self.line_left + used);
        }
        let (lead, between) = self.justify_offsets(id, free, n, main_rev);
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
            // Blit a 0-height item too when it carries a pinned fixed rail (a
            // `min-width` spacer pane whose only child is `position:fixed`) or
            // collected out-of-flow boxes (a fit-content wrapper whose only
            // content is an abspos slide-in panel — Twitch's chat column): the
            // blit draws no rows but propagates the side channels at this
            // item's column, so the overlay lands at its true position.
            if boxes[i].height > 0 || !boxes[i].fixed.is_empty() || !boxes[i].positioned.is_empty()
            {
                let dy = self.align_offset(id, nodes[i], boxes[i].height as usize, line_h);
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
    /// items having eaten the free space makes this moot. `main_rev` = the
    /// container's main axis is reversed (`row-reverse`): the flex-relative
    /// values swap sides (§5.1 — `flex-start` packs at the swapped main-start,
    /// the RIGHT edge), while `start`/`end`/`left`/`right` stay put (CSS Box
    /// Alignment: writing-mode-relative and physical values ignore the flex
    /// direction; we are LTR-only, so start==left, end==right).
    fn justify_offsets(&self, id: NodeId, free: usize, n: usize, main_rev: bool) -> (usize, usize) {
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
            Some("flex-end") if main_rev => (0, 0),
            Some("flex-end" | "end" | "right") => (free, 0),
            Some("center") => (free / 2, 0),
            Some("space-between") if n > 1 => (0, free / (n - 1)),
            Some("space-around") => (free / (2 * n), free / n),
            Some("space-evenly") => (free / (n + 1), free / (n + 1)),
            Some("start" | "left") => (0, 0),
            // `flex-start` / `normal` / unknown: pack at main-start — the
            // right edge when the main axis is reversed.
            _ if main_rev => (free, 0),
            _ => (0, 0),
        }
    }

    /// Cross-axis offset (rows from the top of the line/shelf) for an item of
    /// height `item_h` within a band of height `line_h`: the ITEM's own
    /// `align-self` (Flexbox §8.3 — `auto` defers to the container), else the
    /// CONTAINER's `align-items`. This is the same resolution column flex
    /// (`stack_flex_items`) already does; row/wrap used to read only the
    /// container. We don't stretch item heights, so `stretch`/`baseline`/
    /// `normal`/unknown and `flex-start` all top-align; only `center` and
    /// `flex-end`/`end`/`self-end` shift down.
    fn align_offset(&self, container: NodeId, item: NodeId, item_h: usize, line_h: usize) -> usize {
        let free = line_h.saturating_sub(item_h);
        if free == 0 {
            return 0;
        }
        let align = self
            .dom
            .computed_style(item, "align-self")
            .map(|v| v.trim().to_string())
            .filter(|v| v != "auto")
            .or_else(|| {
                self.dom
                    .computed_style(container, "align-items")
                    .map(|v| v.trim().to_string())
            });
        match align.as_deref() {
            Some("center") => free / 2,
            Some("flex-end" | "end" | "self-end") => free,
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
            && let Some(c) = resolve_cells(
                &v,
                avail,
                (self.viewport_w, self.viewport_h),
                self.units(id),
            )
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
                && let Some(c) =
                    resolve_cells(t, avail, (self.viewport_w, self.viewport_h), self.units(id))
            {
                return c;
            }
        }
        usize::from(!row_axis)
    }

    /// The natural width (cells) of an element's subtree laid out at
    /// `constraint` — its content basis (at `avail`) or min-content (at 1).
    /// The widest DEFINITE `width` declared on `id` or any in-flow, visible
    /// descendant — the floor below which the subtree cannot compress at
    /// min-content (CSS Sizing §5: a box with a definite preferred size does
    /// not shrink below it to fit). Complements the `measure_width(k, 1)`
    /// min-content probe, which clamps every box to the probe band and so
    /// under-reports a subtree holding a definite-width box (Steam's login QR
    /// frame, `width:calc(200px - 2.5em)` nested four levels into a `flex:0`
    /// column). A branch stops at its first definite width — children lay
    /// INSIDE that box. Out-of-flow boxes contribute nothing (they don't
    /// affect their container's min-content).
    fn definite_width_floor(&self, id: NodeId) -> Option<usize> {
        if self.dom.is_hidden(id)
            || matches!(
                self.dom.computed_style(id, "position").as_deref(),
                Some("absolute" | "fixed")
            )
        {
            return None;
        }
        if let Some(w) = self.definite_len_cells(id, "width") {
            return Some(w);
        }
        self.dom
            .child_iter(id)
            .filter_map(|c| self.definite_width_floor(c))
            .max()
    }

    fn measure_width(&self, id: NodeId, constraint: usize) -> usize {
        crate::dom::casc_note_measure();
        let key = (id, constraint, self.table_depth);
        if let Some(&w) = self.measure_cache.borrow().get(&key) {
            return w;
        }
        let w = self
            .layout_subtree_inner(id, constraint, None, true, &Ctx::root())
            .width as usize;
        self.measure_cache.borrow_mut().insert(key, w);
        w
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
            Some(v) => resolve_cells(v, avail, (self.viewport_w, self.viewport_h), self.units(id)),
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
        resolve_cells(
            &self.dom.computed_style(id, prop)?,
            avail,
            (self.viewport_w, self.viewport_h),
            self.units(id),
        )
    }

    /// Whether a block is a horizontal-scroll container (a carousel): it
    /// clips on the x axis AND has a `hscroll_track` (an over-wide child
    /// holding several cards).
    fn is_hscroll(&self, id: NodeId) -> bool {
        self.clips_x(id) && self.hscroll_track(id).is_some()
    }

    /// The over-wide "track" inside a scroll container `id`: a child holding a
    /// rail of ≥3 cards whose combined width overflows the band. Two shapes:
    ///   - a child with a DECLARED width wider than the band (a slick/JS carousel
    ///     that sizes its strip explicitly), OR
    ///   - a non-wrapping FLEX rail whose fixed-width items overflow the band —
    ///     the modern flexbox / scroll-snap carousel, where the track is
    ///     `width:100%` (so the declared-width test misses it) but its `slide`s
    ///     each take `calc((100% − gaps)/N)` and together exceed the band
    ///     (redbubble's featured-collection `carouselInner`). Without this the
    ///     track fell to `flow_flex_row`, which — seeing the track itself doesn't
    ///     clip-x (its scroll-container PARENT does) — stacked the slides
    ///     vertically at full width, so each `width:100%` product image filled
    ///     the band.
    ///
    /// Either shape must be a real rail (≥3 cards), NOT a clearfix wrapping a
    /// single wide layout column.
    fn hscroll_track(&self, id: NodeId) -> Option<NodeId> {
        let avail = self.width.saturating_sub(self.indent).max(1);
        self.dom.children(id).into_iter().find(|&c| {
            if !matches!(self.dom.node(c).data, NodeData::Element { .. }) {
                return false;
            }
            let declared_wide = self.css_cells(c, "width").is_some_and(|w| w > avail)
                && self
                    .dom
                    .children(c)
                    .iter()
                    .filter(|&&g| matches!(self.dom.node(g).data, NodeData::Element { .. }))
                    .count()
                    >= 3;
            declared_wide || self.is_overflowing_flex_rail(c, avail)
        })
    }

    /// Whether `id` is a non-wrapping flex ROW whose ≥3 items have a combined
    /// main-axis size (bases + gaps) exceeding the band — an over-wide carousel
    /// rail. The flexbox/scroll-snap carousel signal `hscroll_track` needs when
    /// the rail's own width is `100%` but its fixed-width cards overflow it.
    fn is_overflowing_flex_rail(&self, id: NodeId, avail: usize) -> bool {
        if self.flex_mode(id) != Some(FlexMode::Row) {
            return false;
        }
        let items = self.flex_items(id);
        if items.len() < 3 {
            return false;
        }
        let mut total = self.flex_gap(id, avail, false) * (items.len() - 1);
        for &it in &items {
            let (basis, ..) = self.flex_props(it, avail);
            total += basis.unwrap_or_else(|| self.measure_width(it, avail));
        }
        total > avail
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
            // A strip past ~65k cells can't be addressed (stops/offsets are
            // u16, and a bare `as u16` would silently WRAP) — nor usefully
            // scrolled. Stop laying further cards there (hostile-input lid;
            // a real rail is a few hundred cells wide).
            if x + avail >= u16::MAX as usize {
                break;
            }
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
                // The deprecated flow engine keeps its always-snap behavior;
                // layout2 honors `scroll-snap-type` (see `paint_carousel`).
                snap: true,
            });
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// The definite scroll-viewport height (rows) of `id` IF it is a VERTICAL
    /// inner-scroll container: `overflow-y: auto|scroll` (CSS Overflow L3 — the
    /// scrollable values; `hidden`/`clip` stay a pure clip with unreachable
    /// content, her call, and `visible` isn't a scroll container) on a box with
    /// a definite height (Phase 0 `definite_height`). `None` otherwise — or for
    /// the region's own buffer pass (the `region_inner` recursion guard). Reads
    /// overflow FIRST so the height walk only runs for actual scroll boxes.
    fn scroll_region_height(&self, id: NodeId) -> Option<usize> {
        if self.region_inner == Some(id) {
            return None;
        }
        match self.axis_overflow(id, true).as_deref() {
            Some("auto" | "scroll") => {
                // The page's PRINCIPAL scroll container is NOT virtualized into an
                // inner band — it flows into the document so the page scroll (and
                // the right scrollbar) scrolls it, exactly as a browser scrolls
                // that panel as "the page". Only genuinely nested scrollers (a
                // <nav>/<aside> sidebar, a chat feed inside the main flow) become
                // Regions.
                if self.is_principal_scroller(id) {
                    return None;
                }
                self.definite_height(id).filter(|&h| h > 0)
            }
            _ => None,
        }
    }

    /// Whether `id` is the page's PRINCIPAL scroll container — the one a LOCKED
    /// viewport delegates document scrolling to (the SPA pattern where
    /// `html`/`body` are `overflow:hidden` and one inner `overflow:auto` box
    /// carries the main flow, e.g. Twitch's `root-scrollable` inside `<main>`).
    /// Such a box is NOT clipped into an inner `Region`; it flows into the
    /// document so `g.scroll` and the page scrollbar scroll it — matching how a
    /// browser scrolls that panel as the page. Read purely from the page's own
    /// declarations: CSS Overflow §3.1 (the root element's overflow propagates
    /// to the viewport; if the root is `visible` but `<body>` is not, the body's
    /// propagates) + HTML sectioning landmarks (`<main>` is the dominant
    /// content, `<nav>`/`<aside>` are complementary) — never the host.
    ///
    /// The criterion, in ONE upward walk from `id` to the root: a
    /// scroll-container ancestor ⇒ `id` is NESTED ⇒ a real inner region; the
    /// nearest sectioning landmark above `id` decides main-flow (`<main>`) vs a
    /// complementary sidebar (`<nav>`/`<aside>`, stays a region); and the
    /// viewport must be LOCKED (an `overflow:hidden`/`clip` on `html`/`body`) or
    /// the document itself scrolls and an inner `overflow:auto` box is a genuine
    /// region. Principal ⇔ locked AND (inside `<main>` OR the page declares no
    /// enclosing landmark at all, i.e. this outermost scroller carries the flow).
    fn is_principal_scroller(&self, id: NodeId) -> bool {
        let mut viewport_locked = false;
        let mut in_main = false;
        let mut landmark_seen = false;
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            // A scroll-container ancestor ⇒ a nested inner region, never the page.
            if self.dom.is_scroll_container(p) {
                return false;
            }
            match self.dom.tag_name(p) {
                Some("main") if !landmark_seen => {
                    in_main = true;
                    landmark_seen = true;
                }
                Some("nav" | "aside") if !landmark_seen => landmark_seen = true,
                Some("html" | "body") if self.node_clips_overflow(p) => viewport_locked = true,
                _ => {}
            }
            cur = self.dom.parent_composed(p);
        }
        viewport_locked && (in_main || !landmark_seen)
    }

    /// Whether `id` clips its overflow on EITHER axis (`hidden`/`clip`) — the
    /// signal (on `html`/`body`) that the viewport can't scroll the document.
    fn node_clips_overflow(&self, id: NodeId) -> bool {
        matches!(
            self.axis_overflow(id, true).as_deref(),
            Some("hidden" | "clip")
        ) || matches!(
            self.axis_overflow(id, false).as_deref(),
            Some("hidden" | "clip")
        )
    }

    /// Lay a vertical inner-scroll viewport (the `flow_element` dispatch). The
    /// content is laid into a SEPARATE `buffer` at the scrollport width; the
    /// layout reserves exactly the box's DEFINITE height `H` in blank doc rows
    /// here and records a `Region` (the view windows the buffer over those rows)
    /// — the document stays flat, so scroll/selection indices are untouched. A
    /// definite-height `overflow-y:auto|scroll` box IS `H` tall whether its
    /// content overflows or fits (CSS: the `height` property fixes the box; the
    /// `auto`/`scroll` distinction is only scrollbar presence, not box size) —
    /// so a short chat list reserves its full band and shows empty space below
    /// the messages, exactly as a browser paints it, instead of growing inline
    /// from empty to full as messages arrive (her call 2026-06-29: follow what
    /// the page declares; don't render a fixed-height box as flexible). The two
    /// measurement passes (`tag_all_nodes`, the JS geometry backing; `measuring`,
    /// intrinsic-width sizing) flow the content INLINE instead, so region content
    /// keeps its honest box geometry.
    fn flow_region(&mut self, id: NodeId, ctx: &Ctx) {
        self.flush_block();
        self.begin_line();
        let band_left = self.line_left;
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        // The scrollport width: the box's own `width`/`max-width` if set, else
        // the available band (the common `width:100%`/stretched column).
        let width = self
            .css_cells(id, "width")
            .or_else(|| self.css_cells(id, "max-width"))
            .map(|w| w.min(avail))
            .unwrap_or(avail)
            .max(1);
        let h = self.scroll_region_height(id).unwrap_or(0);
        // Lay the content into its own buffer under the recursion guard, so the
        // sub-layout flows `id`'s interior (flex/grid/block) instead of
        // re-entering `flow_region` on `id`. The measuring flag carries through:
        // an intrinsic-width pass flows this buffer INLINE (see `clip` below),
        // and laying it un-measured would apply justify-content/text-align
        // offsets — inflating the measured width of any subtree holding a
        // region (a shrink-to-fit float/flex item measured ~half the band).
        let saved = self.region_inner;
        self.region_inner = Some(id);
        let mut buffer = self.layout_subtree_inner(id, width, None, self.measuring, ctx);
        self.region_inner = saved;
        // Reserve the clipped band on the REAL render pass for EVERY definite-
        // height (h > 0) scroll-y box — overflowing or fitting alike, since the
        // box is `h` tall either way (see the doc comment). The two measurement
        // passes flow the region's content INLINE instead, so it contributes its
        // real width/height: `tag_all_nodes` keeps getBoundingClientRect honest,
        // and `measuring` (intrinsic-width sizing) keeps a region from measuring
        // as ~0-wide — which would size a flex item / float / table cell
        // CONTAINING a scroll region down to nothing, then char-break its content
        // into one cell per row.
        let clip = h > 0 && !self.tag_all_nodes && !self.measuring;
        // A clipped region's buffer is windowed by the app — the document-root
        // composite can never paint into it, so its out-of-flow boxes composite
        // into the buffer HERE (they're part of the scroller's scrollable
        // overflow, CSS Overflow 3 §2.2); this also merges their nested
        // fixed/scroll_clips onto the buffer before the lifts below read them.
        // The inline branch instead lets them ride up to the root via `blit`.
        // BEFORE `content_h`, so scrollHeight counts an overlay below the flow.
        if clip {
            self.composite_box_positioned(&mut buffer);
        }
        let content_h = buffer.rows.len();
        let row_base = self.rows.len();
        // The live actor's node id (baked as `data-trust-node` by the serializer)
        // for the Phase-3 geometry round-trip + wheel write-back.
        let live_node: Option<usize> = self
            .dom
            .attr(id, "data-trust-node")
            .and_then(|s| s.parse().ok());
        // Record the CLIP box of EVERY definite-height scroll-y box (h > 0) —
        // region OR currently-fitting — so the app can push its clientHeight even
        // before its content overflows into a region (the chat `atBottom` fix).
        if h > 0
            && !self.tag_all_nodes
            && let Some(node) = live_node
        {
            self.scroll_clips.push((node, h as u16, width as u16));
        }
        if clip {
            // A `position:fixed` box captured inside the region's content is fixed
            // to the VIEWPORT, not the scroll container — it must NOT ride the
            // region's windowed buffer (which scrolls). Lift its pinned overlay OUT
            // of the buffer and pin it at the document level (translated to the
            // region's band, exactly like `blit` does for the inline `else`
            // branch), so a fixed rail nested inside a scrolling column stays put
            // while the column scrolls under it. Without this the buffer's `fixed`
            // is silently dropped when the region reserves its band.
            for f in &buffer.fixed {
                let mut f = f.clone();
                f.col += band_left as u16;
                f.row += row_base as u16;
                self.fixed.push(f);
            }
            // Clip boxes recorded inside the buffer (a nested definite-height
            // scroll box) still describe real client boxes — carry them up.
            // (The nested Region itself is still dropped here — the known
            // Phase-4 nested-regions gap — but its clientHeight stays honest.)
            self.scroll_clips
                .extend(buffer.scroll_clips.iter().copied());
            // A real scroll viewport: reserve exactly H blank doc rows for the
            // band (the renderer fills them from the buffer, clipped/windowed).
            for _ in 0..h {
                self.rows.push(Row::default());
            }
            // The page's own `scrollTop` SIGNAL, baked into the re-parsed HTML by
            // the live serializer. `data-trust-scroll-top` is in ROWS; CSSOM
            // clamps the position to `[0, scrollHeight − clientHeight]` =
            // `[0, content_h − h]`.
            let max_voffset = content_h.saturating_sub(h);
            let signal = self
                .dom
                .attr(id, "data-trust-scroll-top")
                .and_then(|s| s.parse::<usize>().ok());
            let voffset = signal.map_or(0, |rows| rows.min(max_voffset));
            // Collect every `<img>` URL in the region's subtree (decoded or not —
            // an undecoded one is alt text, not a laid item) so an image-decode
            // reflow can be routed to THIS region instead of the whole document.
            let mut image_urls = Vec::new();
            for d in self.dom.descendants(id) {
                if self.dom.tag_name(d) == Some("img")
                    && let Some(u) = self.image_src(d)
                    && !image_urls.contains(&u)
                {
                    image_urls.push(u);
                }
            }
            self.regions.push(Region {
                node: id,
                start_row: row_base,
                left: band_left as u16,
                width: width as u16,
                height: h as u16,
                buffer: buffer.rows,
                voffset,
                live_node,
                voffset_from_page: signal.is_some(),
                // The old engine flows the principal scroller into the document
                // (`scroll_region_height` returns None for it, so it never
                // reaches here), so its regions are always non-principal — the
                // document scroll + right scrollbar already scroll it as "the
                // page". Only layout2 keeps the principal scroller as a Region.
                principal: false,
                carousels: buffer.carousels,
                regions: Vec::new(),
                image_urls,
            });
        } else {
            // `auto` content that fits (or the measure pass): place inline.
            self.blit(&buffer, band_left as u16, row_base);
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// The scroll-container child whose block children are memoized for the
    /// region de-lag (INCREMENTAL_LAYOUT_PLAN.md §14). Descend from `boundary`
    /// through single-element-child wrappers (a chat region is
    /// `scrollable-area > message-container > messages`); stop at the first node
    /// with ≠1 element children. Cache there ONLY when it has ≥2 block-level,
    /// in-flow children laid by the plain block path — the seam
    /// `flow_cached_children` hooks. Anything else (a flex/grid list, mixed
    /// inline content, a 0/1-item list) returns `None` ⇒ the caller lays the
    /// region in full, always correct. Cascade/structure only, no layout.
    fn region_cache_container(&self, boundary: NodeId) -> Option<NodeId> {
        let mut cur = boundary;
        loop {
            let kids: Vec<NodeId> = self
                .dom
                .children(cur)
                .into_iter()
                .filter(|&c| self.dom.tag_name(c).is_some())
                .collect();
            if kids.len() == 1 {
                cur = kids[0];
                continue;
            }
            if kids.len() < 2 {
                return None; // nothing to memoize
            }
            // The seam is the block-children loop, so the container must flow its
            // children there (not flex/grid) and every child must be a block-level
            // in-flow box that owns whole rows (so capturing `rows[snap..]` is
            // exactly that child's content).
            if self.flex_mode(cur).is_some() {
                return None;
            }
            let all_block = kids.iter().all(|&c| {
                matches!(
                    self.flow_of(c, self.dom.tag_name(c).unwrap_or("")),
                    Flow::Block | Flow::ListItem
                )
            });
            return all_block.then_some(cur);
        }
    }

    /// Whether `id` is this pass's region cache container (region-patch path only).
    fn is_region_cache_container(&self, id: NodeId) -> bool {
        self.region_child_cache
            .as_ref()
            .is_some_and(|c| c.borrow().container == id)
    }

    /// The cache key for a region child: its subtree content hash
    /// (`subtree_layout_hash`) folded with each descendant `<img>`'s DECODE
    /// READINESS (whether its intrinsic size is known). A chat emote/avatar/badge
    /// carries no explicit dimensions (verified on Twitch), so its used box is its
    /// intrinsic size (CSS Images §5.1) — laid as alt/placeholder until decoded,
    /// then as the real W×H box. Folding readiness makes a decode invalidate ONLY
    /// the message that image sits in (a cache miss → re-laid with the real box),
    /// leaving every other message reused — so an emote decoding inside the region
    /// re-lays one message, not the whole page. Without it the content hash is
    /// unchanged on decode (same HTML) and the stale placeholder rows reuse.
    fn region_child_key(&self, child: NodeId) -> u64 {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write_u64(self.dom.subtree_layout_hash(child));
        for d in self.dom.descendants(child) {
            if self.dom.tag_name(d) == Some("img")
                && let Some(url) = self.image_src(d)
            {
                let ready = self
                    .images
                    .get(&url)
                    .is_some_and(|&(w, ht)| w > 0 && ht > 0);
                h.write_u8(ready as u8);
            }
        }
        h.finish()
    }

    /// Lay the block children of the region cache container, reusing the cached
    /// rows of every UNCHANGED child (by `subtree_layout_hash`) and laying only
    /// new/changed ones. Each child is captured/reused at a clean row boundary
    /// (`flush_block` before + after) — block children own whole rows, so the
    /// reassembled buffer is row-identical to a full in-flow layout (the §9
    /// differential guard `region_incremental_layout_matches_full` pins this).
    /// This is what turns an appended chat message from an O(all-messages)
    /// re-layout into an O(one-message) one.
    fn flow_cached_children(&mut self, id: NodeId, ctx: &Ctx) {
        let cache = self
            .region_child_cache
            .clone()
            .expect("a cache container implies a cache");
        for child in self.flow_children(id) {
            let key = self.region_child_key(child);
            // Close any open inline line so the child's rows start clean.
            self.flush_block();
            let hit = cache.borrow().old.get(&key).cloned();
            let rows = match hit {
                Some(rows) => {
                    // Reuse: append the cached rows verbatim (same width/indent).
                    self.rows.extend(rows.iter().cloned());
                    rows
                }
                None => {
                    // Miss: lay the child in flow, capture exactly its rows.
                    let snap = self.rows.len();
                    let side_snap = (
                        self.positioned.len(),
                        self.carousels.len(),
                        self.regions.len(),
                        self.fixed.len(),
                        self.scroll_clips.len(),
                    );
                    self.flow_node(child, ctx);
                    self.flush_block();
                    // A child that touched a SIDE channel (an abspos overlay in
                    // `positioned`, a carousel/region/fixed rail/scroll clip)
                    // put content somewhere the captured rows don't hold — a
                    // reuse would splice the rows and silently drop the rest
                    // (the standard's bar is render-as-if-fully-laid; a cache
                    // may only be transparent). Don't memoize it: it re-lays —
                    // and re-collects those channels — every pass. Such
                    // children are rare in a chat-shaped region, so the
                    // de-lag win holds for the plain-rows majority.
                    if (
                        self.positioned.len(),
                        self.carousels.len(),
                        self.regions.len(),
                        self.fixed.len(),
                        self.scroll_clips.len(),
                    ) != side_snap
                    {
                        continue;
                    }
                    std::rc::Rc::new(self.rows[snap..].to_vec())
                }
            };
            cache.borrow_mut().new.insert(key, rows);
        }
        self.col = self.indent;
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
                    // A PAINT-SUPPRESSED (`opacity:0`/`visibility:hidden`)
                    // out-of-flow box paints BLANK, and being out of flow it can
                    // never reserve in-flow space — so it must not be placed at
                    // all: composited last, its blank cells would stomp the live
                    // content beneath them, and a box below the flow would
                    // extend the scrollable region with dead rows. Steam's
                    // featured carousels pre-render ~13 hidden `.next` pages
                    // (`position:absolute`, opacity:0) of game tiles; placing
                    // them buried the real grid ~7 viewports down behind blank
                    // rows. (An IN-FLOW suppressed box still reserves space +
                    // paints blank — Mastodon's virtualized placeholders — that
                    // path never reaches here.)
                    // The MEASUREMENT pass skips them IDENTICALLY — geometry
                    // reports what we render, and once decoded image sizes fed
                    // the measure pass (PageCmd::ImageSizes), placing the hidden
                    // pages ballooned the measured document to ~4× the rendered
                    // one; every section below then "measured" viewports below
                    // the viewport, so Steam's one-shot lazy-image watchers
                    // (CScrollOffsetWatcher: IO, 500px buffer) never fired and
                    // the whole page's delayed-image groups never swapped in.
                    // They still get an honest zero-height box at their computed
                    // position via the recording loop below.
                    && !self.dom.paint_suppressed(d)
                    && !self.dom.visibility_hidden(d)
                    && !self.is_clipped_offscreen(d)
                    // A pinned `position:fixed` box is captured into the overlay
                    // layer at its static position (`capture_fixed_children`),
                    // not placed in the scrolling document — don't double-place.
                    && !self.is_fixed_pinned(d)
                    && self.positioned_containing_block(d) == cb
                    // An abspos `<video>`/`<audio>` whose in-flow wrapper dispatch
                    // already rendered a media representation in THIS Layout
                    // (`flow_media` marked it) must not be placed again — that was
                    // the doubled Twitch player preview. A media element with no
                    // such wrapper (not yet marked) is still placed here.
                    && !self.media_emitted.contains(&d)
            })
            .collect();
        // Geometry for the paint-suppressed out-of-flow boxes skipped above:
        // record their COMPUTED position (zero flow cost — they neither paint
        // nor push content in the render, so they must not in the measure
        // either), and the `element_tops` post-pass gives each an honest
        // zero-height box there. Their descendants stay boxless (honestly not
        // intersecting), matching what the reader's page actually shows.
        if self.tag_all_nodes {
            for d in self.dom.composed_descendants(root) {
                if matches!(self.dom.node(d).data, NodeData::Element { .. })
                    && self.is_out_of_flow(d)
                    && !self.dom.is_hidden(d)
                    && (self.dom.paint_suppressed(d) || self.dom.visibility_hidden(d))
                    && self.positioned_containing_block(d) == cb
                {
                    let gc = (origin_col as i32 + self.abs_used_left(d, cb_w as i32, 0)).max(0);
                    let gr =
                        (origin_row as i32 + self.abs_used_top(d, cb_h as i32, 0).max(0)).max(0);
                    self.element_tops.entry(d).or_insert((
                        u16::try_from(gc).unwrap_or(u16::MAX),
                        u16::try_from(gr).unwrap_or(u16::MAX),
                    ));
                }
            }
        }
        if kids.is_empty() {
            return;
        }
        // DIAG (TRUST_DIAG_POS): trace every out-of-flow placement — the CB,
        // its content box, and each child's used geometry/drop reason — to
        // localize a "content vanished" report to the exact placement step.
        let diag = std::env::var_os("TRUST_DIAG_POS").is_some() && !self.measuring;
        let name = |id: NodeId| -> String {
            let tag = self.dom.tag_name(id).unwrap_or("·");
            let cls = self
                .dom
                .attr(id, "class")
                .map(|c| {
                    let c: String = c.split_whitespace().take(2).collect::<Vec<_>>().join(".");
                    format!(".{c}")
                })
                .unwrap_or_default();
            format!("<{tag}>{}", cls.chars().take(48).collect::<String>())
        };
        if diag {
            eprintln!(
                "DIAGPOS cb={} origin=({origin_col},{origin_row}) cb_w={cb_w} cb_h={cb_h} band_w={}",
                cb.map_or("<ICB>".into(), name),
                self.width
            );
        }
        // Lay each box and resolve its used (left, top) in cells. `left`/`top`
        // are the UN-transformed §10.3.7/§10.6.4 positions and `used_w` the
        // un-scaled used width (what the occlusion collapse judges — a stack
        // of coincident carousel layers separated only by transform must
        // still collapse); `tx`/`ty`/`sx` are the transform's paint offset and
        // scale, applied at the clip test and the final collect.
        struct Placed {
            node: NodeId,
            left: i32,
            top: i32,
            tx: i32,
            ty: i32,
            sx: f32,
            used_w: usize,
            bottom_pinned: bool,
            z: i32,
            b: LaidBox,
        }
        let mut placed: Vec<Placed> = Vec::new();
        for k in kids {
            // A positioned descendant still inherits an enclosing `<a>`'s link
            // (a badge/menu wrapped in an anchor stays clickable), even though
            // its containing block — not its DOM parent — places it.
            let inherit = self.ancestor_link_ctx(k);
            // Used width (CSS 2.1 §10.3.7): an explicit `width` (or a `left`+
            // `right` stretch), else SHRINK-TO-FIT. We still lay shrink-to-fit at
            // the full band and read back its content extent (laying at a measured
            // width re-wraps floated content — Steam's nav stacked that way), but
            // set `shrink_wrap` so a `width:%` REPLACED child with no sized
            // ancestor contributes its INTRINSIC width instead of stretching to
            // the band (CSS Sizing §5.1). Without it, Twitch's featured carousel
            // player — a `width:100%` streaming thumbnail in a `width:auto` card —
            // filled the whole content area instead of its card-sized box. Floats
            // and text are untouched (still laid at the band), so they don't wrap.
            let explicit_w = self.abs_used_width(k, cb_w);
            // §10.3.7: an EXPLICIT width is the used width — an abspos box
            // freely OVERFLOWS its containing block, so it is never clamped to
            // the CB (Twitch's 34rem chat column hangs on a `width:fit-content`
            // wrapper that legitimately collapses to ~0 because its only
            // content is out-of-flow; clamping to that CB laid the whole chat
            // one cell wide). The only lid is the TERMINAL band (no h-scroll).
            // Shrink-to-fit (`None`) keeps the CB band as its available space,
            // per §10.3.7's shrink-to-fit formula.
            let lay_w = explicit_w
                .unwrap_or(cb_w)
                .clamp(1, self.viewport_w.max(cb_w).max(1));
            let prev_sw = self.shrink_wrap;
            self.shrink_wrap = explicit_w.is_none();
            let b = self.layout_subtree_inner(k, lay_w, Some(k), false, &inherit);
            self.shrink_wrap = prev_sw;
            // The paint offset of a translate transform (CSS Transforms 1 §6 —
            // % against the box's own size). Resolved once here; the clip test
            // and the final placement below judge the PAINTED position
            // (overflow clips painted content, and translation moves it), while
            // the occlusion collapse deliberately stays on the UN-translated
            // stack (coincident layers differentiated only by transform — the
            // peek-carousel pattern — must still collapse to their top layer).
            let used_w = explicit_w.unwrap_or(b.width as usize).max(1);
            let (tx, ty, sx) = self.transform_offset(k, used_w, b.height as usize);
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
                // getBoundingClientRect INCLUDES transforms (CSSOM View §12) —
                // record the translated (painted) coordinate.
                let gc =
                    (origin_col as i32 + self.abs_used_left(k, cb_w as i32, used_w as i32) + tx)
                        .max(0);
                let gr = (origin_row as i32
                    + self.abs_used_top(k, cb_h as i32, b.height as i32).max(0)
                    + ty)
                    .max(0);
                self.element_tops.insert(
                    k,
                    (
                        u16::try_from(gc).unwrap_or(u16::MAX),
                        u16::try_from(gr).unwrap_or(u16::MAX),
                    ),
                );
            }
            // A 0-height box still places when it carries a pinned fixed
            // overlay (an abspos shell whose only content is `position:fixed`)
            // OR nested out-of-flow boxes (an abspos `width:100%;height:100%`
            // link wrapper whose ONLY content is an abspos fill image + abspos
            // corner badges — every thumbnail-card grid): it draws no rows
            // itself, but the composite must still reach those descendants, so
            // it survives occlusion and its side channels propagate.
            if b.height == 0 && b.fixed.is_empty() && b.positioned.is_empty() {
                if diag {
                    eprintln!("DIAGPOS   {} DROP empty (h=0)", name(k));
                }
                continue;
            }
            let left = self.abs_used_left(k, cb_w as i32, used_w as i32);
            // Box height is content-driven: a cell grid has no internal scroll,
            // so a fixed-height `overflow` panel can't be honored. Per §10.6.4
            // that makes height auto; `top` is then clamped to the CB so a
            // bottom-anchored tall panel rides the top instead of off-screen.
            let top = self.abs_used_top(k, cb_h as i32, b.height as i32).max(0);
            // CSS Overflow 3 §3: a containing block that CLIPS its overflow
            // (`overflow:hidden`/`clip`) paints nothing of a positioned child
            // that falls ENTIRELY outside its content box on the clipped axis.
            // Now that an out-of-flow box no longer inflates its ancestors
            // (§9.3.1), a collapsed-overflow wrapper reports its true (often
            // zero) in-flow content box as `cb_h`/`cb_w`, so this row/column
            // test recognizes an off-canvas drawer that a browser hides —
            // e.g. NBC's `height:100vh` hamburger/notification panels inside an
            // `overflow:hidden` wrapper that shrank to its 0 in-flow height.
            // (Fraction-based `is_clipped_offscreen`, checked in the filter
            // above, still catches the `%`/inset off-canvas cases it always
            // did; this adds the laid-geometry case it can't resolve.)
            if let Some(cbid) = cb {
                // STRICTLY past the content box (`> cb_h`, not `>=`): a box AT
                // the origin (`top:0`) stays even when the CB's in-flow content
                // is zero, because the clip is to the PADDING box, and a
                // `padding-bottom:100%` aspect-ratio square (its `inset:0` fill
                // image) or a `top:0` fill overlay must not be dropped. Only a
                // box pushed BELOW the content box — NBC's `top:60px` drawer in
                // a 0-height `overflow:hidden` wrapper — clips away. Judged at
                // the PAINTED (translated) position: overflow clips painted
                // content, and a translate moves it (`left:100%` +
                // `translateX(-100%)` is a fully visible right-docked panel).
                let vclip = self.clips_hard(cbid, true) && top + ty > cb_h as i32;
                let hclip = self.clips_hard(cbid, false) && left + tx > cb_w as i32;
                if vclip || hclip {
                    if diag {
                        eprintln!(
                            "DIAGPOS   {} DROP clipped (top={top} left={left} v={vclip} h={hclip})",
                            name(k)
                        );
                    }
                    continue;
                }
            }
            if diag {
                eprintln!(
                    "DIAGPOS   {} placed left={left} top={top} used_w={used_w} lay_w={lay_w} b={}x{}",
                    name(k),
                    b.width,
                    b.height
                );
            }
            // A box anchored AT or BELOW the CB's bottom edge (`top:auto` and
            // `bottom ≤ 0`, e.g. a footer at `bottom:-1.5rem`) follows the
            // content: it lands just past the extent of its positioned siblings
            // (fixed up below). An INSET bottom (`bottom > 0`) keeps the
            // §10.6.4 clamp.
            let bottom_pinned = self.pos_len(k, "top", cb_h).is_none()
                && self.pos_len(k, "bottom", cb_h).is_some_and(|b| b <= 0.0);
            placed.push(Placed {
                node: k,
                left,
                top,
                tx,
                ty,
                sx,
                used_w,
                bottom_pinned,
                z: self.z_index(k),
                b,
            });
        }
        if placed.is_empty() {
            return;
        }
        // Occlusion collapse: a terminal cell has no z-axis and an image is an
        // atomic, opaque blit (see the ratatui image model), so when two
        // positioned siblings nearly COINCIDE — alternative layers of one region,
        // differentiated only by `z-index` and/or a `transform` we don't apply —
        // the page would composite them by z-order and the lower layers would be
        // occluded. We can't composite, so we paint only the TOP layer and drop
        // the rest (CSS 2.1 Appendix E painting order: higher `z-index`, then
        // later in document order, wins the shared cells; her call: "overlap =
        // topmost wins, no paging carousel"). This is what collapses Twitch's
        // front-page peek-carousel — 5 cards all at `left:calc(50%-375px);top:0`,
        // separated only by `transform: translateX(…) scale(…)` and z 1/2/3/2/1 —
        // to its focused (center, z 3) card instead of stacking 5 full-panel
        // images. A box `i` is occluded (dropped) when some box `j` paints ON TOP
        // of it AND they overlap by a large fraction of BOTH areas. The
        // both-areas test is what spares a small overlay (a corner deal-badge over
        // a store capsule): the badge covers ~all of itself but only a sliver of
        // the image, so it is NOT occluded — it still rides its own row via the
        // image-lift below.
        if placed.len() > 1 {
            let on_top =
                |a: &Placed, b: &Placed, ai: usize, bi: usize| a.z > b.z || (a.z == b.z && ai > bi);
            let mostly_covers = |a: &Placed, b: &Placed| {
                let ax1 = a.left + a.used_w as i32;
                let ay1 = a.top + a.b.height as i32;
                let bx1 = b.left + b.used_w as i32;
                let by1 = b.top + b.b.height as i32;
                let ow = (ax1.min(bx1) - a.left.max(b.left)).max(0);
                let oh = (ay1.min(by1) - a.top.max(b.top)).max(0);
                let overlap = (ow as i64) * (oh as i64);
                let area_a = (a.used_w as i64) * (a.b.height as i64);
                let area_b = (b.used_w as i64) * (b.b.height as i64);
                let frac = |area: i64| area > 0 && overlap * 5 >= area * 3; // ≥60%
                frac(area_a) && frac(area_b)
            };
            // A PAINT-SUPPRESSED box (`opacity:0` or `visibility:hidden`) can't
            // occlude anything — it writes only blank cells (`Item.invisible`),
            // so the box beneath still shows through. This is the slideshow
            // guard: a deck of stacked suppressed slides with one visible active
            // slide would otherwise collapse to the topmost (a blank inactive
            // slide) by painting order. Excluding suppressed occluders leaves the
            // active slide visible while the inactive ones still lay out + paint
            // blank — the same outcome the old `is_hidden` shortcut produced, via
            // the correct paint-suppression model.
            let occluded: Vec<bool> = (0..placed.len())
                .map(|i| {
                    (0..placed.len()).any(|j| {
                        j != i
                            && !self.dom.paint_suppressed(placed[j].node)
                            && !self.dom.visibility_hidden(placed[j].node)
                            && on_top(&placed[j], &placed[i], j, i)
                            && mostly_covers(&placed[i], &placed[j])
                    })
                })
                .collect();
            if diag {
                for (i, occ) in occluded.iter().enumerate() {
                    if *occ {
                        eprintln!("DIAGPOS   {} DROP occluded", name(placed[i].node));
                    }
                }
            }
            let mut iter = occluded.into_iter();
            placed.retain(|_| !iter.next().unwrap_or(false));
        }
        // The painted right extent (translate + scale included): what must fit
        // the band.
        let mut union_w = 0i32;
        for p in &placed {
            let painted_w = ((p.used_w as f32) * p.sx.min(1.0)).round().max(1.0) as i32;
            union_w = union_w.max((p.left + p.tx).max(0) + painted_w);
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
        // The band is the TERMINAL viewport, never a sub-layout's width: an
        // out-of-flow box escapes a narrow flex-item band (§9.3.1/§10.3.7 —
        // Twitch's chat hangs on a wrapper that measures ~0), so judging the
        // fit against `self.width` inside such a sub crushed the box to
        // nothing. Inside a sub `origin_col` is sub-relative (an underestimate
        // of the true document offset), so this errs permissive; the final
        // clamp into the real band happens at the root composite, and content
        // past the right edge clips there like any other overflow.
        let band = self
            .viewport_w
            .max(self.width)
            .saturating_sub(origin_col)
            .max(1) as i32;
        let scale = (band as f32 / union_w as f32).min(1.0);
        if diag && scale < 1.0 {
            eprintln!("DIAGPOS   compress union_w={union_w} band={band} scale={scale:.3}");
        }
        // Collect each box into the `positioned` side channel at its placed
        // (col, row) — do NOT blit it into `rows`. Keeping out-of-flow boxes out
        // of the flow is the whole point: they must not inflate this containing
        // block or push its siblings (CSS 2.1 §9.3.1). The document root paints
        // them last (`composite_positioned`), which is also where the scrollable
        // region grows to reach a box placed below the flow (CSS Overflow 3 §2.2).
        // The collected column stays SIGNED (a translated box can sit left of
        // its collapsed wrapper in this sub's coordinate space — blit offsets
        // rebase it toward the document, and only the composite clamps into
        // its surface's band); a hostile `left:9999999px` is lidded here so a
        // huge offset can't ride the channels as a garbage coordinate.
        let lid = (self.viewport_w.max(1) as i32).saturating_mul(2);
        let clamp_lid = move |c: i32| c.clamp(-lid, lid);
        for p in placed {
            // The box's OWN transform scale (hostile-input lid), composed with
            // the group compress-to-fit factor. A scaled box re-lays at its
            // scaled width — the same reflow the compress path does — and
            // shrinks toward its own center (`transform-origin` default
            // 50% 50%): the column gains half the width it lost, the row half
            // the height (the height delta comes out of the re-lay; a
            // text-heavy box that got TALLER at the narrower width shifts up
            // instead — best-effort centering on the untransformed box).
            let own = p.sx.clamp(0.05, 8.0);
            let target_w = ((p.used_w as f32) * scale * own).round().max(1.0) as usize;
            let (col, b, row_shift) = if scale < 1.0 || (own - 1.0).abs() > 0.01 {
                let pre_h = p.b.height as i32;
                let inherit = self.ancestor_link_ctx(p.node);
                let b = self.layout_subtree_inner(p.node, target_w, Some(p.node), false, &inherit);
                let col = origin_col as i32
                    + ((p.left + p.tx) as f32 * scale).round() as i32
                    + (((p.used_w as f32) * scale * (1.0 - own)) / 2.0).round() as i32;
                let row_shift = if (own - 1.0).abs() > 0.01 {
                    (pre_h - b.height as i32) / 2
                } else {
                    0
                };
                (clamp_lid(col), b, row_shift)
            } else {
                (clamp_lid(origin_col as i32 + p.left + p.tx), p.b, 0)
            };
            let row = (origin_row as i32 + p.top + p.ty + row_shift).max(0) as usize;
            self.collect_positioned(col, row, b);
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// Add an out-of-flow box (and any out-of-flow descendants it collected) to
    /// the `positioned` side channel at `(col, row)` in the current buffer. The
    /// box is pushed FIRST so it composites beneath its own descendants (CSS 2.1
    /// Appendix E tree order for equal `z-index`); each nested box is then lifted
    /// out of `b` and re-based onto this box's coordinate, so `b.positioned` ends
    /// empty and the root composite iterates one flat, document-ordered set.
    fn collect_positioned(&mut self, col: i32, row: usize, mut b: LaidBox) {
        let nested = std::mem::take(&mut b.positioned);
        self.positioned.push(PositionedBox { col, row, b });
        for mut n in nested {
            n.col = n.col.saturating_add(col);
            n.row += row;
            self.positioned.push(n);
        }
    }

    /// Composite the collected out-of-flow boxes over the in-flow document — the
    /// final paint step, run once at the document root (a full page in
    /// `flow_all`, a fragment in `lay_out_subtree_fragment`). Each box was kept
    /// out of `rows` so it never affected flow height (CSS 2.1 §9.3.1); painting
    /// it here is what grows the scrollable region to reach a box placed below
    /// the flow (CSS Overflow 3 §2.2). Boxes paint in row order so the image-lift
    /// row inserts only shift boxes below them; the per-containing-block
    /// occlusion collapse (done at collection) plus this order reproduce the old
    /// in-place paint order. An overlay that would land on a decoded image is
    /// lifted onto its own rows (a sixel is an atomic blit and can't be
    /// composited under an overlay) — and overlays that start on the SAME
    /// pre-lift rows SHARE one lifted band, so the corner badges of a card row
    /// land in a single stripe instead of each inserting its own document-wide
    /// band (which staircased the badges and tore every sibling card).
    fn composite_positioned(&mut self) {
        if self.positioned.is_empty() {
            return;
        }
        let mut boxes = std::mem::take(&mut self.positioned);
        // Stable sort by row keeps the collection (document, then z-collapsed)
        // order among boxes sharing a row, so a child still paints over its
        // parent when both start on the same row.
        boxes.sort_by_key(|p| p.row);
        // The collected column is SIGNED (sub-relative, may be negative for a
        // translated box); this composite is the one place the true band is
        // known, so clamp into it here. A negative final column pins to 0 (no
        // negative cells — the closest cell-model rendering of a box hanging
        // off the left edge); right overflow stays and clips at render like
        // any other overflow (no horizontal scroll).
        let band_max = self.width.max(1) as i32 - 1;
        let mut inserted = 0usize;
        // The active lifted band, as (pre-lift source row of the box that
        // forced it, its post-insert top, its height). A later overlay whose
        // pre-lift row falls inside the band joins it — the per-containing-
        // block confinement the old in-place lift had, restored at the root.
        let mut band: Option<(usize, usize, usize)> = None;
        for p in boxes {
            let col = p.col.clamp(0, band_max) as u16;
            let h = (p.b.height as usize).max(1);
            let natural = p.row + inserted;
            let mut target = natural;
            // Only a box that paints rows can need the lift; an empty shell
            // (its content flattened out at collection) must not insert rows.
            if p.b.height > 0 && self.overlay_hits_image(natural, h, col, p.b.width) {
                match band {
                    Some((src, start, bh)) if p.row >= src && p.row < src + bh => {
                        // Join the existing band at the same relative offset;
                        // grow it if this box is taller than what remains.
                        let off = p.row - src;
                        target = start + off;
                        if off + h > bh {
                            let extra = off + h - bh;
                            self.insert_blank_rows(start + bh, extra);
                            inserted += extra;
                            band = Some((src, start, bh + extra));
                        }
                    }
                    _ => {
                        self.insert_blank_rows(natural, h);
                        inserted += h;
                        band = Some((p.row, natural, h));
                    }
                }
            }
            // `p.b.positioned` is empty (flattened at collection), so this blit
            // adds nothing back to `self.positioned`.
            self.blit(&p.b, col, target);
        }
    }

    /// Composite a box's collected out-of-flow descendants into its OWN rows —
    /// for the consumers that hand a box's rows to a WINDOWED surface the
    /// document-root composite can never paint into: a pinned fixed rail's
    /// `FixedItem.rows` (an overlay layer) and a scroll region's buffer (the
    /// app windows it; its positioned content is part of the scroller's
    /// scrollable overflow, CSS Overflow 3 §2.2). Runs the root machinery on a
    /// scratch sub-layout so the image-lift and side-channel shifts behave
    /// identically, then hands EVERY channel back on the box — the composite's
    /// `blit` merges each `PositionedBox`'s nested carousels/regions/fixed/
    /// scroll_clips/element_tops into the scratch, and dropping them here would
    /// silently lose e.g. a scroll box nested in an abspos hovercard. In-flow
    /// consumers must NOT call this: their boxes ride `b.positioned` up to the
    /// document root via `blit` (CSS 2.1 §9.3.1).
    fn composite_box_positioned(&self, b: &mut LaidBox) {
        if b.positioned.is_empty() {
            return;
        }
        let mut scratch = self.make_sub((b.lay_width as usize).max(1));
        scratch.rows = std::mem::take(&mut b.rows);
        scratch.carousels = std::mem::take(&mut b.carousels);
        scratch.regions = std::mem::take(&mut b.regions);
        scratch.fixed = std::mem::take(&mut b.fixed);
        scratch.scroll_clips = std::mem::take(&mut b.scroll_clips);
        scratch.element_tops = std::mem::take(&mut b.element_tops);
        scratch.boundary_boxes = std::mem::take(&mut b.boundary_boxes);
        scratch.positioned = std::mem::take(&mut b.positioned);
        scratch.composite_positioned();
        b.rows = std::mem::take(&mut scratch.rows);
        b.carousels = std::mem::take(&mut scratch.carousels);
        b.regions = std::mem::take(&mut scratch.regions);
        b.fixed = std::mem::take(&mut scratch.fixed);
        b.scroll_clips = std::mem::take(&mut scratch.scroll_clips);
        b.element_tops = std::mem::take(&mut scratch.element_tops);
        b.boundary_boxes = std::mem::take(&mut scratch.boundary_boxes);
        // The composite can extend the box (an overlay below its flow or wider
        // than its painted content) — report the true extents.
        b.height = b.rows.len().min(u16::MAX as usize) as u16;
        let painted = b
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|it| it.col.saturating_add(it.width))
            .max()
            .unwrap_or(0);
        b.width = b.width.max(painted);
    }

    /// Whether placing a box at rows `[at, at+h)` over columns `[col, col+w)`
    /// would land on a decoded image already laid in those cells. A terminal
    /// cell has no z-axis and a sixel is an atomic blit, so an overlay can't be
    /// composited over an image — `composite_positioned` lifts it onto its
    /// own row instead.
    fn overlay_hits_image(&self, at: usize, h: usize, col: u16, w: u16) -> bool {
        let hi = col.saturating_add(w);
        // GEOMETRIC test against real, paintable image BOXES only:
        //   - `it.width > 0 && it.image.is_some()` — zero-width spacer markers
        //     (the §8.4 frame reservation and image-box row reservers are
        //     `ItemKind::Image` so the blank-row collapse keeps them) and
        //     undecoded placeholders paint NO pixels, so an overlay there is
        //     not over an image. Counting the frame markers made EVERY player-
        //     chrome icon over a hero card "hit an image" — a lift cascade
        //     that inflated the reserved band into a giant void and exiled the
        //     hero images below the fold (Twitch's front page, steady state).
        //   - a tall image's ITEM lives in its TOP row while its box spans
        //     `height` rows down, so the scan starts a window ABOVE `at` and
        //     tests each image's row+height span against [at, at+h).
        let lo = at.saturating_sub(IMG_CSS_MAX_ROWS);
        (lo..(at + h).min(self.rows.len())).any(|r| {
            self.rows[r].items.iter().any(|it| {
                matches!(it.kind, ItemKind::Image)
                    && it.width > 0
                    && it.image.is_some()
                    && r + (it.height.max(1) as usize) > at
                    && it.col < hi
                    && it.col.saturating_add(it.width) > col
            })
        })
    }

    /// Insert `n` blank rows at row `at`, pushing the existing rows — and EVERY
    /// recorded row reference at or below `at` (carousels, floats, scroll
    /// regions, pinned fixed boxes, measured element tops, incremental-layout
    /// boundaries) — down. Lets a lifted overlay (see
    /// `place_positioned_children`) take its own row without painting over what
    /// was there. Shifting only some of the side channels is a correctness
    /// hole: a stale `Region.start_row` drifts the reserved band off its rows
    /// (and mis-marks the reserved set in `finish`), and a stale
    /// `BoundaryRec` span makes a live incremental patch splice the wrong rows.
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
        for rg in &mut self.regions {
            if rg.start_row >= at {
                rg.start_row += n;
            }
        }
        for f in &mut self.fixed {
            if f.row as usize >= at {
                f.row = f.row.saturating_add(n as u16);
            }
        }
        for (_, row) in self.element_tops.values_mut() {
            if *row as usize >= at {
                *row = row.saturating_add(n as u16);
            }
        }
        for rec in self.boundary_boxes.values_mut() {
            if rec.start_row >= at {
                rec.start_row += n;
            }
            // End is EXCLUSIVE: a span ending exactly at the insert point sits
            // wholly above it and must not grow over the inserted blanks.
            if rec.end_row > at {
                rec.end_row += n;
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

    /// The paint GEOMETRY a `transform` applies to an out-of-flow box — the
    /// summed translation offset (columns, rows) and the product of uniform
    /// scale factors. CSS Transforms 1 §6: transforms move/size the PAINTED
    /// box after layout; a translation percentage resolves against the box's
    /// OWN border box. Applied ONLY to out-of-flow boxes: they composite as
    /// movable units, so a whole-cell offset is exact and a scale is a re-lay
    /// at the scaled width (the compress-to-fit machinery) — an in-flow
    /// element's items are welded into shared line rows the paint can't
    /// offset, so in-flow transforms stay unapplied. Translation is the
    /// standard slide-in/centering mechanism (Twitch's chat `translateX
    /// (-34rem)`, `left:50%; translate(-50%,-50%)`); scale is the peek-
    /// carousel card-sizing mechanism (Twitch's hero at `scale(0.703)`).
    /// Documented simplifications: rotate/skew/matrix have no cell analogue
    /// and are ignored; scale is uniform-ized (the X factor; height follows
    /// the re-lay); translation is applied unscaled rather than composed
    /// through the full function order; `transform-origin` is assumed at its
    /// default (the box center).
    fn transform_offset(&self, id: NodeId, used_w: usize, used_h: usize) -> (i32, i32, f32) {
        let Some(t) = self.dom.computed_style(id, "transform") else {
            return (0, 0, 1.0);
        };
        let t = t.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("none") {
            return (0, 0, 1.0);
        }
        let u = self.units(id);
        let (mut tx, mut ty, mut sx) = (0.0f32, 0.0f32, 1.0f32);
        let mut rest = t;
        while let Some(open) = rest.find('(') {
            let name = rest[..open]
                .rsplit(|c: char| c.is_whitespace() || c == ')')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            let Some(close) = rest[open..].find(')') else {
                break;
            };
            let args = &rest[open + 1..open + close];
            rest = &rest[open + close + 1..];
            let mut parts = split_args(args).into_iter();
            let num = |v: Option<&str>| v.and_then(|v| v.trim().parse::<f32>().ok());
            match name.as_str() {
                "translatex" => {
                    if let Some(v) = parts.next() {
                        tx += self.translate_cols(v.trim(), used_w, u);
                    }
                }
                "translatey" => {
                    if let Some(v) = parts.next() {
                        ty += self.translate_rows(v.trim(), used_h, u);
                    }
                }
                "translate" | "translate3d" => {
                    if let Some(v) = parts.next() {
                        tx += self.translate_cols(v.trim(), used_w, u);
                    }
                    if let Some(v) = parts.next() {
                        ty += self.translate_rows(v.trim(), used_h, u);
                    }
                }
                "scale" | "scale3d" => {
                    if let Some(f) = num(parts.next()).filter(|f| *f > 0.0) {
                        sx *= f;
                    }
                }
                "scalex" => {
                    if let Some(f) = num(parts.next()).filter(|f| *f > 0.0) {
                        sx *= f;
                    }
                }
                _ => {}
            }
        }
        (tx.round() as i32, ty.round() as i32, sx)
    }

    /// A translate X argument in cells: `%` against the box's own width, else
    /// the shared width resolver (px/em/rem/vw/calc…). Unresolvable → 0.
    fn translate_cols(&self, v: &str, used_w: usize, u: Units) -> f32 {
        resolve_cells_f32(v, used_w, (self.viewport_w, self.viewport_h), u).unwrap_or(0.0)
    }

    /// A translate Y argument in rows: `%` against the box's own height, else
    /// the height-axis resolver (1em ≈ 1 row; `vh` against the viewport).
    /// Unresolvable → 0.
    fn translate_rows(&self, v: &str, used_h: usize, u: Units) -> f32 {
        parse_percent(v)
            .map(|f| f * used_h as f32)
            .or_else(|| css_height_rows_f32(v, self.viewport_h, u))
            .unwrap_or(0.0)
    }

    /// Whether the element declares a transform that MOVES or RESIZES its
    /// painted box (translate/scale) — the fraction-based offscreen test
    /// can't judge such a box (the transform moves the painted box back
    /// toward/away from the clip), so it must stay indeterminate there and be
    /// judged by the transform-aware laid-geometry clip test at placement.
    fn has_translate(&self, id: NodeId) -> bool {
        self.dom.computed_style(id, "transform").is_some_and(|t| {
            let t = t.to_ascii_lowercase();
            t.contains("translate") || t.contains("scale")
        })
    }

    /// The used `z-index` of a box for painting order — the integer value, or
    /// `0` for `auto`/unset/non-integer. Two consumers, both comparing only the
    /// RELATIVE order within one comparison set (never stacking-context
    /// nesting): the modal-overlay pick (`find_modal_overlay`/
    /// `content_paints_above`) and the positioned-sibling occlusion collapse in
    /// `place_positioned_children`.
    fn z_index(&self, id: NodeId) -> i32 {
        self.dom
            .computed_style(id, "z-index")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
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
        resolve_cells_f32(
            t,
            extent,
            (self.viewport_w, self.viewport_h),
            self.units(id),
        )
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
                pixelated: false,
                emph: Emphasis::default(),
                node: NO_NODE,
                link: Some(Link::CarouselScroll(dir)),
                // Generated scroll control — always painted.
                invisible: false,
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
        let inside: std::collections::HashSet<NodeId> = self.dom.descendants(container).collect();
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
        let mut kids = self.flex_items(id);
        // `column-reverse` (Flexbox §5.1): the main axis runs bottom-to-top,
        // so the stacked items render in reverse order — the LAST item on top
        // (a chat log that appends newest-last but displays newest-first).
        if self.flex_main_reversed(id) {
            kids.reverse();
        }
        // Within the float-narrowed band (set by the block boundary's
        // `begin_line`), not the raw block box — so a stacked column beside a
        // float (a latest-post avatar floated left of its title/date column)
        // flows to its right instead of painting over it.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        // Cross-axis alignment (CSS Flexbox §8.3): in a COLUMN container the
        // cross axis is horizontal. The default `stretch` fills the width
        // (stack_boxes); `center`/`flex-end` size each item to FIT-CONTENT
        // and offset it across the band — Steam's login page centers its
        // whole bounded login card with `align-items:center` on a column
        // wrapper, and stretching it instead pinned the card's `flex:0` QR
        // pane to the far right edge of the terminal. `align-self` on an
        // item overrides the container value, per spec. `flex-start`/
        // `baseline` keep the stretch path: without painted backgrounds a
        // left-aligned fit-content box renders identically to a stretched
        // one, so the narrower box isn't worth the layout churn.
        let container_align = self
            .dom
            .computed_style(id, "align-items")
            .map(|v| v.trim().to_string());
        let mut row = self.rows.len();
        for &k in &kids {
            let align = self
                .dom
                .computed_style(k, "align-self")
                .map(|v| v.trim().to_string())
                .filter(|v| v != "auto")
                .or_else(|| container_align.clone());
            let (w, offset) = match align.as_deref() {
                Some("center" | "flex-end" | "end" | "self-end") => {
                    // Fit-content: the content's measured width, floored by
                    // any definite width in the subtree (a definite box never
                    // compresses — same floor as the §4.5 automatic minimum),
                    // capped to the band.
                    let fit = self
                        .measure_width(k, avail)
                        .max(self.definite_width_floor(k).unwrap_or(0))
                        .min(avail)
                        .max(1);
                    let slack = avail - fit;
                    let off = if matches!(align.as_deref(), Some("center")) {
                        slack / 2
                    } else {
                        slack
                    };
                    (fit, off)
                }
                _ => (avail, 0),
            };
            let b = self.layout_subtree(k, w, ctx);
            // A 0-height item is skipped — unless it carries a pinned fixed
            // overlay or collected out-of-flow boxes: the blit draws no rows
            // but propagates the side channels at this item's position (same
            // rule as the flex-row spacer-pane guard).
            if b.height == 0 && b.fixed.is_empty() && b.positioned.is_empty() {
                continue;
            }
            self.blit(&b, (self.line_left + offset) as u16, row);
            row += b.height as usize;
        }
        self.col = self.line_left;
        self.pending_space = false;
    }

    /// Stack a set of child boxes vertically at `width`, each below the
    /// last (shared by column flex and the row fallback). Blits at the band
    /// left so the column clears an active float (`line_left == indent` when
    /// none is active, leaving the common case unchanged).
    fn stack_boxes(&mut self, kids: &[NodeId], width: usize, ctx: &Ctx) {
        let mut row = self.rows.len();
        for &k in kids {
            let b = self.layout_subtree(k, width, ctx);
            // A 0-height box still blits when it carries a pinned fixed overlay
            // (Mastodon's side panes hold only a `position:fixed` rail) or
            // collected out-of-flow boxes: the blit adds no rows but propagates
            // the side channels at the box's stack position, instead of
            // silently dropping the overlay with the box.
            if b.height == 0 && b.fixed.is_empty() && b.positioned.is_empty() {
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
    /// non-`visible` used `overflow` on EITHER axis (the ubiquitous
    /// `overflow:hidden` clearfix — per-axis via `axis_overflow`, so a bare
    /// `overflow-x`/`overflow-y` longhand counts too, CSS 2.1 §9.4.1) and
    /// `display:flow-root`. Flex/grid containers and floats already lay their
    /// content as self-contained boxes, so they're excluded by the caller
    /// (`flex.is_none()`).
    fn establishes_bfc(&self, id: NodeId) -> bool {
        if self.dom.computed_display(id).as_deref() == Some("flow-root") {
            return true;
        }
        [false, true].into_iter().any(|vertical| {
            matches!(
                self.axis_overflow(id, vertical).as_deref(),
                Some("hidden" | "clip" | "auto" | "scroll")
            )
        })
    }

    /// The `float` side of an element (`left`/`right`), or `None`. The
    /// css-logical-1 flow-relative values map by the writing direction —
    /// LTR-only here, so `inline-start` = left and `inline-end` = right
    /// (dom.rs maps logical property NAMES under the same rule).
    fn float_side(&self, id: NodeId) -> Option<FloatSide> {
        match self
            .dom
            .computed_style(id, "float")?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "left" | "inline-start" => Some(FloatSide::Left),
            "right" | "inline-end" => Some(FloatSide::Right),
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
            // An empty float takes no shelf — but a pinned fixed overlay
            // captured inside it must still reach the document layer.
            for f in &boxed.fixed {
                let mut f = f.clone();
                f.col += self.line_left as u16;
                f.row += self.rows.len() as u16;
                self.fixed.push(f);
            }
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
        // css-logical-1 flow-relative values map by direction (LTR-only):
        // `inline-start` = left, `inline-end` = right — same as `float_side`.
        let (l, r) = match sides.as_str() {
            "left" | "inline-start" => (true, false),
            "right" | "inline-end" => (false, true),
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

        // A laid flex line, held until every line is formed so `wrap-reverse`
        // can stack the lines bottom-to-top (Flexbox §5.2: cross-start and
        // cross-end swap) by reversing the shelf order before placement.
        struct Shelf {
            boxes: Vec<LaidBox>,
            widths: Vec<usize>,
            lead: usize,
            between: usize,
            h: usize,
        }
        let main_rev = self.flex_main_reversed(id);
        let mut shelves: Vec<Shelf> = Vec::new();
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
            // Lay each item at its used main size; the line is placed below,
            // honoring `justify-content` (leftover main-axis space) and
            // `align-items` (cross-axis offset within the line's height).
            let mut boxes: Vec<LaidBox> = (0..n)
                .map(|k| self.layout_subtree(line[k].node, widths[k].max(1), ctx))
                .collect();
            let shelf_h = boxes.iter().map(|b| b.height as usize).max().unwrap_or(0);
            let used_w: usize = widths.iter().map(|w| (*w).max(1)).sum::<usize>() + gaps;
            let (lead, between) =
                self.justify_offsets(id, avail.saturating_sub(used_w), n, main_rev);
            // `row-reverse` (§5.1): the items of each LINE render in reverse
            // order — line composition keeps order-modified document order,
            // only the direction along the line flips.
            if main_rev {
                boxes.reverse();
                widths.reverse();
            }
            shelves.push(Shelf {
                boxes,
                widths,
                lead,
                between,
                h: shelf_h,
            });
            i = end;
        }
        // `wrap-reverse` (§5.2): the lines stack bottom-to-top.
        if self.flex_cross_reversed(id) {
            shelves.reverse();
        }
        let mut shelf_top = self.rows.len();
        for s in &shelves {
            let n = s.boxes.len();
            let mut x = s.lead;
            for (k, b) in s.boxes.iter().enumerate() {
                if b.height > 0 {
                    // `b.root` is the flex item this box was laid for (set by
                    // `layout_subtree_inner`), so its own `align-self` applies.
                    let dy = self.align_offset(id, b.root.unwrap_or(id), b.height as usize, s.h);
                    self.blit(b, (self.line_left + x) as u16, shelf_top + dy);
                } else if !b.fixed.is_empty() || !b.positioned.is_empty() {
                    // 0-height item carrying a pinned fixed overlay or
                    // collected out-of-flow boxes: blit adds no rows but
                    // propagates the side channels at the item's slot.
                    self.blit(b, (self.line_left + x) as u16, shelf_top);
                }
                x += s.widths[k].max(1) + if k + 1 < n { gap + s.between } else { 0 };
            }
            shelf_top += s.h + row_gap;
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
                .and_then(|v| resolve_cells(&v, avail, (me.viewport_w, me.viewport_h), me.units(c)))
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
                // A 0-height box still blits when it carries a pinned fixed
                // overlay or collected out-of-flow boxes (blit adds no rows,
                // propagates the side channels).
                if b.height > 0 || !b.fixed.is_empty() || !b.positioned.is_empty() {
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
        let Some(specs) =
            self.parse_track_list(&template, avail as f32, col_gap as f32, self.units(id))
        else {
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
            // Content is measured for intrinsic tracks — and, while MEASURING,
            // for flexible ones too (they size to content in intrinsic sizing,
            // so their content must be known — see `size_grid_tracks`).
            if pl.col_span == 1
                && pl.col < ncols
                && (self.track_is_intrinsic(&specs[pl.col])
                    || (self.measuring && self.track_has_fr(&specs[pl.col])))
            {
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
            // Keep a 0-height item that carries a pinned fixed overlay or
            // collected out-of-flow boxes: its blit below adds no rows but
            // propagates the side channels at its cell.
            if b.height == 0 && b.fixed.is_empty() && b.positioned.is_empty() {
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

        // Caption boxes (CSS 2.1 §17.4): a `table-caption` child renders as an
        // ordinary block ABOVE the table's grid — or below it for
        // `caption-side:bottom` — never as grid content (`table_cell_rows`
        // skips it, which used to drop the caption entirely: Wikipedia
        // infoboxes lost their titles).
        let (top_caps, bottom_caps): (Vec<NodeId>, Vec<NodeId>) = self
            .dom
            .children(id)
            .into_iter()
            .filter(|&c| self.dom.effective_display(c).as_deref() == Some("table-caption"))
            .partition(|&c| {
                self.dom.computed_style(c, "caption-side").as_deref() != Some("bottom")
            });
        self.flow_captions(&top_caps, ctx);

        let band = self.line_right.saturating_sub(self.line_left).max(1);
        let rows = self.table_cell_rows(id);
        let (cells, ncols) = build_table_grid(self, &rows);
        if cells.is_empty() || ncols == 0 {
            self.flow_captions(&bottom_caps, ctx);
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
            let has_css_pad = [
                "padding",
                "padding-left",
                "padding-right",
                "padding-top",
                "padding-bottom",
            ]
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
            self.flow_captions(&bottom_caps, ctx);
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
        self.flow_captions(&bottom_caps, ctx);
        if self.gap_after(id, "table") {
            self.push_blank();
        }
    }

    /// Flow a table's caption children as ordinary blocks (CSS 2.1 §17.4 — the
    /// caption box, above or below the grid per `caption-side`). The UA sheet
    /// centers captions (`caption { text-align: center }`); we apply that only
    /// when the cascade resolves no `text-align` for the caption — an author
    /// value (set on it or inherited) wins inside `flow_element` regardless,
    /// so centering against a resolvable value would be overwritten anyway.
    fn flow_captions(&mut self, caps: &[NodeId], ctx: &Ctx) {
        for &c in caps {
            let saved = self.align;
            if self
                .dom
                .computed_value(c, "text-align")
                .as_deref()
                .and_then(Align::from_css)
                .is_none()
            {
                self.align = Align::Center;
            }
            self.flow_node(c, ctx);
            self.flush_block();
            self.align = saved;
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

    /// Per-column width preferences from the table's `<col>`/`<colgroup>`
    /// elements (CSS 2.1 §17.5.2: a column element's non-auto `width` sets
    /// the column in fixed layout and its target in auto layout — both read
    /// `col_w`). `<col span=N>` repeats its width over N columns; a CHILDLESS
    /// `<colgroup span=N width=…>` acts as N such columns, while one with
    /// `<col>` children defers to them (HTML §4.9.3/§4.9.4). May be shorter
    /// or longer than the cell-derived column count — the caller indexes with
    /// `.get()`. Tag-matched (`<col>` is table-only markup); arbitrary
    /// `display:table-column` elements aren't recognized.
    fn table_col_specs(&self, table: NodeId) -> Vec<Option<TrackWidth>> {
        let mut specs = Vec::new();
        let push_cols = |el: NodeId, specs: &mut Vec<Option<TrackWidth>>| {
            let w = self.declared_track_width(el);
            for _ in 0..self.cell_span(el, "span") {
                specs.push(w);
            }
        };
        for child in self.dom.children(table) {
            match self.dom.tag_name(child) {
                Some("colgroup") => {
                    let cols: Vec<NodeId> = self
                        .dom
                        .children(child)
                        .into_iter()
                        .filter(|&c| self.dom.tag_name(c) == Some("col"))
                        .collect();
                    if cols.is_empty() {
                        push_cols(child, &mut specs);
                    } else {
                        for col in cols {
                            push_cols(col, &mut specs);
                        }
                    }
                }
                Some("col") => push_cols(child, &mut specs),
                _ => {}
            }
        }
        specs
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
        let u = self.units(id);
        if let Some(px) = css_length_px(raw, u) {
            return Some(TrackWidth::Px((px / u.cell_w).round().max(1.0) as usize));
        }
        raw.parse::<f32>()
            .ok()
            .filter(|n| *n > 0.0)
            .map(|n| TrackWidth::Px((n / u.cell_w).round().max(1.0) as usize))
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
        let u = self.units(table);
        css_length_px(first, u)
            .map(|px| (px / u.cell_w).round() as usize)
            .or_else(|| {
                first
                    .parse::<f32>()
                    .ok()
                    .map(|n| (n / u.cell_w).round() as usize)
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

    /// Vertical offset of a cell within its (possibly taller) row band, per
    /// CSS 2.1 §17.5.4 + the HTML rendering spec (§15.3.3/15.3.9): at each
    /// element, author `vertical-align` BEATS the `valign` presentational
    /// hint (hints are author-level rules that precede all other author
    /// rules); an undeclared cell inherits through its row and row group (the
    /// UA sheet's `td,th,tr { vertical-align: inherit }` + `thead,tbody,tfoot
    /// { vertical-align: middle }`), so a bare cell defaults to MIDDLE — the
    /// classic centered table cell, matching browsers. `baseline` (and the
    /// inline-only values, which §17.5.4 says are treated as `baseline` on
    /// cells) is approximated as top in the cell line model.
    fn cell_valign_offset(&self, cell: Option<NodeId>, cell_h: usize, span_h: usize) -> usize {
        let slack = span_h.saturating_sub(cell_h);
        if slack == 0 {
            return 0;
        }
        let Some(id) = cell else { return 0 };
        let mut v = None;
        let mut cur = Some(id);
        while let Some(n) = cur {
            v = self
                .dom
                .computed_style(n, "vertical-align")
                .or_else(|| self.dom.attr(n, "valign").map(str::to_owned))
                .map(|s| s.trim().to_ascii_lowercase());
            if v.is_some() {
                break;
            }
            // Climb cell → row → row group only; anything above doesn't
            // participate in the cell alignment chain.
            cur = self.dom.parent_composed(n).filter(|&p| {
                matches!(
                    self.dom.tag_name(p),
                    Some("tr" | "tbody" | "thead" | "tfoot")
                )
            });
        }
        match v.as_deref() {
            Some("bottom") => slack,
            Some("top" | "baseline") => 0,
            Some("middle") | None => slack / 2,
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

        // The per-column explicit width preference: a `<col>`/`<colgroup>`
        // element's declared width first (§17.5.2.1 lists column elements
        // ahead of first-row cells), else a declared `width` on the column's
        // first single-span cell. Always cheap to gather.
        let col_specs = self.table_col_specs(table);
        let mut col_w: Vec<Option<TrackWidth>> = (0..ncols)
            .map(|c| col_specs.get(c).copied().flatten())
            .collect();
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
        // A declared column width RAISES the column's max-content (§17.5.2.2:
        // a cell `width` greater than the minimum becomes the column's
        // maximum) — without this a `width:80px` column on a width-LESS table
        // collapsed toward its content (`used` never counted the declaration,
        // so the shrink pass took it back). Percentages can't floor here:
        // they resolve against the used table width, which this very sum
        // determines.
        for c in 0..ncols {
            if let Some(TrackWidth::Px(px)) = col_w[c] {
                col_max[c] = col_max[c].max(px.min(avail));
            }
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
    fn parse_track_list(
        &self,
        value: &str,
        avail: f32,
        gap: f32,
        u: Units,
    ) -> Option<Vec<TrackSpec>> {
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
                let list = self.parse_track_list(list_s, avail, gap, u)?;
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
            } else if let Some(spec) = self.parse_one_track(&tok, avail, u) {
                tracks.push(spec);
            } else {
                return None;
            }
        }
        (!tracks.is_empty()).then_some(tracks)
    }

    /// Parse a single track sizing function (`auto`, `Nfr`, a length,
    /// `minmax()`, `fit-content()`, `min/max-content`). `None` if unparseable.
    fn parse_one_track(&self, tok: &str, avail: f32, u: Units) -> Option<TrackSpec> {
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
            let min = self.parse_one_track(a, avail, u)?;
            let max = self.parse_one_track(b, avail, u)?;
            return Some(TrackSpec::Minmax(Box::new(min), Box::new(max)));
        }
        if let Some(inner) = t
            .strip_prefix("fit-content(")
            .and_then(|r| r.strip_suffix(')'))
        {
            let cap = resolve_cells_f32(
                inner.trim(),
                avail as usize,
                (self.viewport_w, self.viewport_h),
                u,
            )?;
            return Some(TrackSpec::FitContent(cap.max(0.0)));
        }
        if let Some(num) = t.strip_suffix("fr") {
            let f: f32 = num.trim().parse().ok()?;
            return Some(TrackSpec::Fr(f.max(0.0)));
        }
        resolve_cells_f32(t, avail as usize, (self.viewport_w, self.viewport_h), u)
            .map(|c| TrackSpec::Fixed(c.max(0.0)))
    }

    /// Whether a track sizes to content (so the content-width pass must measure
    /// its items): `auto`/`min|max-content`/`fit-content`, or a `minmax()` with
    /// an intrinsic bound.
    /// Whether the track's max sizing function is flexible (`fr`). During
    /// INTRINSIC sizing these size to their content (CSS Grid §7.2.3), not
    /// to a share of the measurement probe.
    fn track_has_fr(&self, spec: &TrackSpec) -> bool {
        match spec {
            TrackSpec::Fr(_) => true,
            TrackSpec::Minmax(_, max) => matches!(max.as_ref(), TrackSpec::Fr(_)),
            _ => false,
        }
    }

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
        if fr_sum > 0.0 {
            for i in 0..n {
                if fr[i] <= 0.0 {
                    continue;
                }
                if self.measuring {
                    // Intrinsic sizing (CSS Grid §7.2.3): with indefinite
                    // available space a flexible track sizes to its CONTENT,
                    // not to a share of the measurement probe — else any
                    // `1fr` grid measures as the whole probe width and a
                    // fit-content ancestor (Steam's `align-items:center`ed
                    // login card) can never shrink-wrap it.
                    size[i] = size[i].max(content_w[i]);
                } else if free > 0.0 {
                    size[i] += free * fr[i] / fr_sum;
                }
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
        // and its margin is suppressed (we applied it out here). `make_sub`
        // carries the pass-wide state — notably `table_depth` (the
        // MAX_TABLE_DEPTH lid must survive a bordered box between table
        // levels) and `measuring`/`shrink_wrap`/`modal_root`/
        // `capture_boundaries`, which a hand-built sub silently dropped.
        let mut sub = self.make_sub(inner_w);
        sub.inner_border_box = Some(id);
        // One bordered box deeper — the MAX_BORDER_DEPTH lid's counter
        // (`make_sub` copied the current depth; every other sub-layout kind
        // inherits it unchanged).
        sub.border_depth = self.border_depth + 1;
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
        let (
            rows,
            carousels,
            fixed,
            regions,
            element_tops,
            scroll_clips,
            boundary_boxes,
            positioned,
        ) = sub.finish();
        let content = LaidBox {
            height: rows.len() as u16,
            width: inner_w as u16,
            rows,
            carousels,
            fixed,
            regions,
            scroll_clips,
            element_tops,
            boundary_boxes,
            root: Some(id),
            lay_width: inner_w as u16,
            positioned,
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
                        if it.text.is_empty() {
                            // An image box: clip the reserved box at the bar.
                            it.width = max_w;
                        } else {
                            // Truncate by DISPLAY width (the binding width
                            // rule) so clipped CJK/emoji can't paint through
                            // the right border bar.
                            it.text = truncate_to_width(&it.text, max_w as usize);
                            it.width = display_width(&it.text) as u16;
                        }
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
            pixelated: false,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
            // Structural box-drawing chrome (borders default-off); painted.
            invisible: false,
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
        // recorded empty-element positions — and any scroll regions inside —
        // move with it.
        let mut regions = content.regions;
        for rg in &mut regions {
            rg.start_row += row_shift;
            rg.left += col_shift;
        }
        let mut fixed = content.fixed;
        for f in &mut fixed {
            f.col += col_shift;
            f.row += row_shift as u16;
        }
        let mut positioned = content.positioned;
        for p in &mut positioned {
            p.col += i32::from(col_shift);
            p.row += row_shift;
        }
        let element_tops = content
            .element_tops
            .into_iter()
            .map(|(id, (col, row))| (id, (col + col_shift, row + row_shift as u16)))
            .collect();
        let boundary_boxes = content
            .boundary_boxes
            .into_iter()
            .map(|(id, rec)| {
                (
                    id,
                    BoundaryRec {
                        origin_col: rec.origin_col + col_shift,
                        start_row: rec.start_row + row_shift,
                        end_row: rec.end_row + row_shift,
                        ..rec
                    },
                )
            })
            .collect();
        LaidBox {
            rows,
            width: new_w as u16,
            height: new_h as u16,
            carousels,
            fixed,
            regions,
            // Clip boxes are position-independent — the frame shift doesn't
            // touch them.
            scroll_clips: content.scroll_clips,
            element_tops,
            boundary_boxes,
            // The frame wraps the SAME element — keep its root, but the framed
            // box's used width is `new_w` (interior + borders).
            root: content.root,
            lay_width: new_w as u16,
            positioned,
        }
    }

    /// A CSS length property in terminal cells (absolute units through the
    /// element's font context and the real cell box — see `Units`), resolving
    /// `%`/`vw`/`calc()` against the current band width and the viewport. `None` when unset (or `auto`/an unsupported unit). Clamped
    /// to ≥1 cell — DELIBERATE: a terminal can't paint a sub-cell sliver, so
    /// a tiny-but-nonzero length rounds up to the one cell that can show it,
    /// and a literal `width:0` box is almost always display-hidden by dom's
    /// zero-size rules before this is consulted. The residue (`width:0;
    /// overflow:visible`, whose content should overflow a zero-width box)
    /// keeps a 1-cell band as the documented approximation — modelling
    /// visible overflow out of a zero box isn't worth a band model change.
    fn css_cells(&self, id: NodeId, prop: &str) -> Option<usize> {
        let v = self.dom.computed_style(id, prop)?;
        let avail = self.width.saturating_sub(self.indent).max(1);
        resolve_cells_f32(
            &v,
            avail,
            (self.viewport_w, self.viewport_h),
            self.units(id),
        )
        .map(|c| c.round().max(1.0) as usize)
    }

    /// Like `css_cells`, but FLOORS the result. For a grid column width, `N`
    /// rounded `(100/N)%` widths can sum past the row and drop the last column;
    /// flooring keeps every column (each loses ≤1 cell, absorbed by the slack).
    fn css_cells_floor(&self, id: NodeId, prop: &str) -> Option<usize> {
        let v = self.dom.computed_style(id, prop)?;
        let avail = self.width.saturating_sub(self.indent).max(1);
        resolve_cells_f32(
            &v,
            avail,
            (self.viewport_w, self.viewport_h),
            self.units(id),
        )
        .map(|c| c.floor().max(1.0) as usize)
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
        // `false` exactly as before. (The table path at `flow_table` propagates
        // the flag the same way.) Without the inheritance, a flex row nested
        // BELOW a plain block child ran its `justify-content` during the
        // ancestor's measurement — Steam's right-aligned price cell made its
        // whole `flex-shrink:0` column measure ~the full row, which then
        // shrank the title column to one cell ("…").
        self.layout_subtree_inner(id, content_width, None, self.measuring, inherit)
    }

    /// `layout_subtree`, optionally ignoring the float on the root element
    /// (used when laying a float's own box so it doesn't recurse). `measure`
    /// means this is an intrinsic-width measurement (ignore `text-align`).
    /// `inherit` seeds the sub-layout's root context so a child laid in a
    /// separate pass (a flex/grid item) still inherits an enclosing `<a>`'s
    /// link (and emphasis) — otherwise its contents would lose interactivity,
    /// exactly as a bordered box would without `flow_bordered` threading `ctx`.
    /// A fresh sub-layout carrying every pass-wide flag, shared cache, and
    /// recursion guard a nested pass must inherit. ALL sub-layout construction
    /// funnels through here (`layout_subtree_inner`, `flow_bordered`) so a
    /// hand-built sub can't silently drop pass state again — dropping
    /// `table_depth` reset the `MAX_TABLE_DEPTH` recursion lid across a
    /// bordered box (hostile deep table/border nesting could then overflow the
    /// stack), and dropping `measuring` re-applied alignment offsets (and
    /// re-entered the expensive table column algorithm) inside measure passes.
    /// Callers set only their own entry state on top (`subtree_root` /
    /// `float_skip` / `inner_border_box` / a `measuring` override).
    fn make_sub(&self, content_width: usize) -> Layout<'a> {
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
        sub.viewport_h = self.viewport_h;
        sub.table_depth = self.table_depth;
        // Share the pass-wide intrinsic-width memo so a subtree measured by an
        // ancestor's sizing pass isn't re-measured when this pass measures it.
        sub.measure_cache = self.measure_cache.clone();
        // Share the region child-row cache so the children flowed deep inside a
        // region fragment's sub-layout reuse/record against the same map.
        sub.region_child_cache = self.region_child_cache.clone();
        // A shrink-to-fit box's replaced descendants (the image can be nested in
        // flex/grid sub-layouts) must see the shrink-wrap context too.
        sub.shrink_wrap = self.shrink_wrap;
        sub.measuring = self.measuring;
        // Carry the measurement flag so a sub-layout tags its items with their
        // own nodes and records empty-element flow positions (`element_tops`);
        // `blit` then propagates that geometry back up. The render path keeps
        // this off (no tagging), so it's unaffected.
        sub.tag_all_nodes = self.tag_all_nodes;
        // A box laid within a surfaced modal must know it (so its full-bleed
        // foreground image isn't dropped as a page backdrop, and the modal-root
        // out-of-flow exemption holds in the sub-pass).
        sub.modal_root = self.modal_root;
        // Carry the scroll-region recursion guard so a sub-layout laying a
        // region's own buffer (or a descendant of it) doesn't re-enter
        // `flow_region` on the SAME node (`flow_region` sets `region_inner` on
        // self before laying the buffer; the guard is per-node, so a DIFFERENT
        // nested region still forms).
        sub.region_inner = self.region_inner;
        // Capture incremental-layout boundaries inside sub-layouts too (a
        // chat column lives in an abspos shell's sub-pass), so `blit` propagates
        // them up. Off when this pass isn't capturing (measure / non-live).
        sub.capture_boundaries = self.capture_boundaries;
        // The px→cells conversion must use the same cell box everywhere
        // in a pass (`measure_boxes` sets an explicit one; elsewhere this is
        // the session global either way).
        sub.cell_px_w = self.cell_px_w;
        sub.cell_px_h = self.cell_px_h;
        // The bordered-nesting depth rides every sub-layout (the lid counts
        // bordered ANCESTRY, whatever formatting contexts sit between);
        // `flow_bordered` increments it on top.
        sub.border_depth = self.border_depth;
        // Share the geometry maps (declared floors + clip caps): their values
        // are position-independent sizes, so a box recorded deep inside a
        // flex/grid/bordered sub-layout reaches `measure_boxes` directly.
        sub.declared_boxes = self.declared_boxes.clone();
        sub.clip_heights = self.clip_heights.clone();
        sub
    }

    fn layout_subtree_inner(
        &self,
        id: NodeId,
        content_width: usize,
        skip_float: Option<NodeId>,
        measure: bool,
        inherit: &Ctx,
    ) -> LaidBox {
        let mut sub = self.make_sub(content_width);
        sub.float_skip = skip_float;
        sub.subtree_root = Some(id);
        sub.measuring = measure;
        sub.flow_node(id, inherit);
        sub.flush_block();
        sub.finish_floats();
        // A measured flex row's max-content width is the sum of its items' base
        // sizes, even past its last painted cell (§9.9.1) — floor the reported
        // box width at it. Recorded only under `measuring` (see `flow_flex_row`),
        // so render-pass boxes keep reporting their painted extent. Read before
        // `finish` consumes `sub`.
        let flex_min = sub.flex_min_width as u16;
        let (
            rows,
            carousels,
            fixed,
            regions,
            element_tops,
            scroll_clips,
            boundary_boxes,
            positioned,
        ) = sub.finish();
        let width = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|it| it.col + it.width)
            .max()
            .unwrap_or(0)
            .max(flex_min);
        let height = rows.len() as u16;
        LaidBox {
            rows,
            width,
            height,
            carousels,
            fixed,
            regions,
            scroll_clips,
            element_tops,
            boundary_boxes,
            root: Some(id),
            lay_width: content_width.min(u16::MAX as usize) as u16,
            positioned,
        }
    }

    /// Copy a laid-out box into the parent's rows, shifting every item's
    /// `col` by `col_off` and placing box row `r` into parent row
    /// `row_base + r` (creating parent rows as needed). The 2D placement
    /// primitive — items keep their node/link so selection re-anchors and
    /// vertical scroll still index by the parent row grid.
    ///
    /// PERF (measured 2026-07-04, don't redo): a consuming `blit(LaidBox)`
    /// that MOVES the items instead of cloning them was built and REJECTED —
    /// it was ~26% SLOWER on the deep-flex shape it targeted
    /// (`blit_clone_bench`: clone ~39.6ms, move ~50ms, stable across runs).
    /// The clone at each level re-allocates the surviving strings
    /// back-to-back at blit time — contiguous for the later `finish` walk
    /// and drop — while moved strings stay scattered from leaf-layout time;
    /// the locality is worth more than the copies (the same lesson as the
    /// mimalloc/GC findings). Keep the borrowing clone.
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
        // Scroll regions inside the box move with it too: the reserved band by
        // `row_base`/`col_off`. The buffer is laid in the region's own
        // coordinate space (top-left origin), so it's position-independent and
        // travels unchanged.
        for rg in &b.regions {
            let mut rg = rg.clone();
            rg.start_row += row_base;
            rg.left += col_off;
            self.regions.push(rg);
        }
        // Clip boxes (`Doc.scroll_clips`) carry no coordinates — `(live_node,
        // rows, cells)` — so they propagate verbatim; the app pushes a live
        // page's `clientHeight` from exactly this list, and dropping them here
        // silently lost every scroll box nested in a sub-layout.
        self.scroll_clips.extend(b.scroll_clips.iter().copied());
        // Pinned `position:fixed` boxes captured inside travel with the box: their
        // captured position is the box-relative flow position (their static
        // origin), so it shifts by `col_off`/`row_base` up toward the document's
        // coordinate space, exactly like a scroll region's reserved band.
        for f in &b.fixed {
            let mut f = f.clone();
            f.col += col_off;
            f.row += row_base as u16;
            self.fixed.push(f);
        }
        // Out-of-flow boxes collected inside travel with the box: their placement
        // coordinate is box-relative, so it shifts by `col_off`/`row_base` up
        // toward the document, exactly like a pinned fixed box — and, like every
        // other side channel, it never touches `self.rows`, so blitting this box
        // does NOT let its out-of-flow descendants inflate the parent's flow
        // (CSS 2.1 §9.3.1). Only the document root composites them.
        for p in &b.positioned {
            let mut p = p.clone();
            p.col += i32::from(col_off);
            p.row += row_base;
            self.positioned.push(p);
        }
        // Empty elements recorded in the box (measure pass only) move with it
        // too, so a boxless element nested in this sub-layout keeps its honest
        // flow position in the document's coordinate system.
        for (&id, &(col, row)) in &b.element_tops {
            self.element_tops
                .entry(id)
                .or_insert((col + col_off, row + row_base as u16));
        }
        // Incremental-layout boundaries recorded inside the box (capture pass)
        // move with it: the band left by `col_off`, the row span by `row_base`
        // (the content band width is position-independent).
        for (&id, rec) in &b.boundary_boxes {
            self.boundary_boxes.entry(id).or_insert(BoundaryRec {
                origin_col: rec.origin_col + col_off,
                start_row: rec.start_row + row_base,
                end_row: rec.end_row + row_base,
                ..*rec
            });
        }
        // The box ITSELF is an incremental-layout SUB-BOX boundary when it's a
        // flex/grid item or inline-block cell carrying a baked `data-trust-node`
        // (INCREMENTAL_LAYOUT_PLAN.md §14 — the widening that lets a styled-
        // components flex item / animated counter patch instead of forcing a full
        // render). Recorded at its blit position; the harvest's owns-rows check +
        // the patch's width verify keep only the safe ones.
        if self.capture_boundaries
            && b.height > 0
            && let Some(root) = b.root
            && self.is_sub_box_boundary(root)
            && let Some(node) = self
                .dom
                .attr(root, "data-trust-node")
                .and_then(|s| s.parse::<usize>().ok())
        {
            self.boundary_boxes.entry(root).or_insert(BoundaryRec {
                node,
                content_width: b.lay_width,
                width: b.width,
                origin_col: col_off,
                start_row: row_base,
                end_row: row_base + b.height as usize,
                sub_box: true,
            });
        }
    }

    /// Flow a run of inline text under the active `white-space` mode.
    fn place_text(&mut self, text: &str, ctx: &Ctx) {
        // Paint suppression rides the context: this text's items are tagged
        // `invisible` (painted blank, space reserved) when it flows under an
        // `opacity:0` subtree. Set from the context so pseudo/child/alt text all
        // pick up the right value at their own call site.
        self.invisible = ctx.invisible;
        if text.is_empty() || ctx.font_zero {
            // `font-size:0` renders zero-width glyphs — the text is present in
            // the DOM (copyable) but paints nothing, and its whitespace
            // collapses too, so emit no items and owe no space.
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
    /// (the wrap is gated in `place_word`). Only CSS document white space
    /// (`is_collapsible_space`) collapses or separates words — U+00A0 stays
    /// INSIDE its word as non-breaking glue, per CSS Text 3 §4.1.1.
    fn place_collapsed(&mut self, text: &str, ctx: &Ctx) {
        if text.is_empty() {
            return;
        }
        let leading = text.starts_with(is_collapsible_space);
        let trailing = text.ends_with(is_collapsible_space);
        let mut any = false;
        if leading {
            self.pending_space = true;
        }
        for word in text.split(is_collapsible_space).filter(|w| !w.is_empty()) {
            // Inter-word whitespace within the node collapses to one
            // space; `pending_space` carries it (and any space owed
            // across a node boundary) into the placement.
            if any {
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
        // U+00AD SOFT HYPHEN never renders a cell of its own (CSS Text 3
        // §6.2; unicode-width counts it as 1, so left in the text it painted
        // a stray glyph). Words carrying one take the soft-wrap-opportunity
        // path; it feeds back SHY-free strings, so no recursion.
        if word.contains('\u{AD}') {
            self.place_shy_word(word, ctx);
            return;
        }
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
                    // No cell left for this word — but content IS being
                    // omitted, and §5.1's ellipsis renders even then, by
                    // REMOVING already-placed characters to make room:
                    // replace the line's last cell with `…` (the previous
                    // word may have ended exactly at the box edge).
                    if self.clip_ellipsis
                        && let Some(last) = self.line.last_mut()
                        && !last.text.is_empty()
                        && last.image.is_none()
                    {
                        let mut t =
                            truncate_to_width(&last.text, (last.width as usize).saturating_sub(1));
                        t.push('…');
                        last.width = display_width(&t) as u16;
                        last.text = t;
                    }
                    self.pending_space = false;
                    return;
                }
                let t = if self.clip_ellipsis {
                    // Leave one cell for the ellipsis (drop it only if the
                    // whole box is a single cell wide).
                    let mut t = truncate_to_width(&text, room.saturating_sub(1));
                    t.push('…');
                    t
                } else {
                    // `text-overflow: clip` (the initial value): cut at the
                    // box edge without a marker.
                    truncate_to_width(&text, room)
                };
                if t.is_empty() {
                    self.pending_space = false;
                    return;
                }
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
        // An unbreakable token wider than the whole band can't fit even on a
        // fresh line (a poll's concatenated usernames, a long URL/emote name).
        // Character-break it across rows instead of letting it overflow off the
        // right edge — the terminal has no horizontal scroll, so the overflow
        // would be lost. This is CSS `overflow-wrap:break-word`: we break at
        // RENDER to avoid overflow but DON'T break while measuring intrinsic
        // width, so the box is still sized to fit its longest word (min-content
        // stays the word, not 1 char). Breaking during measurement would be
        // `overflow-wrap:anywhere` — it would collapse a content-sized flex item
        // / table column to one cell and then char-break its short content.
        if self.ws.wraps()
            && !self.measuring
            && wlen > self.line_right.saturating_sub(self.line_left).max(1)
        {
            self.place_oversize(word, ctx.kind, ctx.emph, ctx.node, ctx.link.clone());
            return;
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

    /// Place a word containing U+00AD SOFT HYPHEN — CSS Text 3 §6.2 / UAX #14:
    /// each SHY is an INVISIBLE soft wrap opportunity. Not taken, it renders
    /// nothing (zero cells); taken, a visible hyphen ends the line. The break
    /// chosen is the LATEST opportunity that fits (browsers fill the current
    /// line before pushing to the next). Fragments split AFTER text-transform
    /// (transforms map SHY to itself, and re-applying a transform to a
    /// fragment is idempotent), and widths measured post-transform/
    /// letter-spacing so the fit test matches what `place_word` places.
    /// Deliberate simplifications: while MEASURING, opportunities are not
    /// taken (the word measures whole, consistent with the overflow-wrap
    /// gate below — CSS would break min-content at every SHY), and inside
    /// the nowrap-ellipsis clip the stripped word takes the normal
    /// truncation path.
    fn place_shy_word(&mut self, word: &str, ctx: &Ctx) {
        let transformed = ctx.transform.apply(word);
        let frags: Vec<&str> = transformed.split('\u{AD}').collect();
        let width_of = |s: &str| -> usize {
            let t = ctx.transform.apply(s);
            display_width(letter_space(t.as_ref(), ctx.letter_spacing).as_ref())
        };
        // Longest prefix of `frags[i..]` that fits `cap` cells with its
        // trailing hyphen; `None` when not even the first fragment does.
        let pick = |i: usize, cap: usize| -> Option<usize> {
            let mut best = None;
            for k in (i + 1)..frags.len() {
                let piece = frags[i..k].concat();
                if piece.is_empty() {
                    continue;
                }
                if width_of(&(piece + "-")) <= cap {
                    best = Some(k);
                } else {
                    break;
                }
            }
            best
        };
        let mut i = 0;
        loop {
            let rest = frags[i..].concat();
            let space = usize::from(self.pending_space && self.col > self.line_left);
            // Hand the remainder to the normal path — already SHY-free — when
            // no opportunity is left, the context can't take one (nowrap /
            // the ellipsis clip / measuring), or it simply fits as is.
            if i + 1 >= frags.len()
                || !self.ws.wraps()
                || self.measuring
                || self.clip_right.is_some()
                || self.col + space + width_of(&rest) <= self.line_right
            {
                self.place_word(&rest, ctx);
                return;
            }
            match pick(i, self.line_right.saturating_sub(self.col + space)) {
                Some(k) => {
                    // Fill the current line with the longest fitting prefix,
                    // hyphenated, and continue past the break.
                    let piece = format!("{}-", frags[i..k].concat());
                    self.place_word(&piece, ctx);
                    self.break_line();
                    i = k;
                }
                None if self.col > self.line_left => {
                    // Nothing fits beside the existing content: break first;
                    // the next pass re-tests against the fresh band.
                    self.break_line();
                    self.pending_space = false;
                }
                None => {
                    // Not even the first fragment + hyphen fits a whole band:
                    // the oversize char-breaker takes the remainder from here.
                    self.place_word(&rest, ctx);
                    return;
                }
            }
        }
    }

    /// Place a run whose display width exceeds the band by character-breaking it
    /// across rows — CSS Text L3 §5.4 (`overflow-wrap:anywhere`/`word-break:
    /// break-all`), applied unconditionally because the terminal has no
    /// horizontal scroll (a documented deviation): a token wider than the band
    /// can't be revealed by overflow, so it must wrap or be lost off-screen, and
    /// breaking it keeps a content-sized ancestor from stretching to its width.
    /// Flows from the current column, wraps at `line_right`, continues at
    /// `line_left`; each row is its own item but only the FIRST carries the link
    /// (one selection stop, like a wrapped link). Display-width aware, so a wide
    /// glyph (CJK/emoji) never straddles the edge.
    fn place_oversize(
        &mut self,
        text: &str,
        kind: ItemKind,
        emph: Emphasis,
        node: NodeId,
        link: Option<Link>,
    ) {
        let mut link = link;
        let mut buf = String::new();
        let mut bw = 0usize;
        for ch in text.chars() {
            let gw = display_width(ch.encode_utf8(&mut [0u8; 4]));
            // Wrap before a glyph that would cross the right edge, but never make
            // an empty line (only break once the current line holds content).
            if gw > 0 && self.col + bw + gw > self.line_right && self.col + bw > self.line_left {
                if bw > 0 {
                    self.push_item(std::mem::take(&mut buf), bw, kind, emph, node, link.take());
                    bw = 0;
                }
                self.break_line();
            }
            buf.push(ch);
            bw += gw;
        }
        if !buf.is_empty() {
            self.push_item(buf, bw, kind, emph, node, link);
        }
    }

    /// Place a newline-free segment with its spaces preserved. `pre`
    /// emits it as one unwrapped item; `pre-wrap` breaks it into
    /// width-fitting chunks (spaces kept). Uses `ctx.kind`, so CSS
    /// `white-space:pre` on a non-`<pre>` element keeps its own styling.
    fn place_preserved(&mut self, seg: &str, ctx: &Ctx) {
        // U+00AD SOFT HYPHEN never renders a cell (CSS Text 3 §6.2) — strip
        // it. No soft-wrap opportunities taken here: `pre` never breaks, and
        // the pre-wrap budget breaker keeps its char-level behavior (a
        // deliberate simplification).
        let seg: std::borrow::Cow<str> = if seg.contains('\u{AD}') {
            std::borrow::Cow::Owned(seg.chars().filter(|&c| c != '\u{AD}').collect())
        } else {
            std::borrow::Cow::Borrowed(seg)
        };
        let seg = seg.as_ref();
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
        // The width accumulates per glyph (`place_oversize`'s pattern) instead
        // of re-measuring the whole buffer per pushed char — the old
        // `display_width(&buf)` re-scan cost O(chunk width) per character,
        // an avoidable ×band constant on big pre-wrap blobs.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let mut buf = String::new();
        let mut bw = 0usize;
        let mut chars = seg.chars().peekable();
        while let Some(c) = chars.next() {
            bw += display_width(c.encode_utf8(&mut [0u8; 4]));
            buf.push(c);
            if bw >= avail && chars.peek().is_some() {
                self.push_preserved_item(&buf, bw, ctx);
                self.break_line();
                buf.clear();
                bw = 0;
            }
        }
        if !buf.is_empty() {
            self.push_preserved_item(&buf, bw, ctx);
        }
    }

    /// Push one preserved-whitespace run, inheriting the context's kind
    /// and emphasis.
    fn push_preserved_item(&mut self, text: &str, len: usize, ctx: &Ctx) {
        // Carry the link: a link inside `white-space:pre/nowrap/pre-wrap` (a
        // directory-listing cell, a `<pre>` full of anchors) is still followable.
        // Dropping it here left such anchors styled like links (kind=Link → cyan)
        // but inert — not selectable, not clickable. The collapsing path
        // (`place_word`) already threads `ctx.link`; match it.
        self.push_item(
            text.to_owned(),
            len,
            ctx.kind,
            ctx.emph,
            ctx.node,
            ctx.link.clone(),
        );
    }

    /// Render a `<video>`/`<audio>` element as a media representation: the
    /// video poster (when present and decoded) as a clickable thumbnail, plus
    /// a labelled link (`▶ Video · 720p HD` / `♪ Audio`). Both carry a link to
    /// the playable source, so following it auto-launches mpv (see
    /// `is_playable_media_url` in app.rs). Audio, or a poster-less / not-yet-
    /// decoded video, falls back to the link alone — fully general (not every
    /// embed has a preview frame).
    fn flow_media(&mut self, id: NodeId, tag: &str, ctx: &Ctx) {
        // Record that this Layout has represented `id`, so the coordinate model
        // (`place_positioned_children`) won't ALSO place it. An abspos `<video>`
        // is reachable two ways: the in-flow media-player-wrapper dispatch in
        // `flow_element` (here) renders it where its wrapper sits, and the
        // coordinate model places it at its computed box. They coincide only when
        // the wrapper IS the video's containing block (video.js — the wrapper
        // returns before its own block-tail, so the coordinate model never runs
        // for it); when the containing block is a HIGHER ancestor (Twitch's
        // `<div class="video-ref"><video abspos>`), both fire → a doubled preview
        // + "Watch in mpv". Everything between the media and its nearest positioned
        // ancestor is in-flow, so this dispatch and that ancestor's
        // `place_positioned_children` both run in the SAME Layout — this local
        // mark connects them (`place_positioned_children` skips a media node found
        // here). It is laid in-flow first (before the block-tail), so the in-flow
        // position wins.
        self.media_emitted.insert(id);
        // The URL handed to mpv on follow: the element's own inline source (a
        // direct file mpv plays), else the PAGE itself. A streaming player
        // (Twitch/YouTube/Kick/…) feeds its `<video>` from MSE/blob URLs with
        // no `src`/`<source>`, so there is nothing inline-playable — but yt-dlp
        // resolves the page URL for ~1800 sites, so a `<video>` ALWAYS yields a
        // "play in mpv" affordance. (`<audio>` with no source has no fallback —
        // the page URL isn't an audio stream — so it still represents nothing.)
        let (play_url, src_node, streaming) = match self.media_source(id) {
            Some((u, n)) => (url::Url::parse(&u).ok(), n, false),
            None if tag == "video" => {
                // A sourceless (MSE/blob) streaming video has nothing
                // inline-playable, so the playable target is a PAGE yt-dlp
                // resolves. When the video sits inside a hyperlink, the
                // anchor's target names the content the preview belongs to
                // (the card-embedded autoplay idiom — a front-page hero /
                // hover-preview card links to its watch page); a video with
                // no enclosing link IS this page's player, so the page we're
                // on is the target. Handing the card case the CURRENT page
                // launched mpv on a homepage — yt-dlp found no video there
                // and died silently, an mpv link that "never launches".
                let page = match &ctx.link {
                    Some(Link::Http(u)) => u.clone(),
                    _ => self.base.clone(),
                };
                (Some(page), None, true)
            }
            None => return, // poster-less audio with no source — nothing to show
        };
        let Some(play_url) = play_url else { return };
        // Whether the representation plays the page we are on — the gate for
        // borrowing this page's Open Graph image below (og:image describes
        // THIS page's media and nothing else; borrowing it for a preview that
        // plays a DIFFERENT page pasted the site's og logo onto a front-page
        // hero card as a phantom "video preview").
        let plays_this_page = streaming && play_url == *self.base;
        // A streaming video whose only playable target would be the CURRENT
        // page, on a page that does NOT declare itself a video page (the
        // standard Open Graph convention — see `page_declares_video`), has no
        // playable target at all: the homepage-autoplay hero (Twitch's
        // front-page carousel plays a featured channel inline). Linking the
        // homepage launched mpv into a silent yt-dlp failure ("mpv never
        // launches"), with the site LOGO (og:image) masquerading as a video
        // preview above it. No playable target ⇒ no LINK — but the frame can
        // still show the video's honest STAND-IN: the preview image its card
        // faded under the mounted player (`hidden_preview`, below). Without
        // one there is nothing to represent at all.
        let dead_end = plays_this_page && !page_declares_video(self.dom);
        // The preview image a page FADED under a mounted player — the video's
        // poster in all but name. In a browser the playing video covers it;
        // we deliberately render no player, so the video's representation
        // borrows the hidden image's URL (the image ITSELF stays hidden —
        // paint fidelity is untouched). Only a paint-SUPPRESSED image
        // qualifies: a visible sibling preview keeps flowing as normal
        // content (the hover-preview idiom — video overlaid ON content) and
        // must not double as a poster.
        let hidden_preview = (streaming && tag == "video")
            .then(|| self.hidden_preview_in_cb(id))
            .flatten();
        if dead_end && hidden_preview.is_none() {
            return;
        }
        // Paint suppression: a media representation inside an `opacity:0`/
        // `visibility:hidden` subtree reserves its box but paints blank.
        // `self.invisible` was set by `flow_element` for the wrapper before this
        // dispatch; OR-in the media element's own opacity + computed visibility,
        // then carry it onto `mctx` (the caption flows through `place_text`) and
        // onto the poster item below.
        self.invisible |= self.dom.paint_suppressed(id) || self.dom.visibility_hidden(id);
        // A suppressed OUT-OF-FLOW media element paints NOTHING — not even the
        // reserved affordance line. A browser gives an abspos box no flow
        // space, and opacity:0 hides it; our in-flow "▶ Video" line for it is
        // synthetic, so a suppressed one is a blank selectable row that only
        // distorts its container's height (Steam's sale capsules keep a
        // mounted `opacity:0` abspos microtrailer `<video>` after hover-away —
        // the stray row misaligned the hovered capsule against its grid
        // siblings). IN-flow suppressed media still reserves its line: that
        // box is real layout in a browser too.
        if self.invisible && self.is_out_of_flow(id) {
            return;
        }
        // The representation links to the media; following it launches mpv.
        // A dead-end video (no playable target — the homepage hero) renders
        // its stand-in poster UNLINKED: an mpv link that can't play is worse
        // than none, and the card's own channel links still work.
        let mut mctx = ctx.clone();
        mctx.link = (!dead_end).then_some(Link::Media(play_url));
        mctx.kind = if dead_end {
            ItemKind::Image
        } else {
            ItemKind::Link
        };
        mctx.node = id;
        mctx.invisible = self.invisible;

        self.flush_block();
        self.begin_line();

        // The preview frame, when present AND decoded, renders as a clickable
        // thumbnail. The element's own `poster` first; failing that, ONLY a
        // streaming video that plays THIS page (`plays_this_page`) borrows the
        // page's standard Open Graph image, since og:image is "a still frame
        // of THIS PAGE's media". A poster-less video with a direct source is
        // some inline clip, not the page's media — giving it the page banner
        // painted Steam's sale og:image onto every microtrailer `<video>` in
        // the store's preview pane — and a card preview playing ANOTHER page
        // must not wear this page's banner either. Sized by its decoded box (NOT
        // the `<video>`'s CSS — that carries a `height:0`/`padding-top` 16:9
        // hack a poster must not inherit), capped to the content width.
        let poster = (tag == "video")
            .then(|| {
                self.dom
                    .attr(id, "poster")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .and_then(|p| match crate::http::resolve(self.base, p) {
                        Link::Http(u) => Some(u.to_string()),
                        _ => None,
                    })
                    .or_else(|| {
                        // og:image is the PAGE's still frame — never a
                        // dead-end card's (there it's the homepage logo).
                        (plays_this_page && !dead_end)
                            .then(|| page_preview_image(self.dom, self.base))
                            .flatten()
                    })
                    .or_else(|| hidden_preview.clone())
            })
            .flatten();
        let mut preview_drawn = false;
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
                pixelated: false,
                text: String::new(),
                kind: ItemKind::Image,
                emph: Emphasis::default(),
                node: id,
                link: mctx.link.clone(),
                invisible: self.invisible,
            });
            self.col += w as usize;
            self.line_height = self.line_height.max(h);
            self.break_line();
            preview_drawn = true;
        }

        // A DRAWN preview IS the mpv affordance (the image item above carries
        // the media link — her call 2026-07-04: no extra text line under a
        // preview). Only when no preview drew (no poster, not yet decoded,
        // undecodable) does a text link stand in for the video content: a
        // streaming `<video>` with no inline source says so plainly (it plays
        // the page via yt-dlp); a direct source keeps its kind + quality
        // (`▶ Video · 720p HD`). A dead-end video never writes a label — it
        // has nothing to launch; its stand-in poster paints once decoded.
        if !preview_drawn && !dead_end {
            let label = if streaming {
                String::from("▶ Watch in mpv")
            } else {
                self.media_label(tag, src_node)
            };
            self.place_text(&label, &mctx);
            self.break_line();
        }
    }

    /// The preview `<img>` a page FADED under a mounted player — the video's
    /// poster in all but name. Searched within the video's containing block
    /// (its card): the first image with a usable source whose paint is
    /// suppressed (an `opacity:0`/`visibility:hidden` chain — the fade a page
    /// applies once its player covers the preview). Only a HIDDEN image
    /// qualifies: a visible sibling preview is normal flowing content (the
    /// hover-preview idiom) and must not double as a poster. The hidden image
    /// itself keeps its suppressed paint — only its URL is borrowed for the
    /// video's representation.
    fn hidden_preview_in_cb(&self, video: NodeId) -> Option<String> {
        let cb = self.positioned_containing_block(video)?;
        self.dom.descendants(cb).into_iter().find_map(|d| {
            if self.dom.tag_name(d) != Some("img") {
                return None;
            }
            // Chain-aware suppression walk: the fade usually sits on a
            // transition WRAPPER, not the image itself (`paint_suppressed`
            // reads own-element opacity; the flow accumulates the ancestor
            // chain via `Ctx`, which this out-of-band query can't see) — so
            // test every ancestor up to the containing block.
            let mut cur = Some(d);
            let mut hidden = false;
            while let Some(n) = cur {
                if self.dom.paint_suppressed(n) || self.dom.visibility_hidden(n) {
                    hidden = true;
                    break;
                }
                if n == cb {
                    break;
                }
                cur = self.dom.parent_composed(n);
            }
            hidden.then(|| self.image_src(d)).flatten()
        })
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
        // Paint suppression: an image in an `opacity:0`/`visibility:hidden`
        // subtree reserves its box (so geometry is unaffected) but the renderer
        // skips its pixels. The image's full state = the inherited sticky opacity
        // chain, its own opacity, and its own computed visibility (which inherits
        // the ancestor's `visibility:hidden` but is re-clearable).
        self.invisible =
            ctx.opacity_hidden || self.dom.paint_suppressed(id) || self.dom.visibility_hidden(id);
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
        // Declaration-first replaced-element sizing (CSS 2.1 §10; HTML §4.8.4.4
        // "dimension attributes"). A browser sizes an `<img>` from its DECLARED
        // dimensions — the `width`/`height` presentation attributes, which also
        // give a natural aspect ratio — and reserves that box BEFORE (and
        // independent of) loading the pixels, so layout doesn't shift when the
        // image arrives. We do the same: an as-yet-undecoded image with declared
        // dimensions reserves a real placeholder box instead of collapsing to alt
        // text. This is also what unblocks IntersectionObserver-driven lazy
        // loaders (lazysizes et al.): the box gives the element an on-page
        // position, so the observer can report it entering the viewport and the
        // loader swaps `data-src`→`src`. The box is blank until the pixels decode,
        // then `place_image_box` (above) renders the real image into the same box.
        let avail = self.line_right.saturating_sub(self.line_left).max(1) as u16;
        if let Some((iw, ih)) = self.declared_intrinsic_px(id) {
            // Layers a terminal can't composite are still dropped (a full-bleed
            // page background / a cover backdrop behind in-flow content), matching
            // the decoded path above so a background image doesn't reserve a wall
            // of blank rows.
            let backdrop = !self.within_modal(id)
                && !self.framed_foreground(id)
                && (self.is_background_layer_image(id, iw, ih, avail)
                    || self.is_backdrop_overlay_image(id));
            if !backdrop {
                let (w, h, _crop) = self.image_used_box(id, iw, ih, avail);
                self.place_image_placeholder(id, ctx, w, h);
                return;
            }
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
            font_zero: ctx.font_zero,
            invisible: self.invisible,
            opacity_hidden: ctx.opacity_hidden,
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
        self.collect_rendered_text_inner(id, false, out);
    }

    /// `opacity_hidden` is the STICKY `opacity:0` chain (an ancestor suppressed
    /// it → the subtree stays suppressed). `visibility` is checked PER ELEMENT
    /// (it inherits but is re-clearable), so a `visibility:visible` descendant of
    /// a hidden ancestor DOES contribute to the visible label — matching how the
    /// flow paints it. A paint-suppressed run isn't part of a button's
    /// visible/accessible name (an `opacity:0`/`visibility:hidden` helper span
    /// must not leak into it).
    fn collect_rendered_text_inner(&self, id: NodeId, opacity_hidden: bool, out: &mut String) {
        match &self.dom.node(id).data {
            // Only reachable when `rendered_text` is called directly on a text
            // node (entry `opacity_hidden` is always false); a text CHILD of an
            // element is gated inline in the element arm below.
            NodeData::Text(t) => out.push_str(t),
            NodeData::Element { .. } => {
                let tag = self.dom.tag_name(id).unwrap_or("");
                if SKIP.contains(&tag) || self.dom.is_hidden(id) {
                    return;
                }
                let oh = opacity_hidden || self.dom.paint_suppressed(id);
                // This element's OWN paint state gates its DIRECT text; element
                // children re-derive their own visibility from the cascade (so a
                // visible child of a `visibility:hidden` element still counts).
                let hidden = oh || self.dom.visibility_hidden(id);
                for c in self.dom.children(id) {
                    match &self.dom.node(c).data {
                        NodeData::Text(t) => {
                            if !hidden {
                                out.push_str(t);
                            }
                        }
                        _ => self.collect_rendered_text_inner(c, oh, out),
                    }
                }
            }
            _ => {}
        }
    }

    /// An icon-only `<button>`: no visible text, but a renderable `<img>` icon
    /// inside (the inline `<svg>` rewritten by `rewrite_inline_svgs`, or a real
    /// icon image). Such a button flows its icon like an `<a>`/`<div>` rather
    /// than collapsing to the `[ aria-label ]` form stub that discards it.
    fn button_is_icon_only(&self, id: NodeId) -> bool {
        if !self.rendered_text(id).trim().is_empty() {
            return false;
        }
        self.dom
            .descendants(id)
            .into_iter()
            .any(|d| self.dom.tag_name(d) == Some("img") && !self.dom.is_hidden(d))
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
            .any(|d| self.dom.tag_name(d) == Some("img"))
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
        // A content-less control that FILLS its containing block as a positioned
        // overlay is a click-SCRIM (click-to-play / click-to-dismiss hit target),
        // not a labeled icon button — it has nothing the browser paints (no text,
        // no icon, no glyph above), so surfacing its accessible name invents body
        // text floating over the content the scrim covers. (Twitch's player has a
        // full-bleed `<button aria-label="Play" style="position:absolute;
        // width:100%;height:100%">` over the video.) An ordinary icon button is
        // icon-SIZED, so it isn't caught and keeps its name.
        if self.dom.is_overlay_scrim(id) {
            return None;
        }
        let label = ["aria-label", "title", "alt"]
            .into_iter()
            .filter_map(|a| self.dom.attr(id, a))
            .map(str::trim)
            .find(|v| !v.is_empty())
            .map(str::to_owned)?;
        // A control the author CLIPPED to an icon-sized box never paints its
        // accessible NAME — a browser shows only what fits the box (the icon).
        // When the name overflows a definite `width` under `overflow:hidden/clip`
        // it's assistive-only chrome (an `aria-label`/`title`), not visible
        // content, so don't surface it. This honors the cascade rather than
        // policing intent: the box clips, full stop. (Twitch's per-message reply
        // button is `aria-label="Click to reply to @user"` inside a
        // `width:3.2rem;overflow:hidden` icon box — surfacing it spammed every
        // chat line with the screen-reader name.) Bigger boxes that fit their
        // name (a logo link, a labeled nav button) are unaffected: `css_cells`
        // is `None` for `auto`, and a wide box's width clears the label.
        if self.clips_hard(id, false)
            && let Some(w) = self.css_cells(id, "width")
            && display_width(&label) > w
        {
            return None;
        }
        Some(label)
    }

    /// The absolute URL of an `<img>`'s `src`, resolved against the base.
    fn image_src(&self, id: NodeId) -> Option<String> {
        let src = self.dom.attr(id, "src")?.trim();
        if src.is_empty() {
            return None;
        }
        // A `data:` image (e.g. a rewritten inline SVG) keys on the URL itself;
        // a `blob:` image likewise (resolved from `Doc.blobs`, not the wire).
        if src.starts_with("data:") || src.starts_with("blob:") {
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
        // CSS Images 3 §5.4: `pixelated` (and the `crisp-edges` family) ask
        // for hard-edged nearest-neighbor scaling — the encoder honors it.
        let pixelated = matches!(
            self.dom.computed_value(id, "image-rendering").as_deref(),
            Some("pixelated" | "crisp-edges" | "-moz-crisp-edges" | "-webkit-optimize-contrast")
        );
        self.line.push(Item {
            col: self.col as u16,
            width: w,
            height: h,
            image: Some(url),
            crop,
            pixelated,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node: id,
            // A linked image follows its anchor on Enter/click.
            link: ctx.link.clone(),
            invisible: self.invisible,
        });
        self.col += w as usize;
        self.line_height = self.line_height.max(h);
        if block {
            self.break_line();
        } else {
            self.pending_space = true; // a trailing gap after the image
        }
    }

    /// Reserve the box of an `<img>` that has DECLARED dimensions but no decoded
    /// pixels yet — the browser's not-yet-loaded replaced element. Same flow
    /// discipline as `place_image_box` (inline: wrap-if-needed and ride the line;
    /// block: own line), but the item carries no image URL (`image: None`) so the
    /// renderer paints it blank while it occupies its real `w`×`h` cell box. The
    /// box is attributed to `id`, so `measure_boxes`/`getBoundingClientRect`/
    /// IntersectionObserver see the image at its true on-page position before it
    /// loads. When the pixels decode, a re-layout takes the `place_image_box` path
    /// and fills the same box.
    fn place_image_placeholder(&mut self, id: NodeId, ctx: &Ctx, w: u16, h: u16) {
        let block = matches!(
            self.dom.computed_display(id).as_deref(),
            Some("block" | "flex" | "grid" | "table" | "list-item")
        ) && !self.in_atomic_inline_context(id);
        if block {
            self.flush_block();
        } else {
            let space = self.pending_space && self.col > self.line_left;
            if self.col + space as usize + w as usize > self.line_right && self.col > self.line_left
            {
                self.break_line();
            }
            if self.pending_space && self.col > self.line_left {
                self.col += 1;
            }
            self.pending_space = false;
        }
        self.line.push(Item {
            col: self.col as u16,
            width: w,
            height: h,
            image: None,
            crop: false,
            pixelated: false,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node: id,
            link: ctx.link.clone(),
            invisible: self.invisible,
        });
        self.col += w as usize;
        self.line_height = self.line_height.max(h);
        if block {
            self.break_line();
        } else {
            self.pending_space = true;
        }
    }

    /// The natural (intrinsic) pixel size a browser uses to size an as-yet-
    /// unloaded `<img>`: its `width`/`height` presentation attributes. HTML maps
    /// these to the image's natural dimensions AND derives a natural aspect ratio
    /// from them, so a sized box exists before the pixels load (the modern
    /// layout-shift-free behaviour). `None` when the image declares no explicit
    /// dimensions (a bare inline icon), which keeps the alt-text fallback for a
    /// dimensionless replaced element.
    fn declared_intrinsic_px(&self, id: NodeId) -> Option<(u16, u16)> {
        let w = self.img_attr_px(id, "width")?;
        let h = self.img_attr_px(id, "height")?;
        let clamp = |n: f32| n.round().clamp(1.0, f32::from(u16::MAX)) as u16;
        Some((clamp(w), clamp(h)))
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
        raw_h
            .as_deref()
            .and_then(|v| css_length_rows(v, self.units(id)))
            .is_none()
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
        // The abspos containing block is the nearest positioned ancestor at
        // WHATEVER depth (CSS 2.1 §10.1) — unbounded like
        // `definite_ancestor_width`; the old 4-level cap stopped short of a
        // deeply-wrapped player frame (the Twitch deep-wrapper lesson).
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if matches!(
                self.dom.computed_style(p, "position").as_deref(),
                Some("relative" | "absolute" | "fixed" | "sticky")
            ) {
                return (self.css_aspect_ratio(p).is_some()
                    || self.has_definite_frame_width(p)
                    || self.padding_aspect_frame(p, id))
                .then_some(p);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// Whether `p` establishes an intrinsic-ratio box via percentage vertical
    /// padding (CSS 2.1 §8.4) — on itself (`height:0; padding-bottom:56.25%`)
    /// or on an empty spacer CHILD (Twitch's ScAspectSpacer) — the responsive
    /// media-frame idiom. Such a containing block FRAMES its fill image just
    /// like a definite `aspect-ratio` does; without this arm the frame test
    /// missed the spacer variant and the background-cover drop swallowed every
    /// category/preview boxart on Twitch's towers. `probe` (the image being
    /// framed) is excluded from the spacer scan.
    fn padding_aspect_frame(&self, p: NodeId, probe: NodeId) -> bool {
        let own_pad = |n: NodeId| -> f32 {
            ["padding-bottom", "padding-top"]
                .iter()
                .filter_map(|prop| first_percent(self.dom.computed_style(n, prop).as_deref()?))
                .sum()
        };
        let h_zero = match self.dom.computed_style(p, "height").as_deref() {
            Some(h) => css_length_px(h, self.units(p)) == Some(0.0) || h.trim() == "auto",
            None => true,
        };
        h_zero
            && (own_pad(p) > 0.0
                || self
                    .dom
                    .children(p)
                    .into_iter()
                    .filter(|&c| c != probe && self.dom.tag_name(c).is_some())
                    .any(|c| own_pad(c) > 0.0))
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
            // A percentage resolves against the nearest ancestor that BREAKS the
            // percentage chain (`pct_width_basis`): a definite `width`, OR — for a
            // run of `width:100%` boxes — a nearer capping `max-width`. The cap
            // must win over a definite-width ancestor sitting ABOVE it: YouTube's
            // brand-icon SVG is `width:100%` through `yt-icon-shape`/
            // `ytIconWrapperHost` (the latter `width:undefinedpx`, invalid) up to a
            // `max-width:36px` leading-image box; resolving against the wide column
            // ABOVE that cap rendered the Shorts logo full-screen.
            Some(pct) => match self.pct_width_basis(id) {
                Some(basis) => (pct * basis as f32).round().max(1.0) as usize,
                // No constraint up the chain: normally a percentage falls back to
                // the flow box, so a full-bleed image fills its column. But inside
                // a SHRINK-WRAP box (an abspos `width:auto` card sizing to content)
                // the containing block width is indefinite, so the percentage is
                // treated as the intrinsic width (CSS Sizing §5.1) — else the image
                // would stretch the card out to the whole band (Twitch's featured
                // player). Capped to the band so it can never exceed it.
                None if self.shrink_wrap => iw.min(avail),
                None => avail,
            },
            None => self
                .css_cells(id, "width")
                .or_else(|| {
                    self.img_attr_px(id, "width")
                        .map(|px| (px / self.units(id).cell_w).round().max(1.0) as usize)
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
        let used_h = if let Some(h) = raw_h
            .as_deref()
            .and_then(|v| css_length_rows(v, self.units(id)))
        {
            h
        } else if let Some(ar) = self.css_aspect_ratio(id) {
            rows_for_ratio(used_w, ar, self.units(id))
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
            } else if let Some(basis) = self.containing_block_height(id) {
                (pct * basis as f32).round().max(1.0) as usize
            } else {
                self.container_box_rows(id, used_w).unwrap_or(intrinsic_h)
            }
        } else if let Some(ar) = self.img_attr_ratio(id) {
            rows_for_ratio(used_w, ar, self.units(id))
        } else if let Some(px) = self.img_attr_px(id, "height") {
            // `<img height=N>` alone (no matching width attr to form a ratio):
            // the presentation-hint height in rows.
            (px / self.units(id).cell_h).round().max(1.0) as usize
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
        // Unbounded (the Twitch deep-wrapper lesson — see
        // `definite_ancestor_width`): the sized container can sit at any
        // depth above its `height:100%` image; the old 6-level cap was the
        // same stop-short bug waiting on the height side.
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if let Some(ar) = self.css_aspect_ratio(p) {
                return Some(rows_for_ratio(used_w, ar, self.units(p)));
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
        // The %-vertical-padding of an element itself (`padding-bottom:56.25%`
        // on the box being examined).
        let own_pad = |p: NodeId| -> f32 {
            ["padding-bottom", "padding-top"]
                .iter()
                .filter_map(|prop| first_percent(self.dom.computed_style(p, prop).as_deref()?))
                .sum()
        };
        // Unbounded like `container_box_rows` above — same deep-wrapper
        // rationale.
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            let h_zero = match self.dom.computed_style(p, "height").as_deref() {
                Some(h) => css_length_px(h, self.units(p)) == Some(0.0) || h.trim() == "auto",
                None => true,
            };
            if h_zero {
                // The padding can live on the ancestor ITSELF (`height:0;
                // padding-bottom:56.25%` — the classic hack) or on an empty
                // in-flow SPACER CHILD of it (Twitch's ScAspectSpacer sibling
                // of the fill: the spacer's padding gives the shared parent
                // its height, and the abspos `height:100%` fill resolves
                // against that parent). Both are the same §8.4 geometry.
                let mut frac: f32 = own_pad(p);
                if frac <= 0.0 {
                    frac = self
                        .dom
                        .children(p)
                        .into_iter()
                        .filter(|&c| c != id && self.dom.tag_name(c).is_some())
                        .map(own_pad)
                        .fold(0.0, f32::max);
                }
                if frac > 0.0 {
                    // height_px = frac · width_px, so the box ratio is 1/frac.
                    return Some(rows_for_ratio(used_w, 1.0 / frac, self.units(p)));
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
    ///
    /// The intermediate ancestors are `width:auto`/`100%` (they pass the
    /// percentage straight through), so the FIRST definite-width ancestor is the
    /// effective containing block — at WHATEVER depth. A styled-components card
    /// can bury that container deep: Twitch's preview-card width:30rem
    /// `ScTransitionBase` sits 11 wrappers above its `<img width:100%>` (aspect-
    /// ratio box, transform/hover wrappers, the `<a>`, shelf-card layers). The
    /// old 8-level cap stopped short, so the image fell back to the full column
    /// (~145 cells) and rendered enormous, burying the page.
    ///
    /// CSS sets no depth limit on resolving a containing block — a browser walks
    /// to the actual one — and the walk terminates because the DOM is a tree:
    /// the JS mutation path enforces pre-insertion validity (`Dom::
    /// is_host_including_inclusive_ancestor`, throwing `HierarchyRequestError`),
    /// so no cycle can form in the composed tree this climbs. Unbounded, like a
    /// browser; a real page reaches the document root in dozens of steps.
    fn definite_ancestor_width(&self, id: NodeId) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        while let Some(p) = cur {
            if let Some(w) = self.definite_len_cells(p, "width") {
                return Some(w);
            }
            cur = self.dom.parent_composed(p);
        }
        None
    }

    /// A property's DEFINITE width in cells — a value that fixes the box's width
    /// regardless of its containing block, so it BREAKS a percentage-width chain
    /// (the containing block for a descendant's `width:%`). A context-free length
    /// (px/em/pt/ch), OR a `calc()`/`min|max|clamp()` that reduces to a fixed
    /// length with NO percentage/viewport term (`width:calc(4px*20)` — the 80px
    /// avatar wrapper redbubble sizes its round `width:100%` image against).
    /// `None` for `%`/`auto`/viewport-relative or a calc that DEPENDS on the
    /// containing block: those pass the percentage through rather than anchoring
    /// it (`css_length_em` already returns `None` for them; the calc guard keeps
    /// that contract so a `min(100%, 40rem)` cap isn't mistaken for a fixed box).
    fn definite_len_cells(&self, id: NodeId, prop: &str) -> Option<usize> {
        let raw = self.dom.computed_style(id, prop)?;
        let v = raw.trim();
        let u = self.units(id);
        if let Some(px) = css_length_px(v, u) {
            return Some((px / u.cell_w).round().max(1.0) as usize);
        }
        let lower = v.to_ascii_lowercase();
        let is_math = ["calc(", "min(", "max(", "clamp("]
            .iter()
            .any(|f| lower.starts_with(f));
        let ctx_dependent = lower.contains('%')
            || ["vw", "vh", "vmin", "vmax"]
                .iter()
                .any(|u| lower.contains(u));
        (is_math && !ctx_dependent)
            .then(|| self.css_cells(id, prop))
            .flatten()
    }

    /// The basis a percentage **width** resolves against, in cells: the nearest
    /// ancestor that breaks the percentage chain. Walks the composed chain and
    /// returns the FIRST binding constraint, nearest first:
    ///   - a definite `width` length (px/em) — the containing block width, itself
    ///     clamped by that element's own `max-width` and by any tighter
    ///     `max-width` seen NEARER the descendant;
    ///   - else, if only `max-width`s were seen, the tightest of them — a run of
    ///     `width:100%` boxes can be no wider than its nearest capped ancestor.
    ///
    /// `None` when nothing up the chain constrains the width (a genuine full-bleed
    /// image, which then falls back to the flow box). This is what fixes
    /// YouTube's brand icons: the `max-width:36px` leading-image box is NEARER
    /// than any definite-width column above it, so the cap must win — the old
    /// "nearest definite width, else nearest max-width" split preferred the far
    /// definite width and rendered the Shorts logo full-screen.
    ///
    /// Unbounded like `definite_ancestor_width` (the composed tree is acyclic, so
    /// the walk terminates at the document root).
    fn pct_width_basis(&self, id: NodeId) -> Option<usize> {
        let mut cur = self.dom.parent_composed(id);
        let mut cap: Option<usize> = None; // tightest max-width between `id` and `cur`
        while let Some(p) = cur {
            let mw = self.definite_len_cells(p, "max-width");
            // A GROWN flex item's `width` is only its flex BASE (Flexbox
            // §7.2.2) — the used size stretches past it, so it can't anchor a
            // percentage (Twitch's category tower: `width:12rem; flex-grow:1`
            // items grow to fill the shelf; sizing the fill image to 12rem
            // painted a small image inside a grown aspect frame, the giant-
            // blank-card bug). Keep walking; its max-width still caps.
            if !self.width_is_flex_base(p)
                && let Some(w) = self.definite_len_cells(p, "width")
            {
                // A definite width breaks the chain. Clamp it by this element's
                // own max-width and by any tighter cap nearer the descendant.
                let w = mw.map_or(w, |m| w.min(m));
                return Some(cap.map_or(w, |c| w.min(c)));
            }
            if let Some(mw) = mw {
                cap = Some(cap.map_or(mw, |c| c.min(mw)));
            }
            cur = self.dom.parent_composed(p);
        }
        cap
    }

    /// Whether the element's USED width can exceed its declared `width`
    /// because it is a GROWING flex item on a horizontal main axis (row or
    /// wrapping shelf): computed `flex-grow` > 0 inside a flex/grid container
    /// that isn't column-direction. Such a `width` is the flex BASE, not the
    /// used size.
    fn width_is_flex_base(&self, id: NodeId) -> bool {
        self.flex_number(id, "flex-grow").is_some_and(|g| g > 0.0)
            && self
                .dom
                .parent_composed(id)
                .and_then(|p| self.flex_mode(p))
                .is_some_and(|m| !matches!(m, FlexMode::Column))
    }

    /// The element's USED height in ROWS **if it is DEFINITE** (CSS 2.1 §10.5),
    /// else `None` (its height is `auto`/content-driven, i.e. indefinite). The
    /// single authority for "does this box have a definite height" — used by
    /// percentage-height resolution and (inner scroll, Phase 1) the scroll-region
    /// trigger. A pure query: it never changes normal-flow heights, which stay
    /// content-driven.
    ///
    /// A length (px/em/ch/`vh`) is definite. A `%` height is definite ONLY when
    /// its containing block height is itself definite — the spec rule: "If the
    /// height of the containing block is not specified explicitly … the value
    /// computes to 'auto'." The chain walks containing blocks (the parent block,
    /// per in-flow assumption) multiplying the percentages, and terminates at the
    /// initial containing block (the viewport, whose height is `viewport_h`). An
    /// `auto`/absent height anywhere on the chain makes the whole thing
    /// indefinite. (Absolutely-positioned CBs — the positioned ancestor's padding
    /// box — are not yet modelled here; in-flow is the common scroll-region case.)
    fn definite_height(&self, id: NodeId) -> Option<usize> {
        let mut cur = id;
        let mut factor = 1.0f32;
        loop {
            let raw = self.dom.computed_style(cur, "height");
            let v = raw.as_deref().map(str::trim).unwrap_or("auto");
            if v.is_empty() || v.eq_ignore_ascii_case("auto") {
                // No explicit height ⇒ content-driven/indefinite for a normal
                // block. TWO bridges make it definite, both off the in-flow
                // path:
                //   1. An absolutely/fixed-positioned box with both `top` AND
                //      `bottom` set has a definite used height (CSS 2.1 §10.6.4)
                //      = containing-block height − top − bottom. The app-shell
                //      idiom — a `position:absolute; top:0; bottom:0` panel
                //      filling a positioned ancestor (Twitch's chat column fills
                //      `#root`), holding the `height:100%` scroll area.
                //   2. A flex item stretched/grown to a definite container
                //      height (CSS Flexbox §9.4/§9.7 — `flex_item_definite_height`).
                if let Some(h) = self.abs_pos_definite_height(cur) {
                    return Some((factor * h as f32).round().max(0.0) as usize);
                }
                return self
                    .flex_item_definite_height(cur)
                    .map(|h| (factor * h as f32).round().max(0.0) as usize);
            }
            if let Some(pct) = parse_percent(v) {
                factor *= pct;
                match self.height_cb(cur) {
                    CbHeight::Element(p) => cur = p,
                    CbHeight::Viewport => {
                        return (self.viewport_h > 0)
                            .then(|| (factor * self.viewport_h as f32).round().max(0.0) as usize);
                    }
                }
            } else if let Some(rows) = css_height_rows_f32(v, self.viewport_h, self.units(cur)) {
                return Some((factor * rows).round().max(0.0) as usize);
            } else {
                return None; // an unresolvable unit (e.g. `vh` with unknown viewport)
            }
        }
    }

    /// The containing block used to resolve a percentage `height` on `id`: the
    /// nearest block-level ancestor (in-flow assumption), or the viewport (the
    /// initial containing block) when `id`'s parent is the document root.
    fn height_cb(&self, id: NodeId) -> CbHeight {
        // An out-of-flow box resolves its percentage height against its
        // POSITIONING containing block, not its DOM parent (CSS 2.1 §10.1/
        // §10.6.4): the viewport for `fixed`, else the nearest positioned
        // ancestor (or the viewport when there is none) — mirroring
        // `abs_cb_height`. This is what makes `pane__inner{position:fixed;
        // height:100%}` (Mastodon's fixed rails) resolve to the viewport height
        // instead of walking a parent chain that dead-ends at a `height:auto`.
        if self.is_out_of_flow(id) {
            if self.dom.computed_style(id, "position").as_deref() == Some("fixed") {
                return CbHeight::Viewport;
            }
            return match self.positioned_containing_block(id) {
                Some(cb) => CbHeight::Element(cb),
                None => CbHeight::Viewport,
            };
        }
        match self.dom.parent_composed(id) {
            Some(p) if self.dom.tag_name(p).is_some() => CbHeight::Element(p),
            _ => CbHeight::Viewport,
        }
    }

    /// The definite height (rows) of `id`'s containing block — what a percentage
    /// height on `id` resolves against. `None` when that containing block height
    /// is itself indefinite (so the percentage computes to `auto`). This is the
    /// containing block for a percentage height on an image (the avatar wrapper's
    /// `height:36px`, or a `height:100%` chain up to the viewport).
    fn containing_block_height(&self, id: NodeId) -> Option<usize> {
        match self.height_cb(id) {
            CbHeight::Element(p) => self.definite_height(p),
            CbHeight::Viewport => (self.viewport_h > 0).then_some(self.viewport_h),
        }
    }

    /// The DEFINITE used height (rows) of an absolutely/fixed-positioned box
    /// whose `height` is `auto` but whose `top` AND `bottom` are both definite
    /// (CSS 2.1 §10.6.4): the containing-block height minus the two offsets and
    /// vertical margins. `None` when the box isn't out of flow, an offset is
    /// `auto`/absent, or the containing block height is indefinite. This is the
    /// app-shell idiom — a `top:0; bottom:0` panel filling a positioned ancestor
    /// (Twitch's chat column fills `#root`). Borders/padding are approximated
    /// away (content height ≈ height), as elsewhere in the height model.
    fn abs_pos_definite_height(&self, id: NodeId) -> Option<usize> {
        if !self.is_out_of_flow(id) {
            return None;
        }
        let cb_h = self.abs_cb_height(id)?;
        let top = self.pos_len(id, "top", cb_h)?;
        let bottom = self.pos_len(id, "bottom", cb_h)?;
        let mt = self.pos_len(id, "margin-top", cb_h).unwrap_or(0.0);
        let mb = self.pos_len(id, "margin-bottom", cb_h).unwrap_or(0.0);
        Some((cb_h as f32 - top - bottom - mt - mb).round().max(0.0) as usize)
    }

    /// The definite height (rows) of an out-of-flow box's containing block (the
    /// extent its `top`/`bottom`/`%` resolve against): the viewport for `fixed`,
    /// else the nearest positioned ancestor's definite height — or the initial
    /// containing block (the viewport) when there is no positioned ancestor.
    fn abs_cb_height(&self, id: NodeId) -> Option<usize> {
        if self.dom.computed_style(id, "position").as_deref() == Some("fixed") {
            return (self.viewport_h > 0).then_some(self.viewport_h);
        }
        match self.positioned_containing_block(id) {
            Some(cb) => self.definite_height(cb),
            None => (self.viewport_h > 0).then_some(self.viewport_h),
        }
    }

    /// A flex item's DEFINITE cross height when it has no explicit height (CSS
    /// Flexbox §9.4 "Cross sizing"): a stretched item in a non-wrapping ROW flex
    /// container with a definite height fills the container's content height. So
    /// a chat column with `align-items:stretch` (the default) inside a
    /// viewport-tall horizontal page-flex is itself definite-height, which lets a
    /// `height:100%` scroll area inside it resolve. `None` when the item isn't
    /// stretched, its container isn't a definite-height row flex, or it's out of
    /// flow.
    ///
    /// NOT yet modelled (returns `None`): COLUMN-flex MAIN-size distribution —
    /// splitting the container's definite height across `flex-grow` items
    /// requires the full main-size resolution; such an item resolves only via an
    /// explicit `height`/`%`. Padding/border of the container is approximated
    /// away (its content height ≈ its height) — terminal cell coarseness.
    fn flex_item_definite_height(&self, id: NodeId) -> Option<usize> {
        if self.is_out_of_flow(id) {
            return None;
        }
        let parent = self.dom.parent_composed(id)?;
        match self.flex_mode(parent)? {
            FlexMode::Row if self.flex_cross_stretches(id, parent) => self.definite_height(parent),
            FlexMode::Column => self.column_flex_item_main_height(id, parent),
            _ => None,
        }
    }

    /// A `flex-grow` item's DEFINITE main size (height, in rows) inside a
    /// definite-height COLUMN flex container (CSS Flexbox §9.2/§9.7). The
    /// container's content height is distributed: free space = `H − Σ base`
    /// over every item's flex base size, handed to grow items by their grow
    /// factor — so a stretched chat column inside a viewport-tall
    /// `flex-direction:column` shell (`#root`) becomes definite, letting an
    /// inner `height:100%` scroll area resolve.
    ///
    /// We resolve ONE item, so we must never measure the item itself — it may
    /// be mid-layout (self-measurement would recurse). This is exact for a
    /// SOLE grow item, whose own base size cancels out of the distribution
    /// (`target = base + 1·(H − Σothers − base) = H − Σothers`), so we only
    /// measure the OTHER siblings' bases (disjoint subtrees, re-entrancy-safe).
    /// Multiple grow items would need this item's own base (no cancellation) ⇒
    /// `None` (deferred; the region simply doesn't trigger — an honest gap).
    /// Container padding/border is approximated away (content height ≈ height),
    /// matching the row-stretch bridge — terminal cell coarseness.
    fn column_flex_item_main_height(&self, id: NodeId, parent: NodeId) -> Option<usize> {
        // Only a growing item takes a definite main size from the container; a
        // non-growing auto-height column item is content-sized (indefinite).
        if self.flex_number(id, "flex-grow").unwrap_or(0.0) <= 0.0 {
            return None;
        }
        let container_h = self.definite_height(parent)?;
        let cross_w = self.column_cross_width(parent);
        // Σ flex base sizes of the OTHER (non-grow) items. A second grow item
        // would require `id`'s own base size (no cancellation) — deferred.
        let mut others = 0usize;
        let mut found_self = false;
        for sib in self.flex_items(parent) {
            if self.flex_number(sib, "flex-grow").unwrap_or(0.0) > 0.0 {
                if sib == id {
                    found_self = true;
                } else {
                    return None;
                }
            } else {
                others = others.saturating_add(self.flex_base_height(sib, container_h, cross_w));
            }
        }
        if !found_self {
            return None;
        }
        let mut main = container_h.saturating_sub(others);
        // Explicit `min-height`/`max-height` clamp the grown main size (resolved
        // against the container content height). `min-height:auto`'s content
        // floor (§4.5) is not applied to the grown item — it would require
        // self-measurement and never binds while an item grows into positive
        // free space.
        if let Some(m) = self.len_or_pct_h(id, "max-height", container_h) {
            main = main.min(m);
        }
        if let Some(m) = self.len_or_pct_h(id, "min-height", container_h) {
            main = main.max(m);
        }
        Some(main)
    }

    /// A column-flex item's flex base size on the MAIN (vertical) axis, in rows
    /// (CSS Flexbox §9.2): `flex-basis` if a length (`auto` ⇒ the `height`
    /// property; `content`/`*-content` ⇒ a content size), resolved against the
    /// container content height `cb_h`; an auto/content size is the item's
    /// content height laid out at `cross_w` (the container content width).
    /// Explicit `min-height`/`max-height` clamp it — and the automatic minimum
    /// (`min-height:auto`, §4.5) is satisfied for a measured item, whose content
    /// height already exceeds its content-min.
    fn flex_base_height(&self, id: NodeId, cb_h: usize, cross_w: usize) -> usize {
        let basis = match self
            .dom
            .computed_style(id, "flex-basis")
            .as_deref()
            .map(str::trim)
        {
            None | Some("auto") => self.len_or_pct_h(id, "height", cb_h),
            Some("content" | "max-content" | "min-content" | "fit-content") => None,
            Some(v) => self.resolve_height_rows(v, cb_h, self.units(id)),
        };
        let mut base = basis.unwrap_or_else(|| self.measure_height(id, cross_w));
        if let Some(m) = self.len_or_pct_h(id, "max-height", cb_h) {
            base = base.min(m);
        }
        if let Some(m) = self.len_or_pct_h(id, "min-height", cb_h) {
            base = base.max(m);
        }
        base
    }

    /// A height-axis analogue of `len_or_pct`: a `height`/`min-height`/
    /// `max-height`-style property as rows, resolving `%` against the
    /// containing-block height `cb_h` and `vh` against the viewport.
    fn len_or_pct_h(&self, id: NodeId, prop: &str, cb_h: usize) -> Option<usize> {
        self.resolve_height_rows(&self.dom.computed_style(id, prop)?, cb_h, self.units(id))
    }

    /// Resolve a vertical CSS length to rows: a `%` against the containing-block
    /// height `cb_h`, else a length/`vh` via `css_height_rows_f32` (NOT the
    /// width resolver — a cell is ~1 row/em tall but ~2 cells/em wide, so the
    /// two axes never share `resolve_cells`'s `em·2`). Rounded, never negative.
    fn resolve_height_rows(&self, value: &str, cb_h: usize, u: Units) -> Option<usize> {
        if let Some(pct) = parse_percent(value) {
            return Some((pct * cb_h as f32).round().max(0.0) as usize);
        }
        css_height_rows_f32(value, self.viewport_h, u).map(|r| r.round().max(0.0) as usize)
    }

    /// The content height (rows) of `id`'s subtree laid out at `width`. A
    /// height-axis analogue of `measure_width`; used to size a column-flex
    /// sibling whose main size is content-driven.
    fn measure_height(&self, id: NodeId, width: usize) -> usize {
        self.layout_subtree_inner(id, width.max(1), None, true, &Ctx::root())
            .height as usize
    }

    /// The cross-axis (width) basis for measuring a column flex container's
    /// items: the container's own definite `width` length, else a definite-
    /// width ancestor, else the viewport width (a full-bleed app shell). An
    /// approximation — only the fallback content-height measurement of an
    /// auto-height sibling depends on it.
    fn column_cross_width(&self, container: NodeId) -> usize {
        let u = self.units(container);
        self.dom
            .computed_style(container, "width")
            .as_deref()
            .and_then(|v| css_length_px(v, u))
            .map(|px| (px / u.cell_w).round().max(1.0) as usize)
            .or_else(|| self.definite_ancestor_width(container))
            .unwrap_or_else(|| self.viewport_w.max(1))
    }

    /// Whether a flex item stretches on the cross axis (CSS Flexbox §9.4): its
    /// resolved `align-self` (else the container's `align-items`) is `stretch` or
    /// `normal` — both the initial value, which stretches an auto cross-size to
    /// fill the line.
    fn flex_cross_stretches(&self, id: NodeId, parent: NodeId) -> bool {
        let align = self
            .dom
            .computed_style(id, "align-self")
            .filter(|v| !v.trim().eq_ignore_ascii_case("auto"))
            .or_else(|| self.dom.computed_style(parent, "align-items"));
        match align.as_deref().map(str::trim) {
            None => true, // initial align-items is `normal` ⇒ stretch for flex
            Some(v) => v.eq_ignore_ascii_case("stretch") || v.eq_ignore_ascii_case("normal"),
        }
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
        // A widget LABEL wider than the band (a poll button packed with voter
        // usernames) char-breaks across rows like text rather than overflowing
        // off-screen. Render-only (`overflow-wrap:break-word`, like `place_word`)
        // so measurement still sizes a content-sized box to the full label. Every
        // `place_atom` caller is a text-bearing form stub, and short widgets
        // (`[ ]`/`( )`/`[ select ▾ ]`) never exceed the band, so only genuinely
        // over-wide labels are affected.
        if !self.measuring && len > self.line_right.saturating_sub(self.line_left).max(1) {
            self.place_oversize(&text, kind, Emphasis::default(), node, link);
        } else {
            self.push_item(text, len, kind, Emphasis::default(), node, link);
        }
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
            .and_then(|s| s.trim().parse::<i64>().ok())
            && let Some((c, _)) = self.list_stack.last_mut()
        {
            *c = v;
        }
        let (counter, step) = self.list_stack.last().copied().unwrap_or((1, 1));
        if let Some((c, _)) = self.list_stack.last_mut() {
            *c = c.saturating_add(step);
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
            pixelated: false,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
            // A list marker follows its `<li>`'s paint-suppression (set by
            // `flow_element` before the block-tail marker placement).
            invisible: self.invisible,
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
            pixelated: false,
            text,
            kind,
            emph,
            node,
            link,
            invisible: self.invisible,
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
    #[allow(clippy::type_complexity)]
    fn finish(
        mut self,
    ) -> (
        Vec<Row>,
        Vec<Carousel>,
        Vec<FixedItem>,
        Vec<Region>,
        ElementTops,
        ScrollClips,
        BoundaryRecs,
        Vec<PositionedBox>,
    ) {
        let carousels = std::mem::take(&mut self.carousels);
        let mut fixed = std::mem::take(&mut self.fixed);
        let mut regions = std::mem::take(&mut self.regions);
        let mut positioned = std::mem::take(&mut self.positioned);
        let element_tops = std::mem::take(&mut self.element_tops);
        let scroll_clips = std::mem::take(&mut self.scroll_clips);
        let boundary_boxes = std::mem::take(&mut self.boundary_boxes);
        let n = self.rows.len();
        // A scroll region reserves exactly `height` BLANK doc rows that the
        // renderer fills from its buffer — they're intentional placeholders, so
        // they must survive the blank-row collapse (and the trailing-blank pop)
        // that would otherwise merge or drop them.
        let mut reserved = vec![false; n];
        for rg in &regions {
            for slot in reserved
                .iter_mut()
                .take((rg.start_row + rg.height as usize).min(n))
                .skip(rg.start_row.min(n))
            {
                *slot = true;
            }
        }
        // remap[i] = new index of old row i (for a dropped blank, the index
        // the next kept row takes). remap[n] = total kept rows, for an
        // exclusive `end` that points one past the last row.
        let mut remap = vec![0usize; n + 1];
        let mut out: Vec<Row> = Vec::with_capacity(n);
        let mut out_reserved: Vec<bool> = Vec::with_capacity(n);
        for (i, row) in self.rows.into_iter().enumerate() {
            remap[i] = out.len();
            if !reserved[i] && row.items.is_empty() && out.last().is_none_or(|r| r.items.is_empty())
            {
                continue;
            }
            out_reserved.push(reserved[i]);
            out.push(row);
        }
        remap[n] = out.len();
        while out.last().is_some_and(|r| r.items.is_empty()) && out_reserved.last() == Some(&false)
        {
            out.pop();
            out_reserved.pop();
        }
        for rg in &mut regions {
            rg.start_row = remap[rg.start_row.min(n)];
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
        // Remap each incremental-layout boundary's row span through the same
        // collapse (start inclusive, end exclusive — like a carousel) so its
        // `row_range` indexes the kept-row grid the splice operates on.
        let remap_row = |r: usize| -> usize {
            if r >= n {
                out.len() + (r - n)
            } else {
                remap[r]
            }
        };
        let boundary_boxes = boundary_boxes
            .into_iter()
            .map(|(id, rec)| {
                (
                    id,
                    BoundaryRec {
                        start_row: remap_row(rec.start_row),
                        end_row: remap_row(rec.end_row),
                        ..rec
                    },
                )
            })
            .collect();
        // Pinned fixed boxes move through the same blank-row collapse (their
        // captured row is a flow position in this box's coordinate space).
        for f in &mut fixed {
            f.row = u16::try_from(remap_row(f.row as usize)).unwrap_or(u16::MAX);
        }
        // Out-of-flow boxes not yet composited (a sub-box's — they ride up to
        // the document root) move their placement row through the same collapse,
        // so an overlay stays aligned with the content its coordinate system
        // just re-indexed. Their own `b.rows` are self-contained and untouched.
        for p in &mut positioned {
            p.row = remap_row(p.row);
        }
        (
            out,
            carousels,
            fixed,
            regions,
            element_tops,
            scroll_clips,
            boundary_boxes,
            positioned,
        )
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
            pixelated: false,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node: NO_NODE,
            link: None,
            // A zero-width image-box spacer paints nothing regardless.
            invisible: false,
        }],
    }
}

/// A reserved spacer row for a paint-suppressed overflow-clip placeholder
/// (`flow_element`): renders nothing and survives `finish`'s blank-row collapse.
/// In the RENDER pass it is inert (`node`=NO_NODE, `width`=0). In the GEOMETRY
/// pass (`tag_all_nodes`) it carries the placeholder's node + box width so
/// `measure_boxes` records the reserved box — `getBoundingClientRect` and the
/// page's IntersectionObserver then see the SAME box the render paints, so the
/// app scroll and the engine's row positions stay in the one coordinate system.
fn reserved_clip_row(indent: usize, width: u16, node: NodeId) -> Row {
    Row {
        items: vec![Item {
            col: indent as u16,
            width,
            height: 1,
            image: None,
            crop: false,
            pixelated: false,
            text: String::new(),
            kind: ItemKind::Image,
            emph: Emphasis::default(),
            node,
            link: None,
            invisible: true,
        }],
    }
}

fn same_run(item: &Item, ctx: &Ctx) -> bool {
    item.kind == ctx.kind
        && item.emph == ctx.emph
        && item.node == ctx.node
        && item.link == ctx.link
        // Never coalesce a painted word into a paint-suppressed run (or vice
        // versa): they render differently even at the same node (an `opacity:0`
        // reveal on part of a run).
        && item.invisible == ctx.invisible
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
pub(crate) fn css_is_bold(value: &str) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "bold" | "bolder" => true,
        "normal" | "lighter" => false,
        n => n.parse::<u32>().is_ok_and(|w| w >= 600),
    }
}

/// A CSS `font-style` value reads as italic (`italic`/`oblique`).
pub(crate) fn css_is_italic(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "italic" | "oblique"
    )
}

/// Defensive backstop on a CSS-forced image height (rows) — only the
/// `object-fit:cover` path can reach it (that path keeps the author's box
/// rather than the drawn box). It is NOT a rendering cap: CSS doesn't limit an
/// image's height, a tall image just scrolls, and partial-sixel drawing handles
/// images larger than the viewport. The non-cover path already reserves only
/// what's actually drawn, and the decoder bounds raster dimensions to
/// `img::MAX_DIMENSION` (12 000px ≈ 750 rows even at a 16px cell), so no real
/// image reaches this lid. It exists ONLY so a pathological `object-fit:cover;
/// height: 9999999px` can't reserve a near-infinite blank run and wreck the
/// scroll/selection index math — the same family of hostile-input lid as
/// `http::MAX_BODY`. Set far above any decoded image; a tall image renders in
/// full. (Was 48 — a leftover from before partial sixel, which clipped real
/// tall images; her call 2026-06-27 to raise it.)
const IMG_CSS_MAX_ROWS: usize = 12_000;

/// A vertical CSS length as terminal rows. The cell is ~2:1 (col:row), so 1em
/// ≈ 1 row (vs ≈ 2 cols horizontally) and 1px ≈ 1/16 row. `%`/`auto`/`vh`
/// return `None` (a `100%`/`%` height resolves against a container instead).
fn css_length_rows(value: &str, u: Units) -> Option<usize> {
    css_length_px(value, u).map(|px| (px / u.cell_h).round().max(1.0) as usize)
}

/// A definite CSS height as ROWS (fractional, for chain multiplication): an
/// absolute length (via `css_length_px` / the real cell height) or a
/// viewport-height unit (`vh` against `viewport_h`, already in rows).
/// `%`/`auto` are NOT resolved here — a `%` height needs its containing block
/// (see `Layout::definite_height`). `None` for an indefinite/unresolvable
/// value, or `vh` when the viewport height is unknown (`viewport_h == 0`).
/// Height stays SEPARATE from the width resolver (`resolve_cells`) on
/// purpose: the two axes convert through different cell dimensions
/// (`vmin`/`vmax` as a height would mix them and are vanishingly rare —
/// deferred). Unlike `css_length_rows` it does NOT floor at 1 row (the chain
/// rounds once at the end).
fn css_height_rows_f32(value: &str, viewport_h: usize, u: Units) -> Option<f32> {
    let v = value.trim();
    // Viewport-height units. A terminal has no dynamic browser chrome, so the
    // small/large/dynamic-viewport keywords (`svh`/`lvh`/`dvh`) all equal `vh`:
    // strip the `vh` suffix, then an optional `d`/`s`/`l` qualifier.
    if let Some(rest) = v.strip_suffix("vh") {
        let rest = rest.strip_suffix(['d', 's', 'l']).unwrap_or(rest);
        if let Ok(n) = rest.trim().parse::<f32>() {
            return (viewport_h > 0).then(|| (n / 100.0) * viewport_h as f32);
        }
    }
    css_length_px(v, u).map(|px| px / u.cell_h)
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
            // Cap the occupancy PRODUCT (see `MAX_CELL_SPAN_AREA`): keep the
            // colspan, clamp the rowspan to fit the area lid.
            let rowspan = layout
                .cell_span(cell, "rowspan")
                .min((MAX_CELL_SPAN_AREA / colspan).max(1));
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

/// Rows for a box `width_cols` wide at pixel aspect `ratio` (width÷height):
/// `height_px = width_px / ratio`, converted through the REAL cell box so the
/// drawn aspect is physically true on any terminal font (the nominal 8×16
/// cell gives the historical `rows = cols / (2·ratio)`).
fn rows_for_ratio(width_cols: usize, ratio: f32, u: Units) -> usize {
    ((width_cols as f32 * u.cell_w) / (ratio * u.cell_h))
        .round()
        .max(1.0) as usize
}

/// An absolute CSS length as CSS px, resolved in `u`'s font context: `em`
/// against the element's computed font-size, `rem` against the root's (CSS
/// Values §6.2.1 — a fixed 16px here inflated every rem 1.6× on
/// `html{font-size:62.5%}` sites like Twitch), physical units at CSS's fixed
/// ratios (96px/in), unitless treated as px. `ch`/`ex` measure GLYPHS, and in
/// this renderer every glyph at every font-size is one cell wide — so `ch` is
/// the cell width by definition (the spec's own "advance of 0" evaluated
/// against our real font metrics), keeping a `65ch` prose measure at 65
/// characters. Context-dependent values (`%`/`vw`/`calc()`/`auto`) → `None`
/// here — they go through `resolve_cells`, which knows the containing block
/// and the viewport.
pub(crate) fn css_length_px(value: &str, u: Units) -> Option<f32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    Some(match v[split..].trim() {
        "em" => n * u.fs,
        "rem" => n * u.root,
        "px" | "" => n,
        "pt" => n * 4.0 / 3.0,
        "pc" => n * 16.0,
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        "q" | "Q" => n * 96.0 / 101.6,
        // One glyph advance per count (see above).
        "ch" => n * u.cell_w,
        // x-height: the spec's no-metrics fallback, half the em.
        "ex" => n * 0.5 * u.fs,
        _ => return None,
    })
}

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
pub(crate) fn split_track_tokens(s: &str) -> Vec<String> {
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

/// `viewport` is `(width, height)` in cells — the basis for viewport-percentage
/// units (`vw` against `.0`, `vh` against `.1`, `vmin`/`vmax` against the
/// smaller/larger). A height of `0` means this pass wasn't told the viewport
/// height (a legacy/test caller), so `vh`/`vmin`/`vmax` stay unresolved.
fn resolve_cells_f32(value: &str, avail: usize, viewport: (usize, usize), u: Units) -> Option<f32> {
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
        return resolve_cells_f32(fallback, avail, viewport, u);
    }
    if let Some(inner) = v
        .strip_prefix("calc(")
        .or_else(|| v.strip_prefix("CALC("))
        .and_then(|r| r.strip_suffix(')'))
    {
        return resolve_calc(inner, avail, viewport, u);
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
                .filter_map(|a| resolve_cells_f32(a.trim(), avail, viewport, u))
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
    // Viewport-percentage units, measured in CELLS (the basis `vw` has always
    // used), so the result is directly in cells. `vh`/`vmin`/`vmax` need the
    // viewport height; when it's unknown (`.1 == 0`) they stay `None` rather
    // than collapsing to 0 — preserving the prior "no viewport height" result.
    // Longer suffixes (`vmin`/`vmax`) are tested before `vh`/`vw`.
    let (vw, vh) = (viewport.0 as f32, viewport.1 as f32);
    let known_h = viewport.1 > 0;
    for (suffix, basis) in [
        ("vmin", known_h.then(|| vw.min(vh))),
        ("vmax", known_h.then(|| vw.max(vh))),
        ("vh", known_h.then_some(vh)),
        ("vw", Some(vw)),
    ] {
        // A terminal has no dynamic browser chrome, so a small/large/dynamic-
        // viewport qualifier (`dvw`/`svh`/`lvmin`, …) equals the classic unit:
        // strip the base unit, then an optional trailing `d`/`s`/`l`.
        if let Some(rest) = v.strip_suffix(suffix) {
            let rest = rest.strip_suffix(['d', 's', 'l']).unwrap_or(rest);
            if let Ok(n) = rest.trim().parse::<f32>() {
                return basis.map(|b| (n / 100.0) * b);
            }
        }
    }
    css_length_px(v, u).map(|px| px / u.cell_w)
}

/// `resolve_cells_f32` rounded to whole cells (never negative).
fn resolve_cells(value: &str, avail: usize, viewport: (usize, usize), u: Units) -> Option<usize> {
    resolve_cells_f32(value, avail, viewport, u).map(|c| c.round().max(0.0) as usize)
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
fn resolve_calc(body: &str, avail: usize, viewport: (usize, usize), u: Units) -> Option<f32> {
    let mut p = CalcParser {
        s: body.as_bytes(),
        pos: 0,
        avail,
        viewport,
        units: u,
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
    viewport: (usize, usize),
    units: Units,
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
            return resolve_calc(inner, self.avail, self.viewport, self.units);
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
            resolve_cells_f32(tok, self.avail, self.viewport, self.units)
        }
    }
}

/// Whether a vertical length is big enough to warrant a blank spacer row.
/// A terminal row is precious (~1em of height), so we spend one only when the
/// gap EXCEEDS half a line: an exactly-half-row gap — `8px`/`0.5em`/`1ch`, the
/// web's ubiquitous "tight" spacing (a thumbnail-to-caption tab, an icon-row
/// pad) — no longer costs a whole blank line. Gaps over half a row still do.
fn vertical_space(value: &str, u: Units) -> bool {
    css_length_px(value, u).is_some_and(|px| px > u.cell_h / 2.0)
}

/// A horizontal length as an indent in cells.
fn indent_cells(value: Option<&str>, u: Units) -> usize {
    value
        .and_then(|v| css_length_px(v, u))
        .map(|px| (px / u.cell_w).round().max(0.0) as usize)
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

    /// Bug fix: `Doc.scroll_clips` entries recorded inside a SUB-LAYOUT (here a
    /// definite-height scroll box nested in a flex item) must propagate up
    /// through `blit` like every other harvest channel — the app pushes a live
    /// page's `clientHeight` from exactly this list, and they used to be
    /// silently dropped at `layout_subtree_inner`.
    #[test]
    fn scroll_clips_propagate_out_of_flex_sub_layouts() {
        let html = "<div style='display:flex'>\
            <div style='flex:1'>\
              <div data-trust-node='7' style='height:64px;overflow-y:auto'>\
                <p>one</p><p>two</p><p>three</p><p>four</p><p>five</p><p>six</p>\
              </div>\
            </div>\
            <div style='flex:1'><p>sidebar</p></div>\
          </div>";
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (_rows, _c, regions, clips, _b, _f, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 24),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert!(
            !regions.is_empty(),
            "the nested scroll box still forms a region"
        );
        assert!(
            clips.iter().any(|&(node, h, _w)| node == 7 && h == 4),
            "the nested box's clip (node 7, 64px = 4 rows) reaches the top level: {clips:?}"
        );
    }

    #[test]
    fn fragment_anchors_cover_replaced_floated_and_positioned_targets() {
        // Bug #7: the fragment-anchor capture in `flow_element` sat AFTER the
        // early-return dispatches (img/controls/media/floats/out-of-flow), so
        // `<img id>`, floated, and positioned targets never landed in
        // `anchor_rows` and `#fragment` navigation to them silently no-op'd.
        // In-flow/floated targets anchor at their entry position; an
        // out-of-flow target is captured inside its placement sub-pass and
        // `blit`-merged at its PLACED row.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/pic.png".to_owned(), (16, 8));
        let html = r#"<body>
            <p>intro</p>
            <img id="pic" src="/pic.png">
            <div id="floaty" style="float:left">F</div>
            <div style="position:relative">
              <div id="abs" style="position:absolute;top:32px">A</div>content
            </div>
            <p>outro</p>
          </body>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (_rows, _c, _rg, _sc, _b, _f, anchors) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 24),
            &[],
            &ControlMap::new(),
            &images,
            false,
        );
        assert!(
            anchors.contains_key("pic"),
            "an <img id> is a fragment target: {anchors:?}"
        );
        assert!(
            anchors.contains_key("floaty"),
            "a floated target anchors at its entry row: {anchors:?}"
        );
        assert!(
            anchors.contains_key("abs"),
            "a positioned target anchors at its placed row: {anchors:?}"
        );
    }

    #[test]
    fn a_scroll_region_does_not_inflate_intrinsic_width_measurement() {
        // Bug #8: `flow_region` laid its buffer with `measure=false` even
        // inside a measuring pass. The measuring pass flows the buffer INLINE,
        // so alignment offsets — skipped while measuring everywhere else —
        // applied and inflated the intrinsic width of any region-bearing
        // subtree: this shrink-to-fit float measured ~half the band instead of
        // its content width, shoving the following text far right.
        let html = r#"<body>
            <div style="float:left">
              <div style="overflow-y:auto;height:32px"><div style="text-align:center">hi</div></div>
            </div>
            <p>after</p>
          </body>"#;
        let rows = lay(html, 80);
        let after = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("after"))
            .expect("the following text lays out");
        assert!(
            after.col < 10,
            "the float shrink-wraps to the region's content width, not a \
             centered-offset inflation (after at col {})",
            after.col
        );
    }

    /// Bug fix: the overlay-over-image lift (`insert_blank_rows`) must shift a
    /// scroll region recorded below the insert — regions (unlike carousels and
    /// floats) were not adjusted, so the reserved band drifted off its blank
    /// rows and `finish`'s reserved-row protection marked the wrong rows.
    #[test]
    fn overlay_lift_shifts_a_scroll_region_below_it() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/pic.png".to_string(), (10, 3));
        let html = "<div style='position:relative'>\
            <img src='/pic.png' alt=''>\
            <div style='position:absolute;top:0;left:0'>SALE</div>\
            <div data-trust-node='9' style='height:48px;overflow-y:auto'>\
              <p>m1</p><p>m2</p><p>m3</p><p>m4</p><p>m5</p><p>m6</p>\
            </div>\
            <p>AFTER</p>\
          </div>";
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, _c, regions, _cl, _b, _f, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (60, 24),
            &[],
            &ControlMap::new(),
            &images,
            false,
        );
        // The lift really happened: the overlay sits above the image.
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
                .unwrap_or_else(|| panic!("{needle} laid out"))
        };
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .expect("image laid out");
        assert!(row_of("SALE") < img_row, "overlay lifted above the image");
        // The region's reserved band still points at its own blank rows (a
        // stale start_row lands on the image's spacer rows, which carry
        // marker items).
        assert_eq!(regions.len(), 1);
        let rg = &regions[0];
        for (r, row) in rows
            .iter()
            .enumerate()
            .skip(rg.start_row)
            .take(rg.height as usize)
        {
            assert!(
                row.items.is_empty(),
                "reserved band row {r} must be blank: {:?}",
                row.items
            );
        }
        assert!(
            row_of("AFTER") >= rg.start_row + rg.height as usize,
            "content after the region stays below its band"
        );
    }

    /// Bug fix: region-edge clipping truncates by DISPLAY width, not char
    /// count — a char-count cut kept up to 2× the cells of CJK text, painting
    /// past the scrollport and desyncing `width` from the rendered glyphs.
    #[test]
    fn region_clip_truncates_by_display_width_not_chars() {
        let rg = Region {
            node: NO_NODE,
            start_row: 0,
            left: 2,
            width: 4,
            height: 1,
            buffer: vec![Row {
                items: vec![Item {
                    col: 0,
                    width: 10,
                    height: 1,
                    text: "日本語です".to_string(),
                    kind: ItemKind::Text,
                    image: None,
                    crop: false,
                    pixelated: false,
                    emph: Emphasis::default(),
                    node: NO_NODE,
                    link: None,
                    invisible: false,
                }],
            }],
            voffset: 0,
            live_node: None,
            voffset_from_page: false,
            principal: false,
            carousels: Vec::new(),
            regions: Vec::new(),
            image_urls: Vec::new(),
        };
        let rows = vec![Row::default()];
        let merged = effective_row(&rows, std::slice::from_ref(&rg), 0);
        let it = &merged.items[0];
        assert_eq!(it.text, "日本", "2 wide glyphs = the 4-cell scrollport");
        assert_eq!(
            it.width as usize,
            display_width(&it.text),
            "item width matches what actually renders"
        );
    }

    /// Bug fix: the border frame clips interior overflow by DISPLAY width too,
    /// so clipped CJK can't paint through the right border bar.
    #[test]
    fn border_clip_truncates_by_display_width_not_chars() {
        let html = "<div style='border:1px solid;width:80px;white-space:nowrap'>日本語のテキストです</div>";
        let rows = lay_b(html, 40);
        let text_row = rows
            .iter()
            .find(|r| r.items.iter().any(|i| i.text.contains('日')))
            .expect("content row inside the frame");
        let it = text_row
            .items
            .iter()
            .find(|i| i.text.contains('日'))
            .unwrap();
        let bar_col = text_row
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Border)
            .map(|i| i.col)
            .max()
            .expect("right border bar present");
        assert!(
            it.col as usize + display_width(&it.text) <= bar_col as usize,
            "clipped text must end at or before the right bar: end={} bar={}",
            it.col as usize + display_width(&it.text),
            bar_col
        );
        assert_eq!(it.width as usize, display_width(&it.text));
    }

    /// CSS Text 3 §4.1.1: U+00A0 is NOT document white space — it neither
    /// collapses nor offers a soft-wrap opportunity. `10&nbsp;000` stays one
    /// unbreakable token; ordinary space runs still collapse.
    #[test]
    fn nbsp_neither_collapses_nor_wraps() {
        // Narrow band: without the glue, "10" fits after "wwwwww" and "000"
        // wraps alone; with it the whole number wraps as a unit.
        let rows = lay("<p>wwwwww 10\u{a0}000</p>", 10);
        let lines: Vec<String> = rows.iter().map(render_row).collect();
        assert!(
            lines.iter().any(|l| l.contains("10\u{a0}000")),
            "the nbsp-glued number stays on one line: {lines:?}"
        );
        // Runs of NBSP are preserved, not collapsed to one space.
        let text = all_text(&lay("<p>A\u{a0}\u{a0}\u{a0}B</p>", 20));
        assert!(
            text.contains("A\u{a0}\u{a0}\u{a0}B"),
            "nbsp run preserved: {text:?}"
        );
        // Ordinary whitespace runs still collapse to a single space.
        let text = all_text(&lay("<p>A  \n  B</p>", 20));
        assert!(text.contains("A B"), "plain runs collapse: {text:?}");
    }

    /// HTML rendering, "the details element": a `<details>` without `open`
    /// renders only its first `<summary>` — the rest of the content (elements
    /// AND bare text) is not rendered until it opens.
    #[test]
    fn closed_details_renders_only_its_summary() {
        let text = all_text(&lay(
            "<details><summary>More info</summary><p>SECRET</p>loose text</details>",
            60,
        ));
        assert!(text.contains("More info"), "summary shows: {text:?}");
        assert!(!text.contains("SECRET"), "closed content hidden: {text:?}");
        assert!(!text.contains("loose"), "closed bare text hidden: {text:?}");
        let text = all_text(&lay(
            "<details open><summary>More info</summary><p>SECRET</p></details>",
            60,
        ));
        assert!(
            text.contains("SECRET"),
            "open details shows its content: {text:?}"
        );
    }

    /// CSS 2.1 §17.4: a `<caption>` renders as a block above the table's grid
    /// (below for `caption-side:bottom`), centered by the UA default. It used
    /// to be dropped entirely (`table_cell_rows` skips caption children).
    #[test]
    fn table_caption_renders_above_the_grid_and_centered() {
        let html = "<table><caption>Monthly Savings</caption>\
            <tr><td>January</td><td>100</td></tr></table>";
        let rows = lay(html, 60);
        let row_of = |rows: &[Row], needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
                .unwrap_or_else(|| panic!("{needle} laid out"))
        };
        let cap_row = row_of(&rows, "Monthly");
        assert!(
            cap_row < row_of(&rows, "January"),
            "caption sits above the grid"
        );
        let cap = rows[cap_row]
            .items
            .iter()
            .find(|i| i.text.contains("Monthly"))
            .unwrap();
        assert!(
            cap.col > 0,
            "UA default centers the caption, col={}",
            cap.col
        );
        let html = "<table><caption style='caption-side:bottom'>Legend</caption>\
            <tr><td>January</td><td>100</td></tr></table>";
        let rows = lay(html, 60);
        assert!(
            row_of(&rows, "Legend") > row_of(&rows, "January"),
            "caption-side:bottom flows it below the grid"
        );
    }

    /// A COLUMN flex container's cross-axis alignment (CSS Flexbox §8.3):
    /// `align-items:center` sizes each item to FIT-CONTENT and centers it in
    /// the band — including shrink-wrapping a `1fr` grid inside (CSS Grid
    /// §7.2.3: flexible tracks size to content during intrinsic sizing, so
    /// the grid can't inflate the fit measure to the whole band). Steam's
    /// login page centers its bounded card this way; stretching it instead
    /// pinned the card's QR pane to the terminal's far right edge.
    #[test]
    fn column_flex_align_center_shrink_wraps_and_centers() {
        let html = r#"<style>
            .page{display:flex;flex-direction:column;align-items:center}
            .card{display:grid;grid-template-columns:1fr;gap:12px}
            .row{display:flex;flex-direction:row}
            .frame{width:160px}
        </style>
        <div class="page"><div><div class="card"><div class="row">
            <div><p>USERNAME</p></div>
            <div class="frame"><p>QR</p></div>
        </div></div></div></div>"#;
        let rows = lay(html, 200);
        let name = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("USERNAME"))
            .expect("label laid out");
        let qr = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("QR"))
            .expect("QR cell laid out");
        // Shrink-wrapped: the card is ~label + 160px frame (~33 cells), so
        // centered in 200 the label starts far from column 0 and the frame's
        // right edge lands far from column 199.
        assert!(
            name.col >= 60,
            "card must be centered, label at col {}",
            name.col
        );
        assert!(
            (qr.col + qr.width) <= 160,
            "card must shrink-wrap, QR cell ends at {}",
            qr.col + qr.width
        );
        // And the default (no align-items) still stretches: same structure
        // minus the align lays the label at the left edge.
        let html_stretch = html.replace(";align-items:center", "");
        let rows = lay(&html_stretch, 200);
        let name = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("USERNAME"))
            .expect("label laid out");
        assert!(
            name.col < 10,
            "default stretch keeps the left edge, label at col {}",
            name.col
        );
    }

    /// `image-rendering: pixelated` (CSS Images 3 §5.4) rides the Image item
    /// to the encoder, which then upscales nearest-neighbor — hard-edged
    /// blocks, a scannable QR — instead of Lanczos smoothing. Inherited per
    /// spec, so a wrapper's declaration reaches the `<img>`.
    #[test]
    fn image_rendering_pixelated_marks_the_image_item() {
        let mut images = ImageSizes::new();
        images.insert("https://example.com/qr.gif".to_string(), (5, 3));
        images.insert("https://example.com/photo.jpg".to_string(), (5, 3));
        let html = r#"<style>.qrwrap{image-rendering:pixelated}</style>
            <div class="qrwrap"><img src="qr.gif" alt=""></div>
            <div><img src="photo.jpg" alt=""></div>"#;
        let rows = lay_with_images(html, 80, &images);
        let find = |needle: &str| {
            rows.iter()
                .flat_map(|r| &r.items)
                .find(|it| it.image.as_deref().is_some_and(|u| u.contains(needle)))
                .unwrap_or_else(|| panic!("{needle} laid out"))
                .pixelated
        };
        assert!(find("qr.gif"), "pixelated inherits onto the img");
        assert!(!find("photo.jpg"), "default images stay smooth");
    }

    /// A non-growing flex item (`flex:0` — basis 0, grow 0) still gets its
    /// AUTOMATIC MINIMUM size (CSS Flexbox §4.5: `min-width:auto` = the
    /// content-based minimum), including a definite-width box nested deeper in
    /// the item (CSS Sizing §5: a definite preferred size doesn't compress at
    /// min-content). Steam's login: the QR pane is `flex:0` beside a `flex:1`
    /// form, its 160px frame four wrappers down — without the automatic
    /// minimum the pane collapsed to zero and the whole QR column vanished.
    #[test]
    fn flex_zero_item_keeps_its_content_based_minimum() {
        let mut images = ImageSizes::new();
        images.insert("blob:https://x/qr".to_string(), (5, 3)); // 41x41 GIF
        let html = r#"<style>
            .row{display:flex;flex-direction:row}
            .form{flex:1}
            .qr{flex:0;display:grid;gap:4px;margin-left:40px}
            .center{display:flex;flex-direction:column;align-items:center}
            .rel{position:relative}
            .frame{display:grid;width:160px;height:160px}
            .img{width:100%}
        </style>
        <div class="row">
          <div class="form"><p>account name</p><p>password</p></div>
          <div class="qr"><div><label>Or sign in with QR</label>
            <div class="center"><div class="rel"><div class="frame">
              <img class="img" src="blob:https://x/qr" alt="">
            </div></div></div>
          </div></div>
        </div>"#;
        let rows = lay_with_images(html, 120, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.kind == ItemKind::Image && it.image.is_some())
            .expect("the QR image laid out");
        // The pane holds its 160px (20-cell) frame; the img fills it via
        // width:100% (not its 5-cell intrinsic), beside the form (not under).
        assert!(
            img.width >= 15,
            "img must fill the definite frame, got {}",
            img.width
        );
        assert!(
            img.col >= 60,
            "QR pane must sit beside the flex:1 form, got col {}",
            img.col
        );
    }
    #[test]
    fn font_size_zero_text_renders_nothing_not_one_char_per_line() {
        // The fosstodon glitch: Mastodon hides a link's URL scheme and tail with
        // `.invisible{font-size:0}` and shows only the middle. Without honoring
        // font-size:0 the zero-width invisible text wrapped ONE CHARACTER PER
        // LINE. font-size:0 text must paint nothing; the visible middle stays.
        let html = "<style>.invisible{font-size:0}</style>\
            <p>See: <a href='https://example.com/en/miniFantasyTeater/059.html'>\
            <span class='invisible'>https://</span>\
            <span>example.com/en/miniFantas</span>\
            <span class='invisible'>yTeater/059.html</span></a></p>";
        let rows = lay(html, 80);
        let text = all_text(&rows);
        assert!(
            text.contains("example.com/en/miniFantas"),
            "visible middle missing: {text:?}"
        );
        assert!(!text.contains("https"), "invisible scheme leaked: {text:?}");
        assert!(!text.contains("059"), "invisible tail leaked: {text:?}");
        assert!(!text.contains("Teater"), "invisible tail leaked: {text:?}");
    }

    #[test]
    fn absolute_font_size_reset_reshows_text_under_a_zero_parent() {
        // The inline-block-whitespace-killer idiom: `font-size:0` on a parent to
        // remove gaps, with an absolute reset on children. font-size:0 hides only
        // an element's OWN text — an absolutely-reset descendant re-shows.
        let html = "<style>.z{font-size:0}.r{font-size:1rem}</style>\
            <div class='z'>GONE<span class='r'>SHOWN</span></div>";
        let text = all_text(&lay(html, 80));
        assert!(text.contains("SHOWN"), "reset text hidden: {text:?}");
        assert!(!text.contains("GONE"), "zero-size text leaked: {text:?}");
    }

    #[test]
    fn a_zero_sized_replaced_image_renders_nothing() {
        // The image half of the `.invisible` idiom: Mastodon collapses images with
        // `img{width:0;height:0}` (no overflow rule). A zero-sized replaced element
        // paints nothing — not the 1-cell sliver our box would otherwise clamp to.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cat.png".to_owned(), (10, 4));
        let rows = lay_with_images(
            r#"<body>A<img src="/cat.png" alt="cat" style="width:0;height:0">B</body>"#,
            40,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none()),
            "zero-sized image leaked a box: {rows:#?}"
        );
    }

    #[test]
    fn font_size_zero_leaves_a_real_image_visible() {
        // `font-size:0` hides TEXT, never a replaced element — a browser still
        // paints an image inside a font-size:0 span. (Mastodon pairs it with an
        // explicit width:0 to hide the image; absent that, the image shows.)
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cat.png".to_owned(), (10, 4));
        let rows = lay_with_images(
            r#"<body><span style="font-size:0"><img src="/cat.png" alt="cat"></span></body>"#,
            40,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.image.is_some()),
            "font-size:0 wrongly hid the image: {rows:#?}"
        );
    }

    #[test]
    fn viewport_height_units_including_dynamic_resolve_like_vh() {
        // A terminal has no dynamic browser chrome, so the small/large/dynamic-
        // viewport keywords all equal `vh` (Mastodon's `height:100dvh`).
        for u in ["vh", "dvh", "svh", "lvh"] {
            assert_eq!(
                css_height_rows_f32(&format!("100{u}"), 40, Units::default()),
                Some(40.0),
                "{u}"
            );
            assert_eq!(
                css_height_rows_f32(&format!("50{u}"), 40, Units::default()),
                Some(20.0),
                "{u}"
            );
        }
        // Unknown viewport height ⇒ None (unchanged), non-viewport length still ok.
        assert_eq!(css_height_rows_f32("100dvh", 0, Units::default()), None);
        assert!(css_height_rows_f32("2em", 40, Units::default()).is_some());
    }

    #[test]
    fn a_fixed_rail_is_captured_into_the_pinned_layer_at_its_flex_column() {
        // Mastodon's pattern: a centered flex row with a `min-width` spacer pane
        // whose only child is `position:fixed` (all insets auto). The fixed rail
        // must be captured into the PINNED overlay layer at the pane's reserved
        // (centered) column — NOT laid into the scrolling document.
        let html = "<div style='display:flex;justify-content:center'>\
            <div style='min-width:100px'>\
              <div style='position:fixed;width:100px'>PINNED_RAIL</div>\
            </div>\
            <main style='width:100px'>CENTER_FEED</main>\
          </div>";
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, _c, _rg, _cl, _b, fixed, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 20),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        let doc_text: String = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|i| i.text.as_str())
            .collect();
        // The center feed scrolls in the document; the rail is NOT in it.
        assert!(
            doc_text.contains("CENTER_FEED"),
            "center in doc: {doc_text:?}"
        );
        assert!(
            !doc_text.contains("PINNED_RAIL"),
            "rail must be pinned, not in the scrolling doc: {doc_text:?}"
        );
        // The pinned layer holds exactly the rail, at a non-zero column (the
        // centered flex column — proving the static-position translation).
        assert_eq!(fixed.len(), 1, "one fixed rail captured");
        let rail_text: String = fixed[0]
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|i| i.text.as_str())
            .collect();
        assert!(
            rail_text.contains("PINNED_RAIL"),
            "rail text: {rail_text:?}"
        );
        assert!(
            fixed[0].col > 5,
            "rail pinned at its centered column, not 0: col={}",
            fixed[0].col
        );
    }

    #[test]
    fn flex_auto_min_is_clamped_by_max_width_so_the_row_does_not_stack() {
        // CSS Flexbox §4.5: the content-based minimum size "is clamped by the
        // maximum main size if it's definite". A `width:100%; max-width:` feed
        // column holding one long unbreakable token (a URL pasted as plain
        // text — fosstodon's virtualized post placeholders) must floor at its
        // max-width, NOT its min-content — otherwise the row overflows at
        // minimum and the whole 3-column layout falls back to a stack,
        // dropping the fixed rails.
        let long_token = "x".repeat(90);
        let html = format!(
            "<div style='display:flex;justify-content:center'>\
               <div style='min-width:100px'>\
                 <div style='position:fixed;width:100px'>LEFT_RAIL</div>\
               </div>\
               <main style='width:100%;max-width:320px;flex-grow:0;flex-shrink:1'>\
                 <p>{long_token}</p>\
               </main>\
               <div style='min-width:100px'>\
                 <div style='position:fixed;width:100px'>RIGHT_RAIL</div>\
               </div>\
             </div>"
        );
        let dom = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, _c, _rg, _cl, _b, fixed, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (100, 20),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        // Both rails captured — the row laid as columns, not a stack.
        assert_eq!(fixed.len(), 2, "both rails captured: {}", fixed.len());
        assert!(
            fixed[0].row == 0 && fixed[1].row == 0,
            "rails pin at the top (side-by-side), not stacked below the feed: rows {}/{}",
            fixed[0].row,
            fixed[1].row
        );
        assert!(
            fixed[1].col > fixed[0].col,
            "rails at distinct flanking columns: {}/{}",
            fixed[0].col,
            fixed[1].col
        );
        // The unbreakable token lives in the (clipped) feed column, and the
        // feed itself is in the scrolling document.
        let doc_text: String = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|i| i.text.as_str())
            .collect();
        assert!(doc_text.contains('x'), "feed content in doc");
    }

    #[test]
    fn a_zero_height_stacked_box_still_propagates_its_pinned_fixed_rail() {
        // A column-flex stack whose item's ONLY content is a pinned
        // `position:fixed` rail lays as a zero-height box — the stack must
        // still propagate the captured overlay instead of dropping it with
        // the box (the same rule the flex-row spacer-pane guard applies).
        let html = "<div style='display:flex;flex-direction:column'>\
            <div>\
              <div style='position:fixed;width:50px'>STACKED_RAIL</div>\
            </div>\
            <p>normal flow content</p>\
          </div>";
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, _c, _rg, _cl, _b, fixed, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 20),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert_eq!(fixed.len(), 1, "rail captured through the stack");
        let rail_text: String = fixed[0]
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|i| i.text.as_str())
            .collect();
        assert!(rail_text.contains("STACKED_RAIL"), "rail: {rail_text:?}");
        let doc_text: String = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|i| i.text.as_str())
            .collect();
        assert!(
            !doc_text.contains("STACKED_RAIL"),
            "rail pinned, not in the doc: {doc_text:?}"
        );
        assert!(doc_text.contains("normal flow content"));
    }

    #[test]
    fn viewport_width_units_including_dynamic_resolve_in_cells() {
        let vp = (100usize, 40usize);
        for u in ["vw", "dvw", "svw", "lvw"] {
            assert_eq!(
                resolve_cells(&format!("100{u}"), 0, vp, Units::default()),
                Some(100),
                "{u}"
            );
        }
        for u in ["vh", "dvh", "svh", "lvh"] {
            assert_eq!(
                resolve_cells(&format!("100{u}"), 0, vp, Units::default()),
                Some(40),
                "{u}"
            );
        }
        assert_eq!(resolve_cells("100dvmin", 0, vp, Units::default()), Some(40)); // min(100,40)
        assert_eq!(
            resolve_cells("100lvmax", 0, vp, Units::default()),
            Some(100)
        ); // max(100,40)
    }

    fn measure(html: &str, width: usize) -> (Dom, HashMap<NodeId, PxRect>) {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let boxes = measure_boxes(
            &dom,
            &base,
            (width, 0),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &ImageSizes::new(),
        );
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
    #[ignore = "manual diagnostic, needs TRUST_LAYOUT_FILE=<html>"]
    fn measure_dump() {
        // Companion to http::tests::layout_dump: run the MEASURE pass on the
        // same html file, so app-render vs engine-measure divergence can be
        // split into input (arena vs serialized) or internal causes.
        let Ok(path) = std::env::var("TRUST_LAYOUT_FILE") else {
            eprintln!("set TRUST_LAYOUT_FILE to a post-JS html file");
            return;
        };
        let html = std::fs::read_to_string(&path).unwrap();
        let (w, h): (usize, usize) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((80, 24));
        let mut dom = Dom::parse_document(&html);
        dom.rewrite_inline_svgs();
        let base = Url::parse("https://store.steampowered.com/").unwrap();
        let mut images = ImageSizes::new();
        if let Ok(spec) = std::env::var("TRUST_LAYOUT_IMG_CELL")
            && let Some((cw, ch)) = spec
                .split_once('x')
                .and_then(|(a, b)| Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?)))
        {
            for id in 0..dom.node_count() {
                if dom.tag_name(id) == Some("img")
                    && let Some(src) = dom.attr(id, "src")
                {
                    let key = if src.trim().starts_with("data:") {
                        src.trim().to_string()
                    } else if let Ok(u) = base.join(src.trim()) {
                        u.to_string()
                    } else {
                        continue;
                    };
                    images.insert(key, (cw, ch));
                }
            }
        }
        let boxes = measure_boxes(
            &dom,
            &base,
            (w, h),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &images,
        );
        let max_bottom = boxes
            .values()
            .map(|r| r.top + r.height)
            .fold(0.0f64, f64::max);
        eprintln!(
            "MEASURE_DUMP boxes={} doc_bottom_px={max_bottom} (rows≈{})",
            boxes.len(),
            (max_bottom / 16.0).round()
        );
        // The render path on the SAME input, for divergence accounting.
        let (rows, _carousels, regions, _clips, _bounds, _fixed, _anchors) =
            lay_out_with_carousels(&dom, &base, (w, h), &[], &ControlMap::new(), &images, false);
        let mut clipped = 0usize;
        for r in &regions {
            clipped += r.buffer.len().saturating_sub(r.height as usize);
        }
        eprintln!(
            "MEASURE_DUMP render_rows={} regions={} clipped_away_rows={clipped}",
            rows.len(),
            regions.len()
        );
    }

    #[test]
    fn measure_skips_suppressed_out_of_flow_boxes_like_the_render() {
        // Geometry reports what we render (the binding rule). The render skips
        // paint-suppressed out-of-flow boxes entirely (placing Steam's ~13
        // hidden opacity:0 carousel pages buried the real grid); the measure
        // pass must skip them IDENTICALLY — once decoded image sizes fed the
        // measure pass, placing them ballooned the measured document to ~4×
        // the rendered one, so every section below "measured" viewports past
        // the viewport and one-shot lazy-image watchers never fired (the
        // Steam blank-capsules regression). The suppressed box still gets an
        // honest zero-height rect at its computed position; content following
        // the containing block measures right below the VISIBLE content.
        let html = r#"<body>
            <div id="cb" style="position:relative">
                <div id="vis">visible card</div>
                <div id="hidden" style="position:absolute;opacity:0">
                    h1<br>h2<br>h3<br>h4<br>h5<br>h6<br>h7<br>h8<br>h9<br>h10
                </div>
            </div>
            <div id="after">after the carousel</div>
        </body>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let find = |id: &str| {
            dom.descendants(DOCUMENT)
                .into_iter()
                .find(|&n| dom.attr(n, "id") == Some(id))
                .unwrap()
        };
        let boxes = measure_boxes(
            &dom,
            &base,
            (40, 24),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &ImageSizes::new(),
        );
        let vis = boxes.get(&find("vis")).expect("visible content has a box");
        let after = boxes.get(&find("after")).expect("following content");
        assert_eq!(
            after.top,
            vis.top + vis.height,
            "content after the CB sits right below the visible content — the \
             suppressed box consumed no document height"
        );
        let hidden = boxes
            .get(&find("hidden"))
            .expect("the suppressed box still gets a rect at its position");
        assert_eq!(hidden.height, 0.0, "zero-height: it paints nothing");
    }

    #[test]
    fn measure_boxes_lays_images_from_decoded_sizes() {
        // Geometry must report the layout the page really renders (CSSOM
        // View): the measure pass receives the app's decoded intrinsic sizes
        // (PageCmd::ImageSizes) and reserves the same rows the render does. A
        // virtualized feed caches these heights and declares them back as
        // placeholder sizes, so a divergence here reshapes the document under
        // the reader (the Mastodon feed-scroll bug).
        let html = r#"<body><img id="pic" src="/a.png" alt="a"><div id="after">x</div></body>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let find = |id: &str| {
            dom.descendants(DOCUMENT)
                .into_iter()
                .find(|&n| dom.attr(n, "id") == Some(id))
                .unwrap()
        };
        let (pic, after) = (find("pic"), find("after"));
        let undecoded = measure_boxes(
            &dom,
            &base,
            (40, 24),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &ImageSizes::new(),
        );
        let after_top_before = undecoded.get(&after).unwrap().top;
        let mut sizes = ImageSizes::new();
        sizes.insert(String::from("https://example.com/a.png"), (10, 5));
        let decoded = measure_boxes(
            &dom,
            &base,
            (40, 24),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &sizes,
        );
        assert_eq!(
            decoded.get(&pic).unwrap().height,
            5.0 * 16.0,
            "the decoded image's box is its reserved rows, not the alt line"
        );
        assert!(
            decoded.get(&after).unwrap().top > after_top_before,
            "content below the image moves down by the decoded box"
        );
    }

    #[test]
    fn viewport_height_resolves_vh_lengths_end_to_end() {
        // Phase 0a: a `vh` length resolves once the viewport HEIGHT is threaded
        // through the public entry point — not just in the free-function unit
        // test. `width:50vh` of a 24-row viewport = 12 cells; cell_px width 8 →
        // 96px. The same markup with an UNKNOWN height (0) leaves `vh`
        // unresolved (no width floor → bare content extent), proving the
        // threaded height is what unlocked it, not a baked constant.
        let html = r#"<body><div id="box" style="width:50vh">·</div></body>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let id = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some("box"))
            .unwrap();
        let with_h = measure_boxes(
            &dom,
            &base,
            (40, 24),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &ImageSizes::new(),
        );
        assert_eq!(
            with_h.get(&id).unwrap().width,
            96.0,
            "50vh = 12 cells = 96px once the viewport height is threaded"
        );
        let no_h = measure_boxes(
            &dom,
            &base,
            (40, 0),
            &[],
            &ControlMap::new(),
            (8, 16),
            false,
            &ImageSizes::new(),
        );
        assert_eq!(
            no_h.get(&id).unwrap().width,
            8.0,
            "unknown viewport height ⇒ vh unresolved ⇒ no floor, just content"
        );
    }

    #[test]
    fn nested_clip_placeholder_converts_px_with_the_explicit_cell_height() {
        // Bug #12: sub-layouts fell back to the session-global cell height
        // instead of the pass's explicit `cell_px`, so a clip placeholder
        // nested in a flex item's sub-layout reserved the wrong row count
        // under `measure_boxes`. Latent while both were 16px; real for any
        // other terminal font size. At 32px cells, a 64px placeholder is TWO
        // rows (64px back), not the global-16px four rows (128px).
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6<br>l7<br>l8";
        let html = format!(
            r#"<body><div style="display:flex"><div style="flex:1"><div id="ph" style="height:64px;overflow:hidden;opacity:0">{tall}</div></div><div style="flex:1">side</div></div></body>"#
        );
        let dom = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let boxes = measure_boxes(
            &dom,
            &base,
            (80, 24),
            &[],
            &ControlMap::new(),
            (8, 32),
            false,
            &ImageSizes::new(),
        );
        let ph = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some("ph"))
            .unwrap();
        assert_eq!(
            boxes.get(&ph).expect("the placeholder has a box").height,
            64.0,
            "64px at 32px cells round-trips to 2 reserved rows = 64px"
        );
    }

    #[test]
    fn nested_visible_clip_box_reports_its_clipped_geometry() {
        // `clip_heights` is shared across sub-layouts now: a VISIBLE
        // `height:64px;overflow:hidden` box nested in a flex item's
        // sub-layout reports exactly 64px (4 rows × 16px) — its entry used to
        // die with the sub-layout's map, so it reported the full unclipped
        // content extent (10 rows = 160px).
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6<br>l7<br>l8<br>l9<br>l10";
        let (dom, m) = measure(
            &format!(
                r#"<body><div style="display:flex"><div style="flex:1"><div id="clip" style="height:64px;overflow:hidden">{tall}</div></div><div style="flex:1">side</div></div></body>"#
            ),
            80,
        );
        let n = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some("clip"))
            .unwrap();
        assert_eq!(
            m.get(&n).expect("the clip box has geometry").height,
            64.0,
            "capped to the declared clipped height, not the content extent"
        );
    }

    #[test]
    fn nested_declared_floor_survives_sub_layouts() {
        // `declared_boxes` is shared too: a sized block nested in a flex
        // item's sub-layout keeps its CSS floor (80×32px) instead of
        // collapsing to its 1-cell content extent.
        let (dom, m) = measure(
            r#"<body><div style="display:flex"><div style="flex:1"><div id="f" style="width:80px;height:32px">·</div></div><div style="flex:1">side</div></div></body>"#,
            80,
        );
        let n = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some("f"))
            .unwrap();
        let b = *m.get(&n).expect("the sized block has geometry");
        assert!(b.width >= 80.0, "declared width floor holds: {}", b.width);
        assert!(
            b.height >= 32.0,
            "declared height floor holds: {}",
            b.height
        );
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
    fn opacity_zero_block_keeps_its_measured_box() {
        // Phase 1 (VIRTUALIZED_LIST_LAYOUT_PLAN.md): `opacity:0` suppresses PAINT,
        // not box generation, so a paint-suppressed placeholder reports the SAME
        // measured box as its visible twin — the fix for the React virtualized-
        // list bug where an off-screen `opacity:0` placeholder collapsed (no box)
        // and `getBoundingClientRect` fell back to the whole viewport height. The
        // Mastodon shape: `height:128px;overflow:hidden` on both, one `opacity:0`.
        let (dom, m) = measure(
            r#"<body>
                <article id="vis" style="height:128px;overflow:hidden">real post content that is quite long</article>
                <article id="inv" style="height:128px;overflow:hidden;opacity:0">real post content that is quite long</article>
            </body>"#,
            80,
        );
        let vis = box_by_id(&dom, &m, "vis");
        let inv = box_by_id(&dom, &m, "inv");
        assert_eq!(
            inv.height, vis.height,
            "opacity:0 placeholder reports the same box as its visible twin"
        );
        assert_eq!(
            inv.height, 128.0,
            "the box is its real 128px, not the viewport"
        );
        // The suppressed article sits BELOW the visible one — proving it takes
        // part in normal flow rather than collapsing. (Declared height is
        // geometry-only, not reserved on screen — the documented deviation — so
        // this compares flow position, not the 128px geometry box.)
        assert!(
            inv.top > vis.top,
            "in-flow opacity:0 reserves flow space (inv.top {} below vis.top {})",
            inv.top,
            vis.top
        );
    }

    #[test]
    fn overflow_hidden_definite_height_caps_the_measured_box() {
        // Phase 1 step 4: `overflow:hidden` + a definite `height` clips the
        // content, so the measured box is EXACTLY the height even when the
        // (unclipped) content is much taller — a virtualized placeholder caches
        // its measured height then clips a full article into it. Ten lines of
        // content in a 3-row (48px) clipped box report 48px; the same content
        // with visible overflow extends past the declared height.
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6<br>l7<br>l8<br>l9<br>l10";
        let (dom, m) = measure(
            &format!(
                r#"<body><div id="clip" style="height:48px;overflow:hidden">{tall}</div></body>"#
            ),
            80,
        );
        assert_eq!(
            box_by_id(&dom, &m, "clip").height,
            48.0,
            "overflow:hidden + height:48 caps the box to 48px (3 rows)"
        );
        let (dom2, m2) = measure(
            &format!(r#"<body><div id="noclip" style="height:48px">{tall}</div></body>"#),
            80,
        );
        assert!(
            box_by_id(&dom2, &m2, "noclip").height > 48.0,
            "overflow:visible content extends past the declared height (no cap)"
        );
    }

    #[test]
    fn opacity_zero_content_is_laid_out_but_painted_blank() {
        // Render side: an in-flow `opacity:0` block is laid into the row grid
        // (its text present as items, so the box reserves space) but every item
        // is flagged `invisible` — the renderer writes blank cells. Its visible
        // sibling is untouched.
        let rows = lay(
            r#"<body><div style="opacity:0">SECRET</div><div>SHOWN</div></body>"#,
            80,
        );
        let secret: Vec<&Item> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.text.contains("SECRET"))
            .collect();
        assert!(
            !secret.is_empty(),
            "opacity:0 content is laid out (reserves space): {:?}",
            texts(&rows)
        );
        assert!(
            secret.iter().all(|i| i.invisible),
            "every opacity:0 item is painted blank"
        );
        assert!(shows(&rows, "SHOWN"), "the visible sibling renders");
        assert!(
            !shows(&rows, "SECRET"),
            "the opacity:0 block does not visibly show"
        );
    }

    #[test]
    fn opacity_zero_subtree_paints_blank_but_a_reset_is_not_re_revealed() {
        // Opacity applies to the subtree as a GROUP: a nested element cannot
        // re-reveal itself (unlike visibility — that's Phase 2). So a
        // `opacity:1` child of an `opacity:0` parent still paints blank.
        let rows = lay(
            r#"<body><div style="opacity:0"><span style="opacity:1">CHILD</span></div></body>"#,
            80,
        );
        assert!(
            !shows(&rows, "CHILD"),
            "opacity:1 child of an opacity:0 parent stays suppressed (group)"
        );
    }

    #[test]
    fn visibility_hidden_is_laid_out_but_painted_blank() {
        // Phase 2: `visibility:hidden` (CSS2 §11.2) is paint suppression, not box
        // removal — laid out (reserves space), painted blank. Its visible sibling
        // renders normally. Mirrors the opacity:0 case but via `visibility`.
        let rows = lay(
            r#"<body><div style="visibility:hidden">HIDDENTEXT</div><div>SHOWNTEXT</div></body>"#,
            80,
        );
        let hidden: Vec<&Item> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.text.contains("HIDDENTEXT"))
            .collect();
        assert!(
            !hidden.is_empty(),
            "visibility:hidden content is laid out (reserves space): {:?}",
            texts(&rows)
        );
        assert!(
            hidden.iter().all(|i| i.invisible),
            "visibility:hidden items are painted blank"
        );
        assert!(shows(&rows, "SHOWNTEXT"), "the visible sibling renders");
        assert!(!shows(&rows, "HIDDENTEXT"), "hidden text does not show");
    }

    #[test]
    fn visibility_visible_descendant_of_a_hidden_ancestor_is_painted() {
        // The KEY difference from opacity: `visibility` inherits but is
        // RE-CLEARABLE. A `visibility:visible` descendant of a
        // `visibility:hidden` ancestor IS painted (a plain nested child stays
        // hidden — inheritance). This is why visibility is a per-element cascade
        // read, not the sticky opacity chain.
        let rows = lay(
            r#"<body><div style="visibility:hidden">
                 PARENTHIDDEN
                 <span>STILLHIDDEN</span>
                 <span style="visibility:visible">RESHOWN</span>
               </div></body>"#,
            80,
        );
        assert!(
            !shows(&rows, "PARENTHIDDEN"),
            "the hidden element's own text is blank: {:?}",
            texts(&rows)
        );
        assert!(
            !shows(&rows, "STILLHIDDEN"),
            "a plain child inherits visibility:hidden"
        );
        assert!(
            shows(&rows, "RESHOWN"),
            "a visibility:visible descendant is re-painted: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn visibility_hidden_keeps_its_measured_box() {
        // Like opacity:0, a `visibility:hidden` block keeps its real box for
        // geometry (`getBoundingClientRect`) — CSS2 §11.2 lays it out fully.
        let (dom, m) = measure(
            r#"<body>
                <div id="vis" style="height:64px">visible</div>
                <div id="inv" style="height:64px;visibility:hidden">hidden</div>
            </body>"#,
            80,
        );
        let vis = box_by_id(&dom, &m, "vis");
        let inv = box_by_id(&dom, &m, "inv");
        assert_eq!(inv.height, vis.height, "visibility:hidden keeps its box");
        assert!(inv.top > vis.top, "and reserves its flow space");
    }

    #[test]
    fn visibility_hidden_survives_serialize_and_reparse_as_blank() {
        // The JS pipeline serializes post-cascade HTML (no `<style>`) then
        // re-parses it for layout, so `visibility:hidden` must be BAKED. Round-
        // trip through the serializer and confirm the re-parsed layout still
        // paints the subtree blank (the fix for the no-JS / js-off / css_bake
        // paths, where the layout reads only baked inline styles).
        let dom = Dom::parse_document(
            r#"<head><style>.gone{visibility:hidden}</style></head>
               <body><p class="gone">BAKEDHIDDEN</p><p>BAKEDSHOWN</p></body>"#,
        );
        let html = dom.serialize(DOCUMENT);
        assert!(
            html.contains("visibility:hidden"),
            "suppression baked into the serialized HTML: {html}"
        );
        let reparsed = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, ..) = lay_out_with_carousels(
            &reparsed,
            &base,
            (80, 0),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert!(shows(&rows, "BAKEDSHOWN"), "the visible sibling re-parses");
        assert!(
            !shows(&rows, "BAKEDHIDDEN"),
            "the baked visibility:hidden re-parses as blank: {:?}",
            texts(&rows)
        );
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
        let (rows, carousels, _regions, ..) =
            lay_out_with_carousels(&dom, &base, (w, 0), &[], &ControlMap::new(), &images, true);
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
    fn abspos_never_pushes_a_later_sibling() {
        // CSS 2.1 §9.3.1: "Absolutely positioned boxes are taken out of the
        // normal flow. This means they have no impact on the layout of later
        // siblings." A tall overlay inside a relative wrapper must NOT push the
        // wrapper's following sibling down by the overlay's height — the overlay
        // composites over/after the flow and only extends the scrollable region
        // (CSS Overflow 3 §2.2). The old "grow the containing block to contain
        // its positioned children" model pushed the sibling a whole overlay
        // down (NBC's header, a viewport of blank rows).
        // Margin-free divs so the gap is pure flow, and a 10-line overlay so a
        // push would be unmistakable (~10 rows) versus not (~1).
        let html = "<body>\
            <div style=\"position:relative\"><div>ONE</div>\
              <div style=\"position:absolute;top:0;left:0\">\
                <div>AA</div><div>BB</div><div>CC</div><div>DD</div><div>EE</div>\
                <div>FF</div><div>GG</div><div>HH</div><div>II</div><div>JJ</div></div>\
            </div>\
            <div>NEXT</div></body>";
        let rows = lay(html, 40);
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
                .unwrap_or_else(|| panic!("{needle} not laid: {:?}", texts(&rows)))
        };
        let (one, next) = (row_of("ONE"), row_of("NEXT"));
        // NEXT follows ONE by the wrapper's IN-FLOW height (1 row), NOT by the
        // 10-line overlay stacked inside it.
        assert!(
            next.saturating_sub(one) <= 2,
            "NEXT pushed by the overlay (ONE@{one} NEXT@{next})"
        );
        // The overlay content is still reachable — composited into the document,
        // not dropped.
        let text = all_text(&rows);
        assert!(
            text.contains("AA") && text.contains("EE"),
            "overlay reachable"
        );
    }

    #[test]
    fn clipped_full_viewport_drawer_reserves_no_flow() {
        // NBC's collapsed off-canvas panel: a `height:100vh` drawer, offset into
        // the box (`top`), inside an `overflow:hidden` wrapper with no in-flow
        // content — so the wrapper is 0 rows tall (§9.3.1) and clips the drawer
        // to nothing (CSS Overflow 3 §3). A browser shows the drawer nowhere and
        // lays the following content right after the (empty) wrapper; the old
        // model reserved a whole viewport of blank rows and buried the page.
        let html = "<body>\
            <div style=\"position:relative;overflow:hidden\">\
              <div style=\"position:absolute;top:60px;left:0;width:100%;height:100vh\">DRAWER</div>\
            </div>\
            <p>CONTENT</p></body>";
        let rows = lay_out_with_carousels(
            &Dom::parse_document(html),
            &Url::parse("https://example.com/").unwrap(),
            (100, 40),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        )
        .0;
        assert!(
            !all_text(&rows).contains("DRAWER"),
            "the clipped drawer paints nothing: {:?}",
            texts(&rows)
        );
        let content_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("CONTENT")))
            .expect("CONTENT laid");
        assert!(
            content_row <= 1,
            "CONTENT sits at the top, not a viewport down (row {content_row})"
        );
    }

    #[test]
    fn flex_row_reports_used_width_past_its_content() {
        // CSS Flexbox §9.9.1: a flex item's max-content contribution is its flex
        // BASE SIZE, not its (narrower) content. A flex row of fixed-width,
        // non-shrinking cards whose content is a short label must report its full
        // used width, so an ANCESTOR flex row that measures it hands it enough
        // space and lays it horizontally — Steam's `hero_capsule` spotlight
        // carousel (a `width:28vw` box holding a one-line title) collapsed into a
        // vertical column when the container measured only to the last title.
        let html = "<body><div style=\"display:flex\">\
            <div style=\"display:flex;gap:1px\">\
              <div style=\"width:200px;flex-shrink:0\"><span>AA</span></div>\
              <div style=\"width:200px;flex-shrink:0\"><span>BB</span></div>\
              <div style=\"width:200px;flex-shrink:0\"><span>CC</span></div>\
            </div></div></body>";
        let rows = lay(html, 100);
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
        };
        // All three cards land on the SAME row (laid side by side), not stacked
        // into a column.
        assert_eq!(
            row_of("AA"),
            row_of("BB"),
            "AA and BB share a row: {rows:?}"
        );
        assert_eq!(
            row_of("BB"),
            row_of("CC"),
            "BB and CC share a row: {rows:?}"
        );
    }

    #[test]
    fn abspos_inside_pinned_fixed_rail_composites() {
        // A pinned `position:fixed` rail's rows are an overlay layer
        // (`FixedItem.rows`) — the document-root composite can't paint into it,
        // so `capture_fixed_children` must composite the rail's own abspos
        // descendants (badges, dropdown carets) into the box before capturing.
        // Dropping them left Mastodon-style rails missing their overlays.
        let html = "<body><div class=\"columns\">\
            <div style=\"position:fixed\"><div>RAIL</div>\
              <div style=\"position:relative\">\
                <div style=\"position:absolute;top:0;left:0\">BADGE</div>x</div>\
            </div>\
            <p>a</p><p>b</p><p>c</p></div></body>";
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (.., fixed, _an) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 24),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        let fixed_text: String = fixed
            .iter()
            .flat_map(|f| f.rows.iter())
            .flat_map(|r| r.items.iter())
            .map(|i| i.text.as_str())
            .collect();
        assert!(fixed_text.contains("RAIL"), "rail content present");
        assert!(
            fixed_text.contains("BADGE"),
            "abspos badge inside the fixed rail composites into its rows: {fixed_text:?}"
        );
    }

    #[test]
    fn abspos_inside_scroll_region_buffer_composites() {
        // A clipped scroll region's buffer is windowed by the app — its abspos
        // content is part of the scroller's scrollable overflow (CSS Overflow 3
        // §2.2) and must composite INTO the buffer (`flow_region`), not vanish
        // (the document-root composite can never reach a windowed buffer).
        let mut body = String::from(
            "<body><div style=\"height:64px;overflow-y:scroll\">\
             <div style=\"position:relative\">\
             <div style=\"position:absolute;top:0;right:0\">BADGE</div>",
        );
        for i in 0..30 {
            body.push_str(&format!("<p>line {i}</p>"));
        }
        body.push_str("</div></div><p>AFTER</p></body>");
        let dom = Dom::parse_document(&body);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, _c, regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (80, 24),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        assert!(!regions.is_empty(), "a region formed: {:?}", texts(&rows));
        let buf_text: String = regions
            .iter()
            .flat_map(|rg| rg.buffer.iter())
            .flat_map(|r| r.items.iter())
            .map(|i| i.text.as_str())
            .collect();
        assert!(buf_text.contains("line 3"), "region content present");
        assert!(
            buf_text.contains("BADGE"),
            "abspos badge composites into the region buffer: {buf_text:?}"
        );
    }

    #[test]
    fn abspos_shrink_to_fit_flex_keeps_right_anchor() {
        // The §9.9.1 measured-width floor is MEASURE-ONLY: on the render pass
        // grow has been applied, and flooring the reported width there made a
        // shrink-to-fit `right:0` flyout holding a `flex:1` row report the
        // whole band as its used width — left = cb_w − used_w = 0, the flyout
        // lost its right anchor and stretched full-width.
        let html = "<body><div style=\"position:relative\"><div>x</div>\
            <div style=\"position:absolute;top:0;right:0\">\
              <div style=\"display:flex\"><div style=\"flex:1\">MENU</div></div>\
            </div></div></body>";
        let rows = lay(html, 80);
        let menu = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("MENU"))
            .expect("menu laid");
        assert!(
            menu.col > 40,
            "right-anchored flyout stays right-anchored (col {})",
            menu.col
        );
    }

    #[test]
    fn sibling_badge_lifts_share_one_band() {
        // Two sibling cards, each a corner badge over a decoded image: the
        // image-lifts must SHARE one inserted band (`composite_positioned`'s
        // band reuse) — one insert per box staircased the badges (SALE1 a row
        // above SALE2) and pushed every card down N bands.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.png".to_owned(), (20, 6));
        images.insert("https://example.com/b.png".to_owned(), (20, 6));
        let html = "<body><div style=\"display:flex\">\
            <div style=\"position:relative;width:200px\">\
              <img src=\"a.png\" width=\"160\" height=\"48\">\
              <div style=\"position:absolute;top:0;left:0\">SALE1</div></div>\
            <div style=\"position:relative;width:200px\">\
              <img src=\"b.png\" width=\"160\" height=\"48\">\
              <div style=\"position:absolute;top:0;left:0\">SALE2</div></div>\
            </div><p>AFTER</p></body>";
        let rows = lay_with_images(html, 80, &images);
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
                .unwrap_or_else(|| panic!("{needle} not laid: {:?}", texts(&rows)))
        };
        assert_eq!(
            row_of("SALE1"),
            row_of("SALE2"),
            "sibling badges share one lifted band: {:?}",
            texts(&rows)
        );
        let img_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.items
                    .iter()
                    .any(|i| matches!(i.kind, ItemKind::Image) && i.width > 0)
            })
            .map(|(y, _)| y)
            .collect();
        assert_eq!(
            img_rows.len(),
            1,
            "the images stay side by side, one band down"
        );
        assert!(
            img_rows[0] > row_of("SALE1"),
            "badges lifted above the images"
        );
    }

    #[test]
    fn region_cache_skips_overlay_children_and_keeps_their_badges() {
        // The region child cache captures ROWS only — a child whose subtree
        // contributes to a side channel (here `positioned`) can't be memoized,
        // or the reused rows would silently drop its overlay on every warm
        // relayout (a cache must be transparent: render-as-if-fully-laid).
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let html = "<html style=\"height:100%\"><body style=\"height:100%\">\
            <div id=\"chat\" data-trust-node=\"983\" \
                 style=\"height:100%;overflow-y:scroll;width:30ch\"><div class=\"msgs\">\
              <div class=\"line\"><span>alpha</span></div>\
              <div class=\"line\"><span>bravo</span></div>\
              <div class=\"line\" style=\"position:relative\"><span>charlie</span>\
                <div style=\"position:absolute;top:0;right:0\">TICK</div></div>\
            </div></div></body></html>";
        let dom = Dom::parse_document(html);
        let boundary = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.attr(id, "data-trust-node") == Some("983"))
            .expect("the chat boundary");
        let text_of = |rows: &[Row]| -> String {
            rows.iter()
                .flat_map(|r| r.items.iter())
                .map(|i| i.text.as_str())
                .collect()
        };
        let (rows1, _c1, _sc1, cache1) = lay_out_region_fragment_cached(
            &dom,
            &base,
            30,
            (40, 8),
            &ctrls,
            &imgs,
            boundary,
            &RegionRowCache::default(),
        );
        assert!(
            text_of(&rows1).contains("TICK"),
            "cold pass keeps the overlay"
        );
        assert_eq!(
            cache1.children.len(),
            2,
            "the overlay-bearing child is NOT memoized (only the plain two are)"
        );
        // Warm relayout from the returned cache: the plain children reuse their
        // rows, the overlay child re-lays — and its overlay still renders.
        let (rows2, _c2, _sc2, _cache2) = lay_out_region_fragment_cached(
            &dom,
            &base,
            30,
            (40, 8),
            &ctrls,
            &imgs,
            boundary,
            &cache1,
        );
        assert!(
            text_of(&rows2).contains("TICK"),
            "warm (cached) relayout keeps the overlay: {:?}",
            text_of(&rows2)
        );
        assert_eq!(text_of(&rows1), text_of(&rows2), "warm matches cold");
    }

    #[test]
    fn region_fragment_returns_nested_scroll_clips() {
        // A definite-height scroll box nested INSIDE a region fragment reports
        // its clip box through the fragment relayout, so the app can keep its
        // live `clientHeight` honest without a full re-lay.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let html = "<html style=\"height:100%\"><body style=\"height:100%\">\
            <div id=\"chat\" data-trust-node=\"984\" \
                 style=\"height:100%;overflow-y:scroll;width:30ch\"><div class=\"msgs\">\
              <div class=\"line\"><span>alpha</span></div>\
              <div class=\"line\"><div data-trust-node=\"985\" \
                   style=\"height:32px;overflow-y:scroll\">\
                <p>inner a</p><p>inner b</p><p>inner c</p></div></div>\
            </div></div></body></html>";
        let dom = Dom::parse_document(html);
        let boundary = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.attr(id, "data-trust-node") == Some("984"))
            .expect("the chat boundary");
        let (_rows, _c, scroll_clips, _cache) = lay_out_region_fragment_cached(
            &dom,
            &base,
            30,
            (40, 8),
            &ctrls,
            &imgs,
            boundary,
            &RegionRowCache::default(),
        );
        assert!(
            scroll_clips
                .iter()
                .any(|&(node, h, _)| node == 985 && h > 0),
            "the nested scroll box's clip rides the fragment result: {scroll_clips:?}"
        );
    }

    #[test]
    fn abspos_explicit_width_overflows_a_collapsed_cb() {
        // CSS 2.1 §10.3.7: an abspos box with an explicit `width` USES that
        // width — it freely overflows its containing block. A `fit-content`
        // wrapper whose only content is out-of-flow legitimately collapses to
        // ~0 (out-of-flow content contributes nothing to intrinsic sizing),
        // and the box laid inside it must NOT be crushed to the CB's width
        // (Twitch's 34rem chat column laid ONE CELL wide, then compressed to
        // nothing).
        let html = "<body><div style=\"position:relative;width:2px\">\
            <div style=\"position:absolute;top:0;left:0;width:300px\">WIDE PANEL TEXT</div>\
            </div></body>";
        let rows = lay(html, 80);
        let row = rows
            .iter()
            .find(|r| r.items.iter().any(|i| i.text.contains("WIDE")))
            .expect("panel laid");
        // Laid at its explicit width: the text fits on ONE line (a 1-cell lay
        // would char-break it into a vertical strip).
        assert!(
            row.items.iter().any(|i| i.text.contains("PANEL TEXT")),
            "laid at its explicit width, not the collapsed CB: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn translated_abspos_panel_slides_into_the_band() {
        // The right-docked slide-in idiom (Twitch's chat column): a flex row
        // ends in a `width:fit-content` wrapper that collapses to ~0 (its only
        // content is out-of-flow), and the panel inside it is
        // `position:absolute; width:W` shifted INTO view by
        // `transform:translateX(-W)` — CSS Transforms 1: translation moves the
        // painted box after layout. The panel must render inside the band, on
        // the right side, at its full width.
        let html = "<body><div style=\"display:flex\">\
            <div style=\"flex:1\"><p>MAIN CONTENT</p></div>\
            <div style=\"width:fit-content\"><div style=\"position:relative\">\
              <div style=\"position:absolute;width:240px;transform:translateX(-240px) translateZ(0)\">\
                <p>CHAT PANEL</p></div>\
            </div></div></div></body>";
        let rows = lay(html, 100);
        let chat = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("CHAT"))
            .expect("chat panel laid");
        // 240px = 30 cells: slid left of the wrapper (at ~col 98) into
        // roughly [68, 98] — right side of the band, fully visible.
        assert!(
            chat.col >= 40 && (chat.col as usize) < 100,
            "chat panel slides into the band's right side (col {}): {:?}",
            chat.col,
            texts(&rows)
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("PANEL")),
            "laid at its full width, not a one-cell strip"
        );
    }

    #[test]
    fn translate_percent_centers_against_the_boxes_own_size() {
        // The universal centering idiom: `left:50%` + `translateX(-50%)` —
        // the percentage resolves against the box's OWN width (CSS
        // Transforms 1 §6), landing the box centered on its CB's midpoint.
        let html = "<body><div style=\"position:relative\"><p>x</p>\
            <div style=\"position:absolute;top:0;left:50%;width:40ch;transform:translateX(-50%)\">CENTERED</div>\
            </div></body>";
        let rows = lay(html, 100);
        let c = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("CENTERED"))
            .expect("laid");
        // left = 50 cells, tx = −20 (half of 40ch) → col ≈ 30.
        assert!(
            (25..=35).contains(&(c.col as usize)),
            "centered at ~col 30 (got {}): {:?}",
            c.col,
            texts(&rows)
        );
    }

    #[test]
    fn translated_box_inside_a_clipping_cb_is_kept() {
        // Off-canvas slide-in: `left:100%` parks the panel just past its
        // clipping CB, `translateX(-100%)` slides it fully back INSIDE.
        // Overflow clips PAINTED content and translation moves the painted
        // box, so the panel is fully visible — both the fraction-based
        // offscreen test and the laid-geometry clip test must keep it.
        let html = "<body><div style=\"position:relative;overflow:hidden\"><p>base</p>\
            <div style=\"position:absolute;top:0;left:100%;width:30ch;transform:translateX(-100%)\">DRAWER</div>\
            </div></body>";
        let rows = lay(html, 100);
        assert!(
            all_text(&rows).contains("DRAWER"),
            "slid-in drawer visible: {:?}",
            texts(&rows)
        );
        // The SAME panel without the translate parks fully outside the
        // clipping CB → correctly hidden.
        let html_out = "<body><div style=\"position:relative;overflow:hidden\"><p>base</p>\
            <div style=\"position:absolute;top:0;left:100%;width:30ch\">DRAWER</div>\
            </div></body>";
        assert!(
            !all_text(&lay(html_out, 100)).contains("DRAWER"),
            "the untranslated off-canvas panel stays hidden"
        );
    }

    #[test]
    fn abspos_fill_image_takes_its_aspect_frame_box() {
        // The spacer-sibling aspect idiom (Twitch's hero/feed cards): the
        // frame's height comes from an EMPTY in-flow child's
        // `padding-bottom:56.25%` (CSS 2.1 §8.4), and the abspos
        // `width:100%;height:100%` image must take the FRAME as its used box
        // (§10.3.8 — its percentages resolve against the containing block the
        // frame establishes). Sizing it by its decoded intrinsic box instead
        // left the reserved frame a giant void with a thumbnail-sized image
        // (Twitch's front-page hero).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/live.jpg".to_owned(), (40, 11)); // 320x180
        let html = "<body><div style=\"position:relative;width:600px\">\
            <div style=\"width:100%;overflow:hidden;position:relative\">\
              <div style=\"padding-bottom:56.25%\"></div>\
              <img src=\"/live.jpg\" style=\"position:absolute;top:0;left:0;width:100%;max-width:100%;height:100%\">\
            </div>\
            <p>CAPTION</p></div></body>";
        let rows = lay_with_images(html, 100, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("fill image laid");
        // 600px card = 75 cells; a 16:9 frame of it is ~21 rows.
        assert!(
            img.width >= 70,
            "fills the frame width (got {}): {:?}",
            img.width,
            texts(&rows)
        );
        assert!(
            img.height >= 18,
            "fills the frame height (got {})",
            img.height
        );
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .unwrap();
        let cap_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("CAPTION")))
            .expect("caption laid");
        assert!(
            cap_row >= img_row + 18,
            "caption flows below the filled frame (img@{img_row} cap@{cap_row})"
        );
    }

    #[test]
    fn scaled_abspos_card_relays_at_its_scaled_size() {
        // CSS Transforms scale on an out-of-flow box: the painted box is the
        // SCALED box — re-laid at the scaled width (the compress-to-fit
        // machinery), shrinking toward its own center (`transform-origin`
        // default). Twitch's hero card is a wide card at `scale(0.703)`:
        // unscaled it overflowed the band; scaled it fits like the browser's.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/live.jpg".to_owned(), (40, 11));
        let html = "<body><div style=\"position:relative\"><p>x</p>\
            <div style=\"position:absolute;top:0;left:0;width:800px;transform:scale(0.5)\">\
              <div style=\"width:100%;overflow:hidden;position:relative\">\
                <div style=\"padding-bottom:56.25%\"></div>\
                <img src=\"/live.jpg\" style=\"position:absolute;top:0;left:0;width:100%;height:100%\">\
              </div>\
            </div></div></body>";
        let rows = lay_with_images(html, 120, &images);
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("fill image laid");
        // 800px = 100 cells unscaled; scale(0.5) → a ~50-cell box, centered on
        // the original (col ≈ 25).
        assert!(
            (40..=60).contains(&(img.width as usize)),
            "scaled width ~50 (got {})",
            img.width
        );
        assert!(
            (15..=35).contains(&(img.col as usize)),
            "centered by the origin shift (col {})",
            img.col
        );
    }

    #[test]
    fn a_mounted_player_borrows_the_faded_preview_as_poster() {
        // Steady-state hero card: the page fades its preview image
        // (`opacity:0`) once its player mounts — in a browser the playing
        // video covers it. We deliberately render no player, so the video's
        // representation borrows the HIDDEN image's URL as its poster (the
        // image itself stays suppressed); on a NON-video page (no playable
        // target — the homepage) the poster paints UNLINKED with no phantom
        // "Watch in mpv". Without this the reserved hero frame painted
        // NOTHING at all in the settled state (Twitch front page's void).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/preview.jpg".to_owned(), (40, 11));
        let html = r#"<html><head><meta property="og:type" content="website"></head>
           <body><div style="position:relative;width:600px">
             <div style="opacity:0"><img src="/preview.jpg" style="width:100%"></div>
             <div class="video-ref"><video style="position:absolute;top:0;left:0;width:100%;height:100%"></video></div>
             <p>ChannelName</p>
           </div></body></html>"#;
        let rows = lay_with_images(html, 100, &images);
        let poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.as_deref() == Some("https://example.com/preview.jpg") && !i.invisible)
            .expect("the faded preview paints as the video's poster");
        assert!(
            poster.link.is_none(),
            "dead-end poster is unlinked: {:?}",
            poster.link
        );
        assert!(!shows(&rows, "Watch in mpv"), "{:?}", texts(&rows));
        assert!(shows(&rows, "ChannelName"), "card content renders");
        // The SAME card with a VISIBLE preview (the hover-preview idiom —
        // video overlaid ON content): the image flows once as normal content
        // and must NOT double as a borrowed poster.
        let html = r#"<html><head><meta property="og:type" content="website"></head>
           <body><div style="position:relative;width:600px">
             <div><img src="/preview.jpg" style="width:100%"></div>
             <div class="video-ref"><video style="position:absolute;top:0;left:0;width:100%;height:100%"></video></div>
           </div></body></html>"#;
        let rows = lay_with_images(html, 100, &images);
        let visible_copies = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| {
                i.image.as_deref() == Some("https://example.com/preview.jpg") && !i.invisible
            })
            .count();
        assert_eq!(visible_copies, 1, "no poster duplication");
        assert!(!shows(&rows, "Watch in mpv"), "{:?}", texts(&rows));
    }

    #[test]
    fn player_chrome_over_a_hero_frame_does_not_lift_cascade() {
        // The steady-state hero: a §8.4 aspect frame, its abspos fill image,
        // and small abspos CHROME icons scattered over it at different rows
        // (play/volume/settings/fullscreen…). The frame's zero-width
        // reservation markers are NOT images — chrome overlapping them must
        // not trigger the image-lift; counting them chained a lift per icon
        // that inflated the band into a giant void and exiled the hero image
        // below the fold (Twitch's front page, mounted-player state).
        let icons = r#"<div style="position:absolute;top:16px;left:8px">P</div>
             <div style="position:absolute;top:48px;left:8px">V</div>
             <div style="position:absolute;top:96px;left:8px">S</div>
             <div style="position:absolute;top:160px;right:8px">T</div>
             <div style="position:absolute;top:240px;right:8px">F</div>"#;
        let html = format!(
            r#"<body><div style="position:relative;width:600px">
            <div style="width:100%;overflow:hidden;position:relative">
              <div style="padding-bottom:56.25%"></div>
              <img src="/live.jpg" style="position:absolute;top:0;left:0;width:100%;height:100%">
              {icons}
            </div>
            <p>CAPTION</p></div></body>"#
        );
        // UNDECODED image: the frame holds only markers — nothing paintable,
        // so NO icon may lift. The caption sits right below the ~21-row frame.
        let rows = lay_with_images(&html, 100, &ImageSizes::new());
        let cap_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("CAPTION")))
            .expect("caption laid");
        assert!(
            cap_row <= 24,
            "no lift cascade over an empty frame (caption at row {cap_row} of {})",
            rows.len()
        );
        // DECODED image: the fill paints at the frame top; the icons over it
        // may lift at most a couple of shared bands — never a frame-sized void.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/live.jpg".to_owned(), (40, 11));
        let rows = lay_with_images(&html, 100, &images);
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.image.is_some()))
            .expect("hero image painted");
        assert!(
            img_row <= 6,
            "hero image stays in its frame's band (row {img_row})"
        );
        let cap_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("CAPTION")))
            .expect("caption laid");
        assert!(
            cap_row <= 32,
            "no frame-sized void (caption at row {cap_row} of {})",
            rows.len()
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
        // A small px offset lands INSIDE the clipping box → kept.
        let px_on = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:absolute;left:80px;width:90%\">PXNEAR</div></div></body>";
        // A px offset PAST the clipping box's right edge (1200px = 150 cells in a
        // 100-cell box) is provably off-screen → dropped. The laid-geometry clip
        // resolves the length the old fraction-only check could not.
        let px_off = "<body><div style=\"position:relative;overflow:hidden\">\
            <div style=\"position:absolute;left:1200px;width:90%\">PXFAR</div></div></body>";
        assert!(all_text(&lay(onscreen, 100)).contains("INFLOWISH"));
        assert!(all_text(&lay(corner, 100)).contains("BADGE"));
        assert!(all_text(&lay(modal, 100)).contains("MODAL"));
        assert!(all_text(&lay(px_on, 100)).contains("PXNEAR"));
        assert!(!all_text(&lay(px_off, 100)).contains("PXFAR"));
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
    fn preserved_whitespace_link_stays_interactive() {
        // A link inside a `white-space:nowrap` (or pre/pre-wrap) context flows
        // through `place_preserved`, not the collapsing `place_word` path. That
        // path used to drop the link (`push_preserved_item` passed `None`), so
        // such anchors rendered styled like links (kind=Link → cyan) but were
        // inert — not selectable, not clickable. archive.org's directory-listing
        // tables mark the file-name cells `white-space:nowrap` (names have
        // spaces and must not wrap), so every file link was dead. Cover all three
        // preserved modes plus a `<pre>` block.
        for ws in ["nowrap", "pre", "pre-wrap"] {
            let rows = lay(
                &format!(
                    r#"<body><div style="white-space:{ws};"><a href="/file%20name.jpg">file name.jpg</a></div></body>"#
                ),
                60,
            );
            let link = rows
                .iter()
                .flat_map(|r| &r.items)
                .find(|i| i.text.contains("file name"))
                .unwrap_or_else(|| panic!("{ws}: link item present"));
            assert_eq!(link.kind, ItemKind::Link, "{ws}: styled as a link");
            assert!(
                matches!(&link.link, Some(Link::Http(u)) if u.as_str().ends_with("/file%20name.jpg")),
                "{ws}: still followable, got {:?}",
                link.link
            );
            assert!(link.is_interactive(), "{ws}: selectable");
        }
        // A `<pre>` (white-space:pre by UA default) full of anchors, too.
        let rows = lay(
            r#"<body><pre><a href="/a.txt">a.txt</a>
<a href="/b.txt">b.txt</a></pre></body>"#,
            60,
        );
        let links: Vec<&Item> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.link.is_some())
            .collect();
        assert_eq!(links.len(), 2, "both <pre> anchors interactive: {links:?}");
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
    fn ol_reversed_counts_down() {
        // HTML §4.4.5: `<ol reversed>` counts DOWN, starting at the number of
        // `<li>` children (or `start` if given), and may run through zero
        // into negatives. Previously the attribute was ignored.
        let rows = lay(
            r#"<body><ol reversed><li>alpha</li><li>beta</li><li>gamma</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("3. alpha")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("2. beta")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("1. gamma")), "{lines:?}");
        let rows = lay(
            r#"<body><ol reversed start="1"><li>one</li><li>zero</li><li>minus</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("1. one")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("0. zero")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("-1. minus")), "{lines:?}");
    }

    #[test]
    fn unknown_list_style_type_falls_back_to_decimal() {
        // css-counter-styles-3 §3: a counter-style name we don't implement
        // renders as `decimal`, not a bullet (which numbered nothing).
        let rows = lay(
            r#"<body><ol style="list-style-type:lower-greek"><li>one</li><li>two</li></ol></body>"#,
            40,
        );
        let lines = texts(&rows);
        assert!(lines.iter().any(|l| l.contains("1. one")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("2. two")), "{lines:?}");
    }

    #[test]
    fn text_wrap_nowrap_longhand_keeps_one_row() {
        // CSS Text 4: `text-wrap` (the `text-wrap-mode` shorthand modern
        // Tailwind emits as `text-nowrap`) overrides the wrap half of the
        // white-space mode. Declared on a WRAPPER — the longhand inherits via
        // the registry, so the paragraph inside stays on one row.
        let rows = lay(
            r#"<html><head><style>.n{text-wrap:nowrap}</style></head>
               <body><div class="n"><p>one two three four five six</p></div></body></html>"#,
            14,
        );
        let content: Vec<&Row> = rows.iter().filter(|r| !r.items.is_empty()).collect();
        assert_eq!(
            content.len(),
            1,
            "nowrap via the longhand: {:?}",
            texts(&rows)
        );
        assert_eq!(render_row(content[0]), "one two three four five six");
    }

    #[test]
    fn white_space_collapse_preserve_keeps_spaces() {
        // CSS Text 4: `white-space-collapse: preserve` alone = preserved
        // spaces + wrapping (the pre-wrap composition).
        let rows = lay(
            r#"<body><p style="white-space-collapse:preserve">a   b</p></body>"#,
            40,
        );
        assert_eq!(texts(&rows)[0], "a   b", "spaces preserved");
    }

    #[test]
    fn break_spaces_value_preserves_spaces() {
        // `white-space: break-spaces` = preserve + wrap (its trailing-space
        // breaking coincides with the pre-wrap chunker at cell resolution).
        let rows = lay(
            r#"<body><p style="white-space:break-spaces">a   b</p></body>"#,
            40,
        );
        assert_eq!(texts(&rows)[0], "a   b", "spaces preserved");
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
        // Viewport is (width, height) in cells; here 80×24.
        let vp = (80, 24);
        assert_eq!(
            resolve_cells("10ch", 100, vp, Units::default()),
            Some(10),
            "1ch = 1 cell"
        );
        assert_eq!(
            resolve_cells("50%", 40, vp, Units::default()),
            Some(20),
            "% of avail"
        );
        assert_eq!(
            resolve_cells("50vw", 40, vp, Units::default()),
            Some(40),
            "vw of viewport width"
        );
        // vh/vmin/vmax now resolve against the viewport height (24 cells).
        assert_eq!(
            resolve_cells("50vh", 40, vp, Units::default()),
            Some(12),
            "vh of viewport height"
        );
        assert_eq!(
            resolve_cells("100vh", 40, vp, Units::default()),
            Some(24),
            "100vh = full height"
        );
        assert_eq!(
            resolve_cells("100vmin", 40, vp, Units::default()),
            Some(24),
            "vmin = the smaller axis (height, 24)"
        );
        assert_eq!(
            resolve_cells("100vmax", 40, vp, Units::default()),
            Some(80),
            "vmax = the larger axis (width, 80)"
        );
        assert_eq!(
            resolve_cells("calc(100% - 4ch)", 40, vp, Units::default()),
            Some(36),
            "calc subtracts a ch length from a percentage"
        );
        assert_eq!(
            resolve_cells("calc(50% + 2ch)", 40, vp, Units::default()),
            Some(22),
            "calc adds across unit kinds"
        );
        // calc reaches viewport-height units too.
        assert_eq!(
            resolve_cells("calc(100vh - 4ch)", 40, vp, Units::default()),
            Some(20),
            "calc subtracts from a vh length"
        );
        // Unsupported values are ignored (None), exactly as before.
        assert_eq!(resolve_cells("auto", 40, vp, Units::default()), None);
        // A 0 height basis means the viewport height wasn't threaded — vh/vmin/
        // vmax stay unresolved rather than collapsing to 0 (the prior behaviour).
        assert_eq!(
            resolve_cells("12vh", 40, (80, 0), Units::default()),
            None,
            "no viewport height ⇒ vh unresolved"
        );
        assert_eq!(
            resolve_cells("50vmin", 40, (80, 0), Units::default()),
            None,
            "vmin needs height"
        );
        // calc multiplication/division (a unitless number is a scalar).
        assert_eq!(
            resolve_cells("calc(100% * 2)", 40, vp, Units::default()),
            Some(80),
            "calc multiplies a percentage by a scalar"
        );
        assert_eq!(
            resolve_cells("calc((100% - 4ch) / 3)", 40, vp, Units::default()),
            Some(12),
            "calc divides a grouped sub-expression — the 3-column item width"
        );
        assert_eq!(
            resolve_cells("calc(100% / 3)", 60, vp, Units::default()),
            Some(20),
            "calc divides a percentage by a scalar"
        );
        // ch also flows through the absolute-unit path (indents).
        assert_eq!(indent_cells(Some("3ch"), Units::default()), 3);
    }

    #[test]
    fn a_grown_flex_items_width_is_not_a_percentage_basis() {
        // Twitch's category tower: `width:12rem; flex-grow:1` items in a
        // wrapping shelf grow to fill the row, and the card's aspect frame
        // lays at the GROWN width — so the fill image's `width:100%` must
        // resolve against the used (grown) band too, not the declared 12rem
        // flex base (which painted a small image inside a giant blank frame).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.jpg".to_owned(), (10, 8));
        images.insert("https://example.com/b.jpg".to_owned(), (10, 8));
        let rows = lay_with_images(
            r#"<body><div style="display:flex;flex-wrap:wrap;min-width:100%">
                 <div style="width:12rem;flex-grow:1;flex-shrink:0">
                   <img src="/a.jpg" style="width:100%">
                 </div>
                 <div style="width:12rem;flex-grow:1;flex-shrink:0">
                   <img src="/b.jpg" style="width:100%">
                 </div>
               </div></body>"#,
            100,
            &images,
        );
        let widths: Vec<usize> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.width as usize)
            .collect();
        assert_eq!(widths.len(), 2, "both images render");
        for w in &widths {
            assert!(
                (44..=52).contains(w),
                "the image fills its GROWN ~half-band card, not the 24-cell flex base (got {w})"
            );
        }
    }

    #[test]
    fn rem_lengths_resolve_against_the_root_font_size() {
        // The Twitch shape: `html{font-size:62.5%}` makes 1rem = 10px, so a
        // `width:75rem` featured card is 750px = 94 cells — NOT the 150 cells
        // a fixed 16px rem gave it (the hero-band bug: off-center, past the
        // viewport edge, overlapping the info column).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/x.jpg".to_owned(), (40, 11));
        let rows = lay_with_images(
            r#"<html style="font-size:62.5%"><body>
                 <div style="width:75rem"><img src="/x.jpg" style="width:100%"></div>
               </body></html>"#,
            240,
            &images,
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("the image renders");
        assert_eq!(img.width, 94, "75rem at a 10px root = 750px = 94 cells");
    }

    #[test]
    fn em_lengths_resolve_against_the_elements_computed_font_size() {
        // `width:10em` under a 20px font is 200px = 25 cells; and a
        // `font-size:2em` resolves against the PARENT (CSS Fonts §6.1) while
        // the width's em uses the element's OWN computed 40px.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.jpg".to_owned(), (10, 5));
        images.insert("https://example.com/b.jpg".to_owned(), (10, 5));
        let rows = lay_with_images(
            r#"<body><div style="font-size:20px">
                 <div style="width:10em"><img src="/a.jpg" style="width:100%"></div>
                 <div style="font-size:2em;width:5em"><img src="/b.jpg" style="width:100%"></div>
               </div></body>"#,
            240,
            &images,
        );
        let widths: Vec<usize> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.width as usize)
            .collect();
        assert_eq!(
            widths,
            vec![25, 25],
            "10em × 20px and 5em × (2em of 20px) both = 200px = 25 cells"
        );
    }

    #[test]
    fn heading_and_keyword_font_sizes_feed_em_lengths() {
        // The UA gives h1 2em of its inherited size; `x-large` is 24px.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.jpg".to_owned(), (10, 5));
        images.insert("https://example.com/b.jpg".to_owned(), (10, 5));
        let rows = lay_with_images(
            r#"<body style="font-size:10px">
                 <h1 style="width:10em"><img src="/a.jpg" style="width:100%"></h1>
                 <div style="font-size:x-large;width:10em"><img src="/b.jpg" style="width:100%"></div>
               </body>"#,
            240,
            &images,
        );
        let widths: Vec<usize> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.width as usize)
            .collect();
        assert_eq!(
            widths,
            vec![25, 30],
            "h1: 10em × 20px = 25 cells; x-large: 10em × 24px = 30 cells"
        );
    }

    #[test]
    fn rem_reaches_calc_and_math_function_terms() {
        // rem inside calc()/min() resolves through the same root basis.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.jpg".to_owned(), (10, 5));
        images.insert("https://example.com/b.jpg".to_owned(), (10, 5));
        let rows = lay_with_images(
            r#"<html style="font-size:62.5%"><body>
                 <div style="width:calc(10rem + 60px)"><img src="/a.jpg" style="width:100%"></div>
                 <div style="width:min(50rem, 300px)"><img src="/b.jpg" style="width:100%"></div>
               </body></html>"#,
            240,
            &images,
        );
        let widths: Vec<usize> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
            .map(|i| i.width as usize)
            .collect();
        assert_eq!(
            widths,
            vec![20, 38],
            "calc(100px + 60px) = 20 cells; min(500px, 300px) = 300px = 38 cells"
        );
    }

    #[test]
    fn the_real_cell_box_scales_px_to_physical_cells() {
        // On a terminal with 10×20px cells a CSS px length maps to the same
        // PHYSICAL extent as in a browser — fewer, bigger cells; and the
        // aspect conversion uses the true cell shape. `ch` stays one glyph
        // cell on ANY font: it is the character-count unit, and our glyphs
        // never scale with CSS font-size.
        let u = Units {
            fs: 16.0,
            root: 16.0,
            cell_w: 10.0,
            cell_h: 20.0,
        };
        assert_eq!(resolve_cells("100px", 0, (80, 24), u), Some(10));
        assert_eq!(css_length_rows("100px", u), Some(5));
        assert_eq!(
            rows_for_ratio(30, 1.0, u),
            15,
            "300px wide, 1:1 → 300px tall = 15 rows"
        );
        assert_eq!(resolve_cells("40ch", 0, (80, 24), u), Some(40));
        assert_eq!(
            resolve_cells("40ch", 0, (80, 24), Units::default()),
            Some(40)
        );
        // The nominal default reproduces the historical constants exactly.
        assert_eq!(
            resolve_cells("100px", 0, (80, 24), Units::default()),
            Some(13)
        );
        assert_eq!(rows_for_ratio(30, 1.0, Units::default()), 15);
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

    /// `cargo test --release blit_clone_bench -- --ignored --nocapture` —
    /// the blit cost on a deep-flex page (styled-components shape). This is
    /// the harness that measured the clone-vs-move decision documented on
    /// `blit`: the borrowing clone version ~39.6ms / 50 lays, a consuming
    /// move version ~50ms — clone wins on locality, keep it.
    #[test]
    #[ignore]
    fn blit_clone_bench() {
        let mut inner = String::new();
        for i in 0..300 {
            inner.push_str(&format!("<p><a href=\"/page{i}\">item number {i}</a></p>"));
        }
        let mut html = inner;
        for _ in 0..8 {
            html = format!("<div style='display:flex'><div style='flex:1'>{html}</div></div>");
        }
        let html = format!("<body>{html}</body>");
        let dom = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let t = std::time::Instant::now();
        let mut rows_out = 0usize;
        for _ in 0..50 {
            let rows = lay_out(
                &dom,
                &base,
                100,
                &[],
                &ControlMap::new(),
                &ImageSizes::new(),
                false,
            );
            rows_out = rows.len();
        }
        println!(
            "blit bench: 50 lays of 8-deep flex x 300 links = {:?} ({rows_out} rows)",
            t.elapsed()
        );
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
    fn a_display_block_table_still_lays_as_a_table() {
        // GitHub/markdown CSS forces `display:block;width:max-content;
        // overflow:auto` onto a `<table>` so a wide table scrolls horizontally.
        // The `<thead>`/`<tbody>` keep their table displays, so per CSS 2.1
        // §17.2.1 the row groups generate an anonymous table around them and the
        // cells STILL lay side by side — they must not block-stack.
        let rows = lay(
            "<body><table style=\"display:block;width:max-content;overflow:auto\">\
             <thead><tr><th>Command</th><th>Effect</th></tr></thead>\
             <tbody>\
             <tr><td>website.com</td><td>opens it</td></tr>\
             <tr><td>back</td><td>history pop</td></tr>\
             </tbody></table></body>",
            60,
        );
        // Header cells share a row, side by side.
        assert_eq!(
            row_index_of(&rows, "Command"),
            row_index_of(&rows, "Effect"),
            "header cells lay on the same row"
        );
        assert!(find(&rows, "Effect").col > find(&rows, "Command").col);
        // Body cells align into the header's columns and rows stack.
        assert_eq!(
            row_index_of(&rows, "website.com"),
            row_index_of(&rows, "opens it"),
            "body cells lay on the same row"
        );
        assert_eq!(
            find(&rows, "Command").col,
            find(&rows, "website.com").col,
            "first column aligns header to body"
        );
        assert!(
            row_index_of(&rows, "website.com") > row_index_of(&rows, "Command"),
            "the body row is below the header row"
        );
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
    fn css_vertical_align_beats_the_valign_attribute() {
        // HTML rendering §15.3.3: `valign` is a PRESENTATIONAL HINT — an
        // author-level rule preceding all other author rules — so any author
        // `vertical-align` wins. The old order read the attribute first.
        let rows = lay(
            r#"<body><table><tr>
                 <td>l1<br>l2<br>l3</td>
                 <td valign="bottom" style="vertical-align:top">X</td>
               </tr></table></body>"#,
            40,
        );
        assert_eq!(
            row_index_of(&rows, "X"),
            row_index_of(&rows, "l1"),
            "CSS top beats the bottom hint: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn bare_cells_default_to_middle_vertical_alignment() {
        // CSS 2.1 §17.5.4 / Appendix D: `td,th,tr { vertical-align: inherit }`
        // + `thead,tbody,tfoot { vertical-align: middle }` — a cell with no
        // declaration of its own centers in its row band, as browsers do.
        let rows = lay(
            r#"<body><table><tr>
                 <td>l1<br>l2<br>l3</td>
                 <td>X</td>
               </tr></table></body>"#,
            40,
        );
        assert_eq!(
            row_index_of(&rows, "X"),
            row_index_of(&rows, "l2"),
            "an undeclared cell centers: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn row_valign_inherits_into_undeclared_cells() {
        // The UA chain again: an undeclared cell takes its ROW's alignment
        // (`td { vertical-align: inherit }`), here the row's `valign=bottom`
        // presentational hint.
        let rows = lay(
            r#"<body><table><tr valign="bottom">
                 <td>l1<br>l2<br>l3</td>
                 <td>X</td>
               </tr></table></body>"#,
            40,
        );
        assert_eq!(
            row_index_of(&rows, "X"),
            row_index_of(&rows, "l3"),
            "the row's bottom hint reaches the cell: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn col_elements_size_their_table_columns() {
        // CSS 2.1 §17.5.2: a `<col width>` (or CSS width) sets its column's
        // width preference — previously ignored entirely. A 10% column of a
        // width-100% table in a 40-cell band is 4 cells, so the second
        // column's content starts at col 4 (border-spacing 0).
        let rows = lay(
            r#"<body><table width="100%"><colgroup><col width="10%"><col></colgroup>
                 <tr><td>a</td><td>bb</td></tr></table></body>"#,
            40,
        );
        assert_eq!(pos_of(&rows, "bb").1, 4, "{:?}", texts(&rows));
    }

    #[test]
    fn declared_cell_width_holds_on_a_widthless_table() {
        // §17.5.2.2: a declared column width raises the column's max-content,
        // so it survives the used-width computation even when the TABLE
        // declares no width — an 80px (10-cell) first column holds its 10
        // cells instead of collapsing to its 1-cell content.
        let rows = lay(
            r#"<body><table><tr><td width="80">a</td><td>b</td></tr></table></body>"#,
            40,
        );
        assert_eq!(pos_of(&rows, "b").1, 10, "{:?}", texts(&rows));
    }

    #[test]
    fn cell_css_padding_bottom_suppresses_cellpadding() {
        // The presentational-hint priority check missed "padding-bottom": a
        // cell whose ONLY CSS padding is the bottom edge still has CSS
        // padding, so the `cellpadding` attribute must lose (its horizontal
        // inset shifted the content a cell right).
        let rows = lay(
            r#"<body><table cellpadding="8"><tr><td style="padding-bottom:4px">x</td></tr></table></body>"#,
            40,
        );
        assert_eq!(
            pos_of(&rows, "x").1,
            0,
            "CSS padding wins — no cellpadding inset: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn col_span_and_childless_colgroup_repeat_their_width() {
        // `<col span=N>` covers N columns (HTML §4.9.4); a CHILDLESS
        // `<colgroup span=N width=…>` acts the same (§4.9.3). Two 25%
        // columns of a 40-cell table = 10 cells each: c starts at col 20.
        let with_span = lay(
            r#"<body><table width="100%"><colgroup><col span="2" width="25%"></colgroup>
                 <tr><td>a</td><td>b</td><td>c</td></tr></table></body>"#,
            40,
        );
        assert_eq!(pos_of(&with_span, "b").1, 10, "{:?}", texts(&with_span));
        assert_eq!(pos_of(&with_span, "c").1, 20, "{:?}", texts(&with_span));
        let childless = lay(
            r#"<body><table width="100%"><colgroup span="2" width="25%"></colgroup>
                 <tr><td>a</td><td>b</td><td>c</td></tr></table></body>"#,
            40,
        );
        assert_eq!(pos_of(&childless, "c").1, 20, "{:?}", texts(&childless));
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
    fn deeply_nested_bordered_tables_hit_the_depth_lid() {
        // Bug #9: `flow_bordered`'s hand-built sub-layout reset `table_depth`
        // (and dropped `measuring`), so a bordered box between table levels
        // defeated the MAX_TABLE_DEPTH recursion lid — hostile deep
        // table/border nesting re-entered the full column algorithm per level
        // (exponential measurement, and stack depth bounded only by the
        // nesting). With the state carried through `make_sub`, the lid
        // degrades deep tables to block-stacked content, which terminates
        // and still renders the innermost content.
        let mut html = String::from("DEEPEST");
        for i in 0..40 {
            html = format!(
                "<table><tr><td><div style='border:1px solid'>L{i} {html}</div></td></tr></table>"
            );
        }
        // 400 cells wide: each of the 40 nested frames eats 2 columns, and the
        // innermost text must still have room to render on one line.
        let rows = lay_b(&format!("<body>{html}</body>"), 400);
        assert!(
            all_text(&rows).contains("DEEPEST"),
            "the innermost bordered cell content still renders past the depth lid"
        );
    }

    #[test]
    fn deep_border_nesting_hits_its_own_lid() {
        // MAX_BORDER_DEPTH: past 32 nested bordered boxes the border is
        // dropped and the interior flows as a plain block — hostile nesting
        // can't grow the native-stack recursion a frame per level forever.
        // 60 nestings: the innermost text sits ≤ 32 frame columns in (one
        // left bar each), not 60.
        let mut html = String::from("DEEPEST");
        for _ in 0..60 {
            html = format!("<div style='border:1px solid'>{html}</div>");
        }
        let rows = lay_b(&format!("<body>{html}</body>"), 400);
        let it = find(&rows, "DEEPEST");
        assert!(
            (it.col as usize) <= MAX_BORDER_DEPTH + 2,
            "frames stop at the lid (col {})",
            it.col
        );
    }

    #[test]
    fn hostile_span_products_are_capped() {
        // MAX_CELL_SPAN_AREA: `colspan`/`rowspan` are individually clamped to
        // 1000, but the occupancy product (10^6 grid inserts per cell) was
        // not — a page of such cells ground the grid build to a halt. The
        // rowspan clamps to fit the area; the table still lays and renders.
        let mut body = String::new();
        for r in 0..8 {
            body.push_str(&format!(
                "<tr><td rowspan=\"1000\" colspan=\"1000\">c{r}</td></tr>"
            ));
        }
        let rows = lay(&format!("<body><table>{body}</table></body>"), 40);
        let all = all_text(&rows);
        assert!(all.contains("c0") && all.contains("c7"), "{all:?}");
    }

    #[test]
    fn absurd_positioned_offsets_stay_inside_the_band() {
        // `left:9999999px` is ~1.25M cells — the placed column must CLAMP
        // into the band, never wrap through the `as u16` cast (1,250,000 %
        // 65,536 ≈ 4,816 landed the box at a garbage column).
        let rows = lay(
            r#"<body><div style="position:relative">anchor<div style="position:absolute;left:9999999px;top:0">X</div></div></body>"#,
            80,
        );
        let it = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text == "X")
            .unwrap_or_else(|| panic!("the positioned box renders: {:?}", texts(&rows)));
        assert!(
            (it.col as usize) < 80,
            "clamped into the band: col {}",
            it.col
        );
    }

    #[test]
    fn a_monster_carousel_strip_truncates_at_the_addressable_width() {
        // A hostile rail whose cards sum past u16::MAX cells: the strip stops
        // laying further cards at the addressable limit — the stops stay
        // MONOTONIC (a bare `as u16` used to wrap them back toward zero).
        let cards: String = (0..1800)
            .map(|i| format!("<div style='width:300px'>c{i}</div>"))
            .collect();
        let html = format!(
            "<body><div style='overflow-x:scroll'><div style='display:flex;flex-wrap:nowrap;width:200000px'>{cards}</div></div></body>"
        );
        let dom = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let (_r, carousels, _rg, _sc, _b, _f, _a) = lay_out_with_carousels(
            &dom,
            &base,
            (40, 24),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
            false,
        );
        let c = carousels.first().expect("the rail still forms a carousel");
        assert!(
            c.stops.windows(2).all(|w| w[0] < w[1]),
            "stops stay monotonic — no u16 wrap"
        );
        assert!(
            c.stops.len() < 1800,
            "the strip truncates at the u16 limit ({} cards laid)",
            c.stops.len()
        );
    }

    #[test]
    fn ratio_container_found_past_deep_wrappers() {
        // The Twitch deep-wrapper lesson on the HEIGHT side: the
        // padding-bottom aspect box (and the `aspect-ratio` variant below)
        // can sit at ANY depth above its `height:100%` image — the old 6-level
        // caps stopped short and the image fell back to its intrinsic box.
        // 50% padding of a 40-cell band = 10 rows (2:1 in cell aspect).
        let wrap = |inner: &str, n: usize| {
            let mut s = inner.to_string();
            for _ in 0..n {
                s = format!("<div>{s}</div>");
            }
            s
        };
        let mut images = ImageSizes::new();
        images.insert("https://example.com/i.png".to_owned(), (20, 10));
        let img = r#"<img src="/i.png" style="width:100%;height:100%">"#;
        let html = format!(
            r#"<body><div style="height:0;padding-bottom:50%;overflow:hidden">{}</div></body>"#,
            wrap(img, 8)
        );
        let rows = lay_with_images(&html, 40, &images);
        let it = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.image.is_some())
            .expect("the image lays out");
        assert_eq!(it.height, 10, "sized by the ratio box, not intrinsic");
        // `aspect-ratio` ancestor at the same depth.
        let html = format!(
            r#"<body><div style="aspect-ratio:2">{}</div></body>"#,
            wrap(img, 8)
        );
        let rows = lay_with_images(&html, 40, &images);
        let it = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.image.is_some())
            .expect("the image lays out");
        assert_eq!(it.height, 10, "sized by the aspect-ratio ancestor");
    }

    #[test]
    fn atomic_inline_context_found_past_deep_inline_wrappers() {
        // The same lesson for `in_atomic_inline_context`: a `display:block`
        // image under TEN transparent inline spans inside an inline-block
        // wrapper still rides the line (the old 8-level cap declared it
        // block-level and broke the row).
        let mut img = r#"<img src="/a.png" style="display:block">"#.to_string();
        for _ in 0..10 {
            img = format!("<span>{img}</span>");
        }
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.png".to_owned(), (4, 2));
        let html = format!(r#"<body><div style="display:inline-block">before {img}</div></body>"#);
        let rows = lay_with_images(&html, 80, &images);
        let text_row = rows
            .iter()
            .position(|r| r.items.iter().any(|it| it.text.contains("before")))
            .expect("text renders");
        let img_row = rows
            .iter()
            .position(|r| r.items.iter().any(|it| it.image.is_some()))
            .expect("image renders");
        assert_eq!(
            text_row,
            img_row,
            "the image rides the line inside the atomic inline box: {:?}",
            texts(&rows)
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
            matches!(&poster.link, Some(Link::Media(u)) if u.as_str().ends_with("clip_720p.mp4")),
            "poster links to the media source (follows to mpv)"
        );
        // The DRAWN preview IS the mpv affordance — no extra caption line
        // under it (her call 2026-07-04).
        assert!(
            !shows(&rows, "▶ Video"),
            "no caption under a drawn preview: {:?}",
            texts(&rows)
        );
        // Without a decoded poster, the text link stands in for the video
        // content, keeping its kind + quality.
        let rows = lay_with_images(
            r#"<body><video poster="/poster.jpg"><source src="/clip_720p.mp4" type="video/mp4" res="720" label="HD"></video></body>"#,
            80,
            &ImageSizes::new(),
        );
        let cap = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("▶ Video · 720p HD"))
            .expect("an undecoded poster falls back to the caption link");
        assert!(matches!(&cap.link, Some(Link::Media(u)) if u.as_str().ends_with("clip_720p.mp4")));
    }

    #[test]
    fn a_sourceless_streaming_video_links_to_mpv_with_an_og_image_preview() {
        // A modern player (Twitch/YouTube/Kick/…) feeds its `<video>` from MSE/
        // blob URLs: no `src`/`<source>`/`poster`. On a page that DECLARES
        // itself a video page (og:type video.* — what every real watch page
        // ships) it must still offer a "play in mpv" affordance — on the PAGE
        // url (yt-dlp resolves it) — and use the page's standard Open Graph
        // image as the preview frame.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/preview.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<html><head><meta property="og:type" content="video.other">
                 <meta property="og:image" content="/preview.jpg"></head>
               <body><video aria-label="Live player"></video></body></html>"#,
            80,
            &images,
        );
        let poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("an og:image preview frame");
        assert_eq!(
            poster.image.as_deref(),
            Some("https://example.com/preview.jpg")
        );
        assert!(
            matches!(&poster.link, Some(Link::Media(u)) if u.as_str() == "https://example.com/"),
            "the preview follows to mpv on the page URL: {:?}",
            poster.link
        );
        // The drawn preview IS the affordance — no text line under it.
        assert!(
            !shows(&rows, "▶ Watch in mpv"),
            "no caption under a drawn preview: {:?}",
            texts(&rows)
        );
        // Undecoded og:image → the text link stands in.
        let rows = lay_with_images(
            r#"<html><head><meta property="og:type" content="video.other">
                 <meta property="og:image" content="/preview.jpg"></head>
               <body><video aria-label="Live player"></video></body></html>"#,
            80,
            &ImageSizes::new(),
        );
        let cap = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Watch in mpv"))
            .expect("a caption item when no preview drew");
        assert!(
            matches!(&cap.link, Some(Link::Media(u)) if u.as_str() == "https://example.com/"),
            "the caption follows to mpv"
        );
    }

    #[test]
    fn a_homepage_autoplay_hero_video_renders_no_dead_mpv_link() {
        // A sourceless streaming `<video>` on a page that does NOT declare
        // itself a video page (og:type "website" — a homepage autoplaying a
        // featured stream in a hero card, Twitch's front-page carousel) has
        // no playable target: mpv on the homepage URL finds no video and dies
        // silently, and the page's og:image is the site LOGO, not a preview.
        // No representation at all — the card's own text still renders.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/logo.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<html><head><meta property="og:type" content="website">
                 <meta property="og:image" content="/logo.jpg"></head>
               <body><div class="hero"><video aria-label="Featured stream"></video>
                 <p>FeaturedStreamer</p></div></body></html>"#,
            80,
            &images,
        );
        assert!(
            !shows(&rows, "Watch in mpv"),
            "no dead mpv link on a non-video page: {:?}",
            texts(&rows)
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none()),
            "the site logo does not masquerade as a video preview"
        );
        assert!(shows(&rows, "FeaturedStreamer"), "card content renders");
        // The SAME hero wrapped in the card's channel link: the anchor names
        // the content's page, so the representation plays THAT (yt-dlp
        // resolves the channel page) — as a text link, since this page's
        // og:image describes this page, not the linked one.
        let rows = lay_with_images(
            r#"<html><head><meta property="og:type" content="website">
                 <meta property="og:image" content="/logo.jpg"></head>
               <body><a href="/somechannel"><div class="hero">
                 <video aria-label="Featured stream"></video></div></a></body></html>"#,
            80,
            &images,
        );
        let cap = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Watch in mpv"))
            .expect("an anchor-wrapped card preview links to its channel page");
        assert!(
            matches!(&cap.link, Some(Link::Media(u)) if u.as_str().ends_with("/somechannel")),
            "plays the ANCHOR's page: {:?}",
            cap.link
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none()),
            "no borrowed og:image for a preview playing another page"
        );
    }

    #[test]
    fn a_video_page_with_no_video_element_at_all_still_links_to_mpv() {
        // Regression: a modern low-latency streaming player (Twitch's newer
        // pipeline) can give up before ever inserting a `<video>` DOM element
        // at all (blocked mid-negotiation), which used to mean NO mpv
        // affordance anywhere on the page — `flow_media`'s dispatch never
        // even runs. The page still declares itself a video page via the
        // standard Open Graph video protocol (`og:video`), the exact same
        // cross-site convention `page_preview_image` already reads for a
        // poster — so a page-level fallback offers "Watch in mpv" on the
        // page URL regardless of what the player's own DOM looks like.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/preview.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<html><head>
                 <meta property="og:video" content="https://player.example.com/embed">
                 <meta property="og:image" content="/preview.jpg">
               </head>
               <body><nav><a href="/browse">Browse</a></nav>
                 <div class="player-shell"></div>
                 <div class="chat">Stream Chat</div>
               </body></html>"#,
            80,
            &images,
        );
        assert!(shows(&rows, "Browse"), "the rest of the page still renders");
        assert!(shows(&rows, "Stream Chat"), "and the chat panel too");
        // The decoded og:image preview IS the mpv affordance (no text line);
        // it follows to mpv on the PAGE url (og:video is usually an iframe
        // embed, not yt-dlp-playable).
        let poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect(
                "borrows the page's og:image as a preview frame, like the sourceless-video case",
            );
        assert_eq!(
            poster.image.as_deref(),
            Some("https://example.com/preview.jpg")
        );
        assert!(
            matches!(&poster.link, Some(Link::Media(u)) if u.as_str() == "https://example.com/"),
            "the preview follows to mpv on the PAGE url: {:?}",
            poster.link
        );
        assert!(
            !shows(&rows, "▶ Watch in mpv"),
            "no caption under a drawn preview: {:?}",
            texts(&rows)
        );
        // With the og:image undecoded, the text link is the affordance.
        let rows = lay_with_images(
            r#"<html><head>
                 <meta property="og:video" content="https://player.example.com/embed">
                 <meta property="og:image" content="/preview.jpg">
               </head>
               <body><nav><a href="/browse">Browse</a></nav></body></html>"#,
            80,
            &ImageSizes::new(),
        );
        let cap = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Watch in mpv"))
            .expect("a caption item when no preview drew");
        assert!(
            matches!(&cap.link, Some(Link::Media(u)) if u.as_str() == "https://example.com/"),
            "it follows to mpv on the PAGE url: {:?}",
            cap.link
        );
    }

    #[test]
    fn a_page_without_og_video_gets_no_fallback_link() {
        // The fallback is gated on the page declaring itself a video page
        // (the standard `og:video` convention) — an ordinary page with no
        // video content must not sprout a phantom mpv link.
        let rows = lay(
            r#"<body><nav><a href="/browse">Browse</a></nav><p>Just an article.</p></body>"#,
            80,
        );
        assert!(
            !shows(&rows, "Watch in mpv"),
            "no video page, no mpv affordance: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn a_present_video_element_suppresses_the_page_level_fallback() {
        // When a `<video>` element IS present (even sourceless — the
        // existing streaming case), its own per-element representation is
        // the one and only affordance; the page-level fallback must not ALSO
        // fire and double it up.
        let rows = lay(
            r#"<html><head><meta property="og:video" content="https://player.example.com/embed"></head>
               <body><video></video></body></html>"#,
            80,
        );
        let count = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.text.contains("Watch in mpv"))
            .count();
        assert_eq!(
            count,
            1,
            "exactly one mpv affordance, not doubled: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn a_direct_source_video_without_poster_does_not_borrow_the_page_og_image() {
        // og:image is "a still frame of this PAGE's media" — the right preview
        // for a sourceless streaming player that follows to the page URL, but
        // WRONG for an inline clip with its own direct source (Steam's home
        // page painted its Summer Sale og:image onto every poster-less
        // microtrailer `<video>` in the tab preview pane). Such a video keeps
        // its caption link and renders no borrowed thumbnail.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/banner.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<html><head><meta property="og:image" content="/banner.jpg"></head>
               <body><video><source src="/microtrailer.webm" type="video/webm"></video></body></html>"#,
            80,
            &images,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|i| i.image.is_none()),
            "borrowed og:image thumbnail: {:?}",
            texts(&rows)
        );
        assert!(shows(&rows, "▶ Video"), "caption: {:?}", texts(&rows));
    }

    #[test]
    fn a_suppressed_out_of_flow_video_reserves_no_affordance_row() {
        // Steam's sale capsule after a hover-away: the mounted microtrailer
        // <video> stays in the DOM at `position:absolute; opacity:0`. A
        // browser paints nothing and gives it no flow space; our in-flow
        // "▶ Video" affordance line must vanish with it — the suppressed
        // blank row grew the hovered capsule and misaligned the grid
        // (its price landed a row off its siblings').
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.jpg".to_owned(), (20, 5));
        images.insert("https://example.com/b.jpg".to_owned(), (20, 5));
        let rows = lay_with_images(
            r#"<body><div style="display:flex">
                 <div style="position:relative"><img src="/a.jpg">
                   <video style="position:absolute;opacity:0"><source src="/t.webm" type="video/webm"></video>
                   <div>PRICE-A</div></div>
                 <div><img src="/b.jpg"><div>PRICE-B</div></div>
               </div></body>"#,
            60,
            &images,
        );
        let out = all_text(&rows);
        assert!(
            !out.contains("Video"),
            "suppressed abspos video leaked an affordance: {out:?}"
        );
        let (ra, _) = pos_of(&rows, "PRICE-A");
        let (rb, _) = pos_of(&rows, "PRICE-B");
        assert_eq!(ra, rb, "capsule heights must match: {out:?}");
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
        let poster = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.as_deref() == Some("https://example.com/poster.jpg"))
            .expect("poster renders");
        // The drawn poster IS the mpv link; no caption line under it.
        assert!(
            matches!(&poster.link, Some(Link::Media(u)) if u.as_str().ends_with("clip_720p.mp4")),
            "poster links to the source: {:?}",
            poster.link
        );
        assert!(
            !shows(&rows, "▶ Video"),
            "no caption under a drawn poster: {:?}",
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
    fn an_abspos_video_under_a_higher_containing_block_renders_once() {
        // Twitch's shape: the `<video>` is `position:absolute` but its DIRECT
        // parent (`video-ref`) is in-flow, so its containing block is a HIGHER
        // positioned ancestor. The in-flow wrapper dispatch renders the media
        // representation, and the coordinate model (`place_positioned_children`,
        // run at the ancestor's block-tail) used to place the SAME video again —
        // a doubled preview + "Watch in mpv". It must render exactly once.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/preview.jpg".to_owned(), (40, 22));
        let rows = lay_with_images(
            r#"<html><head><meta property="og:type" content="video.other">
                 <meta property="og:image" content="/preview.jpg"></head>
               <body><div style="position:relative;width:100%;height:100%">
                 <div class="video-ref">
                   <video aria-label="Twitch video player"
                          style="position:absolute;width:100%;height:100%;top:0;left:0"></video>
                 </div>
               </div></body></html>"#,
            120,
            &images,
        );
        // The drawn preview is the single affordance: exactly one frame, and
        // no caption line at all (it only stands in when no preview drew).
        let captions = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.text.contains("Watch in mpv"))
            .count();
        assert_eq!(
            captions,
            0,
            "no caption under a preview: {:?}",
            texts(&rows)
        );
        let posters = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.as_deref() == Some("https://example.com/preview.jpg"))
            .count();
        assert_eq!(posters, 1, "exactly one preview frame, not doubled");
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
        // "Visibly rendered": a paint-suppressed (`opacity:0`) item is laid out
        // and carries its text, but the renderer paints it blank — so it does
        // NOT show. (Matches how the slideshow's inactive slides are present in
        // the row grid yet invisible.)
        rows.iter()
            .flat_map(|r| &r.items)
            .any(|i| !i.invisible && i.text.contains(needle))
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
    fn an_opacity_zero_fade_in_modal_does_not_surface() {
        // Phase 1 regression: a fade-in dialog/lightbox is `opacity:0` until a
        // class animates it in. It covers the viewport geometrically, but paints
        // BLANK — the page behind shows through. Surfacing it would defer the
        // whole page behind a blank screen (the trap opened by opacity:0 keeping
        // its box). The page must render; the blank overlay must not surface.
        let rows = lay(
            r#"<body>
                 <div id="page"><a href="/in">LoginLink</a></div>
                 <div style="position:fixed;width:100%;height:100%;opacity:0">
                   <p>HiddenGate</p><a href="/enter">HiddenButton</a>
                 </div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "LoginLink"),
            "the page renders — a blank (opacity:0) overlay is not a modal: {:?}",
            texts(&rows)
        );
        assert!(
            !shows(&rows, "HiddenGate"),
            "the opacity:0 overlay content is painted blank, not surfaced"
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
    fn a_background_layer_with_equal_z_content_after_it_is_not_a_modal() {
        // solar.lowtechmagazine.com: a decorative full-viewport battery-meter
        // BACKGROUND (`position:absolute;width:100%;height:100%;top:0;left:0`,
        // z-index:auto) sits FIRST in the DOM, with the page's positioned
        // content (a `position:relative` header, z-index:auto) AFTER it. The
        // background and the content share z-index, so by CSS 2.1 Appendix E
        // painting order the LATER content paints on top — it is a background,
        // not a modal. Treating it as a modal deferred the whole magazine (only
        // the meter's caption rendered).
        let rows = lay(
            r#"<body>
                 <div style="position:absolute;width:100%;height:100%;top:0;left:0">
                   <p>BatteryMeter</p>
                 </div>
                 <header style="position:relative">
                   <a href="/">HomeLink</a>
                 </header>
                 <div><a href="/article">ArticleLink</a></div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "HomeLink"),
            "the page header is NOT deferred behind the equal-z background layer"
        );
        assert!(shows(&rows, "ArticleLink"), "the article list renders");
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

    /// Build a `Layout` with a viewport `(width, height)` for direct unit tests
    /// of internal sizing queries like `definite_height`.
    fn layout_vp<'a>(
        dom: &'a Dom,
        base: &'a Url,
        controls: &'a ControlMap,
        images: &'a ImageSizes,
        viewport: (usize, usize),
    ) -> Layout<'a> {
        let mut l = Layout::new(dom, base, viewport.0, &[], controls, images, false);
        l.viewport_h = viewport.1;
        l
    }

    fn node_by_id(dom: &Dom, id: &str) -> NodeId {
        dom.descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap_or_else(|| panic!("no #{id}"))
    }

    #[test]
    fn definite_height_resolves_the_percentage_chain_to_the_viewport() {
        // Phase 0b — CSS 2.1 §10.5: a `height:100%` chain is definite only when
        // EVERY containing block up to the viewport is explicitly sized. With
        // html+body+wrapper all `height:100%` and a 24-row viewport, the wrapper's
        // used height is the full viewport (24 rows) — this is what lets a scroll
        // region (Phase 1) know its definite height.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div id="box" style="height:100%"></div></body></html>"#,
        );
        let l = layout_vp(&dom, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l.definite_height(node_by_id(&dom, "box")),
            Some(24),
            "100% chain resolves to the 24-row viewport"
        );

        // Break the chain at <html> (auto): CSS 2.1 says the percentage then
        // computes to `auto` (indefinite) — we must NOT skip to the viewport.
        let dom2 = Dom::parse_document(
            r#"<html><body style="height:100%"><div id="box" style="height:100%"></div></body></html>"#,
        );
        let l2 = layout_vp(&dom2, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l2.definite_height(node_by_id(&dom2, "box")),
            None,
            "an auto ancestor breaks the chain ⇒ indefinite"
        );

        // A nested percentage multiplies: 50% of 100%-of-viewport = 12 rows.
        let dom3 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div id="box" style="height:50%"></div></body></html>"#,
        );
        let l3 = layout_vp(&dom3, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(l3.definite_height(node_by_id(&dom3, "box")), Some(12));
    }

    #[test]
    fn definite_height_resolves_lengths_and_vh_but_not_auto() {
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<body><div id="px" style="height:192px"></div><div id="vh" style="height:50vh"></div><div id="auto"></div></body>"#,
        );
        // A length is definite regardless of viewport height (192px = 12em ≈ 12 rows).
        let l = layout_vp(&dom, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(l.definite_height(node_by_id(&dom, "px")), Some(12));
        // `vh` is definite once the viewport height is threaded (50% of 24 = 12)…
        assert_eq!(l.definite_height(node_by_id(&dom, "vh")), Some(12));
        // …but indefinite when the viewport height is unknown (0).
        let l0 = layout_vp(&dom, &base, &ctrls, &imgs, (40, 0));
        assert_eq!(l0.definite_height(node_by_id(&dom, "vh")), None);
        // A box with no height is auto ⇒ indefinite.
        assert_eq!(l.definite_height(node_by_id(&dom, "auto")), None);
    }

    #[test]
    fn definite_height_bridges_a_stretched_row_flex_item() {
        // Phase 0b — CSS Flexbox §9.4: a child of a definite-height row flex with
        // no explicit height STRETCHES (default align-items) to the container's
        // height, becoming definite. This is the common chat layout: a horizontal
        // page-flex sized to the viewport, whose chat column stretches to fill it,
        // holding a `height:100%` scroll area. Without this bridge the height
        // chain breaks at the auto-height column and the region never triggers.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%">
               <div id="page" style="display:flex;height:100%">
                 <div id="main">video</div>
                 <div id="chat"><div id="area" style="height:100%">msgs</div></div>
               </div></body></html>"#,
        );
        let l = layout_vp(&dom, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l.definite_height(node_by_id(&dom, "chat")),
            Some(24),
            "a stretched flex item fills the viewport-tall row"
        );
        assert_eq!(
            l.definite_height(node_by_id(&dom, "area")),
            Some(24),
            "height:100% resolves against the now-definite stretched column"
        );

        // Opting out of stretch (align-self) ⇒ indefinite.
        let dom2 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="display:flex;height:100%"><div id="chat" style="align-self:flex-start">x</div></div></body></html>"#,
        );
        let l2 = layout_vp(&dom2, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l2.definite_height(node_by_id(&dom2, "chat")),
            None,
            "align-self:flex-start ⇒ not stretched ⇒ indefinite"
        );

        // A non-definite-height container can't make its items definite.
        let dom3 = Dom::parse_document(
            r#"<body><div style="display:flex"><div id="chat">x</div></div></body>"#,
        );
        let l3 = layout_vp(&dom3, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l3.definite_height(node_by_id(&dom3, "chat")),
            None,
            "stretch only transfers a DEFINITE container height"
        );

        // Column-flex main-size distribution (§9.2/§9.7): a sole `flex:1`
        // column item with auto height fills the container's content height.
        let dom4 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="display:flex;flex-direction:column;height:100%"><div id="grow" style="flex:1">x</div></div></body></html>"#,
        );
        let l4 = layout_vp(&dom4, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l4.definite_height(node_by_id(&dom4, "grow")),
            Some(24),
            "the sole grow item fills the viewport-tall column"
        );
    }

    #[test]
    fn definite_height_distributes_column_flex_main_size() {
        // Phase 0b follow-up — CSS Flexbox §9.2/§9.7: a `flex-grow` item in a
        // definite-height COLUMN flex gets a definite main (height) = the
        // container content height minus the other items' base sizes. This is
        // the Twitch chat shell: `#root` (column, height:100%) holds a fixed
        // header + the growing app body, whose `height:100%` scroll area then
        // resolves. The chain breaks here without main-size distribution.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%">
               <div id="root" style="display:flex;flex-direction:column;height:100%">
                 <div id="hdr" style="height:48px">nav</div>
                 <div id="body" style="flex:1">
                   <div id="area" style="height:100%;overflow-y:auto">msgs</div>
                 </div>
               </div></body></html>"#,
        );
        // 24-row viewport − a fixed 48px (= 3em ≈ 3 rows) header ⇒ the grow body
        // gets the remaining 21 rows, and the inner scroll area inherits it.
        let l = layout_vp(&dom, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l.definite_height(node_by_id(&dom, "body")),
            Some(21),
            "the grow body fills the column minus the fixed header"
        );
        assert_eq!(
            l.definite_height(node_by_id(&dom, "area")),
            Some(21),
            "height:100% resolves against the now-definite grow body"
        );

        // A NON-grow auto-height column item stays content-driven (indefinite).
        let dom2 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="display:flex;flex-direction:column;height:100%"><div id="x">content</div></div></body></html>"#,
        );
        let l2 = layout_vp(&dom2, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l2.definite_height(node_by_id(&dom2, "x")),
            None,
            "a non-growing column item is content-sized ⇒ indefinite"
        );

        // Two grow items need this item's own base size (no cancellation) —
        // deferred, so it stays indefinite rather than guess.
        let dom3 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="display:flex;flex-direction:column;height:100%"><div id="a" style="flex:1">a</div><div id="b" style="flex:1">b</div></div></body></html>"#,
        );
        let l3 = layout_vp(&dom3, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l3.definite_height(node_by_id(&dom3, "a")),
            None,
            "multiple grow items are deferred (no single-item cancellation)"
        );
    }

    #[test]
    fn definite_height_resolves_an_absolutely_positioned_top_bottom_box() {
        // CSS 2.1 §10.6.4: an `position:absolute` box with `top` AND `bottom`
        // set and `height:auto` has a definite used height = containing-block
        // height − top − bottom. This is Twitch's app shell — a `top:0;bottom:0`
        // panel filling `#root` (`position:relative`, viewport-tall), which holds
        // the `height:100%` chat scroll area. Without this the height chain breaks
        // at the auto-height absolute panel and the region never triggers.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%">
               <div id="root" style="position:relative;height:100%">
                 <div id="shell" style="position:absolute;top:0;bottom:0">
                   <div id="area" style="height:100%;overflow-y:auto">msgs</div>
                 </div>
               </div></body></html>"#,
        );
        let l = layout_vp(&dom, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l.definite_height(node_by_id(&dom, "shell")),
            Some(24),
            "top:0;bottom:0 fills the viewport-tall positioned ancestor"
        );
        assert_eq!(
            l.definite_height(node_by_id(&dom, "area")),
            Some(24),
            "height:100% resolves against the now-definite absolute shell"
        );

        // Non-zero offsets carve in. `top`/`bottom` resolve through `pos_len`
        // (the shared positioning-length resolver, in cells), exactly as
        // `abs_used_top`/`place_positioned_children` place the box — so the
        // definite height stays CONSISTENT with the box's actual placement:
        // top:32px (4 cells) + bottom:48px (6 cells) ⇒ 24 − 4 − 6 = 14.
        let dom2 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="position:relative;height:100%"><div id="s" style="position:absolute;top:32px;bottom:48px"></div></div></body></html>"#,
        );
        let l2 = layout_vp(&dom2, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(l2.definite_height(node_by_id(&dom2, "s")), Some(14));

        // Only `top` set (bottom auto) ⇒ height is content-driven ⇒ indefinite.
        let dom3 = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="position:relative;height:100%"><div id="s" style="position:absolute;top:0">x</div></div></body></html>"#,
        );
        let l3 = layout_vp(&dom3, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l3.definite_height(node_by_id(&dom3, "s")),
            None,
            "only one offset ⇒ height stays auto ⇒ indefinite"
        );

        // top+bottom but the positioned ancestor is itself indefinite ⇒ None.
        let dom4 = Dom::parse_document(
            r#"<body><div style="position:relative"><div id="s" style="position:absolute;top:0;bottom:0">x</div></div></body>"#,
        );
        let l4 = layout_vp(&dom4, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(
            l4.definite_height(node_by_id(&dom4, "s")),
            None,
            "an indefinite containing block can't make the offsets definite"
        );

        // `position:fixed; top:0; bottom:0` fills the viewport directly.
        let dom5 = Dom::parse_document(
            r#"<body><div id="s" style="position:fixed;top:0;bottom:0">x</div></body>"#,
        );
        let l5 = layout_vp(&dom5, &base, &ctrls, &imgs, (40, 24));
        assert_eq!(l5.definite_height(node_by_id(&dom5, "s")), Some(24));
    }

    #[test]
    fn overflowing_definite_height_box_becomes_a_clipped_region() {
        // Phase 1 — CSS Overflow L3: a definite-height `overflow-y:auto` box
        // whose content OVERFLOWS becomes a scroll container. The layout reserves
        // exactly H blank rows (keeping the document flat) and stashes the full
        // content in the region's buffer; the view windows it.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let mut body = String::from(r#"<html style="height:100%"><body style="height:100%">"#);
        body.push_str(r#"<div id="scroller" style="height:100%;overflow-y:auto">"#);
        for i in 0..20 {
            body.push_str(&format!("<div>L{i:02}</div>"));
        }
        body.push_str(r#"</div><div>FOOT</div></body></html>"#);
        let dom = Dom::parse_document(&body);
        // 10-row viewport: H = 10 rows, content = 20 rows ⇒ it clips.
        let (rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, (40, 10), &[], &ctrls, &imgs, false);

        assert_eq!(regions.len(), 1, "the overflowing box is one scroll region");
        let rg = &regions[0];
        assert_eq!(rg.height, 10, "the region is its definite height (10 rows)");
        assert_eq!(rg.buffer.len(), 20, "the buffer holds ALL 20 content rows");
        assert!(
            rg.voffset == 0,
            "a fresh region sits at the top (CSSOM origin)"
        );
        // The document stayed FLAT: it reserved H (10) rows + the footer row,
        // NOT the full 20-row content (which would defeat the clip).
        assert!(
            rows.len() <= 12,
            "doc reserved ~H rows, not the content height (got {})",
            rows.len()
        );
        // The footer flows just below the reserved band, at ~row 10 — proof the
        // region occupies a fixed H rows regardless of its content height.
        let foot = rows
            .iter()
            .position(|r| r.items.iter().any(|it| it.text.contains("FOOT")))
            .expect("footer present");
        assert!(
            (9..=11).contains(&foot),
            "footer sits just past the H-row band (row {foot})"
        );
        // The view window shows the TOP of the content (voffset 0): the band's
        // first row is L00, its last visible row L09; L10+ are clipped (in the
        // buffer, not the document).
        let first = effective_row(&rows, &regions, rg.start_row);
        assert!(render_row(&first).contains("L00"), "band top shows L00");
        let last = effective_row(&rows, &regions, rg.start_row + 9);
        assert!(render_row(&last).contains("L09"), "band bottom shows L09");
        assert!(
            !rows
                .iter()
                .any(|r| r.items.iter().any(|it| it.text.contains("L19"))),
            "the clipped tail (L19) is NOT in the document rows"
        );
    }

    #[test]
    fn the_principal_scroller_under_a_locked_viewport_flows_into_the_document() {
        // Twitch's front-page pattern: html/body are `overflow:hidden` (the
        // viewport can't scroll the document), so the page delegates scrolling
        // to one inner `overflow:auto` box that carries `<main>` — the PRINCIPAL
        // scroller. That box must NOT be virtualized into an inner Region; its
        // content flows into the document so the page scroll (and the right
        // scrollbar) scrolls it, exactly as a browser scrolls that panel as "the
        // page". A `<nav>` sidebar's own scroller stays a genuine inner Region.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let mut body = String::from(
            r#"<html style="height:100%;overflow:hidden"><body style="height:100%;overflow:hidden">"#,
        );
        body.push_str(r#"<div style="display:flex;height:100%">"#);
        body.push_str(
            r#"<nav style="height:100%"><div id="side" style="height:100%;overflow-y:auto">"#,
        );
        for i in 0..20 {
            body.push_str(&format!("<div>S{i:02}</div>"));
        }
        body.push_str(r#"</div></nav>"#);
        body.push_str(r#"<main style="height:100%"><div id="main-scroll" style="height:100%;overflow-y:auto">"#);
        for i in 0..20 {
            body.push_str(&format!("<div>M{i:02}</div>"));
        }
        body.push_str(r#"</div></main></div></body></html>"#);
        let dom = Dom::parse_document(&body);
        let (rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, (60, 10), &[], &ctrls, &imgs, false);

        // ONLY the <nav> sidebar is a Region; the <main> scroller flowed inline.
        assert_eq!(
            regions.len(),
            1,
            "only the nav sidebar is a region (the principal main scroller is not)"
        );
        let rg = &regions[0];
        assert_eq!(
            dom.attr(rg.node, "id"),
            Some("side"),
            "the one region is the <nav> sidebar scroller"
        );
        // The principal scroller's FULL content flowed into the document — its
        // clipped tail (M19) is a real document row, not stashed in a buffer.
        assert!(
            rows.iter()
                .any(|r| r.items.iter().any(|it| it.text.contains("M19"))),
            "the principal scroller's content flows into the document (M19 present)"
        );
        // The sidebar's tail (S19) is clipped into its region buffer, NOT the doc.
        assert!(
            !rows
                .iter()
                .any(|r| r.items.iter().any(|it| it.text.contains("S19"))),
            "the sidebar region clips its tail (S19) out of the document rows"
        );
        assert!(
            rg.buffer
                .iter()
                .any(|r| r.items.iter().any(|it| it.text.contains("S19"))),
            "the sidebar region's buffer holds its full content (S19)"
        );

        // CONTROL: the SAME structure with an UNLOCKED viewport (no
        // `overflow:hidden` on html/body) means the document itself scrolls, so
        // the main scroller is a genuine inner Region again — proving the lock
        // signal, not the markup shape, is what excludes the principal scroller.
        let unlocked = body
            .replace(";overflow:hidden\"", "\"")
            .replace("overflow:hidden\"", "\"");
        let dom2 = Dom::parse_document(&unlocked);
        let (_r2, _c2, regions2, ..) =
            lay_out_with_carousels(&dom2, &base, (60, 10), &[], &ctrls, &imgs, false);
        assert_eq!(
            regions2.len(),
            2,
            "with the viewport unlocked, both scrollers are inner regions"
        );
    }

    #[test]
    fn a_region_patch_relayout_matches_a_full_relayout() {
        // INCREMENTAL_LAYOUT_PLAN.md §9 — the materialization guarantee: the
        // boundary's buffer laid from a serialized PATCH fragment (re-parsed,
        // ancestor-less) is byte-for-byte the same as the SAME region produced by
        // a full `lay_out`. The region inherits `font-weight:bold` +
        // `text-transform:uppercase` from <body>; if §4a materialization drops
        // them, the fragment renders non-bold / lowercase and the buffers diverge.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let mut body = String::from(
            r#"<html style="height:100%"><body style="height:100%;font-weight:bold;text-transform:uppercase">"#,
        );
        body.push_str(r#"<div id="chat" style="height:100%;overflow-y:scroll;width:30ch">"#);
        for i in 0..12 {
            body.push_str(&format!("<div>msg{i:02}</div>"));
        }
        body.push_str(r#"</div></body></html>"#);
        let dom = Dom::parse_document(&body);
        let viewport = (40usize, 8usize);
        // FULL path: the region buffer as the page produces it.
        let (_rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, viewport, &[], &ctrls, &imgs, false);
        assert_eq!(regions.len(), 1, "one scroll region");
        let full = &regions[0];
        let boundary = full.node;
        // PATCH path: serialize the boundary (materialized) → re-parse → re-lay.
        let frag = dom.serialize_patch(boundary, &std::collections::HashSet::new());
        let rp = crate::http::lay_region_patch(
            &base,
            frag.as_bytes(),
            full.width as usize,
            viewport,
            &imgs,
            boundary,
            &RegionRowCache::default(),
        )
        .expect("the patch fragment lays out");
        assert_eq!(rp.rows.len(), full.buffer.len(), "same buffer height");
        for (a, b) in rp.rows.iter().zip(full.buffer.iter()) {
            assert_eq!(render_row(a), render_row(b), "same rendered text per row");
            let bolds_a: Vec<bool> = a.items.iter().map(|it| it.emph.bold).collect();
            let bolds_b: Vec<bool> = b.items.iter().map(|it| it.emph.bold).collect();
            assert_eq!(
                bolds_a, bolds_b,
                "materialized font-weight matches per item"
            );
        }
        // Guard against both being wrong-but-equal: the inherited styling really
        // did reach the content (uppercase + bold).
        assert!(
            full.buffer
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.emph.bold),
            "inherited bold reached the region content"
        );
        assert!(
            render_row(&full.buffer[0]).contains("MSG00"),
            "inherited text-transform:uppercase applied"
        );
    }

    #[test]
    fn region_incremental_layout_matches_full() {
        // INCREMENTAL_LAYOUT_PLAN.md §14 / §9 differential guard for the inner-
        // scroll DE-LAG: reusing the cached rows of unchanged messages and laying
        // only the NEW one must produce a buffer BYTE-IDENTICAL to re-laying the
        // whole region from scratch. This is the correctness bar that lets us not
        // re-walk off-screen messages on every append.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let viewport = (40usize, 8usize);
        // Build a chat-like region: a scroll container > message-container > N
        // block messages (each unique content → unique cache key), mirroring the
        // real `scrollable-area > __message-container > chat-line` nesting.
        let region_html = |msgs: &[&str]| {
            let mut s = String::from(
                r#"<html style="height:100%"><body style="height:100%"><div id="chat" data-trust-node="980" style="height:100%;overflow-y:scroll;width:30ch"><div class="msgs">"#,
            );
            for m in msgs {
                s.push_str(&format!("<div class=\"line\"><span>{m}</span></div>"));
            }
            s.push_str("</div></div></body></html>");
            s
        };
        let lay_region = |html: &str, cache: &RegionRowCache| {
            let dom = Dom::parse_document(html);
            let boundary = dom
                .descendants(DOCUMENT)
                .into_iter()
                .find(|&id| dom.attr(id, "data-trust-node") == Some("980"))
                .expect("the chat boundary");
            lay_out_region_fragment_cached(
                &dom, &base, 30, viewport, &ctrls, &imgs, boundary, cache,
            )
        };
        // Patch 1 (cold): 5 messages. Populates the cache.
        let five = ["alpha", "bravo", "charlie", "delta", "echo"];
        let (_rows1, _c1, _sc1, cache1) =
            lay_region(&region_html(&five), &RegionRowCache::default());
        assert_eq!(cache1.children.len(), 5, "five message rows cached");
        // Patch 2 (warm): append a 6th message — only it should be laid, the first
        // five reused from `cache1`.
        let six = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let (rows_inc, _c2, _sc2, cache2) = lay_region(&region_html(&six), &cache1);
        assert_eq!(cache2.children.len(), 6, "six rows cached after the append");
        // The FULL (uncached) layout of the same six messages.
        let (rows_full, ..) = lay_region(&region_html(&six), &RegionRowCache::default());
        assert_eq!(
            rows_inc.len(),
            rows_full.len(),
            "incremental buffer has the same height as a full relayout"
        );
        for (a, b) in rows_inc.iter().zip(rows_full.iter()) {
            assert_eq!(
                render_row(a),
                render_row(b),
                "incremental row matches the full relayout row"
            );
        }
        assert!(
            rows_full.iter().any(|r| render_row(r).contains("foxtrot")),
            "the appended message is present"
        );
        // Top-trim (the chat buffer cap): drop the oldest, keep appending. Still
        // matches a full relayout, and the evicted key is gone from the cache.
        let shifted = ["bravo", "charlie", "delta", "echo", "foxtrot", "golf"];
        let (rows_shift, _c3, _sc3, cache3) = lay_region(&region_html(&shifted), &cache2);
        assert_eq!(cache3.children.len(), 6, "still six after a shift");
        let (rows_shift_full, ..) = lay_region(&region_html(&shifted), &RegionRowCache::default());
        for (a, b) in rows_shift.iter().zip(rows_shift_full.iter()) {
            assert_eq!(render_row(a), render_row(b), "shift matches full relayout");
        }
        assert!(
            !rows_shift.iter().any(|r| render_row(r).contains("alpha")),
            "the trimmed-off oldest message is gone"
        );
    }

    #[test]
    fn a_flex_column_item_is_captured_but_a_shared_row_item_is_not() {
        // INCREMENTAL_LAYOUT_PLAN.md §14 (the widening): a flex/grid ITEM that
        // OWNS its rows (a flex-column item) is captured as a SUB-BOX boundary —
        // so a styled-components item / animated counter patches instead of
        // forcing a full render. But a flex-ROW item that SHARES a row with a
        // sibling is EXCLUDED (the owns-rows safety valve), because a row splice
        // would drop the sibling — it takes the full path.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());

        // A flex COLUMN: each item stacks on its own rows → owns them.
        let col = r#"<html><body><div style="display:flex;flex-direction:column"><div data-trust-node="5"><div>aaa</div><div>aaa</div></div><div data-trust-node="6"><div>bbb</div></div></div></body></html>"#;
        let (_r, _c, _rg, _cl, b1, _f1, _a1) = lay_out_with_carousels(
            &Dom::parse_document(col),
            &base,
            (40, 0),
            &[],
            &ctrls,
            &imgs,
            false,
        );
        let item = b1
            .iter()
            .find(|b| b.node == 5)
            .expect("the flex-column item is captured as a sub-box boundary");
        assert!(item.sub_box, "it is a sub-box (re-lays with subtree_root)");
        assert_eq!(item.row_range.len(), 2, "it spans its two content rows");

        // A flex ROW: the two items sit side by side, sharing a row.
        let row = r#"<html><body><div style="display:flex"><div data-trust-node="5">AAA</div><div data-trust-node="6">BBB</div></div></body></html>"#;
        let (_r2, _c2, _rg2, _cl2, b2, _f2, _a2) = lay_out_with_carousels(
            &Dom::parse_document(row),
            &base,
            (40, 0),
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert!(
            !b2.iter().any(|b| b.node == 5),
            "a row-sharing flex item is excluded (a row splice would drop its sibling)"
        );
    }

    #[test]
    fn an_inline_ifc_boundary_is_captured_with_its_band() {
        // INCREMENTAL_LAYOUT_PLAN.md §14 step 3: a block-filling IFC box
        // (`display:flow-root`) carrying a baked `data-trust-node` is captured in
        // `Doc.boundaries` with its row span + outer band. A plain block is NOT
        // (only IFC containers are addressable boundaries).
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let html = r#"<html><body><p>header</p><div data-trust-node="42" style="display:flow-root"><div>l0</div><div>l1</div><div>l2</div></div><p>footer</p><div data-trust-node="9" style="display:block">plain</div></body></html>"#;
        let dom = Dom::parse_document(html);
        let (rows, _car, _rgn, _clip, boundaries, _fixed, _anchors) =
            lay_out_with_carousels(&dom, &base, (40, 0), &[], &ctrls, &imgs, false);
        let b = boundaries
            .iter()
            .find(|b| b.node == 42)
            .expect("the flow-root boundary is captured");
        assert!(
            !boundaries.iter().any(|b| b.node == 9),
            "a plain display:block is not an IFC boundary"
        );
        assert_eq!(
            b.origin_col, 0,
            "a left-aligned block fills from the margin"
        );
        assert!(b.content_width >= 4, "the band is the page width");
        assert!(b.row_range.start >= 1, "header sits above the boundary");
        assert!(
            render_row(&rows[b.row_range.start]).contains("l0"),
            "the captured range starts at the boundary's first content row"
        );
        assert!(
            render_row(&rows[b.row_range.end - 1]).contains("l2"),
            "the captured range ends at the boundary's last content row"
        );
    }

    #[test]
    fn an_inline_boundary_fragment_lays_like_the_full_document() {
        // INCREMENTAL_LAYOUT_PLAN.md §9/§14: the boundary laid from a serialized
        // PATCH fragment (re-parsed, ancestor-less) is byte-for-byte the SAME as
        // that boundary in the full layout — including `font-weight:bold` +
        // `text-transform:uppercase` inherited from <body> and materialized onto
        // the fragment root (§4a). If the band capture or the standalone re-lay
        // diverged (e.g. a double-applied indent), the rows wouldn't match.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let html = r#"<html><body style="font-weight:bold;text-transform:uppercase"><p>head</p><div data-trust-node="7" style="display:flow-root"><div>msg0</div><div>msg1</div></div></body></html>"#;
        let dom = Dom::parse_document(html);
        let vp = (40usize, 0usize);
        let (rows, _car, _rgn, _clip, boundaries, _fixed, _anchors) =
            lay_out_with_carousels(&dom, &base, vp, &[], &ctrls, &imgs, false);
        let b = boundaries.iter().find(|b| b.node == 7).unwrap();
        let frag = {
            let node = dom
                .descendants(crate::dom::DOCUMENT)
                .into_iter()
                .find(|&id| dom.attr(id, "data-trust-node") == Some("7"))
                .unwrap();
            dom.serialize_patch(node, &std::collections::HashSet::new())
        };
        let laid = crate::http::lay_subtree_patch(
            &base,
            frag.as_bytes(),
            b.content_width as usize,
            vp,
            &imgs,
            7,
            b.sub_box,
        )
        .expect("the patch fragment lays out");
        assert_eq!(
            laid.height,
            b.row_range.len(),
            "fragment height == the boundary's full-doc row span"
        );
        for (i, lr) in laid.rows.iter().enumerate() {
            // Shift the fragment cols into the box's band (origin_col) and compare
            // the rendered row to the full document's.
            let mut shifted = lr.clone();
            for it in &mut shifted.items {
                it.col += b.origin_col;
            }
            assert_eq!(
                render_row(&shifted),
                render_row(&rows[b.row_range.start + i]),
                "row {i} of the fragment matches the full document"
            );
        }
        assert!(
            laid.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.emph.bold),
            "inherited bold was materialized onto the fragment"
        );
        assert!(
            render_row(&laid.rows[0]).contains("MSG0"),
            "inherited text-transform:uppercase reached the fragment"
        );
    }

    #[test]
    fn an_oversize_token_wraps_instead_of_overflowing_or_stretching() {
        // A single unbreakable token wider than the band (a poll's concatenated
        // usernames, a long URL/emote) must character-break across rows: the
        // terminal has no horizontal scroll, so the overflow would be lost AND a
        // content-sized ancestor would be stretched to the token's width.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let band_max = |rows: &[Row]| -> usize {
            rows.iter()
                .flat_map(|r| &r.items)
                .map(|it| it.col as usize + it.width as usize)
                .max()
                .unwrap_or(0)
        };

        // (a) a long link label wraps; no row exceeds the band; nothing dropped;
        // exactly one selection stop (the link rides the first row only).
        let token =
            "goldwiser8826matthewmccarter25Amazeran20goldwiser8826matthewmccarter25Amazeran20";
        let a = format!(
            r#"<html><body><div style="width:40ch"><a href="/x">{token}</a></div></body></html>"#
        );
        let rows = lay_out(
            &Dom::parse_document(&a),
            &base,
            40,
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert!(band_max(&rows) <= 40, "no row exceeds the 40-cell band");
        assert!(rows.len() >= 2, "the long token wrapped across rows");
        let joined: String = rows
            .iter()
            .flat_map(|r| &r.items)
            .map(|it| it.text.as_str())
            .collect();
        assert!(
            joined.contains("Amazeran20goldwiser"),
            "no characters dropped across the wrap"
        );
        assert_eq!(
            rows.iter()
                .flat_map(|r| &r.items)
                .filter(|it| it.link.is_some())
                .count(),
            1,
            "one selection stop for the wrapped link"
        );

        // (b) inside a DEFINITE-width box (the chat column), the long token wraps
        // within the box and following content stays inside the band — no
        // overflow off-screen.
        let b = r#"<html><body><div style="width:30ch"><div><a href="/x">goldwiser8826matthewmccarter25Amazeran20goldwiser8826matthewmccarter25</a></div><div>SIDE</div></div></body></html>"#;
        let rows2 = lay_out(
            &Dom::parse_document(b),
            &base,
            80,
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert!(
            band_max(&rows2) <= 30,
            "the token wraps within the 30-cell box, not past it"
        );
        assert!(
            rows2
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("SIDE")),
            "following content survives the wrap"
        );

        // (b2) REGRESSION GUARD: breaking is `overflow-wrap:break-word` (render-
        // only), NOT `anywhere` — a content-sized flex column of SHORT words must
        // NOT collapse to one cell and char-break them. The word stays intact.
        let g = r#"<html><body><div style="display:flex"><nav><div>S00</div><div>S19</div></nav><main><div>M00</div></main></div></body></html>"#;
        let rows5 = lay_out(
            &Dom::parse_document(g),
            &base,
            60,
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert!(
            rows5
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("S19")),
            "a short word in a content-sized flex item is not char-broken"
        );

        // (c) an over-wide BUTTON label (the poll voter list) wraps too.
        let c = r#"<html><body><div style="width:30ch"><button>goldwiser8826matthewmccarter25Amazeran20</button></div></body></html>"#;
        let rows3 = lay_out(
            &Dom::parse_document(c),
            &base,
            30,
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert!(
            band_max(&rows3) <= 30,
            "the button label wraps within the band"
        );

        // (d) a SHORT button is untouched — one atom, one row, brackets intact.
        let d = r#"<html><body><button>Yes 94% (467)</button></body></html>"#;
        let rows4 = lay_out(
            &Dom::parse_document(d),
            &base,
            40,
            &[],
            &ctrls,
            &imgs,
            false,
        );
        assert_eq!(rows4.len(), 1, "a short widget is not broken");
        assert!(
            rows4[0].items.iter().any(|it| it.text.contains("[ Yes")),
            "the short button keeps its single-atom bracket framing"
        );
    }

    #[test]
    fn auto_overflow_with_a_definite_height_reserves_its_full_band_even_when_content_fits() {
        // A definite-height `overflow-y:auto` box IS its `height` tall whether
        // content overflows or fits — `auto` only governs scrollbar presence, not
        // box size (CSS Overflow L3). So a short content (3 rows) in a tall
        // (10-row) box reserves the full 10-row band and shows empty space below,
        // exactly as a browser paints it — NOT laid inline at content height (her
        // call 2026-06-29: follow the declared box, don't render fixed as
        // flexible). This is the fix for a chat list growing inline from empty to
        // full as messages arrive instead of being full-height from the start.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div style="height:100%;overflow-y:auto"><div>A</div><div>B</div><div>C</div></div></body></html>"#,
        );
        let (rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, (40, 10), &[], &ctrls, &imgs, false);
        assert_eq!(regions.len(), 1, "a definite-height auto box is a region");
        assert_eq!(
            regions[0].height, 10,
            "it reserves its full definite height"
        );
        assert_eq!(
            regions[0].buffer.len(),
            3,
            "the buffer holds the short content"
        );
        // The content shows at the band top; the rest of the band is blank.
        assert!(
            render_row(&effective_row(&rows, &regions, regions[0].start_row)).contains("A"),
            "the fitting content shows at the band top"
        );
    }

    #[test]
    fn explicit_overflow_scroll_reserves_its_height_even_when_content_fits() {
        // `overflow-y:scroll` is a scroll container ALWAYS (unlike `auto`), so a
        // short content still reserves the box's full definite height — a fixed
        // scroll viewport the author asked for.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%"><div id="s" style="height:100%;overflow-y:scroll"><div>ONE</div></div></body></html>"#,
        );
        let (rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, (40, 8), &[], &ctrls, &imgs, false);
        assert_eq!(regions.len(), 1, "overflow:scroll is always a region");
        assert_eq!(regions[0].height, 8, "it reserves the full definite height");
        assert_eq!(regions[0].buffer.len(), 1, "buffer holds the short content");
        // The reserved band is 8 blank rows; only its first shows ONE.
        assert!(
            render_row(&effective_row(&rows, &regions, regions[0].start_row)).contains("ONE"),
            "the single content row shows at the band top"
        );
    }

    #[test]
    fn an_indefinite_height_overflow_box_does_not_become_a_region() {
        // Without a definite height the box can't be a fixed scroll viewport
        // (Phase 0): its content flows normally and nothing is clipped.
        let base = Url::parse("https://example.com/").unwrap();
        let (ctrls, imgs) = (ControlMap::new(), ImageSizes::new());
        let dom = Dom::parse_document(
            r#"<body><div style="overflow-y:auto"><div>X</div><div>Y</div></div></body>"#,
        );
        let (rows, _car, regions, ..) =
            lay_out_with_carousels(&dom, &base, (40, 10), &[], &ctrls, &imgs, false);
        assert!(regions.is_empty(), "no definite height ⇒ no region");
        assert!(rows.iter().any(|r| r.items.iter().any(|it| it.text == "Y")));
    }

    #[test]
    fn scroll_box_report_flags_definite_and_indefinite_regions() {
        // The inner-scroll GATE diagnostic: a chat-like `overflow-y:auto` area in
        // a viewport-tall stretched flex column is REGION-CAPABLE (definite H)…
        let base = Url::parse("https://example.com/").unwrap();
        let dom = Dom::parse_document(
            r#"<html style="height:100%"><body style="height:100%">
               <div style="display:flex;height:100%">
                 <div id="chat"><div id="area" style="height:100%;overflow-y:auto">msgs</div></div>
               </div></body></html>"#,
        );
        let report = scroll_box_report(&dom, &base, (40, 24));
        assert!(
            report.contains("REGION-CAPABLE") && report.contains("definite_height=Some(24)"),
            "chat area should resolve a definite height:\n{report}"
        );
        // …while a bare `overflow-y:auto` with no definite-height chain is flagged
        // indefinite (it would NOT trigger a region today).
        let dom2 = Dom::parse_document(
            r#"<body><div id="area" style="overflow-y:auto">msgs</div></body>"#,
        );
        let report2 = scroll_box_report(&dom2, &base, (40, 24));
        assert!(report2.contains("indefinite"), "{report2}");
    }

    #[test]
    fn percentage_image_finds_a_deeply_nested_definite_width_ancestor() {
        // Twitch's preview card: the `width:30rem` container sits ELEVEN
        // all-`width:100%` wrappers above its `<img width:100%>` (aspect-ratio
        // box, transform/hover wrappers, the `<a>`, shelf layers). The image must
        // size to that 30rem card (≈60 cells), not the full column — the old
        // 8-level ancestor-walk cap fell short and rendered the preview enormous,
        // burying the page below it.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/p.png".to_owned(), (40, 22));
        let html = format!(
            "<body><div style=\"width:30rem\">{}<img src=\"/p.png\" style=\"width:100%\">{}</div></body>",
            "<div style=\"width:100%\">".repeat(11),
            "</div>".repeat(11),
        );
        let rows = lay_with_images(&html, 200, &images);
        let img = image_item(&rows);
        // 30rem → 60 cells; far below the 200-col flow box. Height by aspect ratio.
        assert_eq!(img.width, 60, "image fills the 30rem card, not the column");
        assert!(
            img.height < 40,
            "height follows the card width via aspect ratio, got {}",
            img.height
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
        // The single-line-ellipsis card idiom (the full declared triplet
        // `white-space:nowrap; overflow:hidden; text-overflow:ellipsis`): a
        // too-long line is clipped at the box edge with an ellipsis instead
        // of overflowing it (a forum post title bleeding into the sidebar).
        // Laid in a 10-cell box.
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">Permanently banned forever</div></body>"#,
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
    fn text_overflow_defaults_to_plain_clip() {
        // CSS Overflow 3 §5.1: `text-overflow`'s initial value is `clip` —
        // without a declared `ellipsis` the truncation is a silent cut at the
        // box edge, as browsers render it (`…` used to be synthesized
        // unconditionally).
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow:hidden">Permanently banned forever</div></body>"#,
            10,
        );
        let line = texts(&rows)[0].clone();
        assert_eq!(line, "Permanentl", "a full-width silent cut: {line:?}");
        // And an explicit `text-overflow: clip` says the same thing.
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow:hidden;text-overflow:clip">Permanently banned forever</div></body>"#,
            10,
        );
        assert_eq!(texts(&rows)[0], "Permanentl");
    }

    #[test]
    fn ellipsis_replaces_the_last_cell_when_the_line_is_exactly_full() {
        // §5.1: the ellipsis renders even when the previous word ends exactly
        // at the box edge — characters are REMOVED to make room. "abcd efghi"
        // fills the 10-cell box; omitting "jkl" turns it into "abcd efgh…".
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">abcd efghi jkl</div></body>"#,
            10,
        );
        assert_eq!(texts(&rows)[0], "abcd efgh…");
    }

    #[test]
    fn soft_hyphen_is_invisible_when_no_break_is_taken() {
        // CSS Text 3 §6.2: U+00AD is an INVISIBLE soft wrap opportunity —
        // unicode-width counts it as one cell, so left in the text it painted
        // a stray glyph mid-word ("hy­phen" 7 cells wide instead of 6).
        let rows = lay("<body><p>hy&shy;phen</p></body>", 40);
        let item = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("phen"))
            .expect("the word renders");
        assert_eq!(item.text, "hyphen", "the SHY leaves no glyph");
        assert_eq!(item.width, 6, "and no cell");
    }

    #[test]
    fn soft_hyphen_breaks_at_the_latest_fitting_opportunity() {
        // Two opportunities, a 10-cell band (the layout's minimum width), a
        // 12-cell word: browsers break at the LATEST opportunity that fits —
        // "aaaabbbb-" (9 cells, hyphen shown) then "cccc", never "aaaa-"
        // early or a char-tower.
        let rows = lay("<body><p>aaaa&shy;bbbb&shy;cccc</p></body>", 10);
        let lines = texts(&rows);
        assert_eq!(
            lines[0], "aaaabbbb-",
            "latest fitting opportunity, visible hyphen: {lines:?}"
        );
        assert_eq!(lines[1], "cccc", "the remainder continues: {lines:?}");
    }

    #[test]
    fn soft_hyphen_never_renders_in_preserved_text() {
        // `pre` never wraps, so the opportunity is never taken — the SHY
        // still must not paint a cell.
        let rows = lay("<body><pre>ab&shy;cd</pre></body>", 40);
        let item = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains("cd"))
            .expect("the pre text renders");
        assert_eq!(item.text, "abcd");
    }

    #[test]
    fn a_bare_overflow_x_longhand_truncates_a_nowrap_line() {
        // Bug #11 (overflow unification): the nowrap-truncation check read only
        // the `overflow` shorthand, so the equivalent bare `overflow-x:hidden`
        // longhand — what card CSS commonly declares — never clipped. Every
        // overflow consumer resolves per-axis through `axis_overflow` now.
        let rows = lay(
            r#"<body><div style="white-space:nowrap;overflow-x:hidden;text-overflow:ellipsis">Permanently banned forever</div></body>"#,
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
    }

    #[test]
    fn the_overflow_x_longhand_wins_over_the_shorthand() {
        // `overflow:hidden` sets both axes, but the `overflow-x:visible`
        // longhand re-opens the x axis (CSS Overflow §3) — the box no longer
        // clips its nowrap line. The old check saw only the shorthand and
        // truncated anyway.
        let rows = lay(
            r#"<body><div style="overflow:hidden;overflow-x:visible;white-space:nowrap;text-overflow:ellipsis">Permanently banned forever</div></body>"#,
            10,
        );
        let line = texts(&rows)[0].clone();
        assert_eq!(
            line, "Permanently banned forever",
            "the x axis is visible — the nowrap line is not truncated"
        );
    }

    #[test]
    fn a_bare_overflow_longhand_establishes_a_bfc_containing_floats() {
        // CSS 2.1 §9.4.1 / CSS Overflow 3: ANY non-visible overflow — including
        // a bare `overflow-x:hidden` longhand — makes the block a BFC that
        // contains its descendant floats. The shorthand-only check let the
        // float leak beside the following sibling.
        let html = r#"<body><div style="overflow-x:hidden"><div style="float:left">FLOAT</div></div><p>after</p></body>"#;
        let rows = lay(html, 40);
        let find = |needle: &str| {
            rows.iter()
                .enumerate()
                .find_map(|(y, r)| {
                    r.items
                        .iter()
                        .find(|it| it.text.contains(needle))
                        .map(|it| (y, it.col))
                })
                .unwrap_or_else(|| panic!("{needle} renders"))
        };
        let float = find("FLOAT");
        let after = find("after");
        assert!(
            after.0 > float.0,
            "the float is contained — 'after' starts below it, not beside it: {:?}",
            texts(&rows)
        );
        assert_eq!(
            after.1,
            0,
            "'after' returns to the left margin: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn a_two_value_overflow_shorthand_clips_the_y_axis() {
        // `overflow: auto hidden` is `overflow-x:auto; overflow-y:hidden`
        // (CSS Overflow §3). The old all-tokens test refused the pair, so a
        // suppressed virtualized placeholder declaring it reserved nothing and
        // flowed its full invisible content instead.
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6";
        let rows = lay(
            &format!(
                r#"<body><div style="height:48px;overflow:auto hidden;opacity:0">{tall}</div></body>"#
            ),
            80,
        );
        assert_eq!(
            rows.len(),
            3,
            "48px / 16px rows = 3 reserved rows, not the 6 invisible lines: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn a_visually_hidden_clip_box_renders_nothing() {
        // The sr-only / `clip` accessibility idiom: a sub-cell `overflow:hidden`
        // box (`width:0.1rem;height:0.1rem`) clips its label to nothing — a
        // browser shows a ~1px speck, so we paint NOTHING, not a stray "…". This
        // is the standard CSS clip (not a special case): Twitch side-nav cards
        // carry two per card ("Live", "<n> viewers") that were leaking as "…".
        let rows = lay(
            r#"<body><div>before<p style="width:0.1rem;height:0.1rem;overflow:hidden">Live</p>after</div></body>"#,
            40,
        );
        let all = texts(&rows).join("\n");
        assert!(
            !all.contains('…') && !all.contains("Live"),
            "the clipped sr-only label paints nothing: {all:?}"
        );
        assert!(
            all.contains("before") && all.contains("after"),
            "its siblings still render: {all:?}"
        );
    }

    #[test]
    fn a_subcell_box_that_does_not_clip_overflow_still_renders() {
        // The collapse is gated on a clipping overflow: a sub-cell box with the
        // default `overflow:visible` lets its content paint outside the box (CSS
        // Overflow §3), so it is NOT hidden — only the clip makes it vanish.
        let rows = lay(
            r#"<body><div><span style="width:0.1rem">kept</span></div></body>"#,
            40,
        );
        assert!(
            texts(&rows).join("\n").contains("kept"),
            "a non-clipping sub-cell box still shows its content"
        );
    }

    #[test]
    fn a_paint_suppressed_clip_placeholder_reserves_its_declared_height() {
        // A virtualized-list placeholder — React/Mastodon render an off-screen row
        // as `opacity:0; overflow:hidden; height:<cachedPx>px` holding hidden
        // content — reserves EXACTLY its declared height in rows, not its full
        // invisible content extent. This keeps the document height STABLE as the
        // list swaps placeholders in and out on scroll (without it a placeholder
        // laid its whole invisible article and every intersection swap thrashed the
        // doc height, teleporting the reader onto a different post). Ten lines of
        // content in a 48px (3-row) suppressed+clipped box occupy 3 blank rows.
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6<br>l7<br>l8<br>l9<br>l10";
        let rows = lay(
            &format!(
                r#"<body><div style="height:48px;overflow:hidden;opacity:0">{tall}</div></body>"#
            ),
            80,
        );
        assert_eq!(
            rows.len(),
            3,
            "reserves 48px / 16 = 3 rows, not the 10 lines of invisible content: {:?}",
            texts(&rows)
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .all(|it| it.text.is_empty()),
            "the reserved rows are blank — no clipped content leaks: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn geometry_and_render_agree_on_position_after_a_placeholder() {
        // The scroll fix reserves a virtualized placeholder's height in the RENDER;
        // the GEOMETRY pass (`measure_boxes`, which backs getBoundingClientRect AND
        // the page's IntersectionObserver) MUST reserve it identically. Otherwise
        // every row below the placeholder sits at a different document position in
        // the engine than on screen, so scrolling reveals the wrong posts and lands
        // you on a different one when you scroll back up. A 48px (3-row) opacity:0
        // placeholder holding 10 lines of hidden content: the marker after it lands
        // at the SAME position in both passes — the reserved 3 rows, not the ~10-row
        // full-content extent.
        let tall = "l1<br>l2<br>l3<br>l4<br>l5<br>l6<br>l7<br>l8<br>l9<br>l10";
        let html = format!(
            r#"<body><div style="height:48px;overflow:hidden;opacity:0">{tall}</div><div id="after">MARKER</div></body>"#
        );
        let rows = lay(&html, 80);
        let marker_row = rows
            .iter()
            .position(|r| r.items.iter().any(|it| it.text.contains("MARKER")))
            .expect("MARKER renders");
        assert!(
            marker_row < 6,
            "render places MARKER just after the reserved 3 rows, not the full 10-line extent: row {marker_row}"
        );
        let (dom, m) = measure(&html, 80);
        let after_top = box_by_id(&dom, &m, "after").top;
        assert_eq!(
            after_top,
            marker_row as f64 * 16.0,
            "geometry agrees with the render on where the post after a placeholder sits"
        );
    }

    #[test]
    fn a_visible_clip_box_still_renders_its_content_unchanged() {
        // The reservation is scoped to PAINT-SUPPRESSED boxes (the placeholder
        // idiom). A visible `overflow:hidden; height:Npx` box is NOT blanked — its
        // content renders exactly as before (no regression for fixed-height cards).
        let rows = lay(
            r#"<body><div style="height:48px;overflow:hidden">visibletext</div></body>"#,
            80,
        );
        assert!(
            shows(&rows, "visibletext"),
            "a visible clip box keeps rendering its content: {:?}",
            texts(&rows)
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
    fn an_icon_only_button_renders_its_icon_not_its_aria_label() {
        // A `<button>` is normally an atomic widget stub. But an icon-only button
        // (no visible text, an `<img>` icon inside — the inline `<svg>` rewritten
        // by `rewrite_inline_svgs`) must render its ICON like an `<a>`/`<div>`
        // does. YouTube's masthead chrome is `<button aria-label="Einstellungen">
        // <yt-icon><svg/></yt-icon></button>`; the stub path threw the icon away
        // and dumped the long aria-label as a vertical smear.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/i.svg".to_owned(), (3, 2));
        let rows = lay_with_images(
            r#"<body><button aria-label="Einstellungen"><img src="/i.svg"></button></body>"#,
            40,
            &images,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Einstellungen"),
            "the aria-label is not dumped as a label: {line:?}"
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.items)
                .any(|i| matches!(i.kind, ItemKind::Image)),
            "the icon image renders instead"
        );
    }

    #[test]
    fn a_text_button_still_renders_a_widget_stub() {
        // The carve-out: a button WITH visible text stays a readable `[ text ]`
        // stub — only icon-only buttons flow their icon.
        let rows = lay(r#"<body><button>Subscribe</button></body>"#, 40);
        let line = texts(&rows).join(" ");
        assert!(
            line.contains("Subscribe"),
            "a text button keeps its widget stub: {line:?}"
        );
    }

    #[test]
    fn a_form_bound_icon_button_renders_its_icon_and_stays_clickable() {
        // No special case for form controls: a form button written as an icon (a
        // magnifier submit) renders its icon like any other icon button — the
        // document made it an icon. Its submit `Link` is threaded onto the icon so
        // a click still submits (functional, just not a `[ Search ]` stub).
        let html = r#"<body><form><button type="submit" aria-label="Search"><img src="/i.svg"></button></form></body>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let button = dom
            .descendants(crate::dom::DOCUMENT)
            .into_iter()
            .find(|&n| dom.tag_name(n) == Some("button"))
            .expect("button node");
        let mut controls = ControlMap::new();
        controls.insert(button, (0, 0)); // what the form walk binds at parse time
        let mut images = ImageSizes::new();
        images.insert("https://example.com/i.svg".to_owned(), (3, 2));
        let rows = lay_out(&dom, &base, 40, &[], &controls, &images, false);
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Search"),
            "the icon renders, not the aria-label stub: {line:?}"
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| matches!(i.kind, ItemKind::Image))
            .expect("the icon image renders");
        assert!(
            matches!(img.link, Some(Link::Form { form: 0, field: 0 })),
            "the icon carries the form submit link so it stays clickable: {:?}",
            img.link
        );
    }

    #[test]
    fn an_offscreen_positioned_box_is_hidden() {
        // The "shove it past the corner" visually-hidden idiom (`left:-9999px`,
        // `top:-1000px` — YouTube's Skip-navigation button). We must not paint
        // it; before, the negative offset was clamped to row/col 0 in
        // `place_positioned_children` so the hidden text landed at the top-left.
        // A small negative offset (an `-1.5rem` overlap) is NOT the idiom and
        // stays visible.
        let rows = lay(
            r#"<body>
               <div style="position:absolute;top:-1000px"><a href="/m">Skip navigation</a></div>
               <div style="position:absolute;left:-9999px">Offscreen left</div>
               <div style="position:absolute;top:-1.5rem">Small negative stays</div>
               <p>Real content</p>
            </body>"#,
            60,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Skip navigation"),
            "top:-1000px is hidden: {line:?}"
        );
        assert!(
            !line.contains("Offscreen left"),
            "left:-9999px is hidden: {line:?}"
        );
        assert!(
            line.contains("Real content"),
            "real content renders: {line:?}"
        );
        assert!(
            line.contains("Small negative stays"),
            "a small -1.5rem offset is not hidden: {line:?}"
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
    fn a_clipped_icon_control_does_not_leak_its_accessible_name() {
        // An icon-only control the author CLIPPED to an icon-sized box
        // (a definite `width` under `overflow:hidden`) never paints its
        // `aria-label` — a browser shows only what fits the box (the icon).
        // Twitch's per-message reply button (`aria-label="Click to reply to
        // @user"` in a `width:3.2rem;overflow:hidden` box) spammed every chat
        // line with the screen-reader name; honoring the clip suppresses it.
        let rows = lay(
            r#"<body><a href="/x"><button aria-label="Click to reply to @user" style="width:3.2rem;height:3.2rem;overflow:hidden;white-space:nowrap"></button></a></body>"#,
            80,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Click to reply"),
            "a clipped accessible name is not painted: {line:?}"
        );
    }

    #[test]
    fn a_full_bleed_overlay_button_does_not_leak_a_phantom_label() {
        // A content-less full-area positioned overlay (Twitch's player carries a
        // `<button aria-label="Play" style="position:absolute;width:100%;
        // height:100%">` click-to-play scrim) paints NOTHING in a browser — its
        // accessible name must not be surfaced as a label over the content it
        // covers. A normal small icon button (next test, the roomy case) keeps it.
        let rows = lay(
            r#"<body><div style="position:relative;width:100%;height:100%"><a href="/p"><button aria-label="Play" style="position:absolute;width:100%;height:100%"></button></a></div></body>"#,
            80,
        );
        let line = texts(&rows).join(" ");
        assert!(
            !line.contains("Play"),
            "a full-bleed overlay scrim does not paint its name: {line:?}"
        );
    }

    #[test]
    fn a_roomy_icon_control_still_surfaces_its_accessible_name() {
        // The CONTROL case for the clip suppression above: an icon-only control
        // WITHOUT an icon-sized clip box still surfaces its accessible name (the
        // archive.org "Sign up" / SL logo behaviour). Here the box is wide
        // enough to hold the label, so it must show.
        let rows = lay(
            r#"<body><a href="/x"><button aria-label="Log In" style="width:20rem;overflow:hidden"></button></a></body>"#,
            80,
        );
        let line = texts(&rows).join(" ");
        assert!(
            line.contains("Log In"),
            "a fitting accessible name is still the visible label: {line:?}"
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
    fn a_shrink0_flex_endcap_measures_its_content_not_the_whole_row() {
        // YouTube's masthead `#container`: a flex row of
        // [#start, #center(search, flex:0 1 <wide>, min-width:0),
        //  #end(flex:0 0 auto — shrink 0)]. #end's content is a grow button
        // whose label is centred (`justify-content:center`, like every Material/
        // Polymer button). While MEASURING #end's intrinsic width (flex-basis
        // auto), flex-grow must NOT expand that button to fill the measuring
        // constraint (CSS Flexbox §9.9.1 — there is no free space to distribute
        // during intrinsic sizing). The bug: grow ran while measuring, so the
        // button box widened to the constraint, then its content was re-laid
        // (non-measuring) and `justify-content:center` pushed the label to the
        // box's middle — a column near the constraint, which the box's extent
        // (`max(col+width)`) reported back as the intrinsic width. #end then
        // froze there (shrink:0) and starved #center, wrapping the search query.
        let rows = lay(
            r#"<body><div style="display:flex;width:100%">
                 <div style="flex:0 0 auto">Menu</div>
                 <div style="flex:0 1 80ch;min-width:0">search query that should stay on one line</div>
                 <div style="flex:0 0 auto;display:flex">
                   <div style="display:flex;flex:1 1 0;justify-content:center;white-space:nowrap">Sign in</div>
                 </div>
               </div></body>"#,
            100,
        );
        // The end-cap holds only "Sign in" (~7 cells), so it sits at the far
        // right edge and #center keeps the rest of the row.
        let signin = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Sign in"))
            .expect("sign-in label laid");
        assert!(
            signin.col >= 80,
            "shrink:0 end-cap stays narrow at the right edge (col {}), not inflated by grow+centre during measure",
            signin.col
        );
        // …and the search text keeps a wide box: it lays on a single row, not
        // fragmented down a starved #center.
        let center_rows = rows
            .iter()
            .filter(|r| r.items.iter().any(|i| i.text.contains("search query")))
            .count();
        assert_eq!(
            center_rows, 1,
            "search text fits on one row in the un-starved #center"
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
    fn an_image_is_capped_by_an_ancestor_max_width() {
        // The icon idiom: `<span style="max-width:36px"><img style="width:100%">`.
        // The 36px max-width wrapper (no explicit `width`) is the containing
        // block, so the `width:100%` image must resolve against the CAPPED box,
        // not fall through to the full flow box.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/i.png".into(), (100, 100));
        let rows = lay_with_images(
            r#"<body><div style="width:100%"><span style="max-width:36px"><img src="/i.png" style="width:100%"></span></div></body>"#,
            120,
            &images,
        );
        let img = image_item(&rows);
        assert!(
            img.width <= 6,
            "icon capped by its ancestor's max-width (36px ~= 5 cells), got {}",
            img.width
        );
    }

    #[test]
    fn a_nearer_max_width_caps_a_percentage_image_over_a_far_definite_width() {
        // YouTube's brand icons (the Shorts logo) rendered FULL-SCREEN. The real
        // structure: a `width:100%` SVG sits in a run of `width:100%` boxes
        // (`yt-icon-shape`, the `ytIconWrapperHost` whose own width is the invalid
        // `undefinedpx`) capped by a `max-width:36px` leading-image box — which
        // itself sits inside a WIDE definite-width results column. The percentage
        // must resolve against the NEARER 36px cap, not the far column. The old
        // code preferred the nearest definite WIDTH (the 600px column) and only
        // consulted `max-width` when no definite width existed anywhere up-chain,
        // so the cap lost and the logo filled the screen.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/shorts.svg".into(), (3, 3));
        let rows = lay_with_images(
            r#"<body><div style="width:600px"><div style="max-width:36px"><div style="width:100%"><img src="/shorts.svg" style="width:100%"></div></div></div></body>"#,
            200,
            &images,
        );
        let img = image_item(&rows);
        assert!(
            img.width <= 6,
            "icon caps at the nearer 36px max-width (~5 cells), not the 600px column, got {}",
            img.width
        );
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
    fn undecoded_image_in_overflow_hidden_aspect_placeholder_reserves_its_box() {
        // The Dotdash Meredith / lazysizes idiom (dailypaws, allrecipes,
        // verywell, …): a `height:0; padding-bottom:X%; overflow:hidden`
        // aspect-ratio placeholder holding an `<img width=… height=…>` that is
        // LAZY-LOADED (`data-src`, no `src` until an IntersectionObserver reveals
        // it). Two bugs conspired to give such an image NO box: (1)
        // `is_clip_collapsed` treated the `height:0` + `overflow:hidden`
        // placeholder as collapsed and SKIPPED its whole subtree — ignoring that
        // `overflow` clips to the PADDING box, which the percentage
        // `padding-bottom` makes tall (CSS Overflow §3 / CSS 2.1 §8.4); and (2)
        // an undecoded `<img>` fell back to alt text instead of reserving its
        // DECLARED box. With no box the image had no on-page position, so the
        // site's IntersectionObserver never fired and it never loaded at all.
        // Now the placeholder isn't collapsed and the image reserves its declared
        // (width/height-attribute) aspect box BEFORE decoding — declaration-first
        // replaced-element sizing, exactly as a browser does to avoid layout
        // shift.
        let rows = lay(
            r#"<body><div style="height:0;padding-bottom:66.6%;overflow:hidden;position:relative"><img width="2000" height="1333" style="width:100%;height:auto" alt="a puppy"></div><p>after</p></body>"#,
            40,
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| matches!(i.kind, ItemKind::Image) && i.image.is_none())
            .expect("undecoded image reserves a placeholder box, not skipped");
        // width:100% => 40 cells; height:auto from the 2000×1333 attrs (ratio
        // 1.5) => rows = 40 / (2·1.5) ≈ 13 — a real box, not a collapsed strip.
        assert_eq!(img.width, 40);
        assert_eq!(img.height, 13, "declared aspect box reserved before decode");
        // The reserved box takes real vertical space: following content flows
        // BELOW it, never over a collapsed 0-row placeholder.
        let after_row = rows
            .iter()
            .position(|r| r.items.iter().any(|i| i.text.contains("after")))
            .expect("the following paragraph is laid out");
        assert!(
            after_row >= 13,
            "content flows below the reserved image box (row {after_row})"
        );
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
        let (rows, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (50, 0),
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
    fn align_self_offsets_items_on_a_flex_row() {
        // Flexbox §8.3: `align-self` overrides the container's `align-items`
        // per item. Row flex read only the container before; column flex
        // already resolved it — inconsistent. A 3-row line: `flex-end` lands
        // its item on the last row, `center` on the middle one, and an item
        // saying `flex-start` beats a `flex-end` container.
        let rows = lay(
            r#"<body><div style="display:flex">
                 <div>t1<br>t2<br>t3</div>
                 <div style="align-self:flex-end">END</div>
                 <div style="align-self:center">MID</div>
               </div></body>"#,
            40,
        );
        let (t_row, _) = pos_of(&rows, "t1");
        assert_eq!(pos_of(&rows, "END").0, t_row + 2, "{:?}", texts(&rows));
        assert_eq!(pos_of(&rows, "MID").0, t_row + 1, "{:?}", texts(&rows));
        let rows = lay(
            r#"<body><div style="display:flex;align-items:flex-end">
                 <div>t1<br>t2<br>t3</div>
                 <div style="align-self:flex-start">TOP</div>
                 <div>BOT</div>
               </div></body>"#,
            40,
        );
        let (t_row, _) = pos_of(&rows, "t1");
        assert_eq!(
            pos_of(&rows, "TOP").0,
            t_row,
            "the item's flex-start beats the container's flex-end: {:?}",
            texts(&rows)
        );
        assert_eq!(
            pos_of(&rows, "BOT").0,
            t_row + 2,
            "an auto item still takes the container's flex-end"
        );
    }

    #[test]
    fn align_self_offsets_items_on_a_wrap_shelf() {
        // The same §8.3 resolution on the wrap (shelf) path.
        let rows = lay(
            r#"<body><div style="display:flex;flex-wrap:wrap">
                 <div>t1<br>t2<br>t3</div>
                 <div style="align-self:flex-end">END</div>
               </div></body>"#,
            40,
        );
        let (t_row, _) = pos_of(&rows, "t1");
        assert_eq!(
            pos_of(&rows, "END").0,
            t_row + 2,
            "align-self applies within the shelf: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn flex_row_reverse_reverses_item_order_and_packs_right() {
        // Flexbox §5.1: `row-reverse` swaps main-start and main-end — the
        // items lay out in reverse order AND the default (flex-start) packing
        // hugs the swapped main-start, the RIGHT edge. 3em (6-cell) boxes,
        // gap 1 → used 13 of 40: "b" leads at col 27, "a" ends at col 40.
        let rows = lay(
            r#"<html><head><style>.r{display:flex;flex-direction:row-reverse}.c{width:3em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        let (arow, acol) = pos_of(&rows, "a");
        let (brow, bcol) = pos_of(&rows, "b");
        assert_eq!(arow, brow, "one flex line: {:?}", texts(&rows));
        assert_eq!(bcol, 27, "b leads at the packed-right group's start");
        assert_eq!(acol, 34, "a renders last, ending at the right edge");
    }

    #[test]
    fn flex_row_reverse_flex_end_packs_left() {
        // Under `row-reverse`, `justify-content:flex-end` is the swapped
        // main-END — the LEFT edge (while writing-mode `end` would stay
        // right). Order still reversed: "b" first at col 0.
        let rows = lay(
            r#"<html><head><style>.r{display:flex;flex-direction:row-reverse;justify-content:flex-end}.c{width:3em}</style></head>
               <body><div class="r"><div class="c">a</div><div class="c">b</div></div></body></html>"#,
            40,
        );
        assert_eq!(pos_of(&rows, "b").1, 0, "{:?}", texts(&rows));
        assert_eq!(pos_of(&rows, "a").1, 7, "{:?}", texts(&rows));
    }

    #[test]
    fn flex_column_reverse_stacks_bottom_up() {
        // §5.1: `column-reverse` runs the main axis bottom-to-top — the last
        // item renders on top.
        let rows = lay(
            r#"<body><div style="display:flex;flex-direction:column-reverse"><div>first</div><div>second</div></div></body>"#,
            40,
        );
        assert!(
            pos_of(&rows, "second").0 < pos_of(&rows, "first").0,
            "the last item stacks on top: {:?}",
            texts(&rows)
        );
    }

    #[test]
    fn flex_wrap_reverse_stacks_lines_bottom_to_top() {
        // §5.2: `wrap-reverse` swaps cross-start and cross-end — the flex
        // LINES stack bottom-to-top while each line keeps its left-to-right
        // item order. 4-cell items in a 10-cell band: lines [AAAA BBBB] and
        // [CCCC]; reversed, CCCC's line renders first.
        let rows = lay(
            r#"<body><div style="display:flex;flex-wrap:wrap-reverse">
                 <div style="width:32px">AAAA</div>
                 <div style="width:32px">BBBB</div>
                 <div style="width:32px">CCCC</div>
               </div></body>"#,
            10,
        );
        let (arow, acol) = pos_of(&rows, "AAAA");
        let (brow, bcol) = pos_of(&rows, "BBBB");
        let (crow, _) = pos_of(&rows, "CCCC");
        assert!(
            crow < arow,
            "the last line stacks on top: {:?}",
            texts(&rows)
        );
        assert_eq!(arow, brow, "the first line stays intact");
        assert!(acol < bcol, "items within a line keep their order");
    }

    #[test]
    fn nested_justify_flex_end_does_not_inflate_a_columns_measured_width() {
        // Steam's tab rows: a growable title column (`min-width:0`, nowrap +
        // overflow:hidden) beside a `flex-shrink:0` price column whose price
        // is right-justified by a flex row nested BELOW a plain block wrapper.
        // The nested `justify-content:flex-end` must not run while the price
        // column's intrinsic width is measured (`layout_subtree` inherits the
        // measuring state) — it made the column measure ~the whole row, and
        // the shrink pass then collapsed the title column to one cell ("…").
        let rows = lay(
            r#"<html><body><div style="display:flex">
                 <div style="display:flex;flex-direction:column;min-width:0;flex-grow:1;flex-shrink:1">
                   <div style="white-space:nowrap;overflow:hidden">GAME TITLE HERE</div>
                 </div>
                 <div style="display:flex;flex-direction:column;flex-shrink:0;min-width:0;white-space:nowrap">
                   <div><div style="display:flex;position:relative;justify-content:flex-end">6,15€</div></div>
                 </div>
               </div></body></html>"#,
            100,
        );
        let out = all_text(&rows);
        assert!(out.contains("GAME TITLE HERE"), "title clipped: {out:?}");
        assert!(out.contains("6,15€"), "price missing: {out:?}");
        let (title_row, _) = pos_of(&rows, "GAME TITLE HERE");
        let (price_row, _) = pos_of(&rows, "6,15€");
        assert_eq!(title_row, price_row, "columns fell apart: {out:?}");
    }

    #[test]
    fn auto_basis_flex_item_is_capped_by_its_max_width() {
        // A flex cell wrapping an over-wide replaced element (a `width:100%`
        // @2x image, intrinsic 60 cells) in a `max-width:15em` (30-cell) box:
        // the AUTO flex basis is the measured content width CLAMPED by
        // `max-width` (CSS Flexbox §9.2.3 hypothetical main size), so the
        // neighbour starts right after the cap — not after the intrinsic
        // width, which opened a dead gap between image and text (Steam's
        // capsule cells).
        let mut images = ImageSizes::new();
        images.insert("https://example.com/cap.jpg".to_owned(), (60, 10));
        let rows = lay_with_images(
            r#"<body><div style="display:flex">
                 <div style="max-width:15em;flex-shrink:0"><img src="/cap.jpg" style="width:100%"></div>
                 <div>NEIGHBOR</div>
               </div></body>"#,
            100,
            &images,
        );
        let (_, col) = pos_of(&rows, "NEIGHBOR");
        assert!(
            col <= 32,
            "gap after the capped image cell: col {col} {:?}",
            texts(&rows)
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
    fn calc_width_ancestor_bounds_a_percent_image() {
        // A `width:100%` image inside a `width:calc(4px*20)` (80px = 10 cells)
        // wrapper fills the WRAPPER, not the flow band. `pct_width_basis` must
        // resolve the ancestor's `calc()` length — the context-free
        // `css_length_em` returns None for it, so the old walk skipped the
        // wrapper and the avatar fell back to the full band. redbubble sizes its
        // round artist avatar exactly this way (a `padding-bottom:100%` square
        // inside the calc box), and without this it filled the whole page width.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/a.png".to_owned(), (47, 24));
        let rows = lay_with_images(
            r#"<body><div style="display:inline-block;width:calc(4px*20)">
                 <div style="position:relative;padding-bottom:100%;overflow:hidden">
                   <div style="position:absolute;top:0;right:0;bottom:0;left:0">
                     <img src="/a.png" style="display:block;width:100%;height:100%;object-fit:cover">
                   </div></div></div></body>"#,
            80,
            &images,
        );
        let img = image_item(&rows);
        assert_eq!(
            img.width, 10,
            "image fills the 80px (10-cell) box, not the band"
        );
        assert_eq!(
            img.height, 5,
            "square via padding-bottom:100%: 10 cells → 5 rows"
        );
    }

    #[test]
    fn overflow_x_flex_rail_is_a_carousel_not_a_vertical_stack() {
        // A horizontal-scroll container (`overflow-x`) whose flex TRACK is
        // `width:100%` but holds ≥3 fixed-width cards that overflow the band is a
        // carousel: the cards lay side by side (a scrollable strip), NOT stacked
        // vertically at full width. The track itself doesn't clip-x (its scroll
        // PARENT does), so `flow_flex_row` used to stack the slides — making each
        // `width:100%` product image fill the band (redbubble's featured rail).
        let mut images = ImageSizes::new();
        for s in ["a", "b", "c", "d"] {
            images.insert(format!("https://example.com/{s}.png"), (30, 15));
        }
        let rows = lay_with_images(
            r#"<body><div style="overflow-x:scroll"><div style="display:flex;width:100%">
                 <div style="width:20em"><img src="/a.png" style="width:100%"></div>
                 <div style="width:20em"><img src="/b.png" style="width:100%"></div>
                 <div style="width:20em"><img src="/c.png" style="width:100%"></div>
                 <div style="width:20em"><img src="/d.png" style="width:100%"></div>
               </div></div></body>"#,
            60,
            &images,
        );
        let img_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.items.iter().any(|i| i.image.is_some()))
            .map(|(ri, _)| ri)
            .collect();
        assert_eq!(
            img_rows.len(),
            1,
            "all card images share the strip's top row, not stacked: {img_rows:?}"
        );
        for it in rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|i| i.image.is_some())
        {
            assert!(
                it.width <= 40,
                "card image sized to its 20em (40-cell) card, not the 60-cell band: {}",
                it.width
            );
        }
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
        let (_, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (20, 0),
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
        let (_, none, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (20, 0),
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
        let (_, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (60, 0),
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
            pixelated: false,
            invisible: false,
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
        let (rows, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (20, 0),
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
        let (rows, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (20, 0),
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
        let (rows, carousels, _regions, ..) = lay_out_with_carousels(
            &dom,
            &base,
            (40, 0),
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
    fn a_hidden_out_of_flow_page_does_not_push_content_down() {
        // Regression (Steam featured carousels): a `position:absolute` box that
        // is `opacity:0`/`visibility:hidden` (a pre-rendered, not-yet-shown
        // carousel `.next` page of tiles) paints BLANK and — being out of flow —
        // reserves NO in-flow space. It must NOT be placed in the render, else
        // the "grow the containing block to contain its positioned children"
        // deviation STACKS the invisible pages and buries the visible content
        // (Steam's real game grid ended up ~7 viewports down behind blank rows).
        // The active page shows and the following content stays near the top.
        let row_of = |rows: &[Row], needle: &str| {
            rows.iter()
                .position(|r| r.items.iter().any(|i| i.text.contains(needle)))
        };
        for hide in ["opacity:0", "visibility:hidden"] {
            let html = format!(
                r#"<body>
                     <div style="position:relative">
                       <div style="position:absolute;{hide}">
                         <p>P1</p><p>P2</p><p>P3</p><p>P4</p>
                         <p>P5</p><p>P6</p><p>P7</p><p>P8</p>
                       </div>
                       <p>ACTIVEPAGE</p>
                     </div>
                     <p>AFTERCAROUSEL</p>
                   </body>"#
            );
            let rows = lay(&html, 80);
            assert!(shows(&rows, "ACTIVEPAGE"), "the active page shows ({hide})");
            assert!(
                !shows(&rows, "P1"),
                "the hidden abspos page is not rendered ({hide}): {:?}",
                texts(&rows)
            );
            let after = row_of(&rows, "AFTERCAROUSEL").expect("AFTER present");
            assert!(
                after < 6,
                "following content must not be pushed down by the hidden abspos \
                 page ({hide}): landed at row {after}"
            );
        }
        // The MEASUREMENT pass skips it IDENTICALLY (geometry reports what we
        // render — measuring the hidden pages at full size ballooned the
        // engine's document to ~4× the rendered one once decoded image sizes
        // reached the measure pass, so every section below "measured"
        // viewports past the viewport and one-shot lazy-image watchers never
        // fired: the Steam blank-capsules regression). The box keeps an
        // honest ZERO-SIZE rect at its computed position instead.
        let (dom, m) = measure(
            r#"<body><div style="position:relative;height:100px">
                 <div id="h" style="position:absolute;opacity:0;width:80px;height:64px">x</div>
               </div></body>"#,
            80,
        );
        let h = box_by_id(&dom, &m, "h");
        assert!(
            h.width == 0.0 && h.height == 0.0,
            "hidden abspos box measures zero-size at its position, like the \
             render that paints nothing there: {h:?}"
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
    fn a_peek_carousel_of_coincident_cards_collapses_to_the_top_z_card() {
        // Twitch's front-page featured carousel: 5 cards all `position:absolute`
        // at the SAME spot (`top:0; left:calc(50%-375px)`), separated ONLY by a
        // `transform: translateX(…) scale(…)` we don't apply and `z-index`
        // 1/2/3/2/1 (center highest). A terminal can't composite the z-layers, so
        // instead of stacking 5 full-panel images we paint only the focused
        // (top-z, center) card and drop the occluded peeks.
        let mut images = ImageSizes::new();
        for n in 1..=5 {
            images.insert(format!("https://example.com/c{n}.png"), (60, 18));
        }
        let rows = lay_with_images(
            r#"<body><div style="position:relative;width:100%;height:100%">
                 <div style="position:absolute;top:0;left:0;width:600px;z-index:1"><img src="/c1.png"></div>
                 <div style="position:absolute;top:0;left:0;width:600px;z-index:2"><img src="/c2.png"></div>
                 <div style="position:absolute;top:0;left:0;width:600px;z-index:3"><img src="/c3.png"></div>
                 <div style="position:absolute;top:0;left:0;width:600px;z-index:2"><img src="/c4.png"></div>
                 <div style="position:absolute;top:0;left:0;width:600px;z-index:1"><img src="/c5.png"></div>
               </div></body>"#,
            200,
            &images,
        );
        let imgs: Vec<&str> = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter_map(|i| i.image.as_deref())
            .collect();
        assert_eq!(
            imgs,
            vec!["https://example.com/c3.png"],
            "only the focused (z-index 3, center) card paints, not a stack of all 5"
        );
    }

    #[test]
    fn a_shrink_to_fit_card_sizes_to_its_image_not_the_band() {
        // Twitch's featured carousel: a `position:absolute; width:auto` card
        // (shrink-to-fit) holding a `width:100%` streaming thumbnail with no
        // definite-width ancestor. The card must size to the image's INTRINSIC
        // width — not stretch the image out to the whole content band (the
        // full-page player the user flagged). The image's decoded box is 30×17.
        let mut images = ImageSizes::new();
        images.insert("https://example.com/thumb.jpg".to_owned(), (30, 17));
        let rows = lay_with_images(
            r#"<body><div style="position:relative;width:100%;height:100%">
                 <div style="position:absolute;top:0;left:0">
                   <img src="/thumb.jpg" style="width:100%;max-width:100%;height:100%">
                 </div>
               </div></body>"#,
            200,
            &images,
        );
        let img = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("the thumbnail renders");
        assert!(
            img.width <= 30,
            "the image sizes to its 30-cell intrinsic width, not the 200-cell band (got {})",
            img.width
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
    fn logical_float_values_map_by_direction() {
        // css-logical-1: `float: inline-start`/`inline-end` are the
        // flow-relative left/right (LTR-only here). They were silently
        // ignored — the float fell into normal flow.
        let rows = lay(
            r#"<body><div><div style="float:inline-end">RR</div>text here</div></body>"#,
            20,
        );
        let (rr_row, rr_col) = pos_of(&rows, "RR");
        let (t_row, t_col) = pos_of(&rows, "text");
        assert_eq!(
            rr_row,
            t_row,
            "the float shares the text's row: {:?}",
            texts(&rows)
        );
        assert_eq!(rr_col, 18, "inline-end pins to the right edge");
        assert_eq!(t_col, 0, "text flows in the remaining band");
        let rows = lay(
            r#"<body><div><div style="float:inline-start">LL</div>text here</div></body>"#,
            20,
        );
        let (ll_row, ll_col) = pos_of(&rows, "LL");
        let (t_row, t_col) = pos_of(&rows, "text");
        assert_eq!(ll_row, t_row);
        assert_eq!(ll_col, 0, "inline-start pins to the left edge");
        assert!(t_col >= 2, "text flows beside it: {:?}", texts(&rows));
    }

    #[test]
    fn logical_clear_values_clear_their_side() {
        // `clear: inline-start` clears a left float (LTR), like `clear:left`;
        // and a clearfix pseudo declaring a logical value still contains.
        let rows = lay(
            r#"<body><div style="float:left">FF<br>FF</div><p style="clear:inline-start">BELOW</p></body>"#,
            40,
        );
        let (f_row, _) = pos_of(&rows, "FF");
        let (b_row, b_col) = pos_of(&rows, "BELOW");
        assert!(
            b_row > f_row + 1,
            "cleared below both float rows: {:?}",
            texts(&rows)
        );
        assert_eq!(b_col, 0);
        let html = r#"<html><head><style>
            .row::after{content:"";display:table;clear:inline-end}
            .c{float:right;width:50%}
          </style></head>
          <body>
            <div class="row"><div class="c">RIGHT</div></div>
            <p>BELOW</p>
          </body></html>"#;
        let rows = lay(html, 40);
        let (r_row, _) = pos_of(&rows, "RIGHT");
        let (b_row, _) = pos_of(&rows, "BELOW");
        assert!(
            b_row > r_row,
            "a logical-value clearfix still contains its float: {:?}",
            texts(&rows)
        );
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

    #[test]
    fn a_smallwidget_fullbleed_scrim_is_not_a_modal() {
        // Regression: an ordinary small in-page widget (a video "play" scrim,
        // the video.js/streaming-site idiom) that happens to fill its own
        // (non-positioned, small) container via `position:absolute; inset:0`
        // must NOT be mistaken for a page-covering modal. `position:absolute`
        // with no positioned ancestor resolves against the initial containing
        // block, but — unlike `position:fixed` — it still scrolls away with
        // the document and can be clipped by ordinary ancestors our pre-layout
        // heuristic can't see; treating it as a modal wiped the WHOLE
        // surrounding page (nav, chat, other content) down to this widget's
        // own subtree, dropping the enclosing link along with it (the modal
        // entry point uses a bare, context-free `Ctx::root()`). A live
        // streaming-site page (Twitch) hit exactly this: the JS engine
        // advanced far enough to actually mount an offline/preview "play"
        // scrim, whose text content alone (a category badge) was enough for
        // `overlay_has_content` to qualify it — swallowing the rest of the
        // page and the mpv-launch link with it.
        let rows = lay(
            r#"<body>
                 <nav><a href="/browse">Browse</a></nav>
                 <div class="player-area">
                   <a href="x-trust-js:501:"><div style="position:absolute;top:0;right:0;bottom:0;left:0">
                     <div><p>CATEGORY</p></div>
                     <div style="display:flex;width:100%;height:100%" aria-label="Play">
                       <svg width="24" height="24" viewbox="0 0 24 24"><path d="M0 0"></path></svg>
                     </div>
                   </div></a>
                 </div>
                 <div class="chat"><p>Stream Chat</p></div>
               </body>"#,
            80,
        );
        assert!(
            shows(&rows, "Browse"),
            "the nav is NOT deferred behind the play scrim: {:?}",
            texts(&rows)
        );
        assert!(
            shows(&rows, "Stream Chat"),
            "the chat panel is NOT deferred behind the play scrim: {:?}",
            texts(&rows)
        );
        let play = rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Play"))
            .expect("the icon-only Play control surfaces its accessible name");
        assert!(
            play.link.is_some(),
            "the Play control keeps its enclosing link"
        );
    }
}
