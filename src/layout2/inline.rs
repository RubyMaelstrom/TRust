//! The inline formatting context (CSS 2.1 §9.4.2, CSS Text §4-§7) for the
//! layout2 engine: inline-level content flows into line boxes.
//!
//! White-space collapsing runs as ONE state machine across the whole IFC, so
//! collapsing spans inline-box boundaries (`a <b> b</b>` keeps one space) and
//! the mode can change mid-context (a `white-space:pre` span inside a normal
//! paragraph). Text capacity is quantized to glyph cells at the line-box
//! boundary — one glyph = one cell, one line box = one row — which is the
//! engine's single px→cell interpretation; everything else is spec behavior:
//! greedy breaking at soft-wrap opportunities (spaces, and between CJK
//! ideographs), an unbreakable word longer than the line OVERFLOWS (clipped
//! at the viewport edge at paint, like a browser before you scroll right),
//! `text-align` incl. real justification, `text-indent` on the first line,
//! forced breaks from `<br>`/preserved newlines, tab stops every 8 cells in
//! preserved modes, and bottom-aligned atomics (baseline alignment: a
//! replaced box's baseline is its bottom margin edge, so text beside an
//! image sits on the image's last row).

use url::Url;

use crate::doc::{Form, Link};
use crate::dom::{Dom, NodeId};
use crate::layout::{
    Emphasis, ImageSizes, Item, ItemKind, Units, display_width, is_collapsible_space, letter_space,
};

use super::float::{FloatBox, FloatCtx, FloatPlace};
use super::style::{Align2, BoxStyle, InlineStyle, LEFT, RIGHT};
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

/// One placed inline item: `col` cells from the line box's left edge,
/// `row_off` rows from its top (filled at line finish — bottom alignment).
#[derive(Debug)]
pub(crate) struct Piece {
    pub col: usize,
    pub row_off: u16,
    /// The piece's BOX height in rows — what the line box accommodates and
    /// bottom-aligns. For a replaced element under `object-fit: contain`
    /// the painted item can be SHORTER than its box (`item.height` ≤ `rows`),
    /// letterboxed at `box_off_rows` below the box top.
    pub rows: u16,
    pub box_off_rows: u16,
    pub item: Item,
    /// Whether this piece's text is justification-stretchable (built under a
    /// collapsing white-space mode — preserved spaces never stretch).
    stretch: bool,
    /// A collapsible space materialized as the 1-cell gap before this piece
    /// (a justification slot).
    space_before: bool,
    /// A PLACEHOLDER for an atomic inline box (`inline-block`/`inline-flex`/
    /// `inline-grid`): it reserves the box's cells on the line (pen + line
    /// height) but paints NOTHING — the box's real content is a separate
    /// pre-laid fragment positioned at this piece's resolved spot (`flush_line`
    /// records it, the block flow splices it in). `item.node` is the box id.
    atom_box: bool,
}

impl Piece {
    /// An atomic piece with an explicit box (a replaced flex item laid at
    /// an imposed size): the box is `box_rows` tall, the painted item sits
    /// `col` cells / `off_rows` rows into it (object-fit letterboxing).
    pub(crate) fn boxed(item: Item, box_rows: u16, col: usize, off_rows: u16) -> Piece {
        Piece {
            col,
            row_off: 0,
            rows: box_rows.max(1),
            box_off_rows: off_rows,
            item,
            stretch: false,
            space_before: false,
            atom_box: false,
        }
    }

    /// A synthesized one-item piece (list markers) — placed at the line
    /// origin, one row, never justification-stretchable.
    pub(crate) fn solo(item: Item) -> Piece {
        Piece {
            col: 0,
            row_off: 0,
            rows: item.height.max(1),
            box_off_rows: 0,
            item,
            stretch: false,
            space_before: false,
            atom_box: false,
        }
    }
}

/// The pre-laid used cell size of an atomic inline box (`inline-block`/
/// `inline-flex`/`inline-grid`) — its MARGIN box in cells (margins occupy
/// inline space). The block flow lays the box (`item_frag`) and hands these
/// to the IFC in walk order (`boxes[k]` = k-th `Inline::AtomBox` met), exactly
/// like `FloatBox`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct AtomBoxSize {
    pub w_cells: usize,
    pub h_rows: u16,
}

/// A resolved atomic-inline-box placement returned from the IFC: the box's
/// element `node` (the block flow matches it to the pre-laid fragment), the
/// line it landed on, and its margin-box top-left in cells relative to the line
/// box (`col`) / line top (`row_off`, from bottom alignment).
#[derive(Copy, Clone, Debug)]
pub(crate) struct AtomBoxPlace {
    pub node: NodeId,
    pub line: usize,
    pub col: usize,
    pub row_off: u16,
}

/// One finished line box.
#[derive(Debug)]
pub(crate) struct LineOut {
    pub pieces: Vec<Piece>,
    /// Height in rows (≥1; >1 when an atomic rides the line).
    pub rows: u16,
    /// Ended by a forced break (`<br>`/preserved newline) — exempt from
    /// justification, like the IFC's last line.
    pub forced: bool,
    /// Used cells (the pen at flush) — the alignment/justify extent. Kept on
    /// the line because a `contain`-fitted replaced box occupies more cells
    /// than its painted item reports.
    pub width: usize,
    /// The line box's RIGHT edge in cells (the float-shortened cap it was laid
    /// against — every line can differ beside floats). Justification at
    /// `finish` distributes `cap - width` across this line's slots.
    pub cap: usize,
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
    cell_w: f32,
    cell_h: f32,
    /// The content width in cells (floor-quantized) — the line-box cap when no
    /// float shortens it.
    cap: usize,
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
    pen: usize,
    line_start: usize,
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
    /// Pre-laid margin-box cell sizes, in the order the IFC meets the boxes.
    atom_boxes: &'f [AtomBoxSize],
    /// Index of the next `Inline::AtomBox` to meet (into `atom_boxes`).
    atom_next: usize,
    /// Resolved atom-box placements (line/col/row_off), returned by `finish`.
    atom_places: Vec<AtomBoxPlace>,
    /// The current line box's left content edge in cells (a left float's inset).
    line_left: usize,
    /// The current line box's right edge in cells (a right float pulls it in).
    line_right: usize,
    /// `text-indent`, applied to the FIRST line only (cells).
    indent: usize,
    /// Whether the line about to be composed is the IFC's first (indent gate).
    on_first_line: bool,
}

impl<'a, 'f, 't> Ifc<'a, 'f, 't> {
    #[allow(clippy::too_many_arguments)] // a formatting context has this many real inputs
    pub fn new(
        dom: &'a Dom,
        base: &'a Url,
        images: &'a ImageSizes,
        forms: &'a [Form],
        vp: Vp,
        cell_w: f32,
        cell_h: f32,
        content_w_px: f32,
        cb_h_px: Option<f32>,
        align: Align2,
        indent_px: f32,
        floats: Option<FloatEnv<'f>>,
        atom_boxes: &'f [AtomBoxSize],
    ) -> Ifc<'a, 'f, 't> {
        let cap = ((content_w_px / cell_w) + 1e-3).floor().max(1.0) as usize;
        // `text-indent` on the first line, clamped ≥0 (a terminal cannot
        // paint left of column 0 — the hanging-indent quantization).
        let indent = ((indent_px / cell_w).round().max(0.0) as usize).min(cap.saturating_sub(1));
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
            cell_w,
            cell_h,
            cap,
            cb_w_px: content_w_px,
            cb_h_px,
            align,
            lines: Vec::new(),
            cur: Vec::new(),
            marks: Vec::new(),
            oofs: Vec::new(),
            pen: 0,
            line_start: 0,
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
            line_left: 0,
            line_right: cap,
            indent,
            on_first_line: true,
        };
        ifc.begin_line();
        ifc
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
                let (li, ri) = fc.band(y, self.cell_h);
                let own_l = self.content_left_x;
                let own_r = own_l + self.cb_w_px;
                let l = ((own_l.max(li) - own_l) / self.cell_w).round().max(0.0) as usize;
                let r = ((own_r.min(ri) - own_l) / self.cell_w).floor().max(0.0) as usize;
                (l.min(self.cap), r.min(self.cap))
            }
            _ => (0, self.cap),
        };
        self.line_left = left;
        self.line_right = right.max(left);
        let indent = if self.on_first_line { self.indent } else { 0 };
        self.line_start = (self.line_left + indent).min(self.line_right.max(self.line_left));
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
            line_y + self.cell_h
        };
        let cb_l = self.content_left_x;
        let cb_r = self.content_left_x + self.cb_w_px;
        let (Some(fc), Some(fb)) = (self.fc.as_deref_mut(), self.float_boxes.get(idx).copied())
        else {
            return;
        };
        let (x, y) = fc.place(fb.side, fb.mw, fb.mh, top_min, cb_l, cb_r);
        self.placements.push(FloatPlace { index: idx, x, y });
        if leading {
            // The current (empty) line's edges just moved — re-query the band.
            self.begin_line();
        }
    }

    /// Lay the IFC's content. `root` is the block container's own inline
    /// context (text directly under the block uses it).
    pub fn run(&mut self, content: &'t [Inline], root: &InlineStyle) {
        for inl in content {
            self.walk(inl, root);
        }
    }

    fn walk(&mut self, inl: &'t Inline, ctx: &InlineStyle) {
        match inl {
            Inline::Text(t) => self.text(t, ctx),
            Inline::Br => self.forced_break(),
            Inline::Atom(a) => self.atom(a, ctx),
            // A static-position mark, nothing more: the hypothetical box
            // would have entered here (§10.3.7 — "UAs are free to make a
            // guess"; ours is the exact pen position).
            Inline::OutOfFlow(b) => self.oofs.push(OofMark {
                b,
                line: self.lines.len(),
                x_px: self.pen as f32 * self.cell_w + self.pending_gap_px,
                ctx: ctx.clone(),
            }),
            // A float (§9.5): pulled aside into the float context; it emits no
            // inline content, but shortens the line boxes beside it.
            Inline::Float(_) => self.place_float(),
            // An atomic inline box (inline-block/-flex/-grid): reserve its
            // pre-laid margin box on the line; the block flow splices its
            // content fragment at the resolved position.
            Inline::AtomBox(b) => self.place_atom_box(b.node),
            Inline::Box { node, style, kids } => {
                let inner = InlineStyle::derive(self.dom, *node, ctx, self.base);
                self.marks.push((*node, self.lines.len()));
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
            // `font-size:0` text occupies zero cells (the copyable-but-unseen
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
            // Preserved modes: newlines force breaks; tabs advance to 8-cell
            // stops (CSS Text §3, `tab-size` initial 8); spaces are literal.
            for (i, seg) in t.split('\n').enumerate() {
                if i > 0 {
                    self.forced_break();
                }
                for (j, piece) in seg.split('\t').enumerate() {
                    if j > 0 {
                        let rel = self.pen.saturating_sub(self.line_start);
                        self.pen = self.line_start + (rel / 8 + 1) * 8;
                    }
                    if !piece.is_empty() {
                        self.preserved(piece, ctx);
                    }
                }
            }
        }
    }

    /// One word in a collapsing mode: split at CJK boundaries (a wide glyph
    /// is a soft-wrap opportunity on both sides — the UAX #14 ideograph rule
    /// at cell resolution), then place each segment greedily.
    fn word(&mut self, word: &str, ctx: &InlineStyle) {
        let mut seg = String::new();
        let mut first = true;
        let flush = |s: &mut String, ifc: &mut Self, first: &mut bool| {
            if !s.is_empty() {
                ifc.place(s, ctx, true, *first);
                *first = false;
                s.clear();
            }
        };
        for c in word.chars() {
            if display_width(c.encode_utf8(&mut [0u8; 4])) >= 2 {
                flush(&mut seg, self, &mut first);
                let wide = c.to_string();
                self.place(&wide, ctx, true, first);
                first = false;
            } else {
                seg.push(c);
            }
        }
        flush(&mut seg, self, &mut first);
    }

    /// Preserved-mode text: place, breaking anywhere at capacity when the
    /// mode wraps (`pre-wrap`), overflowing when it doesn't (`pre`/`nowrap`).
    fn preserved(&mut self, t: &str, ctx: &InlineStyle) {
        if !ctx.ws.wraps() {
            self.place(t, ctx, false, true);
            return;
        }
        let mut rest: &str = t;
        while !rest.is_empty() {
            let avail = self.line_right.saturating_sub(self.pen);
            let mut w = 0usize;
            let mut cut = rest.len();
            for (bi, c) in rest.char_indices() {
                let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
                if w + cw > avail {
                    cut = bi;
                    break;
                }
                w += cw;
            }
            if cut == 0 {
                if self.pen > self.line_start {
                    self.soft_break();
                    continue;
                }
                // A single glyph wider than the line: place it anyway.
                cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
            }
            let (head, tail) = rest.split_at(cut);
            self.place(head, ctx, false, true);
            rest = tail;
            if !rest.is_empty() {
                self.soft_break();
            }
        }
    }

    /// Place one unbreakable segment. `may_wrap` = a soft break before it is
    /// allowed; `spaced` = an owed collapsible space applies before it (false
    /// between CJK segments of one word).
    fn place(&mut self, seg: &str, ctx: &InlineStyle, may_wrap: bool, spaced: bool) {
        let text = letter_space(seg, ctx.letter);
        let w = display_width(&text);
        if w == 0 {
            return;
        }
        let space = spaced && self.pending_space && self.pen > self.line_start;
        let gap = self.take_gap();
        if may_wrap
            && ctx.ws.wraps()
            && self.pen + usize::from(space) + gap + w > self.line_right
            && self.pen > self.line_start
        {
            self.soft_break();
            // The owed collapsible space dies at the wrap (CSS Text §4.1.3);
            // an owed inline-box edge travels with the content it precedes.
            self.pending_gap_px = gap as f32 * self.cell_w;
            self.place(seg, ctx, false, false);
            return;
        }
        // Merge into the previous piece when nothing but a collapsible space
        // separates two same-styled runs — fewer, wider items (selection and
        // find work run-wise, matching the old engine's item granularity).
        if gap == 0
            && let Some(last) = self.cur.last_mut()
            && last.item.kind == ctx.kind
            && last.item.emph == ctx.emph
            && last.item.node == ctx.node
            && last.item.link == ctx.link
            && last.item.invisible == ctx.invisible
            && last.stretch == ctx.ws.collapses_spaces()
            && last.item.image.is_none()
            && last.rows == 1
            && last.col + display_width(&last.item.text) == self.pen
        {
            if space {
                last.item.text.push(' ');
            }
            last.item.text.push_str(&text);
            last.item.width = display_width(&last.item.text) as u16;
            self.pen += usize::from(space) + w;
            self.pending_space = false;
            return;
        }
        let col = self.pen + usize::from(space) + gap;
        self.cur.push(Piece {
            col,
            row_off: 0,
            rows: 1,
            box_off_rows: 0,
            item: Item {
                col: 0,
                width: w as u16,
                height: 1,
                text: text.into_owned(),
                kind: ctx.kind,
                image: None,
                emph: ctx.emph,
                node: ctx.node,
                link: ctx.link.clone(),
                crop: false,
                pixelated: false,
                invisible: ctx.invisible,
            },
            stretch: ctx.ws.collapses_spaces(),
            space_before: space,
            atom_box: false,
        });
        self.pen = col + w;
        self.pending_space = false;
    }

    /// An atomic inline box (image or form control). Public so the block
    /// flow can size a block-level replaced box through the same path.
    pub fn atom(&mut self, a: &Atom, ctx: &InlineStyle) {
        match &a.kind {
            AtomKind::Img { url, alt } => self.image(a.node, url.as_deref(), alt, ctx),
            AtomKind::Media { video } => self.media(a.node, *video, ctx),
            AtomKind::Control { form, field } => {
                let Some(f) = self.forms.get(*form).and_then(|f| f.fields.get(*field)) else {
                    return;
                };
                let label = control_label(
                    self.dom,
                    a.node,
                    f,
                    Some(self.cb_w_px),
                    self.cap,
                    self.cell_w,
                    self.vp,
                );
                if label.is_empty() {
                    return;
                }
                let w = display_width(&label);
                self.place_atom(
                    w,
                    1,
                    0,
                    0,
                    Item {
                        col: 0,
                        width: w as u16,
                        height: 1,
                        text: label,
                        kind: ItemKind::Form,
                        image: None,
                        emph: Emphasis::default(),
                        node: a.node,
                        link: Some(Link::Form {
                            form: *form,
                            field: *field,
                        }),
                        crop: false,
                        pixelated: false,
                        invisible: ctx.invisible,
                    },
                    false,
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
    pub fn image(&mut self, node: NodeId, url: Option<&str>, alt: &str, ctx: &InlineStyle) {
        let natural = url
            .and_then(|u| self.images.get(u))
            .filter(|&&(w, h)| w > 0 && h > 0)
            .map(|&(w, h)| (f32::from(w) * self.cell_w, f32::from(h) * self.cell_h));
        if let Some(r) = super::replaced::size(
            self.dom,
            node,
            natural,
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
            let box_w = ((r.box_w / self.cell_w).round().max(1.0) as usize).max(1);
            let box_rows = (r.box_h / self.cell_h).round().max(1.0) as u16;
            let paint_w = ((r.paint_w / self.cell_w).round().max(1.0) as u16).min(box_w as u16);
            let paint_rows = ((r.paint_h / self.cell_h).round().max(1.0) as u16).min(box_rows);
            let off_c =
                ((r.off_x / self.cell_w).round().max(0.0) as u16).min(box_w as u16 - paint_w);
            let off_r =
                ((r.off_y / self.cell_h).round().max(0.0) as u16).min(box_rows - paint_rows);
            self.place_atom(
                box_w,
                box_rows,
                off_c,
                off_r,
                Item {
                    col: 0,
                    width: paint_w,
                    height: paint_rows,
                    text: String::new(),
                    kind: ItemKind::Image,
                    image: natural
                        .is_some()
                        .then(|| url.unwrap_or_default().to_string()),
                    emph: Emphasis::default(),
                    node,
                    link: ctx.link.clone(),
                    crop: r.crop,
                    pixelated,
                    invisible: ctx.invisible,
                },
                false,
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
    /// PAGE yt-dlp resolves — the enclosing card link, else this page; a
    /// streaming video whose only target would be THIS page when the page
    /// does NOT declare itself a video page (Open Graph — `og:video`) is a
    /// DEAD END (the homepage-autoplay hero): no link. og:image is borrowed
    /// as a poster ONLY when the representation plays this page (og:image
    /// describes THIS page's media and nothing else). The old engine's
    /// faded-poster borrow (`hidden_preview_in_cb`) is deletion-list
    /// machinery and deliberately NOT ported — the fragment stack reads it
    /// once positioned layout lands (P4).
    fn media(&mut self, node: NodeId, video: bool, ctx: &InlineStyle) {
        let own_suppressed = self.dom.paint_suppressed(node) || self.dom.visibility_hidden(node);
        // A paint-suppressed OUT-OF-FLOW media element contributes nothing:
        // an abspos box takes no normal-flow space (§9.3.1 — such boxes are
        // laid in-flow only until P4) and a suppressed one paints no cells,
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
                    _ => self.base.clone(),
                };
                (Some(page), None, true)
            }
            None => return, // sourceless audio: nothing to represent
        };
        let Some(play) = play else { return };
        let plays_this_page = streaming && play == *self.base;
        let dead_end = plays_this_page && !crate::layout::page_declares_video(self.dom);
        let link = (!dead_end).then_some(Link::Media(play));
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
                        (plays_this_page && !dead_end)
                            .then(|| crate::layout::page_preview_image(self.dom, self.base))
                            .flatten()
                    })
            })
            .flatten();
        if let Some(poster) = poster
            && let Some(&(iw, ih)) = self.images.get(&poster)
            && iw > 0
            && ih > 0
        {
            // The poster draws at its DECODED box capped to the line — never
            // the video's CSS box, which often carries a `height:0`/padding
            // aspect hack a poster must not inherit.
            let w = (iw as usize).min(self.cap).max(1);
            let h = ((u32::from(ih) * w as u32) / u32::from(iw)).max(1) as u16;
            self.place_atom(
                w,
                h,
                0,
                0,
                Item {
                    col: 0,
                    width: w as u16,
                    height: h,
                    text: String::new(),
                    kind: ItemKind::Image,
                    image: Some(poster),
                    emph: Emphasis::default(),
                    node,
                    link: link.clone(),
                    crop: false,
                    pixelated: false,
                    invisible,
                },
                false,
            );
            return; // the drawn preview IS the mpv affordance
        }
        if dead_end {
            return; // nothing playable and no poster: nothing to show
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

    /// Place an atomic box of `box_w`×`box_rows` cells (unbreakable). The
    /// painted `item` may be smaller than its box (`object-fit: contain`
    /// letterboxing), offset `off_cols`/`off_rows` from the box's top-left;
    /// the pen and the line height always advance by the BOX.
    fn place_atom(
        &mut self,
        box_w: usize,
        box_rows: u16,
        off_cols: u16,
        off_rows: u16,
        item: Item,
        atom_box: bool,
    ) {
        let space = self.pending_space && self.pen > self.line_start;
        let gap = self.take_gap();
        if self.pen + usize::from(space) + gap + box_w > self.line_right
            && self.pen > self.line_start
        {
            self.soft_break();
            self.pending_gap_px = gap as f32 * self.cell_w;
            self.place_atom(box_w, box_rows, off_cols, off_rows, item, atom_box);
            return;
        }
        let col = self.pen + usize::from(space) + gap;
        self.cur.push(Piece {
            col: col + off_cols as usize,
            row_off: 0,
            rows: box_rows,
            box_off_rows: off_rows,
            item,
            stretch: false,
            space_before: space,
            atom_box,
        });
        self.pen = col + box_w;
        self.pending_space = false;
    }

    /// Place an atomic inline box (`inline-block`/`inline-flex`/`inline-grid`):
    /// reserve its pre-laid margin-box cells on the line as an unbreakable
    /// paint-nothing placeholder (`item.node` = the box id). The box's real
    /// content is a fragment the block flow splices at this piece's resolved
    /// position (`finish` returns the placement). The IFC only needs the size.
    fn place_atom_box(&mut self, node: NodeId) {
        let idx = self.atom_next;
        self.atom_next += 1;
        let Some(sz) = self.atom_boxes.get(idx).copied() else {
            return;
        };
        self.place_atom(
            sz.w_cells,
            sz.h_rows,
            0,
            0,
            Item {
                col: 0,
                width: sz.w_cells as u16,
                height: sz.h_rows,
                text: String::new(),
                kind: crate::layout::ItemKind::Text,
                image: None,
                emph: Emphasis::default(),
                node,
                link: None,
                crop: false,
                pixelated: false,
                invisible: false,
            },
            true,
        );
    }

    /// Consume the owed inline-box edge width as whole cells.
    fn take_gap(&mut self) -> usize {
        let cells = (self.pending_gap_px / self.cell_w).round().max(0.0) as usize;
        self.pending_gap_px = 0.0;
        cells
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
        let pieces = std::mem::take(&mut self.cur);
        if pieces.is_empty() && !forced {
            self.pen = self.line_start;
            self.pending_space = false;
            return;
        }
        let rows = pieces.iter().map(|p| p.rows).max().unwrap_or(1);
        // The pen is the line's used extent — a `contain`-fitted replaced
        // box occupies more cells than its painted item, so the pieces alone
        // under-report.
        let width = self.pen;
        let cap = self.line_right;
        let mut line = LineOut {
            pieces,
            rows,
            forced,
            width,
            cap,
        };
        // Baseline alignment quantized: bottoms on the line's last row (a
        // replaced box's baseline is its bottom margin edge — §10.8.1).
        for p in &mut line.pieces {
            p.row_off = rows - p.rows;
        }
        // Center/right shift now, within this line's (float-shortened) band;
        // justification waits for `finish`, where "last line" is known.
        if width < cap {
            let off = match self.align {
                Align2::Center => (cap - width) / 2,
                Align2::Right => cap - width,
                Align2::Left | Align2::Justify => 0,
            };
            if off > 0 {
                for p in &mut line.pieces {
                    p.col += off;
                }
            }
        }
        // Record atomic-inline-box placements (col/row_off now resolved) and
        // drop their paint-nothing placeholders: the block flow splices the
        // box's real content fragment at each spot. Removal doesn't shift the
        // siblings — their columns are already absolute on the line.
        if line.pieces.iter().any(|p| p.atom_box) {
            let li = self.lines.len();
            for p in &line.pieces {
                if p.atom_box {
                    self.atom_places.push(AtomBoxPlace {
                        node: p.item.node,
                        line: li,
                        col: p.col,
                        row_off: p.row_off,
                    });
                }
            }
            line.pieces.retain(|p| !p.atom_box);
        }
        self.lines.push(line);
        // Advance past this line box, then open the next against the band at
        // the new vertical position (a taller float may still shorten it).
        self.laid_h += f32::from(rows) * self.cell_h;
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
                if line.width < line.cap {
                    let extra = line.cap - line.width;
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

/// CSS Text §7.3 justification at cell resolution: distribute `extra` cells
/// across the line's expansion opportunities — the collapsible spaces inside
/// stretchable runs and the materialized inter-run space gaps — left to
/// right, one extra cell per remainder slot.
fn justify(line: &mut LineOut, extra: usize) {
    enum Slot {
        /// The 1-cell gap before piece `i`.
        Gap(usize),
        /// The space at byte `b` inside piece `i`'s text.
        Space(usize, usize),
    }
    let mut slots: Vec<Slot> = Vec::new();
    for (pi, p) in line.pieces.iter().enumerate() {
        if p.space_before {
            slots.push(Slot::Gap(pi));
        }
        if p.stretch {
            for (bi, c) in p.item.text.char_indices() {
                if c == ' ' {
                    slots.push(Slot::Space(pi, bi));
                }
            }
        }
    }
    if slots.is_empty() {
        return;
    }
    let base = extra / slots.len();
    let mut rem = extra % slots.len();
    let mut add_before = vec![0usize; line.pieces.len()];
    let mut add_inside: Vec<Vec<(usize, usize)>> = vec![Vec::new(); line.pieces.len()];
    for s in slots {
        let n = base + usize::from(rem > 0);
        rem = rem.saturating_sub(1);
        if n == 0 {
            continue;
        }
        match s {
            Slot::Gap(pi) => add_before[pi] += n,
            Slot::Space(pi, bi) => add_inside[pi].push((bi, n)),
        }
    }
    let mut shift = 0usize;
    for (pi, p) in line.pieces.iter_mut().enumerate() {
        shift += add_before[pi];
        p.col += shift;
        if !add_inside[pi].is_empty() {
            let mut text = String::with_capacity(p.item.text.len() + extra);
            let mut last = 0usize;
            for &(bi, n) in &add_inside[pi] {
                text.push_str(&p.item.text[last..=bi]);
                for _ in 0..n {
                    text.push(' ');
                }
                shift += n;
                last = bi + 1;
            }
            text.push_str(&p.item.text[last..]);
            p.item.text = text;
            p.item.width = display_width(&p.item.text) as u16;
        }
    }
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

/// A control's widget label. Editable fields (text/password/textarea) pad
/// to their used width — CSS `width` (needs a percentage basis; `None`
/// under an intrinsic-sizing constraint), else the HTML `size`/`cols`
/// attribute, else the UA default of 20 character advances — so a form's
/// input boxes read as boxes, exactly as wide as the page asked. The value
/// is never truncated (typed content outranks the declared width).
pub(crate) fn control_label(
    dom: &Dom,
    node: NodeId,
    f: &crate::doc::Field,
    cb_w: Option<f32>,
    cap: usize,
    cell_w: f32,
    vp: Vp,
) -> String {
    let mut label = f.row_label();
    use crate::doc::FieldKind;
    if !matches!(
        f.kind,
        FieldKind::Text | FieldKind::Password | FieldKind::Textarea
    ) {
        return label;
    }
    let u = Units::of(dom, node);
    let css_cells = dom
        .computed_value(node, "width")
        .and_then(|v| Len::parse(&v, u, vp))
        .and_then(|l| l.resolve(cb_w))
        .map(|px| (px / cell_w).round().max(1.0) as usize);
    let attr_ch = |name: &str| {
        dom.attr(node, name)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
    };
    // `size`/`cols` are measured in character advances = cells; the
    // widget's brackets add 2. HTML defaults both to 20.
    let attr_name = if f.kind == FieldKind::Textarea {
        "cols"
    } else {
        "size"
    };
    let target = css_cells
        .unwrap_or_else(|| attr_ch(attr_name).unwrap_or(20) + 2)
        .min(cap);
    let have = display_width(&label);
    if have < target && label.ends_with(']') {
        let pad = " ".repeat(target - have);
        label.truncate(label.len() - 1);
        label.push_str(&pad);
        label.push(']');
    }
    label
}
