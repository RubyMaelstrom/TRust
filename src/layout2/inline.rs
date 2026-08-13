//! The inline formatting context (CSS 2.1 §9.4.2, CSS Text §4-§7) for the
//! layout2 engine: inline-level content flows into line boxes.
//!
//! White-space collapsing runs as ONE state machine across the whole IFC, so
//! collapsing spans inline-box boundaries (`a <b> b</b>` keeps one space) and
//! the mode can change mid-context (a `white-space:pre` span inside a normal
//! paragraph). Text is shaped and measured in CSS pixels by TRust's Parley
//! adapter; terminal quantization is a later compatibility stage. Everything
//! else is CSS inline behavior:
//! greedy breaking at soft-wrap opportunities (spaces, and between CJK
//! ideographs), an unbreakable word longer than the line OVERFLOWS (clipped
//! at the viewport edge at paint, like a browser before you scroll right),
//! `text-align` incl. real justification, `text-indent` on the first line,
//! forced breaks from `<br>`/preserved newlines, measured tab stops, and real
//! baseline-aligned text and atomic inlines.

use url::Url;

use crate::doc::{Form, Link};
use crate::dom::{Dom, NodeId};
use crate::layout2::{Emphasis, ImageSizes, ItemKind, NO_NODE, Units, is_collapsible_space};

use super::float::{FloatBox, FloatCtx, FloatPlace};
use super::style::{Align2, BoxStyle, InlineStyle, LEFT, RIGHT, TabSize, VerticalAlign};
use super::tree::{Atom, AtomKind, BoxNode, Inline};
use super::value::{Len, Vp};

/// The float environment an IFC lays its line boxes against: the block
/// formatting context's [`FloatCtx`] (queried per line and appended to when an
/// inline float is met), the pre-laid float margin-box sizes in walk order
/// (`boxes[k]` is the k-th `Inline::Float` the IFC encounters — the layout
/// engine owns box laying, so the block flow lays floats and hands the IFC only
/// their sizes), and the content box's absolute top-left in px (the frame the
/// band queries and returned placements use). Absent = no floats: line boxes
/// span the full content width, byte-identical to the pre-float engine.
pub(crate) struct FloatEnv<'f> {
    pub fc: &'f mut FloatCtx,
    pub boxes: &'f [FloatBox],
    pub left_x: f32,
    pub top_y: f32,
}

/// An out-of-flow box met in the IFC: the static-position mark (§10.3.7's
/// hypothetical-box position) — the line it would have entered on and the
/// pen x (px from the content edge) at that point — plus the inline context
/// the box inherits (inheritance follows the DOM tree, not the containing
/// block: an abspos box inside an `<a>` keeps the link).
pub(crate) struct OofMark<'t> {
    pub b: &'t BoxNode,
    pub line: usize,
    pub x_px: f32,
    pub ctx: InlineStyle,
}

/// One placed inline item in CSS pixels. Shaped text is retained so paint does
/// not repeat font selection or shaping.
#[derive(Clone, Debug)]
pub(crate) struct Piece {
    pub x: f32,
    pub y: f32,
    pub box_width: f32,
    pub box_height: f32,
    /// Object-fit paint rectangle relative to the piece box.
    pub paint_x: f32,
    pub paint_y: f32,
    pub paint_width: f32,
    pub paint_height: f32,
    pub ascent: f32,
    pub descent: f32,
    pub vertical_align: VerticalAlign,
    pub item: InlineItem,
    pub shaped: Option<crate::text::ShapedText>,
    /// Ordinary text pieces retain their CSS text style so adjacent words can
    /// be accumulated by advance and shaped once when the line closes.
    text_style: Option<crate::text::TextStyle>,
    /// Whether this piece's text is justification-stretchable (built under a
    /// collapsing white-space mode — preserved spaces never stretch).
    stretch: bool,
    /// A collapsible space materialized as the measured gap before this piece
    /// (a justification slot).
    pub(crate) space_before: bool,
    /// A PLACEHOLDER for an atomic inline box (`inline-block`/`inline-flex`/
    /// `inline-grid`): it reserves the box on the line (pen + line
    /// height) but paints NOTHING — the box's real content is a separate
    /// pre-laid fragment positioned at this piece's resolved spot (`flush_line`
    /// records it, the block flow splices it in). `item.node` is the box id.
    atom_box: bool,
}

/// Frontend-neutral inline paint and interaction payload. Canonical fragments
/// never carry the terminal `Item`'s cell coordinates or dimensions; the
/// terminal adapter constructs those only while quantizing a piece.
#[derive(Clone, Debug)]
pub(crate) struct InlineItem {
    /// Authored/native-control text used by canonical CSS-pixel layout and
    /// graphical paint.
    pub text: String,
    /// Character-cell representation used only by `layout2::paint`.
    ///
    /// Keeping this separate is the same one-way adapter rule as cell
    /// quantization: terminal widget punctuation must never affect graphical
    /// glyphs, flex sizing, wrapping, or CSSOM geometry.
    pub terminal_text: Option<String>,
    pub kind: ItemKind,
    pub image: Option<String>,
    pub emph: Emphasis,
    /// Element whose computed style supplies paint values for this item.
    ///
    /// This is deliberately separate from `node`: CSS Display 3 anonymous
    /// boxes inherit through their box-tree parentage, while synthesized
    /// content such as an outside list marker has no DOM node of its own.
    /// Keeping `node == NO_NODE` preserves the interaction/selection contract
    /// without making graphical paint query the DOM with the sentinel.
    pub style_node: NodeId,
    pub node: NodeId,
    pub link: Option<Link>,
    pub crop: bool,
    pub pixelated: bool,
    pub invisible: bool,
}

impl Piece {
    /// An atomic piece with an explicit box (a replaced flex item laid at
    /// an imposed size), with an independent object-fit paint rectangle.
    pub(crate) fn boxed(
        item: InlineItem,
        box_width: f32,
        box_height: f32,
        paint_x: f32,
        paint_y: f32,
        paint_width: f32,
        paint_height: f32,
    ) -> Piece {
        Piece {
            x: 0.0,
            y: 0.0,
            box_width,
            box_height,
            paint_x,
            paint_y,
            paint_width,
            paint_height,
            ascent: box_height,
            descent: 0.0,
            vertical_align: VerticalAlign::Baseline,
            item,
            shaped: None,
            text_style: None,
            stretch: false,
            space_before: false,
            atom_box: false,
        }
    }

    pub(crate) fn shaped(item: InlineItem, shaped: crate::text::ShapedText) -> Piece {
        let width = shaped.advance;
        let height = shaped.line_height;
        let ascent = shaped.baseline;
        Piece {
            x: 0.0,
            y: 0.0,
            box_width: width,
            box_height: height,
            paint_x: 0.0,
            paint_y: 0.0,
            paint_width: width,
            paint_height: height,
            ascent,
            descent: (height - ascent).max(0.0),
            vertical_align: VerticalAlign::Baseline,
            item,
            shaped: Some(shaped),
            text_style: None,
            stretch: false,
            space_before: false,
            atom_box: false,
        }
    }
}

/// The pre-laid CSS-pixel size of an atomic inline box (`inline-block`/
/// `inline-flex`/`inline-grid`) — its MARGIN box (margins occupy
/// inline space). The block flow lays the box (`item_frag`) and hands these
/// to the IFC in walk order (`boxes[k]` = k-th `Inline::AtomBox` met), exactly
/// like `FloatBox`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct AtomBoxSize {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug)]
struct AtomGeometry {
    box_width: f32,
    box_height: f32,
    paint_x: f32,
    paint_y: f32,
    paint_width: f32,
    paint_height: f32,
}

/// A resolved atomic-inline-box placement returned from the IFC: the box's
/// element `node` (the block flow matches it to the pre-laid fragment), the
/// line it landed on, and its margin-box top-left in CSS pixels relative to the
/// line box.
#[derive(Copy, Clone, Debug)]
pub(crate) struct AtomBoxPlace {
    pub node: NodeId,
    pub line: usize,
    pub x: f32,
    pub y: f32,
}

/// One finished line box.
#[derive(Debug)]
pub(crate) struct LineOut {
    pub pieces: Vec<Piece>,
    pub height: f32,
    pub baseline: f32,
    pub ascent: f32,
    pub descent: f32,
    /// Ended by a forced break (`<br>`/preserved newline) — exempt from
    /// justification, like the IFC's last line.
    pub forced: bool,
    /// Used CSS pixels (the pen at flush) — the alignment/justify extent. Kept on
    /// the line because a `contain`-fitted replaced box occupies more space
    /// than its painted item reports.
    pub width: f32,
    /// The line box's available band in CSS pixels (float-shortened).
    /// against — every line can differ beside floats). Justification at
    pub left: f32,
    pub right: f32,
}

/// The inline formatting context builder. Feed it the IFC's inline content,
/// then `finish()` into line boxes. `'t` is the box tree — out-of-flow
/// boxes met in the content are handed back as static-position marks; `'f` is
/// the float environment borrow.
pub(crate) struct Ifc<'a, 'f, 't> {
    dom: &'a Dom,
    base: &'a Url,
    images: &'a ImageSizes,
    forms: &'a [Form],
    vp: Vp,
    /// The content width in CSS pixels — the line-box cap when no float
    /// shortens it.
    cap: f32,
    /// The content box width in px (percentage basis for inline-box edges
    /// and replaced sizing; also the float band's right containing-block edge).
    cb_w_px: f32,
    /// The containing block's definite content HEIGHT in px, when it has one
    /// — the percentage basis for replaced `height`/`min-height`/`max-height`
    /// (§10.5: a percentage against an indefinite height is auto).
    cb_h_px: Option<f32>,
    align: Align2,
    lines: Vec<LineOut>,
    cur: Vec<Piece>,
    /// Every inline ELEMENT entered, with the index of the line it entered
    /// on — the flow positions of boxes that emit no pieces (an empty
    /// `<a name>`/`<span id>` is a real box and a fragment anchor even
    /// though it paints nothing). Content-bearing elements are covered by
    /// their pieces' nodes too; the mark is just their upper bound.
    marks: Vec<(NodeId, usize)>,
    /// Out-of-flow boxes met (static-position marks for the positioned
    /// post-pass) — they emit no pieces.
    oofs: Vec<OofMark<'t>>,
    pen: f32,
    line_start: f32,
    pending_space: bool,
    /// Owed inline-box edge width (margins/borders/padding of opened/closed
    /// inline boxes), in px — folded into the next placement so an edge at a
    /// wrap point travels with the content it precedes.
    pending_gap_px: f32,
    // ---- floats (§9.5) — inert when `fc` is None ----
    /// The BFC's float context: queried per line (`band`) and appended to when
    /// an inline float is met (`place`). `None` = no floats (intrinsic probe,
    /// atomic-only content) — line boxes span `[0, cap)`, byte-for-byte the
    /// pre-float engine.
    fc: Option<&'f mut FloatCtx>,
    /// The pre-laid float margin-box sizes, in the order the IFC meets them.
    float_boxes: &'f [FloatBox],
    /// Content-box absolute top-left px (the frame float bands/placements use).
    content_left_x: f32,
    content_top_y: f32,
    /// Running px height of the line boxes already flushed — the current line's
    /// top y is `content_top_y + laid_h`.
    laid_h: f32,
    /// Index of the next `Inline::Float` to meet (into `float_boxes`).
    float_next: usize,
    /// Resolved placements (margin-box top-left px), returned by `finish`.
    placements: Vec<FloatPlace>,
    // ---- atomic inline boxes (inline-block/-flex/-grid) — empty slice = none ----
    /// Pre-laid margin-box CSS sizes, in the order the IFC meets the boxes.
    atom_boxes: &'f [AtomBoxSize],
    /// Index of the next `Inline::AtomBox` to meet (into `atom_boxes`).
    atom_next: usize,
    /// Resolved atom-box placements, returned by `finish`.
    atom_places: Vec<AtomBoxPlace>,
    /// The current line box's CSS-pixel float band.
    line_left: f32,
    line_right: f32,
    /// `text-indent`, applied to the first formatted line (CSS px).
    indent: f32,
    /// Whether the line about to be composed is the IFC's first (indent gate).
    on_first_line: bool,
    /// An intrinsic-size probe (`intrinsic_w`), not a real layout pass:
    /// `overflow-wrap: break-word`'s emergency breaks must NOT count as
    /// min-content opportunities (CSS Text §5.5 — unlike `anywhere`).
    measuring: bool,
    /// The block container's inherited font/line-height strut. CSS Inline 3
    /// §5.1 requires it to participate even on an otherwise empty line.
    strut: crate::text::ShapedText,
}

impl<'a, 'f, 't> Ifc<'a, 'f, 't> {
    #[allow(clippy::too_many_arguments)] // a formatting context has this many real inputs
    pub fn new(
        dom: &'a Dom,
        base: &'a Url,
        images: &'a ImageSizes,
        forms: &'a [Form],
        vp: Vp,
        content_w_px: f32,
        cb_h_px: Option<f32>,
        align: Align2,
        indent_px: f32,
        floats: Option<FloatEnv<'f>>,
        atom_boxes: &'f [AtomBoxSize],
    ) -> Ifc<'a, 'f, 't> {
        let cap = content_w_px.max(0.0);
        // CSS Text 3 §9.1 permits a negative hanging indent; do not clamp it
        // to a presentation edge in canonical layout.
        let indent = if indent_px.is_finite() {
            indent_px
        } else {
            0.0
        };
        let (fc, float_boxes, content_left_x, content_top_y) = match floats {
            Some(env) => (Some(env.fc), env.boxes, env.left_x, env.top_y),
            None => (None, &[][..], 0.0, 0.0),
        };
        let mut ifc = Ifc {
            dom,
            base,
            images,
            forms,
            vp,
            cap,
            cb_w_px: content_w_px,
            cb_h_px,
            align,
            lines: Vec::new(),
            cur: Vec::new(),
            marks: Vec::new(),
            oofs: Vec::new(),
            pen: 0.0,
            line_start: 0.0,
            pending_space: false,
            pending_gap_px: 0.0,
            fc,
            float_boxes,
            content_left_x,
            content_top_y,
            laid_h: 0.0,
            float_next: 0,
            placements: Vec::new(),
            atom_boxes,
            atom_next: 0,
            atom_places: Vec::new(),
            line_left: 0.0,
            line_right: cap,
            indent,
            on_first_line: true,
            measuring: false,
            strut: crate::text::shape(" ", &crate::text::TextStyle::default()),
        };
        ifc.begin_line();
        ifc
    }

    /// Mark this IFC as an intrinsic-size probe (see the `measuring` field).
    pub fn mark_measuring(&mut self) {
        self.measuring = true;
    }

    /// Set the current line box's left/right boundaries from the float band at
    /// its vertical position (§9.5.1 — the current and subsequent line boxes are
    /// shortened to make room for a float's margin box). With no floats the band
    /// is the full content width, so `pen`/`line_start` land exactly where the
    /// pre-float engine put them.
    fn begin_line(&mut self) {
        let (left, right) = match &self.fc {
            Some(fc) if !fc.is_empty() => {
                let y = self.content_top_y + self.laid_h;
                // Probe with the root inline's nominal line height. Once a
                // line is built, `laid_h` advances by its actual metrics and
                // the next band is queried at the true y.
                let probe_h = 1.2 * crate::dom::FONT_SIZE_INITIAL;
                let (li, ri) = fc.band(y, probe_h);
                let own_l = self.content_left_x;
                let own_r = own_l + self.cb_w_px;
                let l = (own_l.max(li) - own_l).max(0.0);
                let r = (own_r.min(ri) - own_l).max(0.0);
                (l.min(self.cap), r.min(self.cap))
            }
            _ => (0.0, self.cap),
        };
        self.line_left = left;
        self.line_right = right.max(left);
        let indent = if self.on_first_line { self.indent } else { 0.0 };
        self.line_start = self.line_left + indent;
        self.pen = self.line_start;
        self.pending_space = false;
    }

    /// Place the k-th inline float met (§9.5.1): pull it aside into the float
    /// context and shorten the current + subsequent line boxes. A LEADING float
    /// (empty current line) places at the current line's top and re-shortens
    /// this line; a float met AFTER content on the line can't sit above that
    /// content (rule 6), and reflowing the already-placed content is a v1 cut,
    /// so it starts the NEXT line's band instead.
    fn place_float(&mut self) {
        let idx = self.float_next;
        self.float_next += 1;
        let leading = self.pen <= self.line_start;
        let line_y = self.content_top_y + self.laid_h;
        let top_min = if leading {
            line_y
        } else {
            line_y + crate::dom::FONT_SIZE_INITIAL * 1.2
        };
        let cb_l = self.content_left_x;
        let cb_r = self.content_left_x + self.cb_w_px;
        let (Some(fc), Some(fb)) = (self.fc.as_deref_mut(), self.float_boxes.get(idx).copied())
        else {
            return;
        };
        let (x, y) = fc.place(fb, top_min, cb_l, cb_r);
        self.placements.push(FloatPlace { index: idx, x, y });
        if leading {
            // The current (empty) line's edges just moved — re-query the band.
            self.begin_line();
        }
    }

    /// Lay the IFC's content. `root` is the block container's own inline
    /// context (text directly under the block uses it).
    pub fn run(&mut self, content: &'t [Inline], root: &InlineStyle) {
        self.strut = crate::text::shape(" ", &root.text_style());
        for inl in content {
            self.walk(inl, root);
        }
    }

    fn walk(&mut self, inl: &'t Inline, ctx: &InlineStyle) {
        match inl {
            Inline::Text(t) => self.text(t, ctx),
            Inline::Br => self.forced_break(),
            // An inline-level replaced box: its margin/border/padding edges
            // take real inline space around the content box (§9.4.2/§10.8),
            // through the same owed-edge channel as inline boxes — a
            // `margin-left:50%` hero video sits at mid-line, not col 0.
            Inline::Atom(a) if a.node != NO_NODE => {
                let style = BoxStyle::of(self.dom, a.node, self.vp);
                self.pending_gap_px += self.edge_px(&style, LEFT);
                self.atom(a, ctx);
                self.pending_gap_px += self.edge_px(&style, RIGHT);
            }
            Inline::Atom(a) => self.atom(a, ctx),
            // A static-position mark, nothing more: the hypothetical box
            // would have entered here (§10.3.7 — "UAs are free to make a
            // guess"; ours is the exact pen position).
            Inline::OutOfFlow(b) => self.oofs.push(OofMark {
                b,
                line: self.lines.len(),
                x_px: self.pen + self.pending_gap_px,
                ctx: ctx.clone(),
            }),
            // A float (§9.5): pulled aside into the float context; it emits no
            // inline content, but shortens the line boxes beside it.
            Inline::Float(_) => self.place_float(),
            // An atomic inline box (inline-block/-flex/-grid): reserve its
            // pre-laid margin box on the line; the block flow splices its
            // content fragment at the resolved position.
            Inline::AtomBox(b) => self.place_atom_box(b.node, ctx),
            Inline::Box { node, style, kids } => {
                // Generated ::before/::after inline boxes have no DOM node;
                // their inherited text context is the originating element's
                // surrounding context and their own box model is in `style`.
                let inner = if *node == crate::layout2::NO_NODE {
                    ctx.clone()
                } else {
                    InlineStyle::derive(self.dom, *node, ctx, self.base)
                };
                if *node != crate::layout2::NO_NODE {
                    self.marks.push((*node, self.lines.len()));
                }
                self.pending_gap_px += self.edge_px(style, LEFT);
                for k in kids {
                    self.walk(k, &inner);
                }
                self.pending_gap_px += self.edge_px(style, RIGHT);
            }
        }
    }

    /// An inline box's leading/trailing edge (margin+border+padding) in px —
    /// real inline space per §9.4.2, replacing the old engine's "abutting
    /// links" gap heuristic with the page's own geometry.
    fn edge_px(&self, style: &BoxStyle, side: usize) -> f32 {
        let m = style.margin[side]
            .resolve(Some(self.cb_w_px))
            .unwrap_or(0.0);
        let p = style.padding[side]
            .resolve(Some(self.cb_w_px))
            .unwrap_or(0.0);
        m + style.border[side] + p
    }

    /// Emit a text run under `ctx`.
    pub fn text(&mut self, t: &str, ctx: &InlineStyle) {
        if ctx.font_zero {
            // `font-size:0` text occupies no CSS geometry (the copyable-but-unseen
            // idiom); it neither paints nor owes spaces.
            return;
        }
        let t = ctx.transform.apply(t);
        if ctx.ws.collapses_spaces() {
            let mut word = String::new();
            for c in t.chars() {
                if is_collapsible_space(c) {
                    if !word.is_empty() {
                        self.word(&word, ctx);
                        word.clear();
                    }
                    if c == '\n' && ctx.ws.preserves_newlines() {
                        self.forced_break();
                    } else {
                        self.pending_space = true;
                    }
                } else {
                    word.push(c);
                }
            }
            if !word.is_empty() {
                self.word(&word, ctx);
            }
        } else {
            // Preserved modes: newlines force breaks; tabs advance to the
            // next `tab-size` stop (CSS Text §3; a 0 tab renders no advance);
            // spaces are literal.
            for (i, seg) in t.split('\n').enumerate() {
                if i > 0 {
                    self.forced_break();
                }
                for (j, piece) in seg.split('\t').enumerate() {
                    if j > 0 {
                        let tab = self.tab_advance(ctx);
                        if tab > 0.0 {
                            let rel = (self.pen - self.line_start).max(0.0);
                            self.pen = self.line_start + (rel / tab).floor().mul_add(tab, tab);
                        }
                    }
                    if !piece.is_empty() {
                        self.preserved(piece, ctx);
                    }
                }
            }
        }
    }

    /// Place the anonymous replaced element generated by
    /// `list-style-image` (CSS Lists 3 §3.2). The marker has no DOM node of
    /// its own, but it inherits the list item's paint context and participates
    /// in the same line-breaking stream as the following content.
    pub fn marker_image(&mut self, source: &str, ctx: &InlineStyle) {
        let size = ctx.font_size.max(0.0);
        self.place_atom(
            AtomGeometry {
                box_width: size,
                box_height: size,
                paint_x: 0.0,
                paint_y: 0.0,
                paint_width: size,
                paint_height: size,
            },
            InlineItem {
                text: String::new(),
                terminal_text: None,
                kind: ItemKind::Image,
                image: Some(source.to_string()),
                emph: Emphasis::default(),
                style_node: ctx.node,
                node: NO_NODE,
                link: None,
                crop: false,
                pixelated: false,
                invisible: ctx.invisible,
            },
            None,
            false,
            ctx.vertical_align,
            Self::space_advance(ctx),
        );
    }

    /// One word in a collapsing mode. Parley's Unicode line breaker supplies
    /// UAX #14 opportunities (including CJK and complex scripts); CSS remains
    /// responsible for the selected word/overflow break strengths.
    fn word(&mut self, word: &str, ctx: &InlineStyle) {
        self.place_wrapped(word, ctx, true, true);
    }

    /// Preserved-mode text: place, breaking anywhere at capacity when the
    /// mode wraps (`pre-wrap`), overflowing when it doesn't (`pre`/`nowrap`).
    fn preserved(&mut self, t: &str, ctx: &InlineStyle) {
        if !ctx.ws.wraps() {
            self.place(t, ctx, false, true);
            return;
        }
        self.place_wrapped(t, ctx, false, true);
    }

    fn break_style(&self, ctx: &InlineStyle) -> crate::text::TextBreakStyle {
        use crate::text::{TextBreakStyle, TextOverflowWrap, TextWordBreak};
        let word_break = if ctx.keep_all {
            TextWordBreak::KeepAll
        } else if ctx.brk == super::style::WordBrk::BreakAll {
            TextWordBreak::BreakAll
        } else {
            TextWordBreak::Normal
        };
        let overflow_wrap = match ctx.brk {
            super::style::WordBrk::Anywhere => TextOverflowWrap::Anywhere,
            super::style::WordBrk::BreakWord if !self.measuring => TextOverflowWrap::BreakWord,
            _ => TextOverflowWrap::Normal,
        };
        TextBreakStyle {
            wrap: ctx.ws.wraps(),
            word_break,
            overflow_wrap,
        }
    }

    /// Break a styled run at Unicode cluster boundaries. This is deliberately
    /// an inline-composition loop rather than a second text layout engine:
    /// Parley chooses every break and measures every produced piece.
    fn place_wrapped(
        &mut self,
        mut rest: &str,
        ctx: &InlineStyle,
        may_wrap: bool,
        mut spaced: bool,
    ) {
        while !rest.is_empty() {
            let gap = self.pending_gap_px;
            let space = if spaced && self.pending_space && self.pen > self.line_start {
                Self::space_advance(ctx)
            } else {
                0.0
            };
            let avail = (self.line_right - self.pen - gap - space).max(0.0);
            let full = crate::text::shape(rest, &ctx.text_style());
            // Parley line breaking is substantially more expensive than
            // retaining an already-shaped run. If the complete run fits,
            // there cannot be a chosen soft wrap inside it; only ask Parley
            // for the first legal break when the line actually overflows.
            // CSS Text's break opportunities and emergency-wrap rules remain
            // authoritative in `first_line_end` for that overflow case.
            let break_style = self.break_style(ctx);
            let emergency_wrap = matches!(
                break_style.overflow_wrap,
                crate::text::TextOverflowWrap::Anywhere | crate::text::TextOverflowWrap::BreakWord
            );
            if break_style.wrap
                && emergency_wrap
                && may_wrap
                && full.advance > avail
                && self.pen > self.line_start
            {
                // CSS Text 3 §5/§5.4: overflow wrapping adds an
                // arbitrary opportunity only when there is no otherwise-
                // acceptable break point in the line. Test this word with
                // overflow wrapping disabled first: a UAX #14 opportunity
                // inside it (for example after punctuation) may still fill
                // the current line, but absent one the preceding collapsible
                // space wins and the intact word moves to a fresh line.
                let normal_cut = crate::text::first_line_end(
                    rest,
                    &ctx.text_style(),
                    avail,
                    crate::text::TextBreakStyle {
                        overflow_wrap: crate::text::TextOverflowWrap::Normal,
                        ..break_style
                    },
                );
                if normal_cut == 0 || normal_cut == rest.len() {
                    self.soft_break();
                    spaced = false;
                    continue;
                }
            }
            // Under normal/keep-all breaking an ASCII identifier contains no
            // internal soft-wrap opportunity. This covers common class/tag
            // labels without asking Parley to rediscover that fact under the
            // 0px min-content probe; punctuation and every complex script
            // still go through Parley's Unicode line breaker.
            let plainly_unbreakable = !break_style.wrap
                || (matches!(
                    break_style.word_break,
                    crate::text::TextWordBreak::Normal | crate::text::TextWordBreak::KeepAll
                ) && matches!(
                    break_style.overflow_wrap,
                    crate::text::TextOverflowWrap::Normal
                ) && rest
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_'));
            let cut = if full.advance <= avail || plainly_unbreakable {
                rest.len()
            } else {
                crate::text::first_line_end(rest, &ctx.text_style(), avail, break_style)
            };
            if cut > 0 && cut < rest.len() {
                let (head, tail) = rest.split_at(cut);
                self.place(head, ctx, false, spaced);
                self.soft_break();
                rest = tail;
                spaced = false;
                continue;
            }
            if ctx.ws.wraps()
                && full.advance > avail
                && self.pen > self.line_start
                && (may_wrap || cut == 0)
            {
                self.soft_break();
                // Collapsible leading whitespace is discarded at the break;
                // inline box edges remain pending in CSS pixels.
                spaced = false;
                continue;
            }
            self.place(rest, ctx, false, spaced);
            break;
        }
    }

    fn space_advance(ctx: &InlineStyle) -> f32 {
        crate::text::shape(" ", &ctx.text_style()).advance.max(0.0)
    }

    fn tab_advance(&self, ctx: &InlineStyle) -> f32 {
        match ctx.tab {
            TabSize::Spaces(n) => Self::space_advance(ctx) * n.max(0.0),
            TabSize::Length(px) => px.max(0.0),
        }
    }

    /// Place one unbreakable segment. `may_wrap` = a soft break before it is
    /// allowed; `spaced` = an owed collapsible space applies before it (false
    /// between CJK segments of one word).
    fn place(&mut self, seg: &str, ctx: &InlineStyle, may_wrap: bool, spaced: bool) {
        let shaped = crate::text::shape(seg, &ctx.text_style());
        let w = shaped.advance;
        if w <= 0.0 {
            return;
        }
        let space = spaced && self.pending_space && self.pen > self.line_start;
        let space_width = if space { Self::space_advance(ctx) } else { 0.0 };
        let gap = self.take_gap();
        if may_wrap
            && ctx.ws.wraps()
            && self.pen + space_width + gap + w > self.line_right
            && self.pen > self.line_start
            // `break-all` breaks mid-line inside the word instead of first
            // wrapping whole (CSS Text §5.2 — every character boundary is a
            // soft-wrap opportunity, so the greedy fill uses this line).
            && ctx.brk != super::style::WordBrk::BreakAll
        {
            self.soft_break();
            // The owed collapsible space dies at the wrap (CSS Text §4.1.3);
            // an owed inline-box edge travels with the content it precedes.
            self.pending_gap_px = gap;
            self.place(seg, ctx, false, false);
            return;
        }
        // Merge adjacent same-styled words into one retained shaped run so
        // shaping can cross ordinary whitespace and selection/hit testing
        // keeps useful run granularity. Justified lines keep spaces
        // as explicit gaps because their advances are expanded per line.
        if !self.measuring
            && self.align != Align2::Justify
            && gap == 0.0
            && let Some(last) = self.cur.last_mut()
            && last.item.kind == ctx.kind
            && last.item.emph == ctx.emph
            && last.item.node == ctx.node
            && last.item.link == ctx.link
            && last.item.invisible == ctx.invisible
            && last.stretch == ctx.ws.collapses_spaces()
            && last.item.image.is_none()
            && (last.x + last.box_width - self.pen).abs() < 0.01
        {
            if space {
                last.item.text.push(' ');
            }
            last.item.text.push_str(seg);
            // The advance of ordinary words separated by a collapsed U+0020
            // is additive. Keep the greedy line geometry now, then perform
            // one complete bidi/fallback shape in `flush_line`. Reshaping the
            // growing prefix here performed 1+2+…+N glyph work for N words.
            last.box_width += space_width + w;
            last.paint_width = last.box_width;
            last.shaped = None;
            last.text_style = Some(ctx.text_style());
            self.pen = last.x + last.box_width;
            self.pending_space = false;
            return;
        }
        let x = self.pen + space_width + gap;
        let height = shaped.line_height;
        let ascent = shaped.baseline;
        self.cur.push(Piece {
            x,
            y: 0.0,
            box_width: w,
            box_height: height,
            paint_x: 0.0,
            paint_y: 0.0,
            paint_width: w,
            paint_height: height,
            ascent,
            descent: (height - ascent).max(0.0),
            vertical_align: ctx.vertical_align,
            item: InlineItem {
                text: seg.to_string(),
                terminal_text: None,
                kind: ctx.kind,
                image: None,
                emph: ctx.emph,
                style_node: ctx.node,
                node: ctx.node,
                link: ctx.link.clone(),
                crop: false,
                pixelated: false,
                invisible: ctx.invisible,
            },
            shaped: Some(shaped),
            text_style: Some(ctx.text_style()),
            stretch: ctx.ws.collapses_spaces(),
            space_before: space,
            atom_box: false,
        });
        self.pen = x + w;
        self.pending_space = false;
    }

    /// An atomic inline box (image or form control). Public so the block
    /// flow can size a block-level replaced box through the same path.
    pub fn atom(&mut self, a: &Atom, ctx: &InlineStyle) {
        match &a.kind {
            AtomKind::Img {
                url,
                density,
                dimension_source,
                alt,
            } => self.image(
                a.node,
                *dimension_source,
                url.as_deref(),
                *density,
                alt,
                ctx,
            ),
            AtomKind::Media { video } => self.media(a.node, *video, ctx),
            AtomKind::Control { form, field } => {
                let Some(f) = self.forms.get(*form).and_then(|f| f.fields.get(*field)) else {
                    return;
                };
                let labels = control_labels(
                    self.dom,
                    a.node,
                    f,
                    if self.measuring {
                        ControlWidthBasis::Intrinsic
                    } else {
                        ControlWidthBasis::ContainingBlock(self.cb_w_px)
                    },
                    self.cap,
                    ctx,
                    self.vp,
                );
                if labels.visual.is_empty() {
                    return;
                }
                let shaped = crate::text::shape(&labels.visual, &ctx.text_style());
                let w = shaped.advance;
                self.place_atom(
                    AtomGeometry {
                        box_width: w,
                        box_height: shaped.line_height,
                        paint_x: 0.0,
                        paint_y: 0.0,
                        paint_width: w,
                        paint_height: shaped.line_height,
                    },
                    InlineItem {
                        text: labels.visual,
                        terminal_text: Some(labels.terminal),
                        kind: ItemKind::Form,
                        image: None,
                        emph: Emphasis::default(),
                        style_node: a.node,
                        node: a.node,
                        link: Some(Link::Form {
                            form: *form,
                            field: *field,
                        }),
                        crop: false,
                        pixelated: false,
                        invisible: ctx.invisible,
                    },
                    Some(shaped),
                    false,
                    ctx.vertical_align,
                    Self::space_advance(ctx),
                );
            }
        }
    }

    /// An `<img>`: a decoded or dimension-declared image reserves its used
    /// box per the standard replaced sizing (§10.3.2/§10.6.2/§10.4 +
    /// `aspect-ratio` + `object-fit` — `replaced::size`); otherwise its alt
    /// text flows as an Image-kind run (HTML's inline representation of an
    /// unavailable image), and the decode pipeline's re-layout turns it into
    /// pixels.
    pub fn image(
        &mut self,
        node: NodeId,
        dimension_source: NodeId,
        url: Option<&str>,
        density: f32,
        alt: &str,
        ctx: &InlineStyle,
    ) {
        // A player often paints its `<video poster>` again as an absolutely
        // positioned sibling `<img>` so custom controls can cover the native
        // element. That later image wins CSS painting order; carry the media
        // target onto the pixels the user actually sees, or the real media
        // representation underneath becomes an unreachable link (X-style
        // players). Association is by the standardized poster URL and nearest
        // containing band, never by a class or host name.
        let link = url
            .and_then(|u| poster_media_target(self.dom, self.base, node, u))
            .map(Link::Media)
            .or_else(|| ctx.link.clone());
        let natural = crate::responsive_image::density_corrected_size(
            url.and_then(|url| self.images.get(url)),
            density,
        );
        if let Some(r) = super::replaced::size(
            self.dom,
            node,
            super::replaced::ImageInput {
                dimension_source,
                natural,
                url,
            },
            Some(self.cb_w_px),
            self.cb_h_px,
            self.vp,
        ) {
            let pixelated = matches!(
                self.dom.computed_value(node, "image-rendering").as_deref(),
                Some(
                    "pixelated" | "crisp-edges" | "-moz-crisp-edges" | "-webkit-optimize-contrast"
                )
            );
            self.place_atom(
                AtomGeometry {
                    box_width: r.box_w,
                    box_height: r.box_h,
                    paint_x: r.off_x,
                    paint_y: r.off_y,
                    paint_width: r.paint_w,
                    paint_height: r.paint_h,
                },
                InlineItem {
                    text: String::new(),
                    terminal_text: None,
                    kind: ItemKind::Image,
                    image: natural
                        .is_some()
                        .then(|| url.unwrap_or_default().to_string()),
                    emph: Emphasis::default(),
                    style_node: node,
                    node,
                    link,
                    crop: r.crop,
                    pixelated,
                    invisible: ctx.invisible,
                },
                None,
                false,
                ctx.vertical_align,
                Self::space_advance(ctx),
            );
            return;
        }
        if alt.is_empty() {
            return;
        }
        let mut alt_ctx = ctx.clone();
        alt_ctx.kind = ItemKind::Image;
        alt_ctx.node = node;
        self.text(alt, &alt_ctx);
    }

    /// A `<video>`/`<audio>` media representation. A terminal can't play
    /// media, so the representation IS the "play in mpv" affordance
    /// (`Link::Media`): a decoded poster thumbnail when one exists (the
    /// drawn preview IS the link — her call 2026-07-04, no extra text line
    /// under it), else a labeled text link. Durable decisions ported from
    /// the old engine: a sourceless (MSE/blob) streaming video targets the
    /// PAGE yt-dlp resolves — the enclosing card link, else this page.
    /// Every rendered video remains an activation target: HTML §4.8.8
    /// explicitly permits a user agent that cannot render video to represent
    /// the element as a link to an external playback utility. og:image is
    /// borrowed as a poster ONLY when the representation plays this page
    /// (og:image describes THIS page's media and nothing else). The old engine's
    /// faded-poster borrow (`hidden_preview_in_cb`) is deletion-list
    /// machinery and deliberately NOT ported — the fragment stack reads it
    /// once positioned layout lands (P4).
    fn media(&mut self, node: NodeId, video: bool, ctx: &InlineStyle) {
        let own_suppressed = self.dom.paint_suppressed(node) || self.dom.visibility_hidden(node);
        // A paint-suppressed OUT-OF-FLOW media element contributes nothing:
        // an abspos box takes no normal-flow space (§9.3.1 — such boxes are
        // laid in-flow only until P4) and a suppressed one paints no content,
        // so its net contribution is zero (Steam's lingering `opacity:0`
        // abspos microtrailer must not grow its capsule).
        let invisible = ctx.invisible || own_suppressed;
        if invisible
            && matches!(
                self.dom.computed_value(node, "position").as_deref(),
                Some("absolute" | "fixed")
            )
        {
            return;
        }
        let (play, src_node, streaming) = match media_source(self.dom, self.base, node) {
            Some((u, n)) => (Url::parse(&u).ok(), n, false),
            None if video => {
                let page = match &ctx.link {
                    Some(Link::Http(u)) => u.clone(),
                    // The resident-page serializer rewrites a live anchor to
                    // `x-trust-js:<node>:<original href>` so activation can run
                    // through the page actor. Media is intentionally delegated
                    // to mpv instead; resolve that preserved href directly and
                    // do not run the inline player UI (Twitch-style cards).
                    Some(Link::JsClick { href, .. }) if !href.trim().is_empty() => {
                        match crate::http::resolve(self.base, href) {
                            Link::Http(u) => u,
                            _ => self.base.clone(),
                        }
                    }
                    _ => self.base.clone(),
                };
                (Some(page), None, true)
            }
            None => return, // sourceless audio: nothing to represent
        };
        let Some(play) = play else { return };
        let plays_this_page = streaming && play == *self.base;
        let link = Some(Link::Media(play));
        let poster = video
            .then(|| {
                self.dom
                    .attr(node, "poster")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .and_then(|p| match crate::http::resolve(self.base, p) {
                        Link::Http(u) => Some(u.to_string()),
                        _ => None,
                    })
                    .or_else(|| {
                        plays_this_page
                            .then(|| crate::layout2::page_preview_image(self.dom, self.base))
                            .flatten()
                    })
            })
            .flatten();
        if let Some(poster) = poster
            && let Some((iw, ih)) =
                crate::responsive_image::density_corrected_size(self.images.get(&poster), 1.0)
        {
            // The poster draws at its DECODED box capped to the line — never
            // the video's CSS box, which often carries a `height:0`/padding
            // aspect hack a poster must not inherit.
            let w = iw.min(self.cap).max(1.0);
            let h = (ih * w / iw).max(1.0);
            self.place_atom(
                AtomGeometry {
                    box_width: w,
                    box_height: h,
                    paint_x: 0.0,
                    paint_y: 0.0,
                    paint_width: w,
                    paint_height: h,
                },
                InlineItem {
                    text: String::new(),
                    terminal_text: None,
                    kind: ItemKind::Image,
                    image: Some(poster),
                    emph: Emphasis::default(),
                    style_node: node,
                    node,
                    link: link.clone(),
                    crop: false,
                    pixelated: false,
                    invisible,
                },
                None,
                false,
                ctx.vertical_align,
                Self::space_advance(ctx),
            );
            return; // the drawn preview IS the mpv affordance
        }
        let label = if streaming {
            String::from("▶ Watch in mpv")
        } else {
            media_label(self.dom, video, src_node)
        };
        let mut mctx = ctx.clone();
        mctx.kind = ItemKind::Link;
        mctx.link = link;
        mctx.node = node;
        mctx.invisible = invisible;
        self.text(&label, &mctx);
    }

    /// Place an atomic CSS-pixel box (unbreakable). The
    /// painted `item` may be smaller than its box (`object-fit: contain`
    /// letterboxing), offset from the box's top-left;
    /// the pen and the line height always advance by the BOX.
    fn place_atom(
        &mut self,
        geometry: AtomGeometry,
        item: InlineItem,
        shaped: Option<crate::text::ShapedText>,
        atom_box: bool,
        vertical_align: VerticalAlign,
        space_advance: f32,
    ) {
        let space = self.pending_space && self.pen > self.line_start;
        let space_width = if space { space_advance.max(0.0) } else { 0.0 };
        let gap = self.take_gap();
        if self.pen + space_width + gap + geometry.box_width > self.line_right
            && self.pen > self.line_start
        {
            self.soft_break();
            self.pending_gap_px = gap;
            self.place_atom(
                geometry,
                item,
                shaped,
                atom_box,
                vertical_align,
                space_advance,
            );
            return;
        }
        let x = self.pen + space_width + gap;
        self.cur.push(Piece {
            x,
            y: 0.0,
            box_width: geometry.box_width,
            box_height: geometry.box_height,
            paint_x: geometry.paint_x,
            paint_y: geometry.paint_y,
            paint_width: geometry.paint_width,
            paint_height: geometry.paint_height,
            ascent: geometry.box_height,
            descent: 0.0,
            vertical_align,
            item,
            shaped,
            text_style: None,
            stretch: false,
            space_before: space,
            atom_box,
        });
        self.pen = x + geometry.box_width;
        self.pending_space = false;
    }

    /// Place an atomic inline box (`inline-block`/`inline-flex`/`inline-grid`):
    /// reserve its pre-laid margin box on the line as an unbreakable
    /// paint-nothing placeholder (`item.node` = the box id). The box's real
    /// content is a fragment the block flow splices at this piece's resolved
    /// position (`finish` returns the placement). The IFC only needs the size.
    fn place_atom_box(&mut self, node: NodeId, ctx: &InlineStyle) {
        let idx = self.atom_next;
        self.atom_next += 1;
        let Some(sz) = self.atom_boxes.get(idx).copied() else {
            return;
        };
        self.place_atom(
            AtomGeometry {
                box_width: sz.width,
                box_height: sz.height,
                paint_x: 0.0,
                paint_y: 0.0,
                paint_width: sz.width,
                paint_height: sz.height,
            },
            InlineItem {
                text: String::new(),
                terminal_text: None,
                kind: crate::layout2::ItemKind::Text,
                image: None,
                emph: Emphasis::default(),
                style_node: node,
                node,
                link: None,
                crop: false,
                pixelated: false,
                invisible: false,
            },
            None,
            true,
            ctx.vertical_align,
            Self::space_advance(ctx),
        );
    }

    /// Consume the owed inline-box edge width without quantization.
    fn take_gap(&mut self) -> f32 {
        let gap = self.pending_gap_px.max(0.0);
        self.pending_gap_px = 0.0;
        gap
    }

    fn soft_break(&mut self) {
        self.flush_line(false);
    }

    /// A forced break always terminates the current line — an empty one
    /// still yields a line box (`<br><br>` shows a blank row).
    pub fn forced_break(&mut self) {
        self.flush_line(true);
    }

    fn flush_line(&mut self, forced: bool) {
        let mut pieces = std::mem::take(&mut self.cur);
        if pieces.is_empty() && !forced {
            self.pen = self.line_start;
            self.pending_space = false;
            return;
        }
        // Adjacent same-style words were measured individually for greedy
        // wrapping and accumulated into one logical piece. Shape that final
        // piece once here so bidi ordering, fallback runs, and cluster mapping
        // describe the complete painted text. A tiny shaping delta moves only
        // the following pieces and remains in CSS-pixel geometry.
        let mut shift = 0.0;
        for piece in &mut pieces {
            piece.x += shift;
            let Some(style) = piece.text_style.as_ref() else {
                continue;
            };
            if piece.shaped.is_some() || piece.item.text.is_empty() {
                continue;
            }
            let old_width = piece.box_width;
            let shaped = crate::text::shape(&piece.item.text, style);
            piece.box_width = shaped.advance;
            piece.paint_width = shaped.advance;
            piece.box_height = shaped.line_height;
            piece.paint_height = shaped.line_height;
            piece.ascent = shaped.baseline;
            piece.descent = (shaped.line_height - shaped.baseline).max(0.0);
            piece.shaped = Some(shaped);
            shift += piece.box_width - old_width;
        }
        self.pen += shift;
        let strut = &self.strut;
        let ascent = pieces
            .iter()
            .map(|p| match p.vertical_align {
                VerticalAlign::Shift(rise) => p.ascent + rise,
                _ => p.ascent,
            })
            .fold(strut.baseline, f32::max);
        let descent = pieces
            .iter()
            .map(|p| match p.vertical_align {
                VerticalAlign::Shift(rise) => p.descent - rise,
                _ => p.descent,
            })
            .fold((strut.line_height - strut.baseline).max(0.0), f32::max);
        let height = (ascent + descent).max(strut.line_height);
        let baseline = ascent;
        for p in &mut pieces {
            p.y = match p.vertical_align {
                VerticalAlign::Baseline => baseline - p.ascent,
                VerticalAlign::Shift(rise) => baseline - p.ascent - rise,
                VerticalAlign::Top => 0.0,
                VerticalAlign::Bottom => (height - p.box_height).max(0.0),
                VerticalAlign::Middle => ((height - p.box_height) / 2.0).max(0.0),
            };
        }
        // The pen is the line's used extent. A contain-fitted replaced box
        // occupies its full object box even when its paint rectangle is less.
        let width = (self.pen - self.line_left).max(0.0);
        let mut line = LineOut {
            pieces,
            height,
            baseline,
            ascent,
            descent,
            forced,
            width,
            left: self.line_left,
            right: self.line_right,
        };
        // Center/right shift now, within this line's (float-shortened) band;
        // justification waits for `finish`, where "last line" is known.
        let free = (self.line_right - self.pen).max(0.0);
        if free > 0.0 {
            let off = match self.align {
                Align2::Center => free / 2.0,
                Align2::Right => free,
                Align2::Left | Align2::Justify => 0.0,
            };
            if off > 0.0 {
                for p in &mut line.pieces {
                    p.x += off;
                }
            }
        }
        // Record atomic-inline-box placements (x/y now resolved) and
        // drop their paint-nothing placeholders: the block flow splices the
        // box's real content fragment at each spot. Removal doesn't shift the
        // siblings — their x positions are already absolute on the line.
        if line.pieces.iter().any(|p| p.atom_box) {
            let li = self.lines.len();
            for p in &line.pieces {
                if p.atom_box {
                    self.atom_places.push(AtomBoxPlace {
                        node: p.item.node,
                        line: li,
                        x: p.x,
                        y: p.y,
                    });
                }
            }
            line.pieces.retain(|p| !p.atom_box);
        }
        self.lines.push(line);
        // Advance past this line box, then open the next against the band at
        // the new vertical position (a taller float may still shorten it).
        self.laid_h += height;
        self.on_first_line = false;
        self.begin_line();
    }

    /// Finish the IFC: flush the trailing line, then justify (every line
    /// except forced-break lines and the last — CSS Text §7.1). Returns the
    /// line boxes, the entered-element line marks, the out-of-flow
    /// static-position marks, the resolved float placements (margin-box
    /// top-left px, in the content frame), and the atomic-inline-box
    /// placements (the block flow splices each box's content fragment there).
    #[allow(clippy::type_complexity)]
    pub fn finish(
        mut self,
    ) -> (
        Vec<LineOut>,
        Vec<(NodeId, usize)>,
        Vec<OofMark<'t>>,
        Vec<FloatPlace>,
        Vec<AtomBoxPlace>,
    ) {
        self.flush_line(false);
        if self.align == Align2::Justify {
            let n = self.lines.len();
            for (i, line) in self.lines.iter_mut().enumerate() {
                if i + 1 == n || line.forced {
                    continue;
                }
                let cap = (line.right - line.left).max(0.0);
                if line.width < cap {
                    let extra = cap - line.width;
                    justify(line, extra);
                }
            }
        }
        (
            self.lines,
            self.marks,
            self.oofs,
            self.placements,
            self.atom_places,
        )
    }
}

/// The URL handed to mpv when a media element is activated. Native HTML
/// sources and associated structured-media URLs win; a sourceless video is an
/// MSE/blob player whose stable, externally resolvable target is its nearest
/// enclosing page link or the current page. Sourceless audio has no honest
/// target.
pub(crate) fn media_target(dom: &Dom, base: &Url, node: NodeId) -> Option<Url> {
    if let Some((source, _)) = media_source(dom, base, node) {
        return Url::parse(&source).ok();
    }
    if dom.tag_name(node) != Some("video") {
        return None;
    }
    let mut ancestor = dom.parent_composed(node);
    while let Some(id) = ancestor {
        if dom.tag_name(id) == Some("a")
            && let Some(href) = dom.attr(id, "href")
        {
            return match crate::http::resolve(base, href) {
                Link::Http(url) => Some(url),
                Link::JsClick { href, .. } if !href.trim().is_empty() => {
                    match crate::http::resolve(base, &href) {
                        Link::Http(url) => Some(url),
                        _ => Some(base.clone()),
                    }
                }
                _ => Some(base.clone()),
            };
        }
        ancestor = dom.parent_composed(id);
    }
    Some(base.clone())
}

/// If `image` repeats a nearby `<video poster>`, return that video's playable
/// target. HTML §4.8.8 defines the poster as a representative frame; custom
/// players commonly duplicate it into a sibling image for their overlay UI.
pub(super) fn poster_media_target(
    dom: &Dom,
    base: &Url,
    image: NodeId,
    image_url: &str,
) -> Option<Url> {
    // The ordinary document may contain hundreds of images. Only a positioned
    // image can be the custom overlay that paints over the native poster; this
    // O(1) gate keeps the structural media scan off every normal `<img>` path.
    if !matches!(
        dom.computed_value(image, "position").as_deref(),
        Some("absolute" | "fixed")
    ) {
        return None;
    }
    let image_url = match crate::http::resolve(base, image_url) {
        Link::Http(u) => u,
        _ => return None,
    };
    let videos: Vec<_> = dom
        .descendants(crate::dom::DOCUMENT)
        .filter(|&id| dom.tag_name(id) == Some("video"))
        .filter(|&id| {
            dom.attr(id, "poster")
                .map(str::trim)
                .and_then(|p| match crate::http::resolve(base, p) {
                    Link::Http(u) => Some(u),
                    _ => None,
                })
                .is_some_and(|u| u == image_url)
        })
        .filter_map(|id| {
            media_source(dom, base, id)
                .and_then(|(target, _)| Url::parse(&target).ok())
                .map(|target| (id, target))
        })
        .collect();
    let mut ancestor = dom.parent_composed(image);
    while let Some(root) = ancestor {
        let mut nearby = videos
            .iter()
            .filter(|(id, _)| composed_contains(dom, root, *id));
        let first = nearby.next();
        if let Some((_, target)) = first
            && nearby.next().is_none()
        {
            return Some(target.clone());
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

/// CSS Text §7.3 justification in CSS pixels. Collapsed spaces are retained
/// as explicit gaps between pieces, so expansion changes geometry without
/// manufacturing extra U+0020 characters or reshaping glyph runs.
fn justify(line: &mut LineOut, extra: f32) {
    let slots: Vec<usize> = line
        .pieces
        .iter()
        .enumerate()
        .filter_map(|(index, piece)| piece.space_before.then_some(index))
        .collect();
    if slots.is_empty() {
        return;
    }
    let per_slot = extra / slots.len() as f32;
    let mut slot = slots.into_iter().peekable();
    let mut shift = 0.0;
    for (index, piece) in line.pieces.iter_mut().enumerate() {
        if slot.peek() == Some(&index) {
            shift += per_slot;
            slot.next();
        }
        piece.x += shift;
    }
    line.width += extra;
}

/// The playable URL of a media element and the chosen `<source>` node (for
/// its quality label): the element's own `src` if set, else the first
/// `<source>` with an http(s) `src` (browser source-selection order).
pub(crate) fn media_source(dom: &Dom, base: &Url, id: NodeId) -> Option<(String, Option<NodeId>)> {
    if let Some(src) = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty())
        && let Link::Http(u) = crate::http::resolve(base, src)
    {
        return Some((u.to_string(), None));
    }
    for c in dom.descendants(id) {
        if dom.tag_name(c) == Some("source")
            && let Some(src) = dom.attr(c, "src").map(str::trim).filter(|s| !s.is_empty())
            && let Link::Http(u) = crate::http::resolve(base, src)
        {
            return Some((u.to_string(), Some(c)));
        }
    }
    // Native HTML source selection above remains authoritative. Only when the
    // media element is sourceless consult its associated standardized
    // structured-media item: modern MSE players publish the bytes/player here
    // while never assigning `video.src` (WHATWG HTML §5 Microdata; Schema.org
    // MediaObject `contentUrl`/`embedUrl`).
    let video = dom.tag_name(id) == Some("video");
    if matches!(dom.tag_name(id), Some("video" | "audio"))
        && let Some(media) = crate::layout2::structured_media_for(dom, base, id, video)
    {
        return Some((media.target.to_string(), None));
    }
    None
}

/// The caption for a media representation: a glyph + kind + optional quality
/// from the chosen `<source>`'s `res`/`label` (`▶ Video · 720p HD`).
pub(crate) fn media_label(dom: &Dom, video: bool, src_node: Option<NodeId>) -> String {
    let (glyph, kind) = if video {
        ('▶', "Video")
    } else {
        ('♪', "Audio")
    };
    let mut quality = String::new();
    if let Some(sn) = src_node {
        let res = dom
            .attr(sn, "res")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|r| format!("{r}p"));
        let lab = dom
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

/// A control's widget presentation. WHATWG HTML Rendering §15.5.6 describes
/// text inputs as inline-block one-line text controls: graphical paint carries
/// their value/placeholder with no terminal punctuation. Editable fields pad
/// to their used width — CSS `width` (needs a percentage basis; `None` under an
/// intrinsic-sizing constraint), else the HTML `size`/`cols` attribute, else
/// the UA default of 20 character advances. The value is never truncated
/// (typed content outranks the declared width).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ControlWidthBasis {
    /// A normal layout pass with a definite containing-block width.
    ContainingBlock(f32),
    /// An intrinsic contribution. CSS Sizing 3 §5.2.1 treats a cyclic
    /// percentage size as `auto` while calculating that contribution.
    Intrinsic,
}

pub(crate) fn control_label(
    dom: &Dom,
    node: NodeId,
    f: &crate::doc::Field,
    width_basis: ControlWidthBasis,
    cap: f32,
    inline_style: &InlineStyle,
    vp: Vp,
) -> String {
    control_labels(dom, node, f, width_basis, cap, inline_style, vp).visual
}

struct ControlLabels {
    visual: String,
    terminal: String,
}

fn control_labels(
    dom: &Dom,
    node: NodeId,
    f: &crate::doc::Field,
    width_basis: ControlWidthBasis,
    cap: f32,
    inline_style: &InlineStyle,
    vp: Vp,
) -> ControlLabels {
    let mut visual = f.visual_label();
    let mut terminal = f.row_label();
    use crate::doc::FieldKind;
    if !matches!(
        f.kind,
        FieldKind::Text | FieldKind::Password | FieldKind::Textarea
    ) {
        return ControlLabels { visual, terminal };
    }
    let u = Units::of(dom, node);
    let css_width = dom
        .computed_value(node, "width")
        .and_then(|v| Len::parse(&v, u, vp))
        .and_then(|l| {
            l.resolve(match width_basis {
                ControlWidthBasis::ContainingBlock(width) => Some(width),
                ControlWidthBasis::Intrinsic => None,
            })
        });
    let attr_ch = |name: &str| {
        dom.attr(node, name)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
    };
    // HTML's `size`/`cols` are character advances. They are deliberately
    // resolved with this control's shaped `ch` basis, never terminal cells.
    let attr_name = if f.kind == FieldKind::Textarea {
        "cols"
    } else {
        "size"
    };
    let text_style = inline_style.text_style();
    let target = css_width.unwrap_or_else(|| {
        crate::text::zero_advance(&text_style) * (attr_ch(attr_name).unwrap_or(20) + 2) as f32
    });
    let target = target.min(cap).max(0.0);
    let space = crate::text::shape(" ", &text_style).advance.max(0.01);
    let visual_have = crate::text::shape(&visual, &text_style).advance;
    if visual_have < target {
        visual.push_str(&" ".repeat(((target - visual_have) / space).ceil() as usize));
    }
    let terminal_have = crate::text::shape(&terminal, &text_style).advance;
    if terminal_have < target && terminal.ends_with(']') {
        let pad = " ".repeat(((target - terminal_have) / space).ceil() as usize);
        terminal.truncate(terminal.len() - 1);
        terminal.push_str(&pad);
        terminal.push(']');
    }
    ControlLabels { visual, terminal }
}
