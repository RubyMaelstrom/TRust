//! Shared semantic types plus the terminal compatibility output contract.
//!
//! Canonical geometry lives in the CSS-pixel fragments in `flow.rs`; graphical
//! paint uses `render::PagePaint`. After layout, the terminal adapter represents
//! a page as a vertical stack of `Row`s, each a left-to-right
//! sequence of positioned `Item`s — so a row can hold several links, inline
//! images, and form controls. Vertical scroll indexes by row; lateral
//! navigation indexes by item. This module owns that contract: `Row`/`Item`/
//! `ItemKind`/`Emphasis`; the scroll/overlay surfaces (`Region`, `Carousel`,
//! `FixedItem`, `CompositeLayer`); `PxRect` geometry for the JS box APIs; CSS
//! length-resolution `Units`; and the
//! CSS value and string helpers shared across the engine (`css_length_px`,
//! `split_track_tokens`, `display_width`, `format_list_marker`,
//! `is_collapsible_space`, …).
//!
//! `layout2::lay_out_document` adapts the fragment tree into `Row`s; the
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

/// Map from an image's absolute URL to its decoded resource dimensions in
/// image pixels. Responsive-image width/density descriptors apply their
/// density correction per `<img>` before these become CSS natural dimensions;
/// terminal image scaling happens only after canonical layout.
pub type ImageSizes = HashMap<String, (u32, u32)>;

/// The initial containing block for canonical HTML layout, in CSS pixels.
/// Device scale and terminal cells are presentation concerns and are not part
/// of this contract (CSS Values and Units 4 §6.1.2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: finite_non_negative(width),
            height: finite_non_negative(height),
        }
    }
}

/// Terminal presentation parameters at the sole CSS-pixel-to-cell boundary.
///
/// This is intentionally distinct from [`Viewport`]: canonical layout sees
/// only the derived CSS-pixel initial containing block, while the adapter uses
/// the cell metrics afterward to quantize its compatibility `Row`/`Item`
/// output. Keeping the metrics in this value also avoids process-global font
/// state when pages are laid out concurrently.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalViewport {
    pub columns: usize,
    pub rows: usize,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl TerminalViewport {
    pub fn new(columns: usize, rows: usize, cell_width: f32, cell_height: f32) -> Self {
        Self {
            columns: columns.max(10),
            rows,
            cell_width: finite_positive(cell_width),
            cell_height: finite_positive(cell_height),
        }
    }

    pub fn from_font_pixels(columns: usize, rows: usize, cell_pixels: (u16, u16)) -> Self {
        Self::new(
            columns,
            rows,
            f32::from(cell_pixels.0.max(1)),
            f32::from(cell_pixels.1.max(1)),
        )
    }

    pub fn css_viewport(self) -> Viewport {
        Viewport::new(
            self.columns as f32 * self.cell_width,
            self.rows as f32 * self.cell_height,
        )
    }
}

impl Default for TerminalViewport {
    fn default() -> Self {
        Self::new(80, 24, 8.0, 16.0)
    }
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn finite_positive(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

/// The CLIP box `(live_node, client_h_rows, client_w_cells)` of every
/// definite-height scroll-y box flowed (region or fitting). See `Doc.scroll_clips`.
type ScrollClips = Vec<(usize, u16, u16)>;

/// Sentinel `NodeId` for an item that came from no single element
/// (synthesized text like list markers).
pub const NO_NODE: NodeId = usize::MAX;

/// A laid-out element's box in CSS pixels — the backing for the JS geometry
/// APIs (`getBoundingClientRect`, `offset*`/`client*`, IntersectionObserver/
/// ResizeObserver records). `left`/`top` are the element's document-origin
/// position and `width`/`height` its size. Values come directly from canonical
/// fragments and may be fractional; CSSOM View never observes terminal-cell
/// or device-pixel quantization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PxRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// The context a CSS length resolves in: the element's computed font-size
/// (`em`), the root's (`rem`), and the measured advance of U+0030 (`ch`).
/// Terminal metrics deliberately do not appear here.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Units {
    /// The element's computed font-size, CSS px.
    pub fs: f32,
    /// The root element's computed font-size, CSS px (the `rem` basis).
    pub root: f32,
    /// Advance measure of U+0030 ZERO in the element's actual font, CSS px.
    pub ch: f32,
}

impl Default for Units {
    fn default() -> Self {
        let style = crate::text::TextStyle::default();
        Units {
            fs: style.size,
            root: style.size,
            ch: crate::text::zero_advance(&style),
        }
    }
}

impl Units {
    /// The resolution context for `id` in `dom`.
    pub(crate) fn of(dom: &Dom, id: NodeId) -> Units {
        let fs = dom.font_px(id);
        let family = dom
            .computed_value_resolved(id, "font-family")
            .unwrap_or_else(|| String::from("sans-serif"));
        let weight = dom
            .computed_value_resolved(id, "font-weight")
            .as_deref()
            .and_then(css_font_weight)
            .unwrap_or(400.0);
        let italic = dom
            .computed_value_resolved(id, "font-style")
            .is_some_and(|value| css_is_italic(&value));
        let ch = crate::text::zero_advance(&crate::text::TextStyle {
            family,
            size: fs,
            weight,
            italic,
            ..crate::text::TextStyle::default()
        });
        Units {
            fs,
            root: dom.root_font_px(),
            ch,
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
    /// A generated, non-rendering keyboard target for an interactive CSS box
    /// that emitted no text/image item of its own (for example an empty
    /// absolutely-positioned `<a>` stretched over a card). Pointer geometry
    /// lives in `Row::hits`; this item gives the existing selection/navigation
    /// model one stable address without painting a synthetic glyph.
    HitRegion,
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
    /// Terminal-only collision scope inherited from the outermost independent
    /// horizontal formatting context, expressed as `[left, right)`. Items in
    /// one scope share a fixed-cell cursor without displacing sibling panels.
    /// This is not an overflow clip; canonical CSS clipping remains decisive.
    /// `None` is used by synthetic/non-layout items.
    pub terminal_band: Option<(u16, u16)>,
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
    /// Interactive CSS border boxes that exist independently of painted
    /// terminal cells. Stored only on the row containing the box's top edge;
    /// `height` reaches over later rows. `order` is the display-list position,
    /// so a pointer chooses the topmost eligible box rather than whichever
    /// visible text item happened to survive compositing.
    pub hits: Vec<HitBox>,
}

/// A point-hit-test surface for a generated CSS box. It addresses the
/// non-rendering `ItemKind::HitRegion` in the same row, so mouse and keyboard
/// activation share the ordinary `Link` dispatch path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HitBox {
    pub col: u16,
    pub width: u16,
    pub height: u16,
    pub item: usize,
    pub order: usize,
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
    /// A full-viewport decorative fixed box with `z-index:auto` paints below
    /// later content in its stacking context; it remains viewport-pinned.
    pub under_document: bool,
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
    /// Resident page actor node for this scrolling box. The app uses it to
    /// apply page-originated CSSOM `scrollLeft` writes to the retained strip.
    pub live_node: Option<usize>,
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
    /// The author requested `scrollbar-width:none`. CSS Scrollbars Styling 1
    /// §3 requires hiding the scrollbar without disabling other scrolling
    /// mechanisms, so terminal input still operates this carousel.
    pub hide_scrollbar: bool,
}

/// One image layer of an alpha-composited overlap group (layout2 architecture
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
    /// de-lag, incremental-layout contract §14). Populated on every full render; a
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
/// (incremental-layout contract §14). Captured during a full render so a live
/// `Patched{node}` whose boundary matches can re-lay ONLY that subtree and
/// splice it back in place (Tier 1) or splice+shift+scroll-anchor (Tier 2),
/// leaving the rest of the document identity. Two kinds qualify: BLOCK-FILLING
/// IFC containers (`display:flow-root`/`flex`/`grid`, in-flow) whose outer width
/// is their containing block's; and SUB-BOXES (a flex/grid item, inline-block
/// cell) that OWN their rows (no sibling shares a row — proven geometrically at
/// harvest) and are width-stable (verified on patch). A box that shares its rows
/// with siblings is excluded → full path (always correct).
#[derive(Clone, Debug, PartialEq)]
pub struct BoundaryBox {
    /// The live actor node id (baked as `data-trust-node`) — the app maps a
    /// `Patched{node}` to this cached box.
    pub node: usize,
    /// The rows in `Doc.rows` this boundary's content occupies (`start..end`).
    pub row_range: std::ops::Range<usize>,
    /// The left edge (cells) where the re-laid fragment's column 0 maps when
    /// spliced back.
    pub origin_col: u16,
    /// Fractional CSS-pixel phase of the border-box origin within its terminal
    /// cell. Incremental re-layout reuses this phase before quantization so a
    /// proportional line at (say) y=18.625px snaps exactly as it did in the
    /// full document rather than as if the subtree began at y=0.
    pub quantization_phase: (f32, f32),
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

// `quantization_phase` is produced only by `rem_euclid` over finite positive
// cell metrics and finite fragment coordinates, so it can never contain NaN.
// Under that construction invariant, the derived `PartialEq` is an equivalence
// relation and the terminal document's existing `Eq` contract remains sound.
impl Eq for BoundaryBox {}

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
        .filter(|(_, item)| item.kind != ItemKind::HitRegion)
        .filter_map(|(i, item)| {
            let (col, w, cut) = carousel_place(carousels, row_idx, item)?;
            Some((col, i, w, cut))
        })
        .collect();
    placed.sort_by_key(|&(c, ..)| c);
    let mut out = Vec::with_capacity(placed.len());
    let mut unbanded_col = 0u16;
    let mut band_cols: HashMap<(u16, u16), u16> = HashMap::new();
    for (scol, i, w, cut) in placed {
        let item = &row.items[i];
        let terminal_band = (!carousels
            .iter()
            .any(|carousel| carousel.contains_row(row_idx)))
        .then_some(item.terminal_band)
        .flatten()
        .filter(|(left, right)| left < right);
        if let Some((left, right)) = terminal_band {
            let col = band_cols.entry((left, right)).or_insert(left);
            let displaced = scol.max(*col).max(left);
            // Atomic content cannot be partially represented or painted back
            // over the text that displaced it. The vertical allocator keeps
            // ordinary block successors on distinct rows; if an exceptional
            // overlap still leaves no room, omit the atomic fallback for this
            // cell frame instead of overwriting text or crossing the panel.
            if (item.image.is_some() || item.text.is_empty())
                && scol >= left
                && scol.saturating_add(w) <= right
                && displaced.saturating_add(w) > right
            {
                continue;
            }
            // A band scopes collision recovery; it is not itself an overflow
            // clip. Canonical CSS clipping was already resolved from shaped
            // clusters in paint, while `overflow:visible` and independently
            // sized flex/atomic labels must retain their complete spelling.
            let start = displaced;
            out.push((i, start, w, cut));
            *col = start.saturating_add(w);
        } else {
            let start = scol.max(unbanded_col);
            out.push((i, start, w, cut));
            unbanded_col = start + w;
        }
    }
    out
}

/// The on-screen horizontal interval of a non-painting hit box after applying
/// the same carousel window as its synthetic item. Unlike `visual_columns`,
/// this deliberately does not participate in overlap-append: CSS point hit
/// testing uses the box's painted position, while overlap-append is only the
/// terminal's fallback for displaying overlapping text.
pub fn hit_columns(
    row: &Row,
    carousels: &[Carousel],
    row_idx: usize,
    hit: &HitBox,
) -> Option<(u16, u16)> {
    let item = row.items.get(hit.item)?;
    let (col, width, _) = carousel_place(carousels, row_idx, item)?;
    (width > 0).then_some((col, width))
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
        let mut item_map = HashMap::new();
        for (bi, it) in brow.items.iter().enumerate() {
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
            it.terminal_band = Some(
                it.terminal_band
                    .map(|(left, right)| {
                        (left.saturating_add(rg.left), right.saturating_add(rg.left))
                    })
                    .unwrap_or((rg.left, rg.left.saturating_add(rg.width))),
            );
            item_map.insert(bi, merged.items.len());
            merged.items.push(it);
        }
        for hit in &brow.hits {
            let Some(&item) = item_map.get(&hit.item) else {
                continue;
            };
            let source = &brow.items[hit.item];
            let Some((bcol, visible_w, _)) = carousel_place(&rg.carousels, buf_idx, source) else {
                continue;
            };
            if bcol >= rg.width {
                continue;
            }
            let width = visible_w.min(rg.width - bcol);
            if width == 0 {
                continue;
            }
            merged.hits.push(HitBox {
                col: bcol + rg.left,
                width,
                height: hit.height.min(rg.height),
                item,
                order: hit.order,
            });
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
/// (incremental-layout contract §14). `rows` are in the fragment's own coordinate
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
    unique_structured_media(dom, base, true).and_then(|m| m.thumbnail)
}

/// A media resource described by HTML Microdata + Schema.org. Modern players
/// commonly leave `<video>` sourceless (MSE owns it) while publishing the real
/// resource beside it as a `VideoObject`/`AudioObject`. HTML §5.2.5 defines
/// which nodes are properties of an item (including `itemref`, and stopping at
/// nested `itemscope` nodes); §5.2.4 defines the value-bearing attribute for
/// each element. Schema.org's `MediaObject` vocabulary defines `contentUrl` as
/// the actual media bytes and `embedUrl` as a player for that specific media.
#[derive(Clone, Debug)]
pub(crate) struct StructuredMedia {
    pub(crate) item: NodeId,
    pub(crate) target: Url,
    pub(crate) thumbnail: Option<String>,
}

/// Find the structured-media item associated with `media`. Association is
/// deliberately structural, never host-based: at the nearest ancestor band
/// containing exactly one media element of this kind and exactly one matching
/// Schema.org item, the two are unambiguous. This covers a page's sole player
/// and repeated preview cards without assigning one card's URL to another.
pub(crate) fn structured_media_for(
    dom: &Dom,
    base: &Url,
    media: NodeId,
    video: bool,
) -> Option<StructuredMedia> {
    let candidates = structured_media_items(dom, base, video);
    if candidates.is_empty() {
        return None;
    }
    let media_tag = if video { "video" } else { "audio" };
    let peer_media: Vec<_> = dom
        .flat_descendants(crate::dom::DOCUMENT)
        .into_iter()
        .filter(|&id| dom.tag_name(id) == Some(media_tag))
        .collect();
    let mut ancestor = Some(media);
    while let Some(root) = ancestor {
        let in_band: Vec<_> = candidates
            .iter()
            .filter(|m| composed_contains(dom, root, m.item))
            .collect();
        if in_band.len() == 1
            && peer_media
                .iter()
                .filter(|&&id| composed_contains(dom, root, id))
                .take(2)
                .count()
                == 1
        {
            return Some(in_band[0].clone());
        }
        ancestor = dom.parent_composed(root);
    }
    None
}

fn composed_contains(dom: &Dom, ancestor: NodeId, node: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = dom.parent_composed(id);
    }
    false
}

/// The one structured video describing the page, if there is exactly one.
/// Uniqueness is the cross-site distinction between a watch page and an
/// aggregate/gallery containing several independent videos.
pub(crate) fn unique_structured_media(
    dom: &Dom,
    base: &Url,
    video: bool,
) -> Option<StructuredMedia> {
    let mut items = structured_media_items(dom, base, video).into_iter();
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

fn structured_media_items(dom: &Dom, base: &Url, video: bool) -> Vec<StructuredMedia> {
    let kind = if video { "VideoObject" } else { "AudioObject" };
    dom.descendants(crate::dom::DOCUMENT)
        .filter(|&id| schema_item_is(dom, id, kind))
        .filter_map(|item| {
            let target = ["contentUrl", "embedUrl", "url"]
                .into_iter()
                .find_map(|name| microdata_url_property(dom, base, item, name))?;
            let thumbnail = ["thumbnailUrl", "thumbnail"]
                .into_iter()
                .find_map(|name| microdata_url_property(dom, base, item, name))
                .map(|u| u.to_string());
            Some(StructuredMedia {
                item,
                target,
                thumbnail,
            })
        })
        .collect()
}

fn schema_item_is(dom: &Dom, id: NodeId, kind: &str) -> bool {
    if dom.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml")
        || dom.attr(id, "itemscope").is_none()
    {
        return false;
    }
    dom.attr(id, "itemtype").is_some_and(|types| {
        types.split_ascii_whitespace().any(|ty| {
            Url::parse(ty).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && matches!(u.host_str(), Some("schema.org" | "www.schema.org"))
                    && u.path().trim_matches('/') == kind
            })
        })
    })
}

/// First property value in Microdata tree order. This is the HTML §5.2.5
/// pending-list walk: seed the item's children plus `itemref` targets, do not
/// descend through a nested item, and accept every whitespace-separated
/// `itemprop` name on the current element.
fn microdata_url_property(dom: &Dom, base: &Url, item: NodeId, name: &str) -> Option<Url> {
    use std::collections::{HashSet, VecDeque};

    let mut pending: VecDeque<NodeId> = dom.children(item).into();
    if let Some(refs) = dom.attr(item, "itemref") {
        for target in refs
            .split_ascii_whitespace()
            .filter_map(|id| dom.get_by_id(id))
        {
            pending.push_back(target);
        }
    }
    let mut seen = HashSet::new();
    let mut results = HashSet::new();
    while let Some(id) = pending.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let is_html = dom.namespace_uri(id) == Some("http://www.w3.org/1999/xhtml");
        let is_property = is_html
            && dom
                .attr(id, "itemprop")
                .is_some_and(|names| names.split_ascii_whitespace().any(|n| n == name));
        let has_scope = is_html && dom.attr(id, "itemscope").is_some();
        if is_property && !has_scope {
            results.insert(id);
        }
        if !has_scope {
            pending.extend(dom.children(id));
        }
    }
    // Step 6 sorts the result nodes in tree order before values are read.
    dom.descendants(crate::dom::DOCUMENT)
        .filter(|id| results.contains(id))
        .find_map(|id| {
            let raw = microdata_value(dom, id)?.trim();
            match crate::http::resolve(base, raw) {
                Link::Http(url) => Some(url),
                _ => None,
            }
        })
}

/// The URL-capable cases of HTML §5.2.4's property-value algorithm. Schema.org
/// URL properties should use these URL property elements; `meta[content]` is
/// also accepted because the algorithm defines its string value and deployed
/// Schema.org producers commonly use it for machine-only URLs.
fn microdata_value(dom: &Dom, id: NodeId) -> Option<&str> {
    match dom.tag_name(id)? {
        "meta" => dom.attr(id, "content"),
        "audio" | "embed" | "iframe" | "img" | "source" | "track" | "video" => dom.attr(id, "src"),
        "a" | "area" | "link" => dom.attr(id, "href"),
        "object" => dom.attr(id, "data"),
        _ => None,
    }
}

/// Whether the page declares ITSELF a video page via cross-site metadata: an
/// Open Graph `og:video`(/`:secure_url`) resource, an `og:type` in the
/// `video.*` hierarchy (ogp.me), or exactly one Schema.org `VideoObject`.
/// Uniqueness keeps a gallery/feed from becoming one ambiguous page-level
/// target. This signal — never a host check — is shared by the page-level mpv
/// fallback and preview-image decode gate. A present `<video>` is itself the
/// standardized playback signal and does not need metadata qualification.
pub(crate) fn page_declares_video(dom: &Dom) -> bool {
    ["og:video", "og:video:secure_url"]
        .into_iter()
        .any(|k| dom.meta_content(k).is_some())
        || dom
            .meta_content("og:type")
            .is_some_and(|t| t.trim().to_ascii_lowercase().starts_with("video"))
        || dom
            .descendants(crate::dom::DOCUMENT)
            .filter(|&id| schema_item_is(dom, id, "VideoObject"))
            .take(2)
            .count()
            == 1
}

/// Resolve the numeric CSS Fonts weight used by the font matcher. Relative
/// weights are approximated against the inherited normal face until the
/// cascade stores parent-relative numeric computed values.
pub(crate) fn css_font_weight(value: &str) -> Option<f32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(400.0),
        "bold" => Some(700.0),
        "bolder" => Some(700.0),
        "lighter" => Some(300.0),
        number => number
            .parse::<f32>()
            .ok()
            .map(|weight| weight.clamp(1.0, 1000.0)),
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
/// ratios (96px/in), unitless treated as px. `ch` is the shaped advance of
/// U+0030 ZERO in the element's selected font; `ex` uses the CSS-permitted
/// 0.5em fallback until an x-height query is threaded into `Units`.
/// Context-dependent values (`%`/`vw`/`calc()`/`auto`) return `None` here;
/// `value.rs` resolves them with the containing block and layout viewport.
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
        "ch" => n * u.ch,
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
