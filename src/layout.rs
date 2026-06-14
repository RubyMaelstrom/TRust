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
pub fn lay_out(
    dom: &Dom,
    base: &Url,
    width: usize,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
) -> Vec<Row> {
    let mut layout = Layout::new(dom, base, width.max(10), forms, controls, images);
    let root = body_or_document(dom);
    let ctx = Ctx::root();
    for child in dom.children(root) {
        layout.flow_node(child, &ctx);
    }
    layout.flush_block();
    layout.finish()
}

/// The `<body>` element, or the document node if there isn't one.
fn body_or_document(dom: &Dom) -> NodeId {
    dom.descendants(DOCUMENT)
        .into_iter()
        .find(|&id| dom.tag_name(id) == Some("body"))
        .unwrap_or(DOCUMENT)
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
    rows: Vec<Row>,
    /// The line currently being built.
    line: Vec<Item>,
    /// Next free column on the current line.
    col: usize,
    /// Left indent of the current block.
    indent: usize,
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
    /// Open list counters: `None` = `<ul>`, `Some(n)` = `<ol>` next index.
    list_stack: Vec<Option<u32>>,
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
            rows: Vec::new(),
            line: Vec::new(),
            col: 0,
            indent: 0,
            align: Align::Left,
            pending_space: false,
            ws: WhiteSpace::Normal,
            line_height: 1,
            list_stack: Vec::new(),
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
        if SKIP.contains(&tag.as_str()) || self.dom.is_hidden(id) {
            return;
        }
        match tag.as_str() {
            "br" => {
                self.break_line();
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

        // Build the child formatting context for inline elements.
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
            "b" | "strong" => cctx.emph.bold = true,
            "i" | "em" => cctx.emph.italic = true,
            "u" | "ins" => cctx.emph.underline = true,
            "s" | "strike" | "del" => cctx.emph.strike = true,
            "blockquote" => cctx.kind = ItemKind::Quote,
            "pre" => cctx.kind = ItemKind::Pre,
            _ => {
                if let Some(level) = heading_level(&tag) {
                    cctx.kind = ItemKind::Heading(level);
                }
            }
        }

        // CSS font-weight/font-style override the tag defaults and can also
        // turn emphasis OFF (e.g. `strong{font-weight:normal}`). Both
        // inherit, which the threaded `cctx` already models.
        if let Some(w) = self.dom.computed_style(id, "font-weight") {
            cctx.emph.bold = css_is_bold(&w);
        }
        if let Some(s) = self.dom.computed_style(id, "font-style") {
            cctx.emph.italic = css_is_italic(&s);
        }
        // text-decoration(-line) propagates to descendants. A value names
        // its lines; `none` clears both. Honor the shorthand and longhand.
        if let Some(d) = self
            .dom
            .computed_style(id, "text-decoration-line")
            .or_else(|| self.dom.computed_style(id, "text-decoration"))
        {
            apply_text_decoration(&mut cctx.emph, &d);
        }
        if let Some(t) = self
            .dom
            .computed_style(id, "text-transform")
            .as_deref()
            .and_then(TextTransform::from_css)
        {
            cctx.transform = t;
        }

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
        if block_like {
            self.flush_block();
            if self.gap_before(id, &tag) {
                self.push_blank();
            }
        }

        // text-align inherits; a block that sets it changes alignment for
        // its own lines and its descendants until they override it.
        let saved_align = self.align;
        if block_like
            && let Some(a) = self
                .dom
                .computed_style(id, "text-align")
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
        // At a block boundary the line is empty, so the cursor sits at
        // the (new) indent. Inline elements don't touch it.
        if block_like {
            self.col = self.indent;
        }

        // white-space inherits: `<pre>` defaults to Pre, and CSS overrides
        // either (so `pre{white-space:pre-wrap}` or `white-space:nowrap` on
        // any element both work).
        let saved_ws = self.ws;
        if tag == "pre" {
            self.ws = WhiteSpace::Pre;
        }
        if let Some(w) = self
            .dom
            .computed_style(id, "white-space")
            .as_deref()
            .and_then(WhiteSpace::from_css)
        {
            self.ws = w;
        }
        let pushed_list = match tag.as_str() {
            "ul" => {
                self.list_stack.push(None);
                true
            }
            "ol" => {
                self.list_stack.push(Some(1));
                true
            }
            _ => false,
        };
        if flow == Flow::ListItem {
            self.emit_list_marker();
        }

        // CSS `::before` generated content opens the element's content.
        if let Some(t) = self.pseudo_text(id, crate::dom::PseudoEl::Before) {
            self.place_text(&t, &cctx);
        }

        for child in self.dom.children(id) {
            self.flow_node(child, &cctx);
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
            if self.gap_after(id, &tag) {
                self.push_blank();
            }
        }
        self.ws = saved_ws;
        self.align = saved_align;
        if pushed_list {
            self.list_stack.pop();
        }
        self.indent -= indent_add;
        if block_like {
            self.col = self.indent;
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
            let cols = indent_cells(ml.as_deref()) + indent_cells(pl.as_deref());
            cols.min(self.width / 4)
        } else {
            match tag {
                "ul" | "ol" | "blockquote" | "dd" => 2,
                _ => 0,
            }
        }
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
        let space = self.pending_space && self.col > self.indent;
        if self.ws.wraps()
            && self.col + space as usize + wlen > self.width
            && self.col > self.indent
        {
            self.break_line();
        }
        let space = self.pending_space && self.col > self.indent;
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
        let avail = self.width.saturating_sub(self.indent).max(1);
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
        // Flow the alt text, tagged as an image so the view can mark it
        // and L3 can find the node to render pixels in its place.
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
        self.place_text(&alt, &img_ctx);
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
        let avail = self.width.saturating_sub(self.indent).max(1) as u16;
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
            let space = self.pending_space && self.col > self.indent;
            if self.col + space as usize + w as usize > self.width && self.col > self.indent {
                self.break_line();
            }
            // An owed inter-item space becomes a one-cell gap (the renderer
            // fills column gaps; no need to pollute a neighbor's text).
            if self.pending_space && self.col > self.indent {
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
        let space = self.pending_space && self.col > self.indent;
        if self.col + space as usize + len > self.width && self.col > self.indent {
            self.break_line();
        }
        let space = self.pending_space && self.col > self.indent;
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

    fn emit_list_marker(&mut self) {
        let marker = match self.list_stack.last_mut() {
            Some(Some(n)) => {
                let m = format!("{n}. ");
                *n += 1;
                m
            }
            _ => "• ".to_owned(),
        };
        let len = marker.chars().count();
        self.push_item(
            marker,
            len,
            ItemKind::Text,
            Emphasis::default(),
            NO_NODE,
            None,
        );
    }

    fn push_rule(&mut self) {
        let dashes = "─".repeat(self.width.saturating_sub(self.indent).min(40));
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
        self.rows.push(Row { items });
        self.col = self.indent;
        self.pending_space = false;
        self.line_height = 1;
        // Spacer rows carry a (non-empty) marker so `finish`'s blank-row
        // collapse leaves them intact; the renderer draws nothing for them
        // (the image overdraws the box) and selection skips them (no link).
        for _ in 1..band {
            self.rows.push(image_spacer_row(self.indent));
        }
    }

    /// Offset a finished row's items to honor center/right `text-align`.
    /// The shift is computed within the block's content box `[indent,
    /// width]`, ignoring a trailing space on the last item.
    fn align_row(&self, items: &mut [Item]) {
        if self.align == Align::Left {
            return;
        }
        let Some(last) = items.last() else { return };
        let trailing = last.text.ends_with(' ') as u16;
        let used = (last.col + last.width).saturating_sub(trailing) as usize;
        let free = self.width.saturating_sub(used);
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

    /// Collapse runs of blank rows and trim leading/trailing blanks.
    fn finish(self) -> Vec<Row> {
        let mut out: Vec<Row> = Vec::with_capacity(self.rows.len());
        for row in self.rows {
            if row.items.is_empty() && out.last().is_none_or(|r| r.items.is_empty()) {
                continue;
            }
            out.push(row);
        }
        while out.last().is_some_and(|r| r.items.is_empty()) {
            out.pop();
        }
        out
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

/// Fold a CSS `text-decoration`/`text-decoration-line` value into the
/// emphasis flags. A value lists its lines (`underline`, `line-through`,
/// possibly with a color/style that we ignore); `none` clears both.
fn apply_text_decoration(emph: &mut Emphasis, value: &str) {
    let v = value.to_ascii_lowercase();
    if v.split_whitespace().any(|t| t == "none") {
        emph.underline = false;
        emph.strike = false;
        return;
    }
    if v.contains("underline") {
        emph.underline = true;
    }
    if v.contains("line-through") {
        emph.strike = true;
    }
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

/// A CSS length as an approximate em-equivalent (≈ one text cell). px
/// uses 16px≈1em; unitless treated as px; unknown units (`%`/`auto`/
/// `vh`/…) → `None` (ignored, never spaces).
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
        _ => None,
    }
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
