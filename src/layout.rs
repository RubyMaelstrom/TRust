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

use url::Url;

use crate::doc::{Form, Link};
use crate::dom::{DOCUMENT, Dom, NodeData, NodeId};

/// Map from a control element's `NodeId` to its `(form, field)` indices
/// (built by `http::extract_forms_arena`), so the layout can surface
/// form controls as selectable `Link::Form` items.
pub type ControlMap = HashMap<NodeId, (usize, usize)>;

/// Map from an image's absolute URL to its decoded cell box `(width,
/// height)`, built by the app's decode pipeline. An image present here
/// lays out as a real W×H box (reserving rows); one absent falls back to
/// alt text.
pub type ImageSizes = HashMap<String, (u16, u16)>;

/// Sentinel `NodeId` for an item that came from no single element
/// (synthesized text like list markers).
pub const NO_NODE: NodeId = usize::MAX;

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
        if c.contains_row(row) && item.col >= c.left {
            return c.shows(item.col, item.width).then(|| item.col - c.offset);
        }
    }
    Some(item.col)
}

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
        col = start + it.text.chars().count();
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
) -> Vec<Row> {
    lay_out_with_carousels(dom, base, width, forms, controls, images).0
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
) -> (Vec<Row>, Vec<Carousel>) {
    let mut layout = Layout::new(dom, base, width.max(10), forms, controls, images);
    let root = body_or_document(dom);
    let ctx = Ctx::root();
    for child in dom.children(root) {
        layout.flow_node(child, &ctx);
    }
    layout.flush_block();
    layout.finish_floats();
    layout.finish()
}

/// The `<body>` element, or the document node if there isn't one.
fn body_or_document(dom: &Dom) -> NodeId {
    dom.descendants(DOCUMENT)
        .into_iter()
        .find(|&id| dom.tag_name(id) == Some("body"))
        .unwrap_or(DOCUMENT)
}

/// The narrowest a flexible flex-row column may be before the row stacks
/// vertically instead (the responsive fallback) — below this, columns are
/// too thin to read.
const MIN_COL: usize = 12;

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
    node: NodeId,
    link: Option<Link>,
}

impl Ctx {
    fn root() -> Self {
        Ctx {
            kind: ItemKind::Text,
            emph: Emphasis::default(),
            transform: TextTransform::None,
            node: NO_NODE,
            link: None,
        }
    }
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

/// Elements whose subtree never renders as page text.
const SKIP: &[&str] = &[
    "audio", "base", "canvas", "head", "iframe", "link", "math", "meta", "noscript", "object",
    "script", "style", "svg", "template", "title", "video",
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
}

impl<'a> Layout<'a> {
    fn new(
        dom: &'a Dom,
        base: &'a Url,
        width: usize,
        forms: &'a [Form],
        controls: &'a ControlMap,
        images: &'a ImageSizes,
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

    fn flow_element(&mut self, id: NodeId, ctx: &Ctx) {
        let Some(tag) = self.dom.tag_name(id).map(str::to_owned) else {
            return;
        };
        if SKIP.contains(&tag.as_str())
            || self.dom.is_hidden(id)
            || self.suppressed_controls.contains(&id)
        {
            return;
        }
        // A floated element leaves normal flow: pin it to an edge and let the
        // following content wrap beside it (across blocks, until cleared or
        // its bottom is passed). Checked before the tag dispatch so a floated
        // `<img>` floats too; skipped when laying the float's own box.
        if self.float_skip != Some(id)
            && let Some(side) = self.float_side(id)
        {
            self.flow_float(id, side);
            return;
        }
        // A slide deck (a container whose children are ALL absolutely
        // positioned, so they overlap) renders one slide at a time, paged by
        // generated controls — a carousel of stacked slides. Checked before
        // the tag dispatch so an inline `<a>` wrapping the slides still routes.
        if self.dom.is_slideshow_container(id) {
            self.flow_slideshow(id);
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
                self.flow_form_control(id, &tag);
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

        // Block vs inline is driven by the cascaded `display` (baked into
        // the serialized HTML by the engine, which has the sheets), with
        // the HTML tag default as the fallback. This is what lets an
        // inline-styled `<li>` nav flow across one row instead of becoming
        // a vertical bullet tower.
        let flow = self.flow_of(id, &tag);
        if flow == Flow::None {
            return;
        }
        let block_like = matches!(flow, Flow::Block | Flow::ListItem);
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
        // its own lines and its descendants until they override it.
        let saved_align = self.align;
        if block_like
            && let Some(a) = self
                .dom
                .computed_value(id, "text-align")
                .as_deref()
                .and_then(Align::from_css)
        {
            self.align = a;
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
        // would strand it on a row of its own. Instead we indent the item's
        // content by the marker width and drop the marker into the gutter on
        // the item's first content row at block exit (`place_list_marker`).
        let list_marker = (flow == Flow::ListItem).then(|| self.next_list_marker(id));
        let marker_indent = list_marker.as_ref().map_or(0, |m| m.chars().count());
        if marker_indent > 0 {
            self.indent += marker_indent;
            self.begin_line();
        }
        let marker_start_row = self.rows.len();

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

        if hscroll {
            self.flow_hscroll(id);
        } else {
            match flex {
                Some(FlexMode::Grid) => self.flow_flex_wrap(id),
                Some(FlexMode::Row) => self.flow_flex_row(id),
                Some(FlexMode::Column) => self.stack_flex_items(id),
                None => {
                    for child in self.dom.children(id) {
                        self.flow_node(child, &cctx);
                    }
                }
            }
        }

        // ...and `::after` closes it.
        if let Some(t) = self.pseudo_text(id, crate::dom::PseudoEl::After) {
            self.place_text(&t, &cctx);
        }

        // A button-less form carries its synthetic submit on the form
        // node: emit it on its own row at the end of the form.
        if tag == "form"
            && let Some(&(form, field)) = self.controls.get(&id)
        {
            let label = self.field_label(form, field);
            if !label.is_empty() {
                self.flush_block();
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
            }
            if let Some(marker) = list_marker {
                let gutter = self.indent.saturating_sub(marker_indent);
                self.place_list_marker(&marker, marker_start_row, gutter);
            }
            if self.gap_after(id, &tag) {
                self.push_blank();
            }
        }
        self.ws = saved_ws;
        self.align = saved_align;
        if pushed_list {
            self.list_stack.pop();
        }
        self.indent -= center_pad;
        self.width = saved_width;
        self.indent -= marker_indent;
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

    /// The left indent (cells) a block contributes: CSS `margin-left` +
    /// `padding-left` when set (even to 0), else the HTML tag default.
    fn block_indent(&self, id: NodeId, tag: &str) -> usize {
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

    /// Constrain a block to its explicit `width`/`max-width` when it carries
    /// horizontal `margin:auto`, and shift its band to position it (centered
    /// for both-auto, right for left-auto). Mutates `indent`/`width`; returns
    /// the left pad added (restored at block exit). 0 = left unconstrained.
    /// Only acts on auto-margin blocks (a deliberate "center/position me"
    /// signal) so a bare pixel width never cramps content we'd flow wide.
    fn constrain_block_width(&mut self, id: NodeId) -> usize {
        let avail = self.width.saturating_sub(self.indent).max(1);
        let Some(w) = self
            .css_cells(id, "width")
            .or_else(|| self.css_cells(id, "max-width"))
        else {
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
            (false, false) => return 0,
        };
        self.indent += pad;
        self.width = self.indent + w;
        pad
    }

    /// Whether a block opens with a blank spacer row: CSS top
    /// margin/padding when set, else the tag default (`SPACING`).
    fn gap_before(&self, id: NodeId, tag: &str) -> bool {
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
        // We can't place `position:absolute`/`fixed` boxes at coordinates, so
        // we render them in normal flow — but as INLINE, not block. An overlay
        // (slideshow arrows/dots, a badge, a "new" ribbon) then collapses to a
        // compact run instead of each control claiming its own block line.
        if self.is_out_of_flow(id) {
            return Flow::Inline;
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

    /// Whether an element is taken out of normal flow by `position:absolute`
    /// or `fixed` (an overlay we render compactly inline, since we can't
    /// position it).
    fn is_out_of_flow(&self, id: NodeId) -> bool {
        matches!(
            self.dom.computed_style(id, "position").as_deref(),
            Some("absolute" | "fixed")
        )
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
    fn flow_flex_row(&mut self, id: NodeId) {
        let avail = self.width.saturating_sub(self.indent).max(1);
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
            // Overflow: shrink. A shrinkable item can shrink to its min-content
            // width; a `flex-shrink:0` item keeps its basis. If even the
            // minimum row overflows, stack instead.
            let floor: Vec<usize> = (0..n)
                .map(|i| {
                    if shrink[i] > 0.0 {
                        self.measure_width(nodes[i], 1).min(basis[i])
                    } else {
                        basis[i]
                    }
                })
                .collect();
            let sum_floor: usize = floor.iter().sum();
            if sum_floor + gaps > avail {
                self.stack_boxes(&nodes, avail);
                return;
            }
            let extra = avail - sum_floor - gaps;
            let desired: usize = (0..n).map(|i| basis[i] - floor[i]).sum();
            for i in 0..n {
                // Hand each item its share of the slack above its minimum,
                // proportional to how much it wanted (basis − floor); split it
                // evenly when nothing wants extra.
                let share = (extra * (basis[i] - floor[i]))
                    .checked_div(desired)
                    .unwrap_or(extra / n);
                widths[i] = floor[i] + share;
            }
        }
        // `justify-content` distributes any leftover free space (when grow
        // didn't consume it): a leading offset and/or extra spacing between
        // items. No-op when the row is full (free == 0) or left-packed.
        let used: usize = widths.iter().map(|w| (*w).max(1)).sum::<usize>() + gaps;
        let free = avail.saturating_sub(used);
        let (lead, between) = self.justify_offsets(id, free, n);
        let row_base = self.rows.len();
        let mut x = lead;
        for i in 0..n {
            let cw = widths[i].max(1);
            let mut b = self.layout_subtree(nodes[i], cw);
            // A text/search input that grew (flex-grow) fills its box like a
            // real input field, rather than leaving a gap after its short
            // placeholder. (Buttons/selects don't stretch — see the helper.)
            if grow[i] > 0.0 {
                fill_input_box(&mut b, cw);
            }
            if b.height > 0 {
                self.blit(&b, (self.indent + x) as u16, row_base);
            }
            x += cw + if i + 1 < n { gap + between } else { 0 };
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// `justify-content` main-axis distribution of `free` leftover cells across
    /// `n` items: `(leading offset, extra spacing per inter-item gap)`. Packing
    /// (`flex-start`/`normal`/unknown) and a full row leave both zero; grow
    /// items having eaten the free space makes this moot.
    fn justify_offsets(&self, id: NodeId, free: usize, n: usize) -> (usize, usize) {
        if free == 0 {
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
        self.layout_subtree_inner(id, constraint, None, true).width as usize
    }

    /// A flex item's `(basis, grow, shrink)`. `basis` resolves `flex-basis`
    /// (else `width`, `%` against `avail`, capped by `max-width`); `None`
    /// means auto (size to content). Defaults: grow 0, shrink 1.
    fn flex_props(&self, id: NodeId, avail: usize) -> (Option<usize>, f32, f32) {
        let mut grow = 0.0f32;
        let mut shrink = 1.0f32;
        let mut basis_css: Option<String> = None;
        // The `flex` shorthand seeds all three (keywords or `grow [shrink]
        // [basis]`); the longhands below override.
        if let Some(f) = self.dom.computed_style(id, "flex") {
            match f.trim().to_ascii_lowercase().as_str() {
                "none" => {
                    grow = 0.0;
                    shrink = 0.0;
                    basis_css = Some("auto".into());
                }
                "auto" => {
                    grow = 1.0;
                    shrink = 1.0;
                    basis_css = Some("auto".into());
                }
                "initial" | "" => {}
                other => {
                    let mut nums = Vec::new();
                    for p in other.split_whitespace() {
                        match p.parse::<f32>() {
                            Ok(num) => nums.push(num),
                            Err(_) => basis_css = Some(p.to_string()),
                        }
                    }
                    if let Some(&g) = nums.first() {
                        grow = g;
                    }
                    if let Some(&s) = nums.get(1) {
                        shrink = s;
                    }
                    // A single number (`flex:1`) means basis 0.
                    if nums.len() == 1 && basis_css.is_none() {
                        basis_css = Some("0".into());
                    }
                }
            }
        }
        if let Some(g) = self.flex_number(id, "flex-grow") {
            grow = g;
        }
        if let Some(s) = self.flex_number(id, "flex-shrink") {
            shrink = s;
        }
        if let Some(b) = self.dom.computed_style(id, "flex-basis") {
            basis_css = Some(b);
        }
        let basis = match basis_css.as_deref().map(str::trim) {
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
        (basis, grow.max(0.0), shrink.max(0.0))
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
        for card in self.flex_items(track) {
            let cw = self
                .css_cells(card, "width")
                .or_else(|| self.css_cells(card, "max-width"))
                .map(|w| w.min(avail))
                .unwrap_or_else(|| self.measure_width(card, avail))
                .clamp(1, avail);
            // Lay the card as a block (ignore its own float), then place it.
            let b = self.layout_subtree_inner(card, cw, Some(card), false);
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
            });
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// Lay a slide deck (stacked, absolutely-positioned slides) as a carousel
    /// showing ONE slide at a time: each slide laid at the full band width,
    /// side by side, with generated controls to page between them. The opacity
    /// cascade keeps the inactive slides "in the background" (present but
    /// off-band, so not rendered) until a control reveals the next — the
    /// slideshow analogue of the carousel, for JS slideshows whose switching
    /// we can't drive through the page's own (frozen-timer) script.
    fn flow_slideshow(&mut self, id: NodeId) {
        self.flush_block();
        self.begin_line();
        let band_left = self.line_left;
        let band_w = self.line_right.saturating_sub(self.line_left).max(1);
        // A deck is often the first thing in its box; reserve a row above the
        // band so the generated prev/next controls have somewhere to sit.
        if self.rows.is_empty() {
            self.push_blank();
        }
        let row_base = self.rows.len();
        let mut x = 0usize;
        let mut stops = Vec::new();
        let mut height = 0usize;
        for slide in self.dom.children(id) {
            if self.dom.tag_name(slide).is_none() {
                continue; // text/comment node between slides
            }
            // Each slide fills the band; lay it and place it one band to the
            // right of the last, so exactly one shows at a time.
            let b = self.layout_subtree_inner(slide, band_w, Some(slide), false);
            if b.height == 0 {
                continue;
            }
            stops.push(x as u16);
            self.blit(&b, (band_left + x) as u16, row_base);
            x += band_w; // no gap — one slide per band
            height = height.max(b.height as usize);
        }
        // Two or more slides make a switchable deck; a lone slide just renders.
        if stops.len() >= 2 {
            self.emit_scroll_buttons(id, row_base, band_left, band_w, true);
            self.carousels.push(Carousel {
                start: row_base,
                end: row_base + height,
                left: band_left as u16,
                right: (band_left + band_w) as u16,
                width: x as u16,
                stops,
                offset: 0,
            });
        }
        self.col = self.indent;
        self.pending_space = false;
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
        for d in self.dom.descendants(id) {
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
    fn stack_flex_items(&mut self, id: NodeId) {
        let kids = self.flex_items(id);
        let avail = self.width.saturating_sub(self.indent).max(1);
        self.stack_boxes(&kids, avail);
    }

    /// Stack a set of child boxes vertically at `width`, each below the
    /// last (shared by column flex and the row fallback).
    fn stack_boxes(&mut self, kids: &[NodeId], width: usize) {
        let mut row = self.rows.len();
        for &k in kids {
            let b = self.layout_subtree(k, width);
            if b.height == 0 {
                continue;
            }
            self.blit(&b, self.indent as u16, row);
            row += b.height as usize;
        }
        self.col = self.indent;
        self.pending_space = false;
    }

    /// The element children of a flex container that generate flex items
    /// (skipping hidden ones and whitespace/text nodes).
    fn flex_items(&self, id: NodeId) -> Vec<NodeId> {
        self.dom
            .children(id)
            .into_iter()
            .filter(|&c| {
                matches!(self.dom.node(c).data, NodeData::Element { .. }) && !self.dom.is_hidden(c)
            })
            .collect()
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
    fn flow_float(&mut self, id: NodeId, side: FloatSide) {
        // Floats begin at a line boundary; refresh the band first.
        self.flush_block();
        self.begin_line();
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let explicit = self
            .css_cells(id, "width")
            .or_else(|| self.css_cells(id, "max-width"))
            .map(|w| w.min(avail));
        let constraint = explicit.unwrap_or(avail).max(1);
        let boxed = self.layout_subtree_inner(id, constraint, Some(id), false);
        if boxed.height == 0 {
            return;
        }
        let w = explicit.unwrap_or(boxed.width as usize).min(avail).max(1);
        // Responsive fallback: a float that leaves too thin a band beside it
        // (a desktop-width column dropped into a terminal-width viewport)
        // becomes an in-flow block — stacked, never overlapped. Mirrors the
        // flex-row MIN_COL fallback; this is what keeps a full-width main
        // column from being painted over by what follows it.
        if avail.saturating_sub(w + 1) < MIN_COL {
            let row_base = self.rows.len();
            self.blit(&boxed, self.line_left as u16, row_base);
            self.col = self.line_left;
            self.pending_space = false;
            self.begin_line();
            return;
        }
        let start_row = self.rows.len();
        let bottom = start_row + boxed.height as usize;
        // Pin beside any floats already on this row (line_left/right already
        // account for them).
        let col = match side {
            FloatSide::Left => self.line_left,
            FloatSide::Right => self.line_right.saturating_sub(w),
        } as u16;
        self.floats.push(Float {
            side,
            col,
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

    /// Lay a flex-wrap container's children out as a grid: each child is
    /// laid out as an independent box, then SHELF-PACKED left→right, the
    /// shelf wrapping to a new band when the next box won't fit. Assumes a
    /// block boundary (line empty, `col == indent`); appends finished rows
    /// directly via `blit` and leaves the cursor back at the indent.
    fn flow_flex_wrap(&mut self, id: NodeId) {
        let avail = self.width.saturating_sub(self.indent).max(1);
        let gap = self.flex_gap(id, avail, false);
        let row_gap = self.flex_gap(id, avail, true);
        let mut shelf_top = self.rows.len();
        let mut x = 0usize; // used width of the current shelf (relative to indent)
        let mut shelf_height = 0usize;
        for child in self.flex_items(id) {
            // An explicit `width` fixes the item's box (it occupies that
            // width even when its content is narrower); without one the box
            // is laid out at the available width and shrinks to its content.
            let explicit = self.css_cells(child, "width").map(|w| w.min(avail).max(1));
            let constraint = explicit
                .or_else(|| self.css_cells(child, "max-width").map(|w| w.min(avail)))
                .unwrap_or(avail)
                .max(1);
            let b = self.layout_subtree(child, constraint);
            if b.height == 0 {
                continue;
            }
            let w = explicit.unwrap_or(b.width as usize).min(avail).max(1);
            // Wrap to the next shelf when this box won't fit beside the
            // current one (but never wrap an empty shelf — an over-wide box
            // takes its own band, clamped to the available width).
            if x > 0 && x + gap + w > avail {
                shelf_top += shelf_height + row_gap;
                x = 0;
                shelf_height = 0;
            }
            let lead = if x > 0 { gap } else { 0 };
            let col_off = self.indent + x + lead;
            self.blit(&b, col_off as u16, shelf_top);
            x += lead + w;
            shelf_height = shelf_height.max(b.height as usize);
        }
        self.col = self.indent;
        self.pending_space = false;
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

    /// Lay an element's subtree out as an independent box at `content_width`,
    /// positioned relative to its own top-left (`col` 0). Shares the DOM,
    /// base URL, form/control maps, and image sizes with the parent. The
    /// recursion that powers grids and (later) columns and floats.
    fn layout_subtree(&self, id: NodeId, content_width: usize) -> LaidBox {
        self.layout_subtree_inner(id, content_width, None, false)
    }

    /// `layout_subtree`, optionally ignoring the float on the root element
    /// (used when laying a float's own box so it doesn't recurse). `measure`
    /// means this is an intrinsic-width measurement (ignore `text-align`).
    fn layout_subtree_inner(
        &self,
        id: NodeId,
        content_width: usize,
        skip_float: Option<NodeId>,
        measure: bool,
    ) -> LaidBox {
        let mut sub = Layout::new(
            self.dom,
            self.base,
            content_width.max(1),
            self.forms,
            self.controls,
            self.images,
        );
        sub.float_skip = skip_float;
        sub.measuring = measure;
        sub.flow_node(id, &Ctx::root());
        sub.flush_block();
        sub.finish_floats();
        let (rows, carousels) = sub.finish();
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
            self.carousels.push(c);
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
        let word = transformed.as_ref();
        let wlen = word.chars().count();
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
            let len = seg.chars().count();
            self.push_preserved_item(seg, len, ctx);
            return;
        }
        // pre-wrap: char-budget wrap within the content box, keeping spaces.
        let avail = self.line_right.saturating_sub(self.line_left).max(1);
        let mut buf = String::new();
        let mut chars = seg.chars().peekable();
        while let Some(c) = chars.next() {
            buf.push(c);
            if buf.chars().count() >= avail && chars.peek().is_some() {
                let len = buf.chars().count();
                self.push_preserved_item(&buf, len, ctx);
                self.break_line();
                buf.clear();
            }
        }
        if !buf.is_empty() {
            let len = buf.chars().count();
            self.push_preserved_item(&buf, len, ctx);
        }
    }

    /// Push one preserved-whitespace run, inheriting the context's kind
    /// and emphasis.
    fn push_preserved_item(&mut self, text: &str, len: usize, ctx: &Ctx) {
        self.push_item(text.to_owned(), len, ctx.kind, ctx.emph, ctx.node, None);
    }

    fn place_image(&mut self, id: NodeId, ctx: &Ctx) {
        // A decoded image (size known) lays out as a real W×H box; an
        // undecoded or failed one falls back to its alt text.
        if let Some(url) = self.image_src(id)
            && let Some(&(w, h)) = self.images.get(&url)
            && w > 0
            && h > 0
        {
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
            node: id,
            link: ctx.link.clone(),
        };
        self.place_text(text, &img_ctx);
    }

    /// The accessible name for an element whose content won't render
    /// anything (no text, no `<img>`) — an SVG/icon-only link. Reads
    /// `aria-label`, then `title`, then `alt`. `None` when it has real
    /// content or no name to show.
    fn icon_only_label(&self, id: NodeId) -> Option<String> {
        if !self.dom.text_content(id).trim().is_empty() {
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
    /// its height are emitted by `break_line` from `line_height`. A box
    /// wider than the content width is clamped (height rescaled to keep
    /// the aspect the encode will use).
    fn place_image_box(&mut self, id: NodeId, ctx: &Ctx, url: String, w: u16, h: u16) {
        let avail = self.line_right.saturating_sub(self.line_left).max(1) as u16;
        let (w, h) = if w > avail {
            (avail, ((h as u32 * avail as u32 / w as u32).max(1)) as u16)
        } else {
            (w, h)
        };
        let block = matches!(
            self.dom.computed_display(id).as_deref(),
            Some("block" | "flex" | "grid" | "table" | "list-item")
        );
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

    /// Flow a form control. A control known to the form extraction (in
    /// `controls`) becomes a selectable `Link::Form` widget showing the
    /// field's current value; anything else falls back to a plain stub.
    fn flow_form_control(&mut self, id: NodeId, tag: &str) {
        if let Some(&(form, field)) = self.controls.get(&id) {
            let label = self.field_label(form, field);
            if label.is_empty() {
                return; // hidden control: no widget
            }
            self.place_atom(label, ItemKind::Form, id, Some(Link::Form { form, field }));
            return;
        }
        self.place_form_stub(id, tag);
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

    /// A non-interactive stub for a control we couldn't bind to a form
    /// (e.g. one outside any `<form>`), keeping the page readable.
    fn place_form_stub(&mut self, id: NodeId, tag: &str) {
        if self.dom.is_hidden(id) {
            return;
        }
        let kind = self.dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
        if tag == "input" && kind == "hidden" {
            return;
        }
        let stub = match tag {
            "button" => format!("[ {} ]", self.dom.text_content(id).trim()),
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
        self.place_atom(stub, ItemKind::Form, id, None);
    }

    /// Place a single unbreakable item (a form widget), with the same
    /// inter-item spacing/wrapping rules as a word but kept as one unit.
    fn place_atom(&mut self, text: String, kind: ItemKind, node: NodeId, link: Option<Link>) {
        let len = text.chars().count();
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
            width: marker.chars().count() as u16,
            height: 1,
            text: marker.to_owned(),
            kind: ItemKind::Text,
            image: None,
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
        let len = dashes.chars().count();
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
    fn finish(mut self) -> (Vec<Row>, Vec<Carousel>) {
        let carousels = std::mem::take(&mut self.carousels);
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
        (out, carousels)
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
fn resolve_cells_f32(value: &str, avail: usize, viewport: usize) -> Option<f32> {
    let v = value.trim();
    if let Some(inner) = v
        .strip_prefix("calc(")
        .or_else(|| v.strip_prefix("CALC("))
        .and_then(|r| r.strip_suffix(')'))
    {
        return resolve_calc(inner, avail, viewport);
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

/// A `calc()` body as cells: a whitespace-delimited chain of `+`/`-` terms
/// (CSS requires spaces around those operators), each a length/percentage/vw
/// resolved by `resolve_cells_f32`. Returns `None` if any term is
/// unresolvable or `*`/`/` (unsupported) appears — the caller then ignores
/// the value, as it did before `calc` was understood at all.
fn resolve_calc(body: &str, avail: usize, viewport: usize) -> Option<f32> {
    let s = body.trim();
    if s.contains('*') || s.contains('/') {
        return None;
    }
    let mut total = 0.0f32;
    let mut sign = 1.0f32;
    let mut term_start = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 3 <= s.len() {
        let op = &s[i..i + 3];
        if op == " + " || op == " - " {
            total += sign * resolve_cells_f32(s[term_start..i].trim(), avail, viewport)?;
            sign = if bytes[i + 1] == b'+' { 1.0 } else { -1.0 };
            i += 3;
            term_start = i;
        } else {
            i += 1;
        }
    }
    total += sign * resolve_cells_f32(s[term_start..].trim(), avail, viewport)?;
    Some(total)
}

/// Whether a vertical length is big enough to warrant a blank spacer row
/// (≥ half a line).
fn vertical_space(value: &str) -> bool {
    css_length_em(value).is_some_and(|em| em >= 0.5)
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
        )
    }

    fn lay_with_images(html: &str, width: usize, images: &ImageSizes) -> Vec<Row> {
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        lay_out(&dom, &base, width, &[], &ControlMap::new(), images)
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
            // Skip SVGs: the real decode pipeline never rasters them, so
            // they fall through to the alt-text path (where star glyphs etc.
            // resolve) — faking them here would hide that.
            if dom.tag_name(id) == Some("img")
                && let Some(src) = dom.attr(id, "src")
                && !src.trim_end().ends_with(".svg")
                && let Link::Http(u) = crate::http::resolve(&base, src)
            {
                images.insert(u.to_string(), (10, 4));
            }
        }
        let (rows, carousels) =
            lay_out_with_carousels(&dom, &base, w, &[], &ControlMap::new(), &images);
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
        assert_eq!(
            resolve_cells("calc(100% * 2)", 40, 80),
            None,
            "no * / in calc"
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
        let (_, carousels) =
            lay_out_with_carousels(&dom, &base, 20, &[], &ControlMap::new(), &ImageSizes::new());
        assert_eq!(carousels.len(), 1, "one carousel detected");
        let c = &carousels[0];
        assert_eq!(c.stops.len(), 5, "a snap stop per card");
        assert!(c.width as usize > 20, "the strip overflows the viewport");
        assert_eq!(c.offset, 0, "starts at the first card");
        // A clearfix wrapping ONE wide column is NOT a carousel.
        let plain = r#"<html><head><style>.wrap{overflow:hidden}.col{width:50em}</style></head>
            <body><div class="wrap"><div class="col">just one wide column</div></div></body></html>"#;
        let dom = Dom::parse_document(plain);
        let (_, none) =
            lay_out_with_carousels(&dom, &base, 20, &[], &ControlMap::new(), &ImageSizes::new());
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
        let (rows, carousels) =
            lay_out_with_carousels(&dom, &base, 20, &[], &ControlMap::new(), &ImageSizes::new());
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
        let (rows, carousels) =
            lay_out_with_carousels(&dom, &base, 20, &[], &ControlMap::new(), &ImageSizes::new());
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
    fn slideshow_deck_renders_slides_side_by_side_with_paging_controls() {
        use crate::doc::Link;
        // A deck of stacked, absolutely-positioned slides (one revealed by
        // opacity) becomes a one-at-a-time carousel: all slides kept "in the
        // background" but laid one band apart, with generated prev/next
        // controls. (An <h2> above gives the controls a row to sit on.)
        // The deck's own controls (arrows + a dot) follow it in document
        // order, like a real JS slideshow — they must be suppressed even
        // though they're laid AFTER the deck.
        let html = r##"<html><head><style>
             .slide { position: absolute; opacity: 0 }
             .slide.active { opacity: 1 }
           </style></head>
           <body>
             <h2>Banner</h2>
             <div class="show">
               <div class="deck">
                 <div class="slide active">ALPHA</div>
                 <div class="slide">BETA</div>
                 <div class="slide">GAMMA</div>
               </div>
               <a class="prev-slide" href="#">PREVARROW</a>
               <a class="next-slide" href="#">NEXTARROW</a>
               <a class="dot" href="#">DOTLINK</a>
             </div>
           </body></html>"##;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let (rows, carousels) =
            lay_out_with_carousels(&dom, &base, 40, &[], &ControlMap::new(), &ImageSizes::new());
        assert_eq!(carousels.len(), 1, "the deck is one carousel");
        assert_eq!(carousels[0].stops.len(), 3, "three slides → three stops");
        // The page's own controls — arrows AND the dead dot — are all gone.
        for ctrl in ["PREVARROW", "NEXTARROW", "DOTLINK"] {
            assert!(
                !rows
                    .iter()
                    .flat_map(|r| &r.items)
                    .any(|it| it.text.contains(ctrl)),
                "author control {ctrl} suppressed: {:?}",
                texts(&rows)
            );
        }
        // All three slides are present (kept in the background), even though
        // two are opacity:0 — the deck exemption keeps them alive.
        let col_of = |t: &str| {
            rows.iter()
                .flat_map(|r| &r.items)
                .find(|it| it.text.contains(t))
                .unwrap_or_else(|| panic!("{t} missing: {:?}", texts(&rows)))
                .col
        };
        assert!(col_of("ALPHA") < col_of("BETA"), "slides laid side by side");
        assert!(col_of("BETA") < col_of("GAMMA"));
        // Generated prev/next controls page the deck.
        let buttons = rows
            .iter()
            .flat_map(|r| &r.items)
            .filter(|it| matches!(it.link, Some(Link::CarouselScroll(_))))
            .count();
        assert_eq!(buttons, 2, "prev/next generated: {:?}", texts(&rows));
        // Only one slide occupies the band at a time (band = one slide wide).
        let c = &carousels[0];
        assert_eq!(
            c.view_width(),
            c.stops[1],
            "the band holds exactly one slide"
        );
    }

    #[test]
    fn out_of_flow_overlay_controls_collapse_to_one_line() {
        // `position:absolute`/`fixed` overlays (slideshow arrows + dots) can't
        // be coordinate-positioned, so we render them inline and drop a `<br>`
        // that only trails an overlay — keeping the controls on one line under
        // the slide instead of stacking three rows.
        let rows = lay(
            r#"<html><head><style>
                 .arrow{position:absolute}
                 .dots{position:absolute}
               </style></head>
               <body>
                 <div class="slide">IMG</div>
                 <a class="arrow">PREV</a><a class="arrow">NEXT</a>
                 <br>
                 <div class="dots">DOTS</div>
               </body></html>"#,
            80,
        );
        let (r_img, _) = pos_of(&rows, "IMG");
        let (r_prev, _) = pos_of(&rows, "PREV");
        let (r_next, _) = pos_of(&rows, "NEXT");
        let (r_dots, _) = pos_of(&rows, "DOTS");
        assert!(
            r_img < r_prev,
            "the slide stays above its controls: {:?}",
            texts(&rows)
        );
        assert_eq!(r_prev, r_next, "arrows share one line: {:?}", texts(&rows));
        assert_eq!(
            r_next,
            r_dots,
            "dots join the arrows' line (the trailing <br> is dropped): {:?}",
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
