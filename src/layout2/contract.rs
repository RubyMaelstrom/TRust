//! The shared layout contract: the output model the browser view renders,
//! and the primitives the `layout2` engine produces it with.
//!
//! A page lays out as a vertical stack of `Row`s, each a left-to-right
//! sequence of positioned `Item`s — so a row can hold several links, inline
//! images, and form controls. Vertical scroll indexes by row; lateral
//! navigation indexes by item. This module owns that contract: `Row`/`Item`/
//! `ItemKind`/`Emphasis`; the scroll/overlay surfaces (`Region`, `Carousel`,
//! `FixedItem`, `CompositeLayer`); `PxRect` geometry for the JS box APIs; the
//! `Units` px→cell context and the session cell-size/border settings; and the
//! CSS value and string helpers shared across the engine (`css_length_px`,
//! `split_track_tokens`, `display_width`, `format_list_marker`, `letter_space`,
//! `is_collapsible_space`, …).
//!
//! `layout2::lay_out_document` produces `Row`s from the fragment tree; the
//! renderer (`ui.rs`/`app.rs`) consumes them. These items are re-exported flat
//! at `crate::layout2::*`.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::doc::Link;
use crate::dom::{Dom, NodeId};

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
/// `set borders on` command flips it; `http::parse_seeded` reads it when
/// laying a document out.
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
/// than threaded through every layout call; defaults to 16 so tests and the
/// 8×16 measurement fixtures round-trip exactly. Read through `Units`;
/// `layout2::measure_boxes_and_grid_tracks` overrides it with its explicit
/// `cell_px.1`.
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
/// Built per element by the engine (`Units::of` for callers outside a layout
/// pass); the default is the
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
    /// box — for callers outside a layout pass (dom.rs's clip checks).
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
    /// the JS box APIs/`getBoundingClientRect` are unaffected) but the renderer
    /// writes BLANK cells for it (spaces for text, no pixels for an image) —
    /// exactly like a browser painting the element transparent. Set by the
    /// engine's inline formatting context, so a whole `opacity:0`
    /// subtree paints blank while still occupying space. This is what makes
    /// React virtualized-list placeholders (`opacity:0` + cached height) report
    /// their real height instead of collapsing.
    pub invisible: bool,
}

/// Inline text emphasis, set by tags (`<b>`/`<i>`/`<u>`/`<s>`) and by CSS
/// (`font-weight`/`font-style`/`text-decoration`). All inherit/propagate, so
/// the engine resolves them from the cascade as it lays each run.
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
    carousel_place(carousels, row, item).map(|(col, _, _)| col)
}

/// The on-screen placement of `item` under any carousel windowing `row` —
/// `Some((screen_col, visible_width, head_cut))`, or `None` if it is scrolled
/// entirely out of the band. TWO regimes, by whether the item is CLIPPABLE:
///
/// - an ATOMIC card (an image/replaced box, no sliceable text) that fits the
///   band keeps the all-or-nothing rule — drawn only when it sits WHOLLY inside
///   the band (`head_cut` 0), so image strips scroll card-by-card (a terminal
///   cell can't horizontally half-paint an image);
/// - TEXT (any width — a `white-space:pre` code line and every piece of it), or
///   an atomic box wider than the band, is PARTIALLY clipped to the band so it
///   can be scrolled through: `head_cut` display columns are shaved off its left
///   and `visible_width` cells remain. (A sub-band text piece is clipped, NOT
///   dropped — the old width-based split vanished code-line pieces at the edge.)
///
/// Items outside every carousel (or left of the band — page content beside the
/// strip) place at their own column, full width. The right frame bar of an
/// enclosing bordered box is static chrome at its fixed column.
fn carousel_place(carousels: &[Carousel], row: usize, item: &Item) -> Option<(u16, u16, usize)> {
    for c in carousels {
        if !c.contains_row(row) {
            continue;
        }
        if item.kind == ItemKind::Border && Some(item.col) == c.frame_right {
            return Some((item.col, item.width, 0));
        }
        if item.col < c.left {
            return Some((item.col, item.width, 0));
        }
        // Screen position of the item's left edge; SIGNED — a wide run scrolled
        // so its head is off the band's left edge starts at a negative column.
        let screen_start = item.col as i32 - c.offset as i32;
        let screen_end = screen_start + item.width as i32;
        let (left, right) = (c.left as i32, c.right as i32);
        // An ATOMIC card — an image/replaced box with no sliceable text — keeps
        // the all-or-nothing rule (drawn only when WHOLLY inside the band): a
        // terminal cell can't horizontally half-paint an image, so image strips
        // scroll card-by-card. TEXT is always clipped to the band's visible
        // slice regardless of width, so a code line's SHORT pieces (a narrow
        // line, a highlighted token, a trailing run) scroll through by the cell
        // instead of VANISHING the instant they straddle a band edge — the
        // width-based split used to treat any sub-band run as an image card,
        // which dropped code-line pieces whole and cut lines short mid-band.
        let atomic = item.image.is_some() || item.text.is_empty();
        if atomic && item.width <= c.right.saturating_sub(c.left) {
            return (screen_start >= left && screen_end <= right).then_some((
                screen_start as u16,
                item.width,
                0,
            ));
        }
        // Text (any width), or an atomic box wider than the band: clip the
        // visible slice.
        let vis_start = screen_start.max(left);
        let vis_end = screen_end.min(right);
        if vis_start >= vis_end {
            return None;
        }
        return Some((
            vis_start as u16,
            (vis_end - vis_start) as u16,
            (vis_start - screen_start) as usize,
        ));
    }
    Some((item.col, item.width, 0))
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
/// visual_start_col, visible_width, head_cut)` left to right — the VISIBLE width
/// (a carousel-clipped wide run shows only its in-band slice, `head_cut` display
/// columns shaved off the left) is the extent to draw / hit-test.
/// Carousel-clipped-out items (not drawn) are omitted.
pub fn visual_columns(
    row: &Row,
    carousels: &[Carousel],
    row_idx: usize,
) -> Vec<(usize, u16, u16, usize)> {
    let mut placed: Vec<(u16, usize, u16, usize)> = row
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let (col, w, cut) = carousel_place(carousels, row_idx, item)?;
            Some((col, i, w, cut))
        })
        .collect();
    placed.sort_by_key(|&(c, ..)| c);
    let mut out = Vec::with_capacity(placed.len());
    let mut col = 0u16;
    for (scol, i, w, cut) in placed {
        let start = scol.max(col);
        out.push((i, start, w, cut));
        col = start + w;
    }
    out
}

/// The substring of `s` covering display columns `[skip, skip + take)` — the
/// window of a horizontally-scrolled inline run (a `white-space:pre` code line
/// in a carousel) that falls inside the visible band. Widths are DISPLAY cells
/// (`display_width`); a wide glyph straddling either cut is dropped (a cell
/// can't show half a glyph), leaving its cell blank.
pub(crate) fn slice_display(s: &str, skip: usize, take: usize) -> String {
    let end = skip + take;
    let mut out = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        if col >= end {
            break;
        }
        let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
        if col >= skip && col + cw <= end {
            out.push(c);
        }
        col += cw;
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

/// CSS `white-space`: how whitespace collapses and whether lines wrap.
/// Inherits; the engine reads it from the cascade (`computed_value`) per
/// element, generalizing the old `<pre>` bool.
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

/// CSS `text-transform`: alters the rendered text of a run. Inherits; read
/// from the cascade (`computed_value`) per run.
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
