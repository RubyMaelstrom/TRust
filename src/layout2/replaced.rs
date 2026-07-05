//! Replaced-element sizing (CSS 2.1 §10.3.2/§10.6.2, the §10.4 min/max
//! constraint table, css-sizing-4 `aspect-ratio`, css-images-3 `object-fit`).
//!
//! THE standard algorithm, replacing the old engine's `image_used_box`
//! fallback chains: natural size and ratio in, specified sizes resolved
//! against the containing block, the spec's auto-resolution and fallbacks,
//! then the ratio-preserving constraint table. Everything in f32 CSS px;
//! the caller quantizes at the paint boundary like all other geometry.

use crate::dom::{Dom, NodeId};
use crate::layout::Units;

use super::value::{Len, Vp};

/// A replaced element's used geometry: the BOX (what layout flows around,
/// the element's used content size) and the PAINT rect inside it (what the
/// pixels map to — differing from the box only under `contain`/`scale-down`,
/// where the image letterboxes at its natural ratio, centered per the
/// `object-position` initial value).
#[derive(Debug, PartialEq)]
pub(crate) struct Replaced {
    pub box_w: f32,
    pub box_h: f32,
    pub paint_w: f32,
    pub paint_h: f32,
    pub off_x: f32,
    pub off_y: f32,
    /// `object-fit: cover` — the encoder fills the box and crops overflow.
    pub crop: bool,
}

/// Resolve a replaced element's used size. `natural` is the decoded
/// intrinsic size in px when known. `None` = nothing determines a box (no
/// natural size, no usable specified sizes, no ratio): the element renders
/// its fallback content instead (HTML's "image not available" inline alt
/// representation).
pub(crate) fn size(
    dom: &Dom,
    node: NodeId,
    natural: Option<(f32, f32)>,
    cb_w: Option<f32>,
    cb_h: Option<f32>,
    vp: Vp,
) -> Option<Replaced> {
    let u = Units::of(dom, node);
    let css = |prop: &str, basis: Option<f32>| {
        dom.computed_value(node, prop)
            .and_then(|v| Len::parse(&v, u, vp))
            .and_then(|l| l.resolve(basis))
            .filter(|&v| v >= 0.0)
    };
    // The HTML width/height attributes are presentational hints for the
    // specified size (and, as a pair, the modern pre-decode ratio source).
    let attr = |name: &str| {
        dom.attr(node, name)
            .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
            .filter(|&v| v > 0.0)
    };
    let spec_w = css("width", cb_w).or_else(|| attr("width"));
    let spec_h = css("height", cb_h).or_else(|| attr("height"));
    let ratio = ratio_of(dom, node, natural);

    // §10.3.2/§10.6.2 auto resolution. The 300×150/2:1 caps are the spec's
    // own last resort for a ratio-less axis.
    let (w0, h0) = match (spec_w, spec_h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let h = match (ratio, natural) {
                (Some(r), _) if r > 0.0 => w / r,
                (None, Some((_, nh))) => nh,
                _ => (w / 2.0).min(150.0),
            };
            (w, h)
        }
        (None, Some(h)) => {
            let w = match (ratio, natural) {
                (Some(r), _) => h * r,
                (None, Some((nw, _))) => nw,
                _ => (h * 2.0).min(300.0),
            };
            (w, h)
        }
        (None, None) => match (natural, ratio) {
            (Some(n), _) => n,
            (None, Some(r)) if r > 0.0 => (300.0, 300.0 / r),
            (None, _) => return None, // fallback-content representation
        },
    };

    // §10.4 min/max. Ratio-preserving table when BOTH dimensions were auto
    // and a ratio exists; a specified axis clamps plainly and re-derives the
    // auto one through the ratio.
    let min_w = css("min-width", cb_w).unwrap_or(0.0);
    let max_w = match dom
        .computed_value(node, "max-width")
        .and_then(|v| Len::parse(&v, u, vp))
    {
        Some(Len::None) | None => f32::INFINITY,
        Some(l) => l.resolve(cb_w).unwrap_or(f32::INFINITY),
    }
    .max(min_w);
    let min_h = css("min-height", cb_h).unwrap_or(0.0);
    let max_h = match dom
        .computed_value(node, "max-height")
        .and_then(|v| Len::parse(&v, u, vp))
    {
        Some(Len::None) | None => f32::INFINITY,
        Some(l) => l.resolve(cb_h).unwrap_or(f32::INFINITY),
    }
    .max(min_h);

    let (box_w, box_h) = match (spec_w, spec_h, ratio) {
        (None, None, Some(r)) if r > 0.0 => constrain_ratio(w0, h0, min_w, max_w, min_h, max_h),
        (Some(_), None, Some(r)) if r > 0.0 => {
            let w = w0.clamp(min_w, max_w);
            let h = (w / r).clamp(min_h, max_h);
            (w, h)
        }
        (None, Some(_), Some(r)) if r > 0.0 => {
            let h = h0.clamp(min_h, max_h);
            let w = (h * r).clamp(min_w, max_w);
            (w, h)
        }
        _ => (w0.clamp(min_w, max_w), h0.clamp(min_h, max_h)),
    };
    let (box_w, box_h) = (box_w.max(1.0), box_h.max(1.0));

    // object-fit (css-images-3 §5.5). Meaningful only with a natural size to
    // map; a reserved-but-undecoded box paints blank regardless. `none` maps
    // to `scale-down` (painting the natural size CLIPPED by the box needs
    // sub-image crop offsets the emission model doesn't carry — the
    // documented cell-scale approximation; `scale-down` is its ≤-natural
    // half and identical whenever the image doesn't overflow the box).
    Some(apply_fit(dom, node, natural, box_w, box_h))
}

/// css-images-3 §5.5 `object-fit` over a used box: the paint rect and crop
/// flag. `fill` (initial) stretches to the box; `cover` fills and crops;
/// `contain` letterboxes centered (`object-position` initial 50% 50%);
/// `none` maps to `scale-down` (sub-image crop offsets don't exist in the
/// emission model — the documented cell-scale approximation, identical
/// whenever the image doesn't overflow its box).
pub(crate) fn apply_fit(
    dom: &Dom,
    node: NodeId,
    natural: Option<(f32, f32)>,
    box_w: f32,
    box_h: f32,
) -> Replaced {
    let fit = dom
        .computed_value(node, "object-fit")
        .map(|v| v.trim().to_ascii_lowercase());
    let mut out = Replaced {
        box_w,
        box_h,
        paint_w: box_w,
        paint_h: box_h,
        off_x: 0.0,
        off_y: 0.0,
        crop: false,
    };
    if let Some((nw, nh)) = natural {
        match fit.as_deref() {
            Some("cover") => out.crop = true,
            Some("contain") | Some("none") | Some("scale-down") => {
                let scale = (box_w / nw.max(1.0)).min(box_h / nh.max(1.0));
                let scale = if matches!(fit.as_deref(), Some("none") | Some("scale-down")) {
                    scale.min(1.0)
                } else {
                    scale
                };
                let (pw, ph) = (nw * scale, nh * scale);
                if pw < box_w - 0.5 || ph < box_h - 0.5 {
                    out.paint_w = pw.max(1.0);
                    out.paint_h = ph.max(1.0);
                    out.off_x = (box_w - out.paint_w) / 2.0;
                    out.off_y = (box_h - out.paint_h) / 2.0;
                }
            }
            _ => {}
        }
    }
    out
}

/// The natural-ratio chain a replaced element sizes through: intrinsic,
/// else the width/height ATTRIBUTE pair (HTML's pre-decode reservation
/// rule), else CSS `aspect-ratio`.
pub(crate) fn ratio_of(dom: &Dom, node: NodeId, natural: Option<(f32, f32)>) -> Option<f32> {
    let attr = |name: &str| {
        dom.attr(node, name)
            .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
            .filter(|&v| v > 0.0)
    };
    natural
        .map(|(w, h)| w / h.max(1.0))
        .or_else(|| match (attr("width"), attr("height")) {
            (Some(w), Some(h)) if h > 0.0 => Some(w / h),
            _ => None,
        })
        .or_else(|| {
            dom.computed_value(node, "aspect-ratio")
                .as_deref()
                .and_then(parse_ratio)
        })
}

/// CSS 2.1 §10.4's constraint table for replaced elements with a natural
/// ratio and both dimensions auto: min/max violations resolve preserving the
/// ratio where the table says so.
fn constrain_ratio(w: f32, h: f32, min_w: f32, max_w: f32, min_h: f32, max_h: f32) -> (f32, f32) {
    let over_w = w > max_w;
    let under_w = w < min_w;
    let over_h = h > max_h;
    let under_h = h < min_h;
    match (over_w, under_w, over_h, under_h) {
        (false, false, false, false) => (w, h),
        (true, _, false, false) => (max_w, (max_w * h / w).max(min_h)),
        (_, true, false, false) => (min_w, (min_w * h / w).min(max_h)),
        (false, false, true, _) => ((max_h * w / h).max(min_w), max_h),
        (false, false, _, true) => ((min_h * w / h).min(max_w), min_h),
        (true, _, true, _) => {
            if max_w / w <= max_h / h {
                (max_w, (max_w * h / w).max(min_h))
            } else {
                ((max_h * w / h).max(min_w), max_h)
            }
        }
        (_, true, _, true) => {
            if min_w / w <= min_h / h {
                ((min_h * w / h).min(max_w), min_h)
            } else {
                (min_w, (min_w * h / w).min(max_h))
            }
        }
        (_, true, true, _) => (min_w, max_h),
        (true, _, _, true) => (max_w, min_h),
    }
}

/// Parse a CSS `aspect-ratio`: `R`, `W / H`, `auto W / H` (`auto` with a
/// ratio uses the ratio for boxes without a natural one — our caller only
/// consults this when no natural ratio exists, which is that exact rule).
fn parse_ratio(value: &str) -> Option<f32> {
    let v = value.trim().trim_start_matches("auto").trim();
    if v.is_empty() || v == "auto" {
        return None;
    }
    let ratio = if let Some((a, b)) = v.split_once('/') {
        a.trim().parse::<f32>().ok()? / b.trim().parse::<f32>().ok()?
    } else {
        v.parse::<f32>().ok()?
    };
    (ratio.is_finite() && ratio > 0.0).then_some(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraint_table_preserves_ratio() {
        let inf = f32::INFINITY;
        // Natural 800×320 (2.5:1), max-width 320 → scaled to 320×128.
        assert_eq!(
            constrain_ratio(800.0, 320.0, 0.0, 320.0, 0.0, inf),
            (320.0, 128.0)
        );
        // min-width pulls up, height follows the ratio.
        assert_eq!(
            constrain_ratio(100.0, 50.0, 200.0, inf, 0.0, inf),
            (200.0, 100.0)
        );
        // max-height governs when it is the tighter constraint.
        assert_eq!(
            constrain_ratio(800.0, 320.0, 0.0, 400.0, 0.0, 80.0),
            (200.0, 80.0)
        );
        // Both under: the LARGER scale-up wins (table's min/min row).
        assert_eq!(constrain_ratio(100.0, 50.0, 300.0, inf, 0.0, inf).0, 300.0);
        // Cross violations pin both.
        assert_eq!(
            constrain_ratio(100.0, 500.0, 200.0, inf, 0.0, 300.0),
            (200.0, 300.0)
        );
    }

    #[test]
    fn ratio_parsing() {
        assert_eq!(parse_ratio("16 / 9"), Some(16.0 / 9.0));
        assert_eq!(parse_ratio("2"), Some(2.0));
        assert_eq!(parse_ratio("auto 4/3"), Some(4.0 / 3.0));
        assert_eq!(parse_ratio("auto"), None);
        assert_eq!(parse_ratio("0/5"), None);
    }
}
