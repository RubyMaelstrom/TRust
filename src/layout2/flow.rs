//! Block-level layout (CSS 2.1 §10.3.3 widths, §8.3.1 margin collapsing,
//! §10.5-§10.7 heights) producing the fragment tree.
//!
//! Everything here is f32 CSS px; positions are ABSOLUTE document
//! coordinates, assigned as a single cursor descends the box tree. Widths
//! resolve top-down: a box's used width comes from its containing block's
//! already-computed used width — a child never guesses (the whole point of
//! the overhaul). Heights resolve bottom-up from content.
//!
//! Margin collapsing is a streaming "margin strut": adjoining vertical
//! margins accumulate as (max positive, min negative) and flush to the
//! cursor the moment real content — a line box, border, or padding —
//! separates them. A parent whose top margin collapses with its first
//! child's takes its border-top edge from that joint flush (the positions
//! coincide per §8.3.1); a fully self-collapsing box takes the position it
//! would flush at "if it had a non-zero bottom border" (the spec's own
//! definition).

use url::Url;

use crate::doc::Form;
use crate::dom::{Dom, NodeId};
use crate::layout::{ImageSizes, Item, NO_NODE, display_width};

use super::flex::{
    AlignContent, AlignItem, FlexCalc, align_content_offsets, align_item_from, container_style,
    item_flex, justify_offsets, resolve_flexible_lengths,
};
use super::inline::{Ifc, LineOut, OofMark, Piece};
use super::intrinsic::IMode;
use super::style::{BOTTOM, BoxStyle, InlineStyle, LEFT, Pos, RIGHT, TOP, block_align};
use super::tree::{AtomKind, BoxNode, Content};
use super::value::{Len, Vp};

/// One laid-out fragment: a border-box rect in absolute px, plus content.
/// `'t` is the box tree (out-of-flow placeholders reference their box until
/// the positioned post-pass replaces them).
#[derive(Debug)]
pub(crate) struct Frag<'t> {
    /// The generating element (`NO_NODE` for anonymous boxes/line boxes).
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    /// Used border-box size — the stored used geometry (replaced sizing,
    /// positioned containing blocks, JS geometry from fragments — P7).
    pub w: f32,
    pub h: f32,
    /// Used border widths TRBL: the padding box (the containing block a
    /// positioned fragment offers its abspos descendants — §10.1) is the
    /// border box inset by these.
    pub border: [f32; 4],
    /// How the Appendix E painter treats this fragment.
    pub paint: PaintFlags,
    /// The effective clip rectangle (absolute px) applied to this fragment's
    /// OWN painted cells at paint time — `None` = unclipped. Set by the
    /// CB-aware `resolve_oof` pass: clip inheritance follows the containing-
    /// block chain (a positioned box is clipped by its containing block's
    /// clip, NOT by its static-position tree parent), which is exactly the
    /// chain `resolve_oof` already walks. CSS Overflow L3 §2/§3.
    pub clip: Option<Clip>,
    pub kind: FragKind<'t>,
    pub children: Vec<Frag<'t>>,
}

/// A clip rectangle in absolute CSS px. Each axis is independent — an
/// unclipped axis carries ±∞ bounds so intersection is a component-wise
/// max-of-lows / min-of-highs. The clip established by a scroll container is
/// its **padding box** (the scrollport, CSS Overflow L3 §2).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Clip {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Clip {
    /// Intersect two clips (a `None` operand is the unbounded clip).
    fn intersect(a: Option<Clip>, b: Option<Clip>) -> Option<Clip> {
        match (a, b) {
            (None, c) | (c, None) => c,
            (Some(a), Some(b)) => Some(Clip {
                x0: a.x0.max(b.x0),
                y0: a.y0.max(b.y0),
                x1: a.x1.min(b.x1),
                y1: a.y1.min(b.y1),
            }),
        }
    }
}

#[derive(Debug)]
pub(crate) enum FragKind<'t> {
    Block,
    /// A line box: placed inline items (cols in cells relative to `x`).
    Line(Vec<Piece>),
    /// An out-of-flow box's placeholder, sitting at its STATIC POSITION
    /// (§10.3.7/§10.6.4) in the fragment tree so every later translation
    /// moves it consistently; the positioned post-pass (`resolve_oof`)
    /// replaces it with the laid box. Carries the inline context the box
    /// inherits (by DOM tree, not by containing block).
    Oof(&'t BoxNode, Box<InlineStyle>),
}

/// The painter-facing summary of a box's stacking/positioning style
/// (§9.9/Appendix E, css-position-3 §2.2, css-transforms-1 §3).
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct PaintFlags {
    /// A positioned box (§9.3.2) — Appendix E step 8 when no real stacking
    /// context is formed (the z:auto pseudo-stacking-context).
    pub positioned: bool,
    /// Forms a stacking context, painted atomically at level
    /// `z.unwrap_or(0)`.
    pub sc: bool,
    pub z: Option<i32>,
    /// Paints a background: an opaque fill over the border box in the cell
    /// compositor (erases what's beneath in paint order).
    pub bg: bool,
    /// Containing block for absolutely positioned descendants (§10.1:
    /// positioned; transforms-1 §3: any transform).
    pub cb_abs: bool,
    /// Containing block for FIXED descendants (transformed boxes only).
    pub cb_fixed: bool,
}

/// Derive the paint flags from a box style. `item` = the box is a flex/grid
/// item (a non-auto z-index then forms a stacking context even at
/// position:static — css-flexbox §4.3).
pub(super) fn paint_flags(s: &BoxStyle, item: bool) -> PaintFlags {
    PaintFlags {
        positioned: s.position.positioned(),
        sc: s.stacking_context(item),
        z: s.z_index,
        bg: s.bg,
        cb_abs: s.position.positioned() || s.has_transform,
        cb_fixed: s.has_transform,
    }
}

impl<'t> Frag<'t> {
    pub(super) fn empty() -> Frag<'t> {
        Frag {
            node: NO_NODE,
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            border: [0.0; 4],
            paint: PaintFlags::default(),
            clip: None,
            kind: FragKind::Block,
            children: Vec::new(),
        }
    }

    /// The lowest border-box bottom edge in this subtree (scrollable-extent
    /// contribution). `pub(crate)` because paint recomputes the document
    /// height after scroll-region extraction empties region frags.
    pub(crate) fn max_bottom(&self) -> f32 {
        self.children
            .iter()
            .map(Frag::max_bottom)
            .fold(self.y + self.h, f32::max)
    }
}

/// A containing block's padding-box rectangle in absolute px (§10.1).
#[derive(Copy, Clone, Debug)]
struct CbRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// An out-of-flow box's placeholder fragment at its static position
/// (`content_x + the IFC pen offset`, the line's y).
fn oof_placeholder(m: OofMark<'_>, content_x: f32, y: f32) -> Frag<'_> {
    Frag {
        node: NO_NODE,
        x: content_x + m.x_px,
        y,
        w: 0.0,
        h: 0.0,
        border: [0.0; 4],
        paint: PaintFlags::default(),
        clip: None,
        kind: FragKind::Oof(m.b, Box::new(m.ctx)),
        children: Vec::new(),
    }
}

/// The §8.3.1 margin strut + the descending cursor.
#[derive(Default)]
struct Cursor {
    y: f32,
    pos: f32,
    neg: f32,
    /// Every flush position, in order — a box whose top edge collapsed
    /// through to a descendant reads its border-top position from the first
    /// flush that happened inside it.
    flush_log: Vec<f32>,
    /// Flow positions of inline ELEMENTS (from the IFCs' entry marks) — the
    /// anchor rows of boxes that paint nothing (`<a name>`, empty ids).
    anchors: Vec<(NodeId, f32)>,
}

impl Cursor {
    fn margin(&mut self, m: f32) {
        if m >= 0.0 {
            self.pos = self.pos.max(m);
        } else {
            self.neg = self.neg.min(m);
        }
    }

    /// Where a flush NOW would land (§8.3.1: collapsed margin = max of the
    /// positives + min of the negatives).
    fn preview(&self) -> f32 {
        self.y + self.pos + self.neg
    }

    fn flush(&mut self) -> f32 {
        let y = self.preview();
        self.y = y;
        self.pos = 0.0;
        self.neg = 0.0;
        self.flush_log.push(y);
        y
    }
}

pub(crate) struct Flow<'a> {
    pub dom: &'a Dom,
    pub base: &'a Url,
    pub forms: &'a [Form],
    pub images: &'a ImageSizes,
    pub vp: Vp,
    pub cell_w: f32,
    pub cell_h: f32,
    /// The intrinsic-size memo: (element, is-min-mode) → content px. Pass-
    /// wide, so nested flex towers query each subtree once per mode.
    pub imemo: std::cell::RefCell<std::collections::HashMap<(NodeId, bool), f32>>,
}

/// Resolved horizontal geometry of one block box (§10.3.3/§10.4).
struct H {
    ml: f32,
    bp_l: f32,
    bp_r: f32,
    content_w: f32,
}

impl Flow<'_> {
    /// Lay the document: the root element against the initial containing
    /// block (the viewport). Returns the fragment tree, the in-flow bottom
    /// in px (paint finalizes the document height post-extraction), the flow
    /// positions of paint-less inline elements (anchor targets), and the laid
    /// `position:fixed` boxes (viewport coordinates — the pinned layer,
    /// painted separately).
    #[allow(clippy::type_complexity)]
    pub fn layout<'t>(
        &self,
        root: &'t BoxNode,
    ) -> (Frag<'t>, f32, Vec<(NodeId, f32)>, Vec<Frag<'t>>) {
        let mut cur = Cursor::default();
        let inl = InlineStyle::root();
        // §8.3.1: margins of the root element's box do not collapse. Its top
        // margin is a plain offset; its children then collapse among
        // themselves inside it.
        let mt = root.style.margin[TOP]
            .resolve(Some(self.vp.w))
            .unwrap_or(0.0);
        let mb = root.style.margin[BOTTOM]
            .resolve(Some(self.vp.w))
            .unwrap_or(0.0);
        cur.y = mt.max(0.0);
        let cb_h = (self.vp.h > 0.0).then_some(self.vp.h);
        let mut frag = self.block(root, 0.0, self.vp.w, cb_h, &mut cur, &inl);
        // The document's height includes trailing collapsed-out margins (the
        // scrollable extent a browser gives a body bottom margin).
        let flow_bottom = cur.flush() + mb.max(0.0);
        let mut anchors = std::mem::take(&mut cur.anchors);
        // The positioned post-pass: every out-of-flow placeholder resolves
        // against its containing block's FINAL geometry (§10.1 — all
        // translations already applied), fixed boxes peel off to the pinned
        // layer (§9.6.1: their containing block is the viewport).
        let icb = CbRect {
            x: 0.0,
            y: 0.0,
            w: self.vp.w,
            h: self.vp.h.max(0.0),
        };
        let mut fixed: Vec<Frag<'t>> = Vec::new();
        self.resolve_oof(
            &mut frag,
            icb,
            None,
            icb,
            &mut anchors,
            &mut fixed,
            None,
            None,
            None,
        );
        let mut fi = 0;
        while fi < fixed.len() {
            // A fixed box's own subtree can hold more out-of-flow boxes
            // (its abspos children position against IT; nested fixed boxes
            // pin independently).
            let mut f = std::mem::replace(&mut fixed[fi], Frag::empty());
            self.resolve_oof(
                &mut f,
                icb,
                None,
                icb,
                &mut Vec::new(),
                &mut fixed,
                None,
                None,
                None,
            );
            fixed[fi] = f;
            fi += 1;
        }
        // Return the in-flow bottom (trailing collapsed margins included);
        // paint computes the final document height as `max_bottom ∪ flow_bottom`
        // AFTER scroll-region extraction has emptied any region frags (so an
        // extracted region contributes its reserved band height, not the full
        // height of its scrolled-away content).
        (frag, flow_bottom.max(0.0), anchors, fixed)
    }

    /// Lay one block-level box. `cb_x`/`cb_w` are the containing block's
    /// content-box left edge and width; `cb_h` its definite content height
    /// when it has one (percentage-height basis, §10.5).
    fn block<'t>(
        &self,
        b: &'t BoxNode,
        cb_x: f32,
        cb_w: f32,
        cb_h: Option<f32>,
        cur: &mut Cursor,
        parent_inl: &InlineStyle,
    ) -> Frag<'t> {
        let s = &b.style;
        // Anchors recorded inside this box shift with its §9.4.3/transform
        // paint offset.
        let a0 = cur.anchors.len();
        let inl = if b.node == NO_NODE {
            parent_inl.clone()
        } else {
            InlineStyle::derive(self.dom, b.node, parent_inl, self.base)
        };
        // Anonymous boxes inherit the text properties of their parent
        // element (§9.2.1.1) — alignment/indent read from it.
        let style_node = if b.node == NO_NODE { inl.node } else { b.node };
        // `h`/`x_border` are mutable because a table box shrinks to fit
        // (§17.5.2) and repositions within its band (§17.4 auto margins /
        // align) — its used content width and border-box left are known only
        // after the column algorithm runs, inside the `Content::Table` arm.
        let mut h = self.horizontal(s, cb_w);
        let mut x_border = cb_x + h.ml;
        let content_x = x_border + h.bp_l;
        let bt = s.border[TOP] + self.pad(s, TOP, cb_w);
        let bb = s.border[BOTTOM] + self.pad(s, BOTTOM, cb_w);
        let mt = s.margin[TOP].resolve(Some(cb_w)).unwrap_or(0.0);
        let mb = s.margin[BOTTOM].resolve(Some(cb_w)).unwrap_or(0.0);
        cur.margin(mt);

        // Definite heights (content-box px). A percentage against an
        // indefinite CB height is auto (§10.5).
        let spec_h = self.height_px(&s.height, s, bt, bb, cb_h);
        let min_h = self
            .height_px(&s.min_height, s, bt, bb, cb_h)
            .unwrap_or(0.0);
        let max_h = self
            .height_px(&s.max_height, s, bt, bb, cb_h)
            .unwrap_or(f32::INFINITY);

        // The definite content height children/replaced content resolve
        // percentages against (clamped — §10.5 wants the used basis).
        let ifc_cb_h = spec_h.map(|v| v.clamp(min_h, max_h.max(min_h)));

        let mut y_border: Option<f32> = None;
        if bt > 0.0 {
            let yb = cur.flush();
            y_border = Some(yb);
            cur.y = yb + bt;
        }
        let content_top_of = |yb: f32| yb + bt;

        // ---- content ----
        let mut children: Vec<Frag> = Vec::new();
        match &b.content {
            Content::Blocks(kids) => {
                let log = cur.flush_log.len();
                for k in kids {
                    children.push(self.block(k, content_x, h.content_w, ifc_cb_h, cur, &inl));
                }
                if y_border.is_none() && cur.flush_log.len() > log {
                    // Top margin collapsed through to a descendant: our
                    // border-top edge coincides with that first flush.
                    y_border = Some(cur.flush_log[log]);
                }
            }
            Content::Inlines(inls) => {
                let mut ifc = Ifc::new(
                    self.dom,
                    self.base,
                    self.images,
                    self.forms,
                    self.vp,
                    self.cell_w,
                    self.cell_h,
                    h.content_w,
                    ifc_cb_h,
                    block_align(self.dom, style_node),
                    self.indent_px(style_node, h.content_w),
                );
                if b.marker_inside
                    && let Some(m) = &b.marker
                {
                    let mut mctx = inl.clone();
                    mctx.kind = crate::layout::ItemKind::Text;
                    ifc.text(m, &mctx);
                }
                ifc.run(inls, &inl);
                let (lines, marks, oofs) = ifc.finish();
                if !lines.is_empty() {
                    let yb = *y_border.get_or_insert_with(|| {
                        let yb = cur.flush();
                        cur.y = yb; // bt == 0 on this path
                        yb
                    });
                    cur.y = content_top_of(yb).max(cur.y);
                    let n = lines.len();
                    self.emit_lines(lines, content_x, cur, &mut children);
                    let first = children.len() - n;
                    let end_y = cur.y;
                    let line_y = |idx: usize, children: &[Frag<'_>]| {
                        if idx < n {
                            children[first + idx].y
                        } else {
                            end_y
                        }
                    };
                    for (node, idx) in marks {
                        cur.anchors.push((node, line_y(idx, &children)));
                    }
                    for m in oofs {
                        let y = line_y(m.line, &children);
                        children.push(oof_placeholder(m, content_x, y));
                    }
                } else {
                    // No line boxes: the elements still sit at this flow
                    // position (where the box self-collapses to).
                    for (node, _) in marks {
                        cur.anchors.push((node, cur.preview()));
                    }
                    let y = cur.preview();
                    for m in oofs {
                        children.push(oof_placeholder(m, content_x, y));
                    }
                }
            }
            Content::Flex(items) => {
                // A flex container establishes an independent formatting
                // context: child margins never escape it, so an occupied
                // container flushes at its top edge like an IFC does; an
                // empty one self-collapses like an empty block.
                if !items.is_empty() || !b.oof.is_empty() {
                    let yb = *y_border.get_or_insert_with(|| {
                        let yb = cur.flush();
                        cur.y = yb;
                        yb
                    });
                    cur.y = content_top_of(yb).max(cur.y);
                    let top = cur.y;
                    if !items.is_empty() {
                        let (frags, fh) = self.flex_content(
                            b,
                            items,
                            content_x,
                            top,
                            h.content_w,
                            ifc_cb_h,
                            (min_h, max_h),
                            &inl,
                            &mut cur.anchors,
                        );
                        children.extend(frags);
                        cur.y += fh;
                    }
                    self.container_oof(b, &inl, content_x, top, &mut children);
                }
            }
            Content::Grid(items) => {
                // A grid container establishes an independent formatting
                // context, exactly like the flex arm above.
                if !items.is_empty() || !b.oof.is_empty() {
                    let yb = *y_border.get_or_insert_with(|| {
                        let yb = cur.flush();
                        cur.y = yb;
                        yb
                    });
                    cur.y = content_top_of(yb).max(cur.y);
                    let top = cur.y;
                    if !items.is_empty() {
                        let (frags, gh) = self.grid_content(
                            b,
                            items,
                            content_x,
                            top,
                            h.content_w,
                            ifc_cb_h,
                            &inl,
                            &mut cur.anchors,
                        );
                        children.extend(frags);
                        cur.y += gh;
                    }
                    self.container_oof(b, &inl, content_x, top, &mut children);
                }
            }
            Content::Table(tb) => {
                // A table establishes an independent formatting context (like
                // flex/grid): it flushes at its top edge when occupied and
                // self-collapses when empty.
                let occupied =
                    tb.ncols > 0 || !tb.top_captions.is_empty() || !tb.bottom_captions.is_empty();
                if occupied {
                    let yb = *y_border.get_or_insert_with(|| {
                        let yb = cur.flush();
                        cur.y = yb;
                        yb
                    });
                    cur.y = content_top_of(yb).max(cur.y);
                    // §17.5.2: a definite width fills the band; an auto (or
                    // min/max-content) width shrinks to fit and repositions
                    // by §17.4 auto margins / align context. The table's width
                    // is its CSS `width` (already resolved into `h.content_w`),
                    // else the HTML `width` attribute (HTML §15.3.13 — not a
                    // CSS property, so `BoxStyle` never saw it), else auto.
                    let (avail_w, width_auto) = if s.width.resolve(Some(cb_w)).is_some() {
                        (h.content_w, false)
                    } else {
                        match super::tree::declared_track_width(self.dom, b.node) {
                            Some(super::tree::ColSpec::Px(px)) => (px.max(1.0), false),
                            Some(super::tree::ColSpec::Pct(p)) => ((p * cb_w).max(1.0), false),
                            None => (h.content_w, true),
                        }
                    };
                    let cols = self.table_columns(tb, b.node, avail_w, width_auto, &inl);
                    // Position a table narrower than its band (§17.4 auto
                    // margins / align). A CSS-definite width already centered
                    // via §10.3.3 auto margins in `horizontal` (its band ==
                    // `h.content_w` == the width, so this is a no-op there).
                    let lead = self.table_lead(b.node, cols.table_w, h.content_w);
                    let cap_x = content_x + lead;
                    let cap_w = cols.table_w.max(1.0);
                    // Top captions (§17.4), then the grid, then bottom captions
                    // — each a block box at the table's used width.
                    for cap in &tb.top_captions {
                        children.push(self.block(cap, cap_x, cap_w, ifc_cb_h, cur, &inl));
                    }
                    let grid_top = cur.flush();
                    let (frags, gh) = self.table_grid(
                        tb,
                        b.node,
                        &cols,
                        cap_x,
                        grid_top,
                        ifc_cb_h,
                        &inl,
                        &mut cur.anchors,
                    );
                    children.extend(frags);
                    cur.y = grid_top + gh;
                    for cap in &tb.bottom_captions {
                        children.push(self.block(cap, cap_x, cap_w, ifc_cb_h, cur, &inl));
                    }
                    // Enclose the (possibly shrunk + shifted) table.
                    x_border += lead;
                    h.content_w = cols.table_w;
                }
            }
            Content::Atomic(atom) => {
                // A block-level replaced box: size through the same replaced
                // sizing the IFC uses, then place per §10.3.4 (auto margins
                // center a definite-width replaced box).
                let mut ifc = Ifc::new(
                    self.dom,
                    self.base,
                    self.images,
                    self.forms,
                    self.vp,
                    self.cell_w,
                    self.cell_h,
                    h.content_w,
                    ifc_cb_h,
                    super::style::Align2::Left,
                    0.0,
                );
                ifc.atom(atom, &inl);
                let (lines, _, _) = ifc.finish();
                if !lines.is_empty() {
                    let yb = *y_border.get_or_insert_with(|| {
                        let yb = cur.flush();
                        cur.y = yb;
                        yb
                    });
                    cur.y = content_top_of(yb).max(cur.y);
                    // §10.3.4: auto left/right margins center the box. The
                    // line's pen-based width is the box extent — a painted
                    // `contain` item can be narrower than its box.
                    let box_w = lines
                        .iter()
                        .map(|l| l.width as f32 * self.cell_w)
                        .fold(0.0f32, f32::max);
                    let free = (h.content_w - box_w).max(0.0);
                    let off = match (s.margin[LEFT].is_auto(), s.margin[RIGHT].is_auto()) {
                        (true, true) => free / 2.0,
                        (true, false) => free,
                        _ => 0.0,
                    };
                    self.emit_lines(lines, content_x + off, cur, &mut children);
                }
            }
        }

        // ---- the ::marker (outside position) ----
        if let Some(m) = &b.marker
            && !b.marker_inside
        {
            children.push(self.marker_frag(m, &inl, content_x, &children, y_border, bt));
        }

        // ---- bottom edge / used height (§10.6.3, §10.7, §8.3.1) ----
        let frag_h;
        let y_final;
        match spec_h {
            Some(h0) => {
                let hc = h0.clamp(min_h, max_h.max(min_h));
                let yb = match y_border {
                    Some(yb) => yb,
                    None if hc == 0.0 && bt == 0.0 && bb == 0.0 => {
                        // Zero definite height with no border/padding still
                        // self-collapses (§8.3.1's "zero computed height").
                        let yb = cur.preview();
                        cur.margin(mb);
                        return self.finish_frag(
                            b,
                            x_border,
                            yb,
                            &h,
                            0.0,
                            children,
                            (cb_w, cb_h),
                            cur,
                            a0,
                        );
                    }
                    None => cur.flush(),
                };
                // Children's trailing margins stay inside a definite-height
                // box (no collapse-through; nothing paints in them).
                cur.pos = 0.0;
                cur.neg = 0.0;
                frag_h = bt + hc + bb;
                y_final = yb;
                cur.y = yb + frag_h;
                cur.margin(mb);
            }
            None => {
                if bb > 0.0 {
                    // Bottom border/padding separate: trailing child margins
                    // resolve inside the box.
                    let content_bottom = cur.flush();
                    let yb = y_border.unwrap_or(content_bottom);
                    let hc = (content_bottom - content_top_of(yb))
                        .max(0.0)
                        .clamp(min_h, max_h.max(min_h));
                    frag_h = bt + hc + bb;
                    y_final = yb;
                    cur.y = content_top_of(yb) + hc + bb;
                    cur.margin(mb);
                } else {
                    match y_border {
                        Some(yb) => {
                            // Bottom margin adjoins the last child's
                            // (height:auto, no separation): the strut keeps
                            // the trailing margins and gains ours.
                            let content_bottom = cur.y;
                            let hc = (content_bottom - content_top_of(yb))
                                .max(0.0)
                                .clamp(min_h, max_h.max(min_h));
                            frag_h = bt + hc;
                            y_final = yb;
                            if min_h > 0.0 || max_h < f32::INFINITY {
                                // A clamp made the height definite-ish: the
                                // box ends at its clamped edge.
                                cur.y = content_top_of(yb) + hc;
                                cur.pos = 0.0;
                                cur.neg = 0.0;
                            }
                            cur.margin(mb);
                        }
                        None => {
                            // Fully self-collapsing: position = the strut's
                            // current landing point (§8.3.1), then our bottom
                            // margin joins the same strut.
                            if min_h > 0.0 {
                                // min-height keeps the box open (§8.3.1's
                                // self-collapse requires min-height 0).
                                let yb = cur.flush();
                                cur.y = yb + min_h;
                                cur.margin(mb);
                                return self.finish_frag(
                                    b,
                                    x_border,
                                    yb,
                                    &h,
                                    min_h,
                                    children,
                                    (cb_w, cb_h),
                                    cur,
                                    a0,
                                );
                            }
                            frag_h = 0.0;
                            y_final = cur.preview();
                            cur.margin(mb);
                        }
                    }
                }
            }
        }
        self.finish_frag(
            b,
            x_border,
            y_final,
            &h,
            frag_h,
            children,
            (cb_w, cb_h),
            cur,
            a0,
        )
    }

    /// Finish a block-level box's fragment: record its used geometry and
    /// paint flags, then apply its paint-time offset — §9.4.3 relative
    /// positioning and the transform translation — to the whole subtree
    /// (following boxes are unaffected: the cursor is never touched).
    /// `a0` = the anchor high-water mark at the box's entry, so anchors
    /// recorded inside it ride along.
    #[allow(clippy::too_many_arguments)]
    fn finish_frag<'t>(
        &self,
        b: &'t BoxNode,
        x: f32,
        y: f32,
        h: &H,
        frag_h: f32,
        children: Vec<Frag<'t>>,
        cb: (f32, Option<f32>),
        cur: &mut Cursor,
        a0: usize,
    ) -> Frag<'t> {
        let mut frag = Frag {
            node: b.node,
            x,
            y,
            w: h.bp_l + h.content_w + h.bp_r,
            h: frag_h,
            border: b.style.border,
            paint: paint_flags(&b.style, false),
            clip: None,
            kind: FragKind::Block,
            children,
        };
        let (dx, dy) = self.paint_offset(&b.style, cb.0, cb.1, frag.w, frag.h);
        if dx != 0.0 || dy != 0.0 {
            Self::offset_frag(&mut frag, dx, dy);
            for a in &mut cur.anchors[a0..] {
                a.1 += dy;
            }
        }
        frag
    }

    /// The paint-time offset of a box: §9.4.3 relative positioning plus the
    /// translation component of its transform (css-transforms-1 — % against
    /// the box's own border box). Sticky offsets are scroll-driven and
    /// contribute zero at the initial scroll position (css-position-3 §3.4).
    pub(super) fn paint_offset(
        &self,
        s: &BoxStyle,
        cb_w: f32,
        cb_h: Option<f32>,
        w: f32,
        h: f32,
    ) -> (f32, f32) {
        let mut dx = 0.0f32;
        let mut dy = 0.0f32;
        if s.position == Pos::Relative {
            let l = s.inset[LEFT].resolve(Some(cb_w));
            let r = s.inset[RIGHT].resolve(Some(cb_w));
            // §9.4.3: used left = -right; both auto → 0; both set → left
            // wins (ltr). A % against an indefinite CB height stays auto.
            dx += match (l, r) {
                (Some(l), _) => l,
                (None, Some(r)) => -r,
                (None, None) => 0.0,
            };
            let t = s.inset[TOP].resolve(cb_h);
            let bo = s.inset[BOTTOM].resolve(cb_h);
            dy += match (t, bo) {
                (Some(t), _) => t,
                (None, Some(b)) => -b,
                (None, None) => 0.0,
            };
        }
        if s.has_transform {
            dx += s.tx.0 * w + s.tx.1;
            dy += s.ty.0 * h + s.ty.1;
        }
        (dx, dy)
    }

    /// Placeholders for a flex/grid container's out-of-flow children. Their
    /// static-position rectangle is the container's content box (css-flexbox
    /// §4.1 / css-grid §9.1; the "as if it were the sole item" alignment
    /// refinement is documented as not done — the origin is used).
    fn container_oof<'t>(
        &self,
        b: &'t BoxNode,
        inl: &InlineStyle,
        x: f32,
        y: f32,
        out: &mut Vec<Frag<'t>>,
    ) {
        for ob in &b.oof {
            out.push(Frag {
                node: NO_NODE,
                x,
                y,
                w: 0.0,
                h: 0.0,
                border: [0.0; 4],
                paint: PaintFlags::default(),
                clip: None,
                kind: FragKind::Oof(ob, Box::new(inl.clone())),
                children: Vec::new(),
            });
        }
    }

    /// Turn finished line boxes into Line fragments at `x`, advancing the
    /// cursor by each line's height (line boxes pack — no margins between).
    fn emit_lines(&self, lines: Vec<LineOut>, x: f32, cur: &mut Cursor, out: &mut Vec<Frag<'_>>) {
        for line in lines {
            let hpx = f32::from(line.rows) * self.cell_h;
            out.push(Frag {
                node: NO_NODE,
                x,
                y: cur.y,
                w: 0.0,
                h: hpx,
                border: [0.0; 4],
                paint: PaintFlags::default(),
                clip: None,
                kind: FragKind::Line(line.pieces),
                children: Vec::new(),
            });
            cur.y += hpx;
        }
    }

    /// The outside `::marker` of a list item: right-aligned against the
    /// content edge, on the first line's row (CSS Lists — the marker sits in
    /// the gutter the UA list padding provides).
    fn marker_frag(
        &self,
        marker: &str,
        inl: &InlineStyle,
        content_x: f32,
        children: &[Frag<'_>],
        y_border: Option<f32>,
        bt: f32,
    ) -> Frag<'static> {
        fn first_line_y(frags: &[Frag<'_>]) -> Option<f32> {
            let mut best: Option<f32> = None;
            for f in frags {
                let y = match &f.kind {
                    FragKind::Line(_) => Some(f.y),
                    FragKind::Block => first_line_y(&f.children),
                    FragKind::Oof(..) => None,
                };
                if let Some(y) = y {
                    best = Some(best.map_or(y, |b: f32| b.min(y)));
                }
            }
            best
        }
        let y = first_line_y(children)
            .or(y_border.map(|yb| yb + bt))
            .unwrap_or(0.0);
        let w = display_width(marker);
        let x = (content_x - w as f32 * self.cell_w).max(0.0);
        Frag {
            node: NO_NODE,
            x,
            y,
            w: w as f32 * self.cell_w,
            h: self.cell_h,
            border: [0.0; 4],
            paint: PaintFlags::default(),
            clip: None,
            kind: FragKind::Line(vec![Piece::solo(Item {
                col: 0,
                width: w as u16,
                height: 1,
                text: marker.to_string(),
                kind: crate::layout::ItemKind::Text,
                image: None,
                emph: inl.emph,
                node: NO_NODE,
                link: None,
                crop: false,
                pixelated: false,
                invisible: inl.invisible,
            })]),
            children: Vec::new(),
        }
    }

    /// §10.3.3 + §10.4: the used horizontal geometry of a block-level,
    /// non-replaced box in normal flow.
    fn horizontal(&self, s: &BoxStyle, cb_w: f32) -> H {
        let bp_l = s.border[LEFT] + self.pad(s, LEFT, cb_w);
        let bp_r = s.border[RIGHT] + self.pad(s, RIGHT, cb_w);
        let bp = bp_l + bp_r;
        // A declared width, content-box px (box-sizing adjusts).
        let spec = |l: &Len| {
            l.resolve(Some(cb_w)).map(|w| {
                if s.border_box {
                    (w - bp).max(0.0)
                } else {
                    w.max(0.0)
                }
            })
        };
        let min_w = spec(&s.min_width).unwrap_or(0.0);
        let max_w = match &s.max_width {
            Len::None => f32::INFINITY,
            l => spec(l).unwrap_or(f32::INFINITY),
        };
        // §10.3.3 for one candidate width; returns (ml, content_w).
        let solve = |w: Option<f32>| -> (f32, f32) {
            let ml = s.margin[LEFT].resolve(Some(cb_w));
            let mr = s.margin[RIGHT].resolve(Some(cb_w));
            match w {
                None => {
                    // width:auto — auto margins become 0 and the width fills.
                    let ml = ml.unwrap_or(0.0);
                    let mr = mr.unwrap_or(0.0);
                    (ml, (cb_w - ml - mr - bp).max(0.0))
                }
                Some(w) => {
                    let free = cb_w - w - bp;
                    let ml_auto = s.margin[LEFT].is_auto();
                    let mr_auto = s.margin[RIGHT].is_auto();
                    let ml = match (ml_auto, mr_auto) {
                        // Both auto: center (negative free → treated 0/ltr).
                        (true, true) => (free / 2.0).max(0.0),
                        (true, false) => free - mr.unwrap_or(0.0),
                        // ml known (or over-constrained: mr gives way, ltr).
                        _ => ml.unwrap_or(0.0),
                    };
                    (ml, w)
                }
            }
        };
        let (ml, w) = solve(spec(&s.width));
        let (ml, w) = if w > max_w {
            solve(Some(max_w))
        } else {
            (ml, w)
        };
        let (ml, w) = if w < min_w {
            solve(Some(min_w))
        } else {
            (ml, w)
        };
        H {
            ml,
            bp_l,
            bp_r,
            content_w: w,
        }
    }

    /// One padding side in px (percentages against the CB WIDTH — §8.4, both
    /// axes; that spec sentence is the whole "aspect-spacer idiom").
    pub(super) fn pad(&self, s: &BoxStyle, side: usize, cb_w: f32) -> f32 {
        s.padding[side].resolve(Some(cb_w)).unwrap_or(0.0).max(0.0)
    }

    /// A height-family property as content-box px, `None` when indefinite
    /// (`auto`, or a percentage against an indefinite CB height — §10.5).
    fn height_px(&self, l: &Len, s: &BoxStyle, bt: f32, bb: f32, cb_h: Option<f32>) -> Option<f32> {
        let v = l.resolve(cb_h)?;
        Some(if s.border_box {
            (v - bt - bb).max(0.0)
        } else {
            v.max(0.0)
        })
    }

    /// `text-indent` in px for a box's IFC (inherited; percentages against
    /// the box's own content width — CSS Text §6.2).
    pub(crate) fn indent_px(&self, node: NodeId, content_w: f32) -> f32 {
        if node == NO_NODE {
            return 0.0;
        }
        let u = crate::layout::Units::of(self.dom, node);
        self.dom
            .computed_value(node, "text-indent")
            .and_then(|v| Len::parse(&v, u, self.vp))
            .and_then(|l| l.resolve(Some(content_w)))
            .unwrap_or(0.0)
    }
}

/// One flex item mid-algorithm: prepared numbers, then the laid fragment
/// and its final placement (content-relative until the single translation
/// at the end).
struct FItem<'t> {
    b: &'t BoxNode,
    /// Resolved margins TRBL (`auto` → 0 — §9.5/§9.6 hand autos their
    /// share separately) and the auto flags.
    m: [f32; 4],
    auto: [bool; 4],
    bp_main: f32,
    align: AlignItem,
    /// The item's definite content height, when its `height` resolves
    /// (row: the cross size; column: the specified-size suggestion).
    def_h: Option<f32>,
    /// The cross-size property is `auto` (stretch eligibility).
    cross_auto: bool,
    frag: Option<Frag<'t>>,
    anchors: Vec<(NodeId, f32)>,
    border_x: f32,
    border_y: f32,
}

/// §9.3 greedy line collection over outer hypothetical main sizes. The
/// first item of a line always fits (spec: "If the very first uncollected
/// item wouldn’t fit, collect just it").
fn collect_lines(outer: &[f32], wrap: bool, avail: f32, gap: f32) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    if !wrap || outer.len() <= 1 {
        lines.push(0..outer.len());
        return lines;
    }
    let mut start = 0usize;
    let mut sum = 0.0f32;
    for (i, &oh) in outer.iter().enumerate() {
        if i == start {
            sum = oh;
            continue;
        }
        if sum + gap + oh > avail + 1e-3 {
            lines.push(start..i);
            start = i;
            sum = oh;
        } else {
            sum += gap + oh;
        }
    }
    lines.push(start..outer.len());
    lines
}

/// §9.6's per-item cross placement: the margin-box shift within the line
/// for the given auto-margin flags and alignment.
pub(super) fn cross_shift(extra: f32, auto_start: bool, auto_end: bool, align: AlignItem) -> f32 {
    let extra = extra.max(0.0);
    match (auto_start, auto_end) {
        (true, true) => extra / 2.0,
        (true, false) => extra,
        (false, true) => 0.0,
        (false, false) => match align {
            AlignItem::Start | AlignItem::Stretch => 0.0,
            AlignItem::Center => extra / 2.0,
            AlignItem::End => extra,
        },
    }
}

impl Flow<'_> {
    /// Lay a flex container's items (css-flexbox-1 §9, from the spec text).
    /// Returns the item fragments (absolute coordinates) and the container's
    /// content height.
    #[allow(clippy::too_many_arguments)] // the container's whole resolved context
    fn flex_content<'t>(
        &self,
        b: &'t BoxNode,
        items: &'t [BoxNode],
        content_x: f32,
        content_top: f32,
        content_w: f32,
        def_ch: Option<f32>,
        cross_clamp: (f32, f32),
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag<'t>>, f32) {
        let u = crate::layout::Units::of(self.dom, b.node);
        let fs = container_style(self.dom, b.node, u, self.vp);
        // Gap percentages resolve against the container's content box in
        // the gap's own axis (indefinite → zero).
        let (gap_main, gap_cross) = if fs.row {
            (
                fs.gap_main.resolve(Some(content_w)),
                fs.gap_cross.resolve(def_ch),
            )
        } else {
            (
                fs.gap_main.resolve(def_ch),
                fs.gap_cross.resolve(Some(content_w)),
            )
        };
        let gap_main = gap_main.unwrap_or(0.0).max(0.0);
        let gap_cross = gap_cross.unwrap_or(0.0).max(0.0);
        // §5.4 `order`: stable reorder.
        let mut idx: Vec<usize> = (0..items.len()).collect();
        idx.sort_by_key(|&i| self.order_of(items[i].node));
        let ordered: Vec<&BoxNode> = idx.iter().map(|&i| &items[i]).collect();
        if fs.row {
            self.flex_row(
                &fs,
                &ordered,
                gap_main,
                gap_cross,
                content_x,
                content_top,
                content_w,
                def_ch,
                cross_clamp,
                inl,
                anchors,
            )
        } else {
            self.flex_col(
                &fs,
                &ordered,
                gap_main,
                gap_cross,
                content_x,
                content_top,
                content_w,
                def_ch,
                inl,
                anchors,
            )
        }
    }

    /// Row-direction flex layout: §9.2 base sizes → §9.3 lines → §9.7
    /// flexing → §9.4 cross sizing → §9.5/§9.6 alignment.
    #[allow(clippy::too_many_arguments)]
    fn flex_row<'t>(
        &self,
        fs: &super::flex::FlexStyle,
        items: &[&'t BoxNode],
        gap_main: f32,
        gap_cross: f32,
        content_x: f32,
        content_top: f32,
        content_w: f32,
        def_ch: Option<f32>,
        cross_clamp: (f32, f32),
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag<'t>>, f32) {
        // ---- §9.2: flex base size and hypothetical main size ----
        let mut fi: Vec<FItem> = Vec::with_capacity(items.len());
        let mut calcs: Vec<FlexCalc> = Vec::with_capacity(items.len());
        for it in items {
            let s = &it.style;
            let (m, auto) = self.margins_of(s, content_w);
            let bp_l = s.border[LEFT] + self.pad(s, LEFT, content_w);
            let bp_r = s.border[RIGHT] + self.pad(s, RIGHT, content_w);
            let bp_t = s.border[TOP] + self.pad(s, TOP, content_w);
            let bp_b = s.border[BOTTOM] + self.pad(s, BOTTOM, content_w);
            let bp_main = bp_l + bp_r;
            let (grow, shrink, basis) = if it.node == NO_NODE {
                (0.0, 1.0, Len::Auto)
            } else {
                item_flex(
                    self.dom,
                    it.node,
                    crate::layout::Units::of(self.dom, it.node),
                    self.vp,
                )
            };
            // Content-box normalization WITHOUT the zero floor — §9.2.3's
            // own note (border-box basis 0 with padding ⇒ negative inner
            // base, corrected by the §9.7 clamp).
            let to_content = |v: f32| if s.border_box { v - bp_main } else { v };
            let width_def = s.width.resolve(Some(content_w)).map(to_content);
            let base = match &basis {
                Len::Auto => width_def,
                Len::MinContent => Some(self.intrinsic_w(it, IMode::Min, inl)),
                Len::MaxContent | Len::FitContent => None,
                l => l.resolve(Some(content_w)).map(to_content),
            }
            .unwrap_or_else(|| self.intrinsic_w(it, IMode::Max, inl));
            let max_main = match &s.max_width {
                Len::None => f32::INFINITY,
                l => l
                    .resolve(Some(content_w))
                    .map(to_content)
                    .map(|v| v.max(0.0))
                    .unwrap_or(f32::INFINITY),
            };
            // §4.5 automatic minimum: content-based for non-scroll-
            // containers (min-content, capped by the specified size
            // suggestion and a definite max), zero for scroll containers.
            let min_main = match &s.min_width {
                Len::Auto => {
                    if self.scroll_container(it.node) {
                        0.0
                    } else {
                        let mut v = self.intrinsic_w(it, IMode::Min, inl);
                        if let Some(sp) = width_def {
                            v = v.min(sp.max(0.0));
                        }
                        v.min(max_main)
                    }
                }
                l => l
                    .resolve(Some(content_w))
                    .map(to_content)
                    .map(|v| v.max(0.0))
                    .unwrap_or(0.0),
            };
            // Cross-axis (height) resolution for stretch/def-h bookkeeping.
            let bp_v = bp_t + bp_b;
            let to_content_v = |v: f32| {
                if s.border_box {
                    (v - bp_v).max(0.0)
                } else {
                    v.max(0.0)
                }
            };
            let hmin = s
                .min_height
                .resolve(def_ch)
                .map(to_content_v)
                .unwrap_or(0.0);
            let hmax = match &s.max_height {
                Len::None => f32::INFINITY,
                l => l.resolve(def_ch).map(to_content_v).unwrap_or(f32::INFINITY),
            }
            .max(hmin);
            let def_h = s
                .height
                .resolve(def_ch)
                .map(to_content_v)
                .map(|v| v.clamp(hmin, hmax));
            let align = self.item_align(it.node, fs.align_items);
            calcs.push(FlexCalc::new(
                base,
                min_main,
                max_main.max(min_main),
                grow,
                shrink,
                bp_main + m[LEFT] + m[RIGHT],
            ));
            fi.push(FItem {
                b: it,
                m,
                auto,
                bp_main,
                align,
                def_h,
                cross_auto: matches!(s.height, Len::Auto),
                frag: None,
                anchors: Vec::new(),
                border_x: 0.0,
                border_y: 0.0,
            });
        }

        // ---- §9.3 lines, §9.7 per line, item layout, line cross sizes ----
        let outer: Vec<f32> = calcs.iter().map(FlexCalc::outer_hypo).collect();
        let lines = collect_lines(&outer, fs.wrap, content_w, gap_main);
        let mut line_cross: Vec<f32> = Vec::with_capacity(lines.len());
        for r in &lines {
            let n = r.len();
            let inner = content_w - gap_main * n.saturating_sub(1) as f32;
            resolve_flexible_lengths(inner, &mut calcs[r.clone()]);
            let mut cross = 0.0f32;
            for i in r.clone() {
                let used = calcs[i].target.max(0.0);
                let (frag, anc) = self.item_frag(fi[i].b, used, content_w, fi[i].def_h, inl);
                cross = cross.max(frag.h + fi[i].m[TOP] + fi[i].m[BOTTOM]);
                fi[i].frag = Some(frag);
                fi[i].anchors = anc;
            }
            line_cross.push(cross);
        }
        // §9.4: single-line + definite container cross → the line IS the
        // container's inner cross, clamped by the container's min/max.
        if lines.len() == 1 {
            if let Some(d) = def_ch {
                line_cross[0] = d;
            }
            line_cross[0] = line_cross[0].clamp(cross_clamp.0, cross_clamp.1.max(cross_clamp.0));
        } else if let Some(d) = def_ch
            && fs.align_content == AlignContent::Stretch
        {
            let total: f32 = line_cross.iter().sum::<f32>() + gap_cross * (lines.len() - 1) as f32;
            if d > total {
                let extra = (d - total) / lines.len() as f32;
                for c in &mut line_cross {
                    *c += extra;
                }
            }
        }

        // ---- §9.5 main-axis: auto margins, then justify-content ----
        for r in &lines {
            let n = r.len();
            let gaps = gap_main * n.saturating_sub(1) as f32;
            let used_sum: f32 = r
                .clone()
                .map(|i| calcs[i].target.max(0.0) + calcs[i].mbp)
                .sum();
            let mut free = content_w - gaps - used_sum;
            let autos: usize = r
                .clone()
                .map(|i| usize::from(fi[i].auto[LEFT]) + usize::from(fi[i].auto[RIGHT]))
                .sum();
            let share = if free > 0.0 && autos > 0 {
                let s = free / autos as f32;
                free = 0.0;
                s
            } else {
                0.0
            };
            let (lead, between) = justify_offsets(fs.justify, free, n);
            let mut run = lead;
            for (k, i) in r.clone().enumerate() {
                let ml = fi[i].m[LEFT] + if fi[i].auto[LEFT] { share } else { 0.0 };
                let mr = fi[i].m[RIGHT] + if fi[i].auto[RIGHT] { share } else { 0.0 };
                let outer_full = calcs[i].target.max(0.0) + fi[i].bp_main + ml + mr;
                let margin_x = run;
                run += outer_full;
                if k + 1 < n {
                    run += between + gap_main;
                }
                fi[i].border_x = if fs.reverse {
                    content_w - (margin_x + outer_full) + mr
                } else {
                    margin_x + ml
                };
            }
        }

        // ---- cross stacking (+ align-content), §9.6 per-item alignment ----
        let total_cross: f32 =
            line_cross.iter().sum::<f32>() + gap_cross * line_cross.len().saturating_sub(1) as f32;
        let (lead_c, between_c) = match def_ch {
            Some(d) => align_content_offsets(fs.align_content, d - total_cross, lines.len()),
            None => (0.0, 0.0),
        };
        let order: Vec<usize> = if fs.wrap_reverse {
            (0..lines.len()).rev().collect()
        } else {
            (0..lines.len()).collect()
        };
        let mut top = lead_c;
        let mut frags: Vec<Frag<'t>> = Vec::with_capacity(fi.len());
        for &li in &order {
            let cross = line_cross[li];
            for i in lines[li].clone() {
                let it = &mut fi[i];
                let frag = it.frag.as_mut().expect("laid above");
                // §9.4 step 11: stretch grows the item's box to the line
                // (cross-size auto, no auto cross margins), clamped by its
                // own min/max cross sizes.
                if it.align == AlignItem::Stretch
                    && it.cross_auto
                    && !it.auto[TOP]
                    && !it.auto[BOTTOM]
                {
                    frag.h = frag.h.max(cross - it.m[TOP] - it.m[BOTTOM]).max(0.0);
                }
                let extra = cross - (frag.h + it.m[TOP] + it.m[BOTTOM]);
                let shift = cross_shift(extra, it.auto[TOP], it.auto[BOTTOM], it.align);
                it.border_y = top + shift + it.m[TOP];
            }
            top += cross + between_c + gap_cross;
        }
        for it in &mut fi {
            let mut frag = it.frag.take().expect("laid above");
            // §9.4.3 relative offset + transform translation — a flex item's
            // containing block is the container's content box.
            let (rx, ry) = self.paint_offset(&it.b.style, content_w, def_ch, frag.w, frag.h);
            let dx = content_x + it.border_x + rx;
            let dy = content_top + it.border_y + ry;
            Self::offset_frag(&mut frag, dx, dy);
            for &(n, y) in &it.anchors {
                anchors.push((n, y + dy));
            }
            frags.push(frag);
        }
        (frags, total_cross.max(0.0))
    }

    /// Column-direction flex layout. The cross axis (width) is always
    /// definite, so cross sizing runs FIRST (stretch/fit-content per §9.4's
    /// hypothetical cross size), items lay at those widths, and their laid
    /// content heights feed the §9.2 base sizes; a container with an auto
    /// main size takes each line at its content sum (§9.2's "automatic
    /// block size ... is its max-content size" — no free space to flex).
    #[allow(clippy::too_many_arguments)]
    fn flex_col<'t>(
        &self,
        fs: &super::flex::FlexStyle,
        items: &[&'t BoxNode],
        gap_main: f32,
        gap_cross: f32,
        content_x: f32,
        content_top: f32,
        content_w: f32,
        def_ch: Option<f32>,
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag<'t>>, f32) {
        let mut fi: Vec<FItem> = Vec::with_capacity(items.len());
        let mut calcs: Vec<FlexCalc> = Vec::with_capacity(items.len());
        for it in items {
            let s = &it.style;
            let (m, auto) = self.margins_of(s, content_w);
            let bp_l = s.border[LEFT] + self.pad(s, LEFT, content_w);
            let bp_r = s.border[RIGHT] + self.pad(s, RIGHT, content_w);
            let bp_t = s.border[TOP] + self.pad(s, TOP, content_w);
            let bp_b = s.border[BOTTOM] + self.pad(s, BOTTOM, content_w);
            let bp_cross = bp_l + bp_r;
            let bp_main = bp_t + bp_b;
            let (grow, shrink, basis) = if it.node == NO_NODE {
                (0.0, 1.0, Len::Auto)
            } else {
                item_flex(
                    self.dom,
                    it.node,
                    crate::layout::Units::of(self.dom, it.node),
                    self.vp,
                )
            };
            let align = self.item_align(it.node, fs.align_items);
            // Cross size (width): definite → that; stretch-eligible → fill;
            // else fit-content (§9.4.1 "treating auto as fit-content").
            let to_content_h = |v: f32| {
                if s.border_box {
                    (v - bp_cross).max(0.0)
                } else {
                    v.max(0.0)
                }
            };
            let wmin = s
                .min_width
                .resolve(Some(content_w))
                .map(to_content_h)
                .unwrap_or(0.0);
            let wmax = match &s.max_width {
                Len::None => f32::INFINITY,
                l => l
                    .resolve(Some(content_w))
                    .map(to_content_h)
                    .unwrap_or(f32::INFINITY),
            }
            .max(wmin);
            let width_def = s.width.resolve(Some(content_w)).map(to_content_h);
            let avail_c = content_w - m[LEFT] - m[RIGHT] - bp_cross;
            let cross_auto = matches!(s.width, Len::Auto);
            let w = match width_def {
                Some(v) => v,
                None if align == AlignItem::Stretch
                    && cross_auto
                    && !auto[LEFT]
                    && !auto[RIGHT] =>
                {
                    avail_c
                }
                None => {
                    // fit-content = min(max-content, max(min-content, avail))
                    let minc = self.intrinsic_w(it, IMode::Min, inl);
                    let maxc = self.intrinsic_w(it, IMode::Max, inl).max(minc);
                    avail_c.max(minc).min(maxc)
                }
            }
            .clamp(wmin, wmax)
            .max(0.0);
            // The specified-size suggestion / % basis for children.
            let to_content_v = |v: f32| {
                if s.border_box { v - bp_main } else { v }
            };
            let def_h = s.height.resolve(def_ch).map(to_content_v);
            let (frag, anc) = self.item_frag(it, w, content_w, def_h.map(|v| v.max(0.0)), inl);
            let natural_main = (frag.h - bp_main).max(0.0);
            let base = match &basis {
                Len::Auto => def_h,
                Len::MinContent | Len::MaxContent | Len::FitContent => None,
                l => l.resolve(def_ch).map(to_content_v),
            }
            .unwrap_or(natural_main);
            let max_main = match &s.max_height {
                Len::None => f32::INFINITY,
                l => l
                    .resolve(def_ch)
                    .map(|v| to_content_v(v).max(0.0))
                    .unwrap_or(f32::INFINITY),
            };
            // §4.5 in the block axis: the content-based minimum is the laid
            // content height (exact in a cell model — text cannot compress).
            let min_main = match &s.min_height {
                Len::Auto => {
                    if self.scroll_container(it.node) {
                        0.0
                    } else {
                        let mut v = natural_main;
                        if let Some(sp) = def_h {
                            v = v.min(sp.max(0.0));
                        }
                        v.min(max_main)
                    }
                }
                l => l
                    .resolve(def_ch)
                    .map(|v| to_content_v(v).max(0.0))
                    .unwrap_or(0.0),
            };
            calcs.push(FlexCalc::new(
                base,
                min_main,
                max_main.max(min_main),
                grow,
                shrink,
                bp_main + m[TOP] + m[BOTTOM],
            ));
            fi.push(FItem {
                b: it,
                m,
                auto,
                bp_main,
                align,
                def_h,
                cross_auto,
                frag: Some(frag),
                anchors: anc,
                border_x: 0.0,
                border_y: 0.0,
            });
        }

        // ---- lines (columns) ----
        let outer: Vec<f32> = calcs.iter().map(FlexCalc::outer_hypo).collect();
        let lines = collect_lines(
            &outer,
            fs.wrap && def_ch.is_some(),
            def_ch.unwrap_or(f32::INFINITY),
            gap_main,
        );
        // ---- §9.7 per column + used box heights ----
        let mut line_main: Vec<f32> = Vec::with_capacity(lines.len());
        let mut line_cross: Vec<f32> = Vec::with_capacity(lines.len());
        for r in &lines {
            let n = r.len();
            let gaps = gap_main * n.saturating_sub(1) as f32;
            let sum_hypo: f32 = r.clone().map(|i| outer[i]).sum();
            let inner = def_ch.unwrap_or(sum_hypo + gaps) - gaps;
            resolve_flexible_lengths(inner, &mut calcs[r.clone()]);
            let mut cross = 0.0f32;
            for i in r.clone() {
                let used = calcs[i].target.max(0.0);
                let bp_main = fi[i].bp_main;
                let (ml, mr) = (fi[i].m[LEFT], fi[i].m[RIGHT]);
                let frag = fi[i].frag.as_mut().expect("laid above");
                frag.h = bp_main + used;
                cross = cross.max(frag.w + ml + mr);
            }
            line_main.push(inner + gaps);
            line_cross.push(cross);
        }

        // ---- §9.5 main (vertical): auto margins then justify ----
        for (li, r) in lines.iter().enumerate() {
            let n = r.len();
            let gaps = gap_main * n.saturating_sub(1) as f32;
            let used_sum: f32 = r
                .clone()
                .map(|i| calcs[i].target.max(0.0) + calcs[i].mbp)
                .sum();
            let main_size = line_main[li];
            let mut free = main_size - gaps - used_sum;
            let autos: usize = r
                .clone()
                .map(|i| usize::from(fi[i].auto[TOP]) + usize::from(fi[i].auto[BOTTOM]))
                .sum();
            let share = if free > 0.0 && autos > 0 {
                let s = free / autos as f32;
                free = 0.0;
                s
            } else {
                0.0
            };
            let (lead, between) = justify_offsets(fs.justify, free, n);
            let mut run = lead;
            for (k, i) in r.clone().enumerate() {
                let mt = fi[i].m[TOP] + if fi[i].auto[TOP] { share } else { 0.0 };
                let mb = fi[i].m[BOTTOM] + if fi[i].auto[BOTTOM] { share } else { 0.0 };
                let outer_full = calcs[i].target.max(0.0) + fi[i].bp_main + mt + mb;
                let margin_y = run;
                run += outer_full;
                if k + 1 < n {
                    run += between + gap_main;
                }
                fi[i].border_y = if fs.reverse {
                    main_size - (margin_y + outer_full) + mb
                } else {
                    margin_y + mt
                };
            }
        }

        // §9.4 step 8: a column container's cross size (its width) is always
        // definite — a single line IS the container's inner cross size, and
        // align-content:stretch grows multiple lines to fill it.
        if line_cross.len() == 1 {
            line_cross[0] = content_w;
        } else if fs.align_content == AlignContent::Stretch {
            let total: f32 =
                line_cross.iter().sum::<f32>() + gap_cross * (line_cross.len() - 1) as f32;
            if content_w > total {
                let extra = (content_w - total) / line_cross.len() as f32;
                for c in &mut line_cross {
                    *c += extra;
                }
            }
        }

        // ---- columns across (cross axis is the definite content width) ----
        let total_cross: f32 =
            line_cross.iter().sum::<f32>() + gap_cross * line_cross.len().saturating_sub(1) as f32;
        let (lead_c, between_c) =
            align_content_offsets(fs.align_content, content_w - total_cross, lines.len());
        let order: Vec<usize> = if fs.wrap_reverse {
            (0..lines.len()).rev().collect()
        } else {
            (0..lines.len()).collect()
        };
        let mut left = lead_c;
        for &li in &order {
            let cross = line_cross[li];
            for i in lines[li].clone() {
                let it = &mut fi[i];
                let frag = it.frag.as_mut().expect("laid above");
                let extra = cross - (frag.w + it.m[LEFT] + it.m[RIGHT]);
                let shift = cross_shift(extra, it.auto[LEFT], it.auto[RIGHT], it.align);
                it.border_x = left + shift + it.m[LEFT];
            }
            left += cross + between_c + gap_cross;
        }
        let mut frags: Vec<Frag<'t>> = Vec::with_capacity(fi.len());
        for it in &mut fi {
            let mut frag = it.frag.take().expect("laid above");
            let (rx, ry) = self.paint_offset(&it.b.style, content_w, def_ch, frag.w, frag.h);
            let dx = content_x + it.border_x + rx;
            let dy = content_top + it.border_y + ry;
            Self::offset_frag(&mut frag, dx, dy);
            for &(n, y) in &it.anchors {
                anchors.push((n, y + dy));
            }
            frags.push(frag);
        }
        let content_h = line_main.iter().copied().fold(0.0f32, f32::max);
        (frags, content_h.max(0.0))
    }

    /// Lay a flex item at its IMPOSED used content width. A flex item
    /// establishes an independent formatting context (§4): child margins
    /// stay inside (no collapsing across the boundary), and the item's OWN
    /// margins belong to the flex algorithm, not this fragment. Returns the
    /// fragment at (0,0) border-box origin plus its local anchor marks —
    /// the caller translates both.
    pub(super) fn item_frag<'t>(
        &self,
        b: &'t BoxNode,
        content_w: f32,
        pct_basis: f32,
        def_h: Option<f32>,
        parent_inl: &InlineStyle,
    ) -> (Frag<'t>, Vec<(NodeId, f32)>) {
        let s = &b.style;
        let inl = if b.node == NO_NODE {
            parent_inl.clone()
        } else {
            InlineStyle::derive(self.dom, b.node, parent_inl, self.base)
        };
        let style_node = if b.node == NO_NODE { inl.node } else { b.node };
        let bp_l = s.border[LEFT] + self.pad(s, LEFT, pct_basis);
        let bp_r = s.border[RIGHT] + self.pad(s, RIGHT, pct_basis);
        let bt = s.border[TOP] + self.pad(s, TOP, pct_basis);
        let bb = s.border[BOTTOM] + self.pad(s, BOTTOM, pct_basis);
        let mut cur = Cursor {
            y: bt,
            ..Default::default()
        };
        let mut children: Vec<Frag<'t>> = Vec::new();
        match &b.content {
            Content::Blocks(kids) => {
                for k in kids {
                    children.push(self.block(k, bp_l, content_w, def_h, &mut cur, &inl));
                }
            }
            Content::Inlines(inls) => {
                let mut ifc = Ifc::new(
                    self.dom,
                    self.base,
                    self.images,
                    self.forms,
                    self.vp,
                    self.cell_w,
                    self.cell_h,
                    content_w,
                    def_h,
                    block_align(self.dom, style_node),
                    self.indent_px(style_node, content_w),
                );
                if b.marker_inside
                    && let Some(mk) = &b.marker
                {
                    let mut mctx = inl.clone();
                    mctx.kind = crate::layout::ItemKind::Text;
                    ifc.text(mk, &mctx);
                }
                ifc.run(inls, &inl);
                let (lines, marks, oofs) = ifc.finish();
                let n = lines.len();
                if n > 0 {
                    self.emit_lines(lines, bp_l, &mut cur, &mut children);
                }
                let first = children.len() - n;
                let end_y = cur.y;
                let line_y = |idx: usize, children: &[Frag<'_>]| {
                    if n > 0 && idx < n {
                        children[first + idx].y
                    } else {
                        end_y
                    }
                };
                for (node, i2) in marks {
                    cur.anchors.push((node, line_y(i2, &children)));
                }
                for m in oofs {
                    let y = line_y(m.line, &children);
                    children.push(oof_placeholder(m, bp_l, y));
                }
            }
            Content::Atomic(atom) => match &atom.kind {
                AtomKind::Img { url, alt: _ } => {
                    // The imposed width IS the replaced item's used main
                    // size; the cross comes from its definite height, else
                    // through the natural ratio (§9.4 replaced hypothetical
                    // cross), else the natural/ratio-less fallbacks.
                    let line = self.img_line_at(
                        atom.node,
                        url.as_deref(),
                        content_w,
                        def_h,
                        &inl,
                        bp_l,
                        bt,
                    );
                    cur.y = bt + line.h;
                    children.push(line);
                }
                _ => {
                    let mut ifc = Ifc::new(
                        self.dom,
                        self.base,
                        self.images,
                        self.forms,
                        self.vp,
                        self.cell_w,
                        self.cell_h,
                        content_w,
                        def_h,
                        super::style::Align2::Left,
                        0.0,
                    );
                    ifc.atom(atom, &inl);
                    let (lines, _, _) = ifc.finish();
                    if !lines.is_empty() {
                        self.emit_lines(lines, bp_l, &mut cur, &mut children);
                    }
                }
            },
            Content::Flex(nested) => {
                if !nested.is_empty() {
                    let (frags, fh) = self.flex_content(
                        b,
                        nested,
                        bp_l,
                        bt,
                        content_w,
                        def_h,
                        (0.0, f32::INFINITY),
                        &inl,
                        &mut cur.anchors,
                    );
                    children.extend(frags);
                    cur.y = bt + fh;
                }
                self.container_oof(b, &inl, bp_l, bt, &mut children);
            }
            Content::Grid(nested) => {
                if !nested.is_empty() {
                    let (frags, gh) = self.grid_content(
                        b,
                        nested,
                        bp_l,
                        bt,
                        content_w,
                        def_h,
                        &inl,
                        &mut cur.anchors,
                    );
                    children.extend(frags);
                    cur.y = bt + gh;
                }
                self.container_oof(b, &inl, bp_l, bt, &mut children);
            }
            Content::Table(tb) => {
                // A table as a flex/grid item: the imposed content width is the
                // item's used width; a width:auto table still shrinks within it.
                // Any declared width (CSS or the HTML attr) means "fill".
                let width_auto = s.width.resolve(Some(pct_basis)).is_none()
                    && super::tree::declared_track_width(self.dom, b.node).is_none();
                let cols = self.table_columns(tb, b.node, content_w, width_auto, &inl);
                let lead = if width_auto {
                    self.table_lead(b.node, cols.table_w, content_w)
                } else {
                    0.0
                };
                let cap_x = bp_l + lead;
                let cap_w = cols.table_w.max(1.0);
                for cap in &tb.top_captions {
                    children.push(self.block(cap, cap_x, cap_w, def_h, &mut cur, &inl));
                }
                let grid_top = cur.flush();
                let (frags, gh) = self.table_grid(
                    tb,
                    b.node,
                    &cols,
                    cap_x,
                    grid_top,
                    def_h,
                    &inl,
                    &mut cur.anchors,
                );
                children.extend(frags);
                cur.y = grid_top + gh;
                for cap in &tb.bottom_captions {
                    children.push(self.block(cap, cap_x, cap_w, def_h, &mut cur, &inl));
                }
            }
        }
        let mut content_h = (cur.flush() - bt).max(0.0);
        if let Some(hd) = def_h {
            content_h = hd;
        }
        let anchors = std::mem::take(&mut cur.anchors);
        (
            Frag {
                node: b.node,
                x: 0.0,
                y: 0.0,
                w: bp_l + content_w + bp_r,
                h: bt + content_h + bb,
                border: s.border,
                // `item = true`: only flex/grid items and out-of-flow boxes
                // lay through here, and for the (always-positioned)
                // out-of-flow ones the item bit can't change the result.
                paint: paint_flags(s, true),
                clip: None,
                kind: FragKind::Block,
                children,
            },
            anchors,
        )
    }

    /// A replaced flex item's single line at an IMPOSED content width:
    /// height from its definite height, else the natural ratio, else the
    /// natural height, else the spec's ratio-less 2:1/150px cap; object-fit
    /// maps the pixels into that box.
    #[allow(clippy::too_many_arguments)]
    fn img_line_at<'t>(
        &self,
        node: NodeId,
        url: Option<&str>,
        content_w: f32,
        def_h: Option<f32>,
        inl: &InlineStyle,
        x: f32,
        y: f32,
    ) -> Frag<'t> {
        let natural = url
            .and_then(|u| self.images.get(u))
            .filter(|&&(w, h)| w > 0 && h > 0)
            .map(|&(w, h)| (f32::from(w) * self.cell_w, f32::from(h) * self.cell_h));
        let ratio = super::replaced::ratio_of(self.dom, node, natural);
        let box_h = def_h
            .or_else(|| ratio.map(|r| content_w / r))
            .or(natural.map(|(_, nh)| nh))
            .unwrap_or_else(|| (content_w / 2.0).min(150.0));
        let r =
            super::replaced::apply_fit(self.dom, node, natural, content_w.max(1.0), box_h.max(1.0));
        let box_w_c = ((r.box_w / self.cell_w).round().max(1.0) as usize).max(1);
        let box_rows = (r.box_h / self.cell_h).round().max(1.0) as u16;
        let paint_w = ((r.paint_w / self.cell_w).round().max(1.0) as u16).min(box_w_c as u16);
        let paint_rows = ((r.paint_h / self.cell_h).round().max(1.0) as u16).min(box_rows);
        let off_c =
            ((r.off_x / self.cell_w).round().max(0.0) as usize).min(box_w_c - paint_w as usize);
        let off_r = ((r.off_y / self.cell_h).round().max(0.0) as u16).min(box_rows - paint_rows);
        let pixelated = matches!(
            self.dom.computed_value(node, "image-rendering").as_deref(),
            Some("pixelated" | "crisp-edges" | "-moz-crisp-edges" | "-webkit-optimize-contrast")
        );
        let item = Item {
            col: 0,
            width: paint_w,
            height: paint_rows,
            text: String::new(),
            kind: crate::layout::ItemKind::Image,
            image: natural
                .is_some()
                .then(|| url.unwrap_or_default().to_string()),
            emph: crate::layout::Emphasis::default(),
            node,
            link: inl.link.clone(),
            crop: r.crop,
            pixelated,
            invisible: inl.invisible,
        };
        Frag {
            node: NO_NODE,
            x,
            y,
            w: r.box_w,
            h: f32::from(box_rows) * self.cell_h,
            border: [0.0; 4],
            paint: PaintFlags::default(),
            clip: None,
            kind: FragKind::Line(vec![Piece::boxed(item, box_rows, off_c, off_r)]),
            children: Vec::new(),
        }
    }

    pub(super) fn margins_of(&self, s: &BoxStyle, basis: f32) -> ([f32; 4], [bool; 4]) {
        let mut m = [0.0f32; 4];
        let mut auto = [false; 4];
        for i in 0..4 {
            auto[i] = s.margin[i].is_auto();
            m[i] = s.margin[i].resolve(Some(basis)).unwrap_or(0.0);
        }
        (m, auto)
    }

    fn item_align(&self, node: NodeId, container: AlignItem) -> AlignItem {
        if node == NO_NODE {
            return container;
        }
        align_item_from(
            self.dom
                .computed_value(node, "align-self")
                .as_deref()
                .unwrap_or(""),
            container,
        )
    }

    pub(super) fn order_of(&self, node: NodeId) -> i32 {
        if node == NO_NODE {
            return 0;
        }
        self.dom
            .computed_value(node, "order")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
    }

    /// §4.5: a scroll container's automatic minimum size is zero.
    fn scroll_container(&self, node: NodeId) -> bool {
        if node == NO_NODE {
            return false;
        }
        for prop in ["overflow-x", "overflow-y", "overflow"] {
            if matches!(
                self.dom.computed_value(node, prop).as_deref(),
                Some("hidden" | "auto" | "scroll" | "clip" | "overlay")
            ) {
                return true;
            }
        }
        false
    }

    pub(super) fn offset_frag(f: &mut Frag<'_>, dx: f32, dy: f32) {
        f.x += dx;
        f.y += dy;
        for c in &mut f.children {
            Self::offset_frag(c, dx, dy);
        }
    }

    /// The positioned post-pass: walk the finished fragment tree, lay every
    /// out-of-flow placeholder against its containing block's FINAL padding
    /// box (§10.1), and splice the laid fragment back at the placeholder's
    /// tree position (which is its Appendix E paint-order slot). Fixed boxes
    /// with no transformed ancestor peel off into `fixed_out` — the pinned
    /// layer (§9.6.1: their containing block is the viewport); under a
    /// transformed ancestor that ancestor IS their containing block
    /// (css-transforms-1 §3) and they stay in the document.
    ///
    /// This pass ALSO computes each fragment's clip rectangle: `own_clip` is
    /// the clip applying to a fragment's own painted cells (from its
    /// containing-block chain), and it threads a separate abspos/fixed clip so
    /// a positioned box picks up its containing block's clip — the CB chain,
    /// not the static-position tree parent, is what CSS Overflow L3 §3 clips a
    /// positioned box by, and this walk already tracks it.
    ///
    /// Which axes of `id` are a PURE CLIP — `overflow: hidden|clip`, the
    /// non-scrolling values that clip content to the padding box with no
    /// scroll (CSS Overflow L3 §2). The scrolling values (`auto`/`scroll`) are
    /// NOT clipped in place: their overflow rides the scroll axis into a buffer
    /// (a vertical Region) or strip (a horizontal Carousel), handled by the
    /// paint-time scroller extraction — so only `hidden`/`clip` land here.
    /// Mirrors the old single overflow authority: the `overflow-x`/`-y`
    /// longhand wins, else the `overflow` shorthand (one value = both axes).
    fn overflow_clips(&self, id: NodeId) -> (bool, bool) {
        if id == NO_NODE {
            return (false, false);
        }
        let clips =
            |v: Option<String>| matches!(v.as_deref().map(str::trim), Some("hidden" | "clip"));
        let (sx, sy) = match self.dom.computed_value(id, "overflow") {
            Some(sh) => {
                let mut t = sh.split_whitespace();
                let x = t.next().map(str::to_string);
                let y = t.next().map(str::to_string).or_else(|| x.clone());
                (x, y)
            }
            None => (None, None),
        };
        let ox = self.dom.computed_value(id, "overflow-x").or(sx);
        let oy = self.dom.computed_value(id, "overflow-y").or(sy);
        (clips(ox), clips(oy))
    }

    /// The clip rectangle a fragment ESTABLISHES for its descendants: its
    /// padding box on each clipped axis (CSS Overflow L3 §2 — the scrollport
    /// is the padding box), ±∞ on an unclipped axis. `None` when it clips
    /// neither axis.
    fn clip_box(&self, f: &Frag<'_>) -> Option<Clip> {
        // Anonymous/line frags never clip. (Guarded first: `tag_name` indexes
        // the arena, so it must not see `NO_NODE`.)
        if f.node == NO_NODE {
            return None;
        }
        // The ROOT element's overflow propagates to the VIEWPORT (CSS Overflow
        // L3 §3.1) — it never clips the document to a sub-box. paint applies the
        // viewport clip (columns + document height) and page scroll handles the
        // rest (a locked `html`/`body` overflow delegates to the principal
        // scroller, which flows into the document).
        if matches!(self.dom.tag_name(f.node), Some("html" | "body")) {
            return None;
        }
        let (cx, cy) = self.overflow_clips(f.node);
        if !cx && !cy {
            return None;
        }
        let x0 = f.x + f.border[LEFT];
        let y0 = f.y + f.border[TOP];
        let x1 = (f.x + f.w - f.border[RIGHT]).max(x0);
        let y1 = (f.y + f.h - f.border[BOTTOM]).max(y0);
        Some(Clip {
            x0: if cx { x0 } else { f32::NEG_INFINITY },
            x1: if cx { x1 } else { f32::INFINITY },
            y0: if cy { y0 } else { f32::NEG_INFINITY },
            y1: if cy { y1 } else { f32::INFINITY },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_oof<'t>(
        &self,
        f: &mut Frag<'t>,
        abs_cb: CbRect,
        fixed_cb: Option<CbRect>,
        icb: CbRect,
        anchors: &mut Vec<(NodeId, f32)>,
        fixed_out: &mut Vec<Frag<'t>>,
        own_clip: Option<Clip>,
        abs_clip: Option<Clip>,
        fixed_clip: Option<Clip>,
    ) {
        // This fragment's own painted cells are clipped by its containing-block
        // chain (`own_clip`); its in-flow descendants — and the abspos/fixed
        // descendants for which it is the containing block — are additionally
        // clipped by the padding box it establishes when it clips overflow.
        f.clip = own_clip;
        let content_clip = Clip::intersect(own_clip, self.clip_box(f));
        let child_abs_clip = if f.paint.cb_abs {
            content_clip
        } else {
            abs_clip
        };
        let child_fixed_clip = if f.paint.cb_fixed {
            content_clip
        } else {
            fixed_clip
        };
        let pad = CbRect {
            x: f.x + f.border[LEFT],
            y: f.y + f.border[TOP],
            w: (f.w - f.border[LEFT] - f.border[RIGHT]).max(0.0),
            h: (f.h - f.border[TOP] - f.border[BOTTOM]).max(0.0),
        };
        let child_abs = if f.paint.cb_abs { pad } else { abs_cb };
        let child_fixed = if f.paint.cb_fixed {
            Some(pad)
        } else {
            fixed_cb
        };
        let mut i = 0;
        while i < f.children.len() {
            let child_own_clip = if matches!(f.children[i].kind, FragKind::Oof(..)) {
                let ph = f.children.remove(i);
                let (x0, y0) = (ph.x, ph.y);
                let FragKind::Oof(b, ctx) = ph.kind else {
                    unreachable!()
                };
                // A paint-suppressed (opacity:0 chain) out-of-flow box
                // contributes NOTHING — no cells, no scrollable extent (the
                // Steam-microtrailer decision, now structural).
                if ctx.opacity_suppressed() || self.dom.paint_suppressed(b.node) {
                    continue;
                }
                let fixed = b.style.position == Pos::Fixed;
                let pinned = fixed && child_fixed.is_none();
                let cb = if fixed {
                    child_fixed.unwrap_or(icb)
                } else {
                    child_abs
                };
                let (laid, anc) = self.lay_oof(b, cb, (x0 - cb.x, y0 - cb.y), &ctx);
                if pinned {
                    // Pinned-layer content scrolls with the viewport, not the
                    // document — its anchors can't be doc scroll targets. It is
                    // clipped only by the viewport, so it resolves with no
                    // ambient clip in `layout`'s separate pinned-layer pass.
                    fixed_out.push(laid);
                    continue;
                }
                anchors.extend(anc);
                f.children.insert(i, laid);
                // A positioned box is clipped by its containing block's clip
                // chain (§3) — threaded down as the abspos/fixed clip.
                if fixed {
                    child_fixed_clip
                } else {
                    child_abs_clip
                }
            } else {
                // An in-flow child is clipped by this box's content clip.
                content_clip
            };
            self.resolve_oof(
                &mut f.children[i],
                child_abs,
                child_fixed,
                icb,
                anchors,
                fixed_out,
                child_own_clip,
                child_abs_clip,
                child_fixed_clip,
            );
            i += 1;
        }
    }

    /// Lay one absolutely positioned box against its containing block:
    /// §10.3.7 (widths; §10.3.8 replaced), §10.6.4 (heights; §10.6.5
    /// replaced), §10.4/§10.7 min/max re-solving, ltr. `stat` is the static
    /// position relative to the CB's padding-box origin. Returns the laid
    /// fragment in ABSOLUTE coordinates plus its (already offset) anchors.
    fn lay_oof<'t>(
        &self,
        b: &'t BoxNode,
        cb: CbRect,
        stat: (f32, f32),
        ctx: &InlineStyle,
    ) -> (Frag<'t>, Vec<(NodeId, f32)>) {
        let s = &b.style;
        let (m, mauto) = self.margins_of(s, cb.w);
        let bp_l = s.border[LEFT] + self.pad(s, LEFT, cb.w);
        let bp_r = s.border[RIGHT] + self.pad(s, RIGHT, cb.w);
        let bp_t = s.border[TOP] + self.pad(s, TOP, cb.w);
        let bp_b = s.border[BOTTOM] + self.pad(s, BOTTOM, cb.w);
        let bp_h = bp_l + bp_r;
        let bp_v = bp_t + bp_b;
        let left = s.inset[LEFT].resolve(Some(cb.w));
        let right = s.inset[RIGHT].resolve(Some(cb.w));
        let top = s.inset[TOP].resolve(Some(cb.h));
        let bottom = s.inset[BOTTOM].resolve(Some(cb.h));
        // A replaced box's used size comes from the replaced sizing
        // algorithm (§10.3.8/§10.6.5) — its own §10.4 ratio table included —
        // before any constraint solving.
        let replaced = match &b.content {
            Content::Atomic(atom) => match &atom.kind {
                AtomKind::Img { url, .. } => {
                    let natural = url
                        .as_deref()
                        .and_then(|u| self.images.get(u))
                        .filter(|&&(w, h)| w > 0 && h > 0)
                        .map(|&(w, h)| (f32::from(w) * self.cell_w, f32::from(h) * self.cell_h));
                    super::replaced::size(
                        self.dom,
                        atom.node,
                        natural,
                        Some(cb.w),
                        Some(cb.h),
                        self.vp,
                    )
                    .map(|r| (r.box_w, r.box_h))
                }
                _ => None,
            },
            _ => None,
        };
        let spec_w = |l: &Len| {
            l.resolve(Some(cb.w)).map(|v| {
                if s.border_box {
                    (v - bp_h).max(0.0)
                } else {
                    v.max(0.0)
                }
            })
        };
        let min_w = spec_w(&s.min_width).unwrap_or(0.0);
        let max_w = match &s.max_width {
            Len::None => f32::INFINITY,
            l => spec_w(l).unwrap_or(f32::INFINITY),
        }
        .max(min_w);
        // §10.3.7's constraint: left + ml + bp + width + mr + right = cb.w.
        // Returns (left, width, margin-left) — what placement needs.
        let solve_h = |width: Option<f32>| -> (f32, f32, f32) {
            let (ml0, mr0) = (m[LEFT], m[RIGHT]);
            match (left, width, right) {
                // All three auto: auto margins → 0, left = static position,
                // width shrink-to-fit (rule 3's shape).
                (None, None, None) => {
                    let avail = cb.w - stat.0 - ml0 - mr0 - bp_h;
                    (stat.0, self.shrink_to_fit(b, avail, ctx), ml0)
                }
                // None auto: solve the margins — both auto split the rest
                // equally (negative → ml 0, ltr); over-constrained ignores
                // 'right'.
                (Some(l), Some(w), Some(r)) => {
                    let rest = cb.w - l - w - r - bp_h;
                    let ml = match (mauto[LEFT], mauto[RIGHT]) {
                        (true, true) => {
                            if rest >= 0.0 {
                                rest / 2.0
                            } else {
                                0.0
                            }
                        }
                        (true, false) => rest - mr0,
                        _ => ml0,
                    };
                    (l, w, ml)
                }
                // Rule 1: left+width auto → shrink-to-fit, solve left.
                (None, None, Some(r)) => {
                    let avail = cb.w - r - ml0 - mr0 - bp_h;
                    let w = self.shrink_to_fit(b, avail, ctx);
                    (cb.w - r - w - bp_h - ml0 - mr0, w, ml0)
                }
                // Rule 2: left+right auto → left = static position.
                (None, Some(w), None) => (stat.0, w, ml0),
                // Rule 3: width+right auto → shrink-to-fit at left.
                (Some(l), None, None) => {
                    let avail = cb.w - l - ml0 - mr0 - bp_h;
                    (l, self.shrink_to_fit(b, avail, ctx), ml0)
                }
                // Rule 4: solve left.
                (None, Some(w), Some(r)) => (cb.w - r - w - bp_h - ml0 - mr0, w, ml0),
                // Rule 5: solve width (negative → 0).
                (Some(l), None, Some(r)) => (l, (cb.w - l - r - ml0 - mr0 - bp_h).max(0.0), ml0),
                // Rule 6: solve right — placement needs nothing more.
                (Some(l), Some(w), None) => (l, w, ml0),
            }
        };
        let (mut lx, mut used_w, mut ml) = solve_h(replaced.map(|(rw, _)| rw).or_else(|| {
            spec_w(&s.width).map(|w| w.clamp(min_w, max_w)) // §10.4 on a specified width
        }));
        if replaced.is_none() {
            // §10.4 on a SOLVED width: clamp, then re-solve with the clamped
            // value as the computed width.
            if used_w > max_w {
                (lx, used_w, ml) = solve_h(Some(max_w));
            }
            if used_w < min_w {
                (lx, used_w, ml) = solve_h(Some(min_w));
            }
        }
        // §10.6.4 heights. A height solvable BEFORE layout — specified, or
        // both insets given (rule 5) — becomes the definite content height
        // children resolve against; otherwise the content decides (§10.6.7).
        let spec_v = |l: &Len| {
            l.resolve(Some(cb.h)).map(|v| {
                if s.border_box {
                    (v - bp_v).max(0.0)
                } else {
                    v.max(0.0)
                }
            })
        };
        let min_h = spec_v(&s.min_height).unwrap_or(0.0);
        let max_h = match &s.max_height {
            Len::None => f32::INFINITY,
            l => spec_v(l).unwrap_or(f32::INFINITY),
        }
        .max(min_h);
        let height_spec = match replaced {
            Some((_, rh)) => Some(rh),
            None => spec_v(&s.height),
        };
        let pre_h = height_spec.or(match (top, bottom, replaced) {
            (Some(t), Some(bm), None) => Some((cb.h - t - bm - m[TOP] - m[BOTTOM] - bp_v).max(0.0)),
            _ => None,
        });
        let def_h = pre_h.map(|v| {
            if replaced.is_some() {
                v
            } else {
                v.clamp(min_h, max_h)
            }
        });
        let (mut frag, mut anc) = self.item_frag(b, used_w, cb.w, def_h, ctx);
        let used_h = match def_h {
            Some(h) => h,
            // §10.7: the content-based height clamps; content overflows the
            // clamped box visibly (items keep their laid positions).
            None => (frag.h - bp_v).max(0.0).clamp(min_h, max_h),
        };
        frag.h = bp_v + used_h;
        // Vertical placement (§10.6.4/§10.6.5): only top + margin-top place
        // the box; over-constrained ignores 'bottom'.
        let mt_used = match (top, bottom, mauto[TOP], mauto[BOTTOM]) {
            (Some(t), Some(bm), true, true) => (cb.h - t - bm - used_h - bp_v) / 2.0,
            (Some(t), Some(bm), true, false) => cb.h - t - bm - used_h - bp_v - m[BOTTOM],
            _ => m[TOP],
        };
        let top_used = match (top, bottom) {
            (Some(t), _) => t,
            (None, Some(bm)) => cb.h - bm - used_h - bp_v - mt_used - m[BOTTOM],
            (None, None) => stat.1,
        };
        // The box's own transform translation (an abspos box is never also
        // relative, so this is the whole paint offset).
        let (dx, dy) = self.paint_offset(s, cb.w, Some(cb.h), frag.w, frag.h);
        let x = cb.x + lx + ml + dx;
        let y = cb.y + top_used + mt_used + dy;
        Self::offset_frag(&mut frag, x, y);
        for a in &mut anc {
            a.1 += y;
        }
        (frag, anc)
    }

    /// §10.3.7's shrink-to-fit width: min(max(preferred minimum width,
    /// available width), preferred width), via the intrinsic probe.
    fn shrink_to_fit(&self, b: &BoxNode, avail: f32, ctx: &InlineStyle) -> f32 {
        let minc = self.intrinsic_w(b, IMode::Min, ctx);
        let maxc = self.intrinsic_w(b, IMode::Max, ctx).max(minc);
        avail.max(minc).min(maxc).max(0.0)
    }
}
