//! The intrinsic-size query (css-sizing-3 min-/max-content), the plan's
//! explicit, memoized replacement for the old engine's `measuring`-flag
//! probes.
//!
//! The load-bearing idea: INLINE intrinsic widths are measured by running
//! the REAL line breaker (`Ifc`) under the spec's constraints — min-content
//! = lay at a 1-cell available width (every soft-wrap opportunity taken; a
//! line's used width is then its widest unbreakable segment), max-content =
//! lay at an effectively infinite width (only forced breaks break). One
//! source of truth: an item floored at its min-content size provably fits
//! when later laid at that width, because the measurement IS the layout.
//! The old engine's probe-band leaks and measuring-state contamination are
//! structurally impossible here — the probe constructs a fresh IFC and
//! touches no flow state.
//!
//! Results are CONTENT-box px, memoized per (element, mode); anonymous
//! boxes are cheap composites of memoized elements.

use crate::layout::{NO_NODE, Units, display_width};

use super::flow::Flow;
use super::inline::{Ifc, control_label, media_label, media_source};
use super::style::{Align2, InlineStyle};
use super::tree::{AtomKind, BoxNode, Content};
use super::value::Len;

/// The max-content probe's "infinite" available width, in cells. Far beyond
/// any real content, small enough that cell↔px round-trips stay exact in f32.
const PROBE_MAX_CELLS: usize = 1_000_000;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum IMode {
    Min,
    Max,
}

impl Flow<'_> {
    /// The CONTENT-box intrinsic width of `b`'s content, px. `inl` is the
    /// inherited inline context (only measurement-relevant pieces matter:
    /// white-space, letter-spacing, transform, font-zero — all re-derived
    /// per element from the cascade, so element results are context-free
    /// and memoizable; anonymous boxes use the passed context).
    pub(crate) fn intrinsic_w(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        if b.node != NO_NODE
            && let Some(&hit) = self.imemo.borrow().get(&(b.node, mode == IMode::Min))
        {
            return hit;
        }
        let v = self.intrinsic_w_inner(b, mode, inl);
        if b.node != NO_NODE {
            self.imemo
                .borrow_mut()
                .insert((b.node, mode == IMode::Min), v);
        }
        v
    }

    fn intrinsic_w_inner(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        let here = if b.node == NO_NODE {
            inl.clone()
        } else {
            InlineStyle::derive(self.dom, b.node, inl, self.base)
        };
        match &b.content {
            Content::Blocks(kids) => kids
                .iter()
                .map(|k| self.contribution(k, mode, &here))
                .fold(0.0f32, f32::max),
            Content::Inlines(inls) => {
                let cap = match mode {
                    IMode::Min => 1,
                    IMode::Max => PROBE_MAX_CELLS,
                };
                let mut ifc = Ifc::new(
                    self.dom,
                    self.base,
                    self.images,
                    self.forms,
                    self.vp,
                    self.cell_w,
                    self.cell_h,
                    cap as f32 * self.cell_w,
                    None,
                    Align2::Left,
                    // text-indent participates in intrinsic widths;
                    // percentages resolve against a zero basis here.
                    self.indent_px(if b.node == NO_NODE { inl.node } else { b.node }, 0.0),
                );
                ifc.run(inls, &here);
                let (lines, _, _) = ifc.finish();
                lines.iter().map(|l| l.width).max().unwrap_or(0) as f32 * self.cell_w
            }
            Content::Atomic(atom) => self.atom_intrinsic_w(atom, mode),
            Content::Grid(items) => {
                // Grid intrinsic sizing under a constraint (§11.9 runs the
                // whole track algorithm) — approximated pending real
                // constraint plumbing: min = the widest item's min
                // contribution; max = the widest item's max contribution
                // (a shrink-wrapped grid sizes to its largest column set;
                // definite templates dominate via the container's own
                // width property in practice).
                let contributions = items.iter().map(|it| self.contribution(it, mode, &here));
                contributions.fold(0.0f32, f32::max)
            }
            Content::Flex(items) => {
                let u = Units::of(self.dom, b.node);
                let fs = super::flex::container_style(self.dom, b.node, u, self.vp);
                let gap = fs.gap_main.resolve(None).unwrap_or(0.0).max(0.0);
                let contributions = items.iter().map(|it| self.contribution(it, mode, &here));
                if fs.row {
                    // §9.9.1 shape: a row container's max-content is the sum
                    // of its items' max-content contributions; its
                    // min-content is the largest item contribution when it
                    // can wrap, else the sum (nowrap can't break the row).
                    if mode == IMode::Min && fs.wrap {
                        contributions.fold(0.0f32, f32::max)
                    } else {
                        let n = items.len();
                        contributions.sum::<f32>() + gap * n.saturating_sub(1) as f32
                    }
                } else {
                    // Column: the widest item governs both modes.
                    contributions.fold(0.0f32, f32::max)
                }
            }
            Content::Table(tb) => self.table_intrinsic(tb, b.node, mode, &here),
        }
    }

    /// A block-level child's margin-box contribution to its parent's
    /// intrinsic width: its definite (non-percentage) width — else its own
    /// intrinsic width — clamped by non-percentage min/max, plus borders,
    /// padding, and margins (percentages and `auto` behave as zero under an
    /// intrinsic-sizing constraint — css-sizing-3 §5.2.2).
    pub(crate) fn contribution(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        let s = &b.style;
        let side = |l: &Len| l.resolve(None).unwrap_or(0.0);
        let bp = s.border[super::style::LEFT]
            + s.border[super::style::RIGHT]
            + side(&s.padding[super::style::LEFT]).max(0.0)
            + side(&s.padding[super::style::RIGHT]).max(0.0);
        let to_content = |v: f32| {
            if s.border_box {
                (v - bp).max(0.0)
            } else {
                v.max(0.0)
            }
        };
        let content = s
            .width
            .resolve(None)
            .map(to_content)
            .unwrap_or_else(|| self.intrinsic_w(b, mode, inl));
        let min = s.min_width.resolve(None).map(to_content).unwrap_or(0.0);
        let max = match &s.max_width {
            Len::None => f32::INFINITY,
            l => l.resolve(None).map(to_content).unwrap_or(f32::INFINITY),
        }
        .max(min);
        content.clamp(min, max)
            + bp
            + side(&s.margin[super::style::LEFT])
            + side(&s.margin[super::style::RIGHT])
    }

    /// A replaced/control/media atom's content intrinsic width, px. Under an
    /// intrinsic constraint percentages behave as auto, so the replaced
    /// sizing runs with no percentage basis.
    fn atom_intrinsic_w(&self, atom: &super::tree::Atom, mode: IMode) -> f32 {
        match &atom.kind {
            AtomKind::Img { url, alt } => {
                let natural = url
                    .as_deref()
                    .and_then(|u| self.images.get(u))
                    .filter(|&&(w, h)| w > 0 && h > 0)
                    .map(|&(w, h)| (f32::from(w) * self.cell_w, f32::from(h) * self.cell_h));
                match super::replaced::size(self.dom, atom.node, natural, None, None, self.vp) {
                    Some(r) => r.box_w,
                    None => text_intrinsic_cells(alt, mode) as f32 * self.cell_w,
                }
            }
            AtomKind::Control { form, field } => {
                let Some(f) = self.forms.get(*form).and_then(|f| f.fields.get(*field)) else {
                    return 0.0;
                };
                let label = control_label(
                    self.dom,
                    atom.node,
                    f,
                    None,
                    usize::MAX,
                    self.cell_w,
                    self.vp,
                );
                display_width(&label) as f32 * self.cell_w
            }
            AtomKind::Media { video } => {
                // A decoded poster's box, else the text affordance's width
                // (over-estimating a dead-end costs nothing but space).
                let poster = self
                    .dom
                    .attr(atom.node, "poster")
                    .and_then(|p| match crate::http::resolve(self.base, p.trim()) {
                        crate::doc::Link::Http(u) => Some(u.to_string()),
                        _ => None,
                    })
                    .and_then(|p| self.images.get(&p).copied());
                if let Some((w, _)) = poster.filter(|&(w, h)| w > 0 && h > 0) {
                    return f32::from(w) * self.cell_w;
                }
                let label = match media_source(self.dom, self.base, atom.node) {
                    Some((_, sn)) => media_label(self.dom, *video, sn),
                    None if *video => String::from("▶ Watch in mpv"),
                    None => return 0.0,
                };
                display_width(&label) as f32 * self.cell_w
            }
        }
    }
}

/// Plain-text intrinsic width in cells: min = the widest unbreakable
/// segment (words split at wide-glyph boundaries, like the line breaker);
/// max = the collapsed single-line width.
fn text_intrinsic_cells(t: &str, mode: IMode) -> usize {
    let mut best = 0usize;
    let mut line = 0usize;
    let mut seg = 0usize;
    let mut pending_space = false;
    for c in t.chars() {
        if crate::layout::is_collapsible_space(c) {
            best = best.max(seg);
            seg = 0;
            pending_space = line > 0;
            continue;
        }
        let cw = display_width(c.encode_utf8(&mut [0u8; 4]));
        if cw >= 2 {
            // A wide glyph is its own unbreakable segment.
            best = best.max(seg).max(cw);
            seg = 0;
        } else {
            seg += cw;
        }
        line += cw + usize::from(std::mem::take(&mut pending_space));
    }
    best = best.max(seg);
    match mode {
        IMode::Min => best,
        IMode::Max => line,
    }
}
