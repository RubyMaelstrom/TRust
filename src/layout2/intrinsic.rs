//! The intrinsic-size query (css-sizing-3 min-/max-content), the
//! explicit, memoized replacement for the old engine's `measuring`-flag
//! probes.
//!
//! The load-bearing idea: INLINE intrinsic widths are measured by running
//! the REAL line breaker (`Ifc`) under the spec's constraints — min-content
//! = lay at a near-zero CSS-pixel available width (every soft-wrap opportunity taken; a
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

use crate::layout2::{NO_NODE, Units};

use super::flow::Flow;
use super::inline::{AtomBoxSize, Ifc, media_label, media_source};
use super::style::{Align2, InlineStyle};
use super::tree::{AtomKind, BoxNode, Content};
use super::value::Len;

/// The max-content probe's effectively infinite CSS-pixel width.
const PROBE_MAX_PX: f32 = 10_000_000.0;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum IMode {
    Min,
    Max,
}

impl Flow<'_> {
    /// CSS Sizing 3/4 #sizing-values. Intrinsic keywords name content-box
    /// sizes; the argument of fit-content() resolves like a specified size,
    /// including box-sizing, before applying its min/max-content bounds.
    pub(super) fn intrinsic_width_value(
        &self,
        value: &Len,
        b: &BoxNode,
        basis: Option<f32>,
        inl: &InlineStyle,
    ) -> Option<f32> {
        let mode = match value {
            Len::MinContent => Some(IMode::Min),
            Len::MaxContent => Some(IMode::Max),
            Len::FitContent | Len::FitContentLimit(_) => None,
            _ => return None,
        };
        if let Some(mode) = mode {
            return Some(self.intrinsic_w(b, mode, inl));
        }
        let s = &b.style;
        let side = |v: &Len| v.resolve(basis).unwrap_or(0.);
        let bp =
            s.border[3] + s.border[1] + side(&s.padding[3]).max(0.) + side(&s.padding[1]).max(0.);
        let stretch = match value {
            Len::FitContentLimit(limit) => limit
                .resolve(basis)
                .map(|v| (v - if s.border_box { bp } else { 0. }).max(0.))?,
            _ => (basis? - bp - side(&s.margin[3]) - side(&s.margin[1])).max(0.),
        };
        let min = self.intrinsic_w(b, IMode::Min, inl);
        let max = self.intrinsic_w(b, IMode::Max, inl).max(min);
        Some(stretch.clamp(min, max))
    }

    /// The CONTENT-box intrinsic width of `b`'s content, px. `inl` is the
    /// inherited inline context (only measurement-relevant pieces matter:
    /// white-space, letter-spacing, transform, font-zero — all re-derived
    /// per element from the cascade, so element results are context-free
    /// and memoizable; anonymous boxes use the passed context).
    pub(crate) fn intrinsic_w(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        // CSS Containment 2 §3.1/§3.2: content cannot contribute to a size
        // container's inline intrinsic size, otherwise queries form a cycle.
        if b.style.size_container != 0 {
            return 0.0;
        }
        if b.node != NO_NODE
            && let Some(&hit) = self.imemo.borrow().get(&(b.node, mode == IMode::Min))
        {
            return hit;
        }
        let request = super::memo::Request {
            node: b,
            parent: inl,
            constraint: super::memo::Constraint::Intrinsic(mode == IMode::Min),
        };
        let reuse = self.reuse && b.node != NO_NODE;
        if reuse && let Some(value) = self.dom.layout_cache.borrow_mut().intrinsic(&request) {
            self.imemo
                .borrow_mut()
                .insert((b.node, mode == IMode::Min), value);
            return value;
        }
        let v = self.intrinsic_w_inner(b, mode, inl);
        if reuse {
            self.dom
                .layout_cache
                .borrow_mut()
                .store_intrinsic(&request, v);
        }
        if b.node != NO_NODE {
            self.imemo
                .borrow_mut()
                .insert((b.node, mode == IMode::Min), v);
        }
        v
    }

    fn intrinsic_w_inner(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        let here = if b.node == NO_NODE {
            inl.with_pseudo(self.dom, b.style.pseudo)
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
                    IMode::Min => 0.01,
                    IMode::Max => PROBE_MAX_PX,
                };
                // An atomic inline box is opaque: it contributes its own
                // min/max-content MARGIN-box width to the line (it never breaks
                // across the parent's lines). The probe IFC places it like the
                // real one, so pre-size each in walk order (mirroring floats,
                // which the IFC instead skips + folds below).
                let mut atoms: Vec<(&BoxNode, InlineStyle)> = Vec::new();
                super::flow::collect_atom_boxes(self.dom, self.base, inls, &here, &mut atoms);
                let atom_sizes: Vec<AtomBoxSize> = atoms
                    .iter()
                    .map(|(ab, actx)| {
                        let w = self.contribution(ab, mode, actx);
                        AtomBoxSize {
                            width: w.max(0.0),
                            height: actx.text_style().size.max(1.0),
                            baseline: None,
                        }
                    })
                    .collect();
                let mut ifc = Ifc::new(
                    self.dom,
                    self.base,
                    self.images,
                    self.forms,
                    self.vp,
                    cap,
                    None,
                    Align2::Left,
                    // text-indent participates in intrinsic widths;
                    // percentages resolve against a zero basis here.
                    self.indent_px(if b.node == NO_NODE { inl.node } else { b.node }, 0.0),
                    None,
                    &atom_sizes,
                );
                ifc.mark_measuring();
                ifc.run(inls, &here);
                let (lines, _, _, _, _) = ifc.finish();
                let inline_w = lines.iter().map(|l| l.width).fold(0.0, f32::max);
                // Floats: the probe IFC skips them, so fold them here. At
                // max-content no soft-wrap opportunity is taken (§10.3.5 /
                // css-sizing-3), so consecutive floats sit side by side on one
                // shelf and inline content flows beside them — their margin
                // boxes ADD (a float menu bar measures as the sum of its
                // items); a float that clears an earlier one starts a new
                // shelf below, and shelves compete by max. At min-content
                // every wrap is taken — each float may drop to its own shelf,
                // so each contributes individually by max.
                let mut floats: Vec<(&BoxNode, InlineStyle)> = Vec::new();
                super::flow::collect_floats(self.dom, self.base, inls, &here, &mut floats);
                if mode == IMode::Max {
                    let mut shelves: Vec<f32> = vec![0.0];
                    let mut has = super::float::Clear::default();
                    for (fb, fctx) in &floats {
                        let cl = fb.style.clear;
                        if (cl.left && has.left) || (cl.right && has.right) {
                            shelves.push(0.0);
                            has = super::float::Clear::default();
                        }
                        match fb.style.float {
                            Some(super::float::Side::Left) => has.left = true,
                            Some(super::float::Side::Right) => has.right = true,
                            None => {}
                        }
                        *shelves.last_mut().unwrap() += self.contribution(fb, mode, fctx);
                    }
                    // Inline content flows beside the first shelf's floats.
                    let first = shelves[0] + inline_w;
                    shelves.into_iter().skip(1).fold(first, f32::max)
                } else {
                    floats
                        .iter()
                        .map(|(fb, fctx)| self.contribution(fb, mode, fctx))
                        .fold(inline_w, f32::max)
                }
            }
            Content::Atomic(atom) => self.atom_intrinsic_w(atom, mode, &here),
            Content::Grid(items) => self.grid_intrinsic_width(b, items, mode, &here),
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
    /// padding, and margins (css-sizing-3 §5.2.1).
    pub(crate) fn contribution(&self, b: &BoxNode, mode: IMode, inl: &InlineStyle) -> f32 {
        let s = &b.style;
        // Cyclic percentages in min sizes, margins, and padding resolve
        // against zero for every intrinsic contribution. A compressible
        // replaced element's preferred/max size does the same specifically
        // for its min-content contribution; this is what lets an image with
        // `max-width:100%` shrink inside an intrinsically-sized flex item.
        let side = |l: &Len| l.resolve(Some(0.0)).unwrap_or(0.0);
        let compressible_min = mode == IMode::Min
            && matches!(
                &b.content,
                Content::Atomic(atom) if matches!(&atom.kind, AtomKind::Img { .. })
            );
        let preferred_basis = compressible_min.then_some(0.0);
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
        let content = self
            .intrinsic_width_value(&s.width, b, preferred_basis, inl)
            .or_else(|| s.width.resolve(preferred_basis).map(to_content))
            .unwrap_or_else(|| self.intrinsic_w(b, mode, inl));
        let min = self
            .intrinsic_width_value(&s.min_width, b, Some(0.), inl)
            .or_else(|| s.min_width.resolve(Some(0.0)).map(to_content))
            .unwrap_or(0.0);
        let max = match &s.max_width {
            Len::None => f32::INFINITY,
            l => self
                .intrinsic_width_value(l, b, preferred_basis, inl)
                .or_else(|| l.resolve(preferred_basis).map(to_content))
                .unwrap_or(f32::INFINITY),
        }
        .max(min);
        content.clamp(min, max)
            + bp
            + side(&s.margin[super::style::LEFT])
            + side(&s.margin[super::style::RIGHT])
    }

    /// A replaced/control/media atom's content intrinsic width, px. Under an
    /// intrinsic constraint percentages behave as auto here; `contribution`
    /// applies the special cyclic-percentage constraints around this content.
    fn atom_intrinsic_w(&self, atom: &super::tree::Atom, mode: IMode, inl: &InlineStyle) -> f32 {
        match &atom.kind {
            AtomKind::GeneratedImage { url } => {
                crate::responsive_image::density_corrected_size(self.images.get(url), 1.0)
                    .map_or(0.0, |(w, _)| w)
            }
            AtomKind::Img {
                url,
                density,
                dimension_source,
                alt,
            } => {
                let natural = crate::responsive_image::density_corrected_size(
                    url.as_deref().and_then(|url| self.images.get(url)),
                    *density,
                );
                match super::replaced::size(
                    self.dom,
                    atom.node,
                    super::replaced::ImageInput {
                        dimension_source: *dimension_source,
                        natural,
                        url: url.as_deref(),
                    },
                    None,
                    None,
                    self.vp,
                ) {
                    Some(r) => r.box_w,
                    None => text_intrinsic(alt, mode, inl),
                }
            }
            AtomKind::Control { form, field } => {
                let Some(f) = self.forms.get(*form).and_then(|f| f.fields.get(*field)) else {
                    return 0.0;
                };
                if mode == IMode::Min {
                    super::inline::control_intrinsic_width_for_mode(
                        self.dom, atom.node, f, inl, self.vp, true,
                    )
                } else {
                    super::inline::control_intrinsic_width(self.dom, atom.node, f, inl, self.vp)
                }
            }
            AtomKind::Media { video } => {
                // A decoded poster's box, else the external-player text
                // affordance's width.
                let poster = self
                    .dom
                    .attr(atom.node, "poster")
                    .and_then(|p| match crate::http::resolve(self.base, p.trim()) {
                        crate::doc::Link::Http(u) => Some(u.to_string()),
                        _ => None,
                    })
                    .and_then(|p| {
                        crate::responsive_image::density_corrected_size(self.images.get(&p), 1.0)
                    });
                if let Some((w, _)) = poster {
                    return w;
                }
                let label = match media_source(self.dom, self.base, atom.node) {
                    Some((_, sn)) => media_label(self.dom, *video, sn),
                    None if *video => String::from("▶ Watch in mpv"),
                    None => return 0.0,
                };
                text_intrinsic(&label, mode, inl)
            }
        }
    }
}

/// Text intrinsic width from the same Unicode shaper/break analyzer used by
/// inline layout. No character count or terminal-cell proxy participates.
fn text_intrinsic(t: &str, mode: IMode, inl: &InlineStyle) -> f32 {
    let (min, max) =
        crate::text::content_widths(t, &inl.text_style(), crate::text::TextBreakStyle::default());
    match mode {
        IMode::Min => min,
        IMode::Max => max,
    }
}
