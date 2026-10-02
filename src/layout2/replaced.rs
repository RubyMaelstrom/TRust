//! Replaced-element sizing (CSS 2.1 §10.3.2/§10.6.2, the §10.4 min/max
//! constraint table, css-sizing-4 `aspect-ratio`, css-images-3 `object-fit`).
//!
//! THE standard algorithm, replacing the old engine's `image_used_box`
//! fallback chains: natural size and ratio in, specified sizes resolved
//! against the containing block, the spec's auto-resolution and fallbacks,
//! then the ratio-preserving constraint table. Everything in f32 CSS px;
//! the caller quantizes at the paint boundary like all other geometry.

use crate::dom::{Dom, NodeId};
use crate::layout2::Units;

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

/// Resource-dependent inputs to replaced-image sizing. Keeping the selected
/// dimension source, density-corrected natural size, and selected URL together
/// prevents callers from accidentally mixing metadata from the `src` fallback
/// with the active responsive candidate.
#[derive(Clone, Copy)]
pub(crate) struct ImageInput<'a> {
    pub dimension_source: NodeId,
    pub natural: Option<(f32, f32)>,
    pub url: Option<&'a str>,
}

/// Resolve a replaced element's used size. `natural` is the decoded
/// intrinsic size in px when known. `None` = nothing determines a box (no
/// natural size, no usable specified sizes, no ratio): the element renders
/// its fallback content instead (HTML's "image not available" inline alt
/// representation).
pub(crate) fn size(
    dom: &Dom,
    node: NodeId,
    image: ImageInput<'_>,
    cb_w: Option<f32>,
    cb_h: Option<f32>,
    vp: Vp,
) -> Option<Replaced> {
    let ImageInput {
        dimension_source,
        natural,
        url,
    } = image;
    let replacement = dom.content_replacement_image(node).is_some();
    let natural = (!replacement)
        .then(|| dom.canvas_size(node))
        .flatten()
        .map(|(w, h)| (w as f32, h as f32))
        .or(natural)
        // CSS Content 3 #replaced: an unavailable replacement has zero
        // natural dimensions, never the HTML source's fallback/alt contents.
        .or_else(|| replacement.then_some((0.0, 0.0)));
    let u = Units::of(dom, node);
    let css = |prop: &str, basis: Option<f32>| {
        dom.computed_value_resolved(node, prop)
            .and_then(|v| Len::parse(&v, u, vp))
            .and_then(|l| l.resolve(basis))
            .filter(|&v| v >= 0.0)
    };
    // The HTML width/height attributes are presentational hints for the
    // specified size (and, as a pair, the modern pre-decode ratio source).
    // HTML #maps-to-the-dimension-property: a percentage is of the
    // containing block, and behaves as auto against an indefinite one.
    // Unlike the "(ignoring zero)" mapping, this uses the rules for parsing
    // dimension values, which accept zero: `width=0 height=0` is a definite
    // 0×0 box (a hidden counter image), never the natural size.
    let attr = |name: &str, basis: Option<f32>| match dom
        .attr(dimension_source, name)
        .and_then(html_dimension)?
    {
        HtmlDimension::Pixels(px) => (px >= 0.0).then_some(px),
        HtmlDimension::Percentage(percent) => basis.map(|basis| basis * percent / 100.0),
    };
    // HTML Rendering §14.3.3 maps width/height attributes to presentational
    // hints for the corresponding CSS properties and to `aspect-ratio`.
    // Presentational hints sit below ordinary author declarations in the
    // author origin. Apply that cascade independently per axis: an explicit
    // author `height:auto` therefore suppresses the `height` attribute even
    // though parsing `auto` yields no definite length, while a width attribute
    // remains available if the author did not declare `width`.
    //
    // Checking only the parsed lengths loses the distinction between an
    // undeclared axis and an explicitly-auto axis. On 9to5linux article hero
    // images that turned width=1400 height=800 + max-width:100%;height:auto
    // into a width-clamped but still 800px-tall box.
    // css-sizing-3 #box-sizing: under `border-box`, the <length-percentage>
    // values of the sizing properties (presentational hints included) apply
    // to the border box; the content box is that less the padding and
    // border, floored at zero. Every caller treats the result as the
    // content box, which receives the edges outside it.
    let (edges_w, edges_h) =
        if dom.computed_value_resolved(node, "box-sizing").as_deref() == Some("border-box") {
            let style = super::style::BoxStyle::of(dom, node, vp);
            // CSS Box 4 #padding-physical: percentages refer to the containing
            // block's inline size, in both axes.
            let padding = |side: usize| style.padding[side].resolve(cb_w).unwrap_or(0.0).max(0.0);
            use super::style::{BOTTOM, LEFT, RIGHT, TOP};
            (
                style.border[LEFT] + style.border[RIGHT] + padding(LEFT) + padding(RIGHT),
                style.border[TOP] + style.border[BOTTOM] + padding(TOP) + padding(BOTTOM),
            )
        } else {
            (0.0, 0.0)
        };
    let content_w = |value: f32| (value - edges_w).max(0.0);
    let content_h = |value: f32| (value - edges_h).max(0.0);
    let css_w = css("width", cb_w);
    let css_h = css("height", cb_h);
    let spec_w = if dom.author_declares(node, "width") {
        css_w
    } else {
        attr("width", cb_w)
    }
    .map(content_w);
    let spec_h = if dom.author_declares(node, "height") {
        css_h
    } else {
        attr("height", cb_h)
    }
    .map(content_h);
    // An img representing its alt text is not a replaced box.
    if represents_alt_text(dom, node, dimension_source, url, vp) {
        return None;
    }
    // CSS 2.2 §10.3.2/§10.6.2: a replaced element with an intrinsic
    // ratio but NO intrinsic width or height (an SVG referenced with only a
    // `viewBox`), sized auto/auto, takes its width from the block constraint
    // equation — the containing block's available width — not the decoder's
    // fabricated default object size (the huge-icon bug). That equation needs a
    // definite containing-block width, so under an intrinsic-size probe (`cb_w`
    // None) it does not apply: sizing falls back to the decoder's natural size
    // (the element's intrinsic contribution), unchanged from before.
    // SVG 2 intrinsic sizing: a root `viewBox` supplies an intrinsic ratio even
    // when it supplies no intrinsic width or height. Read that ratio for every
    // auto-axis combination, including width:auto + definite height (the
    // absolute-replaced rule is width = height × ratio). The special
    // containing-block-width rule below remains limited to auto/auto.
    // Responsive images size from the selected resource, never the `src`
    // fallback that HTML suppresses when a width-descriptor source set is
    // active.
    // CSS Images 3 #default-sizing: two definite dimensions determine the
    // box without consulting the intrinsic ratio. In particular, do not
    // repeatedly decode/parse a potentially large data-URL SVG when neither
    // axis can use its metadata. Min/max still clamp those axes below, and
    // object-fit still receives the decoded natural size independently.
    // Test the resolved dimensions: an indefinite percentage remains auto.
    let svg_ratio = if spec_w.is_none() || spec_h.is_none() {
        svg_view_box_ratio(url)
    } else {
        None
    };
    let ratio_only = (spec_w.is_none() && spec_h.is_none() && cb_w.is_some())
        .then_some(svg_ratio)
        .flatten();
    // Prefer the exact viewBox ratio for a ratio-only image. A decoder may
    // supply a rasterized fallback size whose ratio differs slightly from the
    // vector's author-provided viewBox.
    // SVG 2 §8.12: an inline SVG retains its exact viewBox ratio when only
    // one CSS dimension is definite. Baking that dimension into the image
    // resource must not hide the ratio before asynchronous raster decoding.
    let ratio = svg_ratio
        .or_else(|| inline_svg_ratio(dom, node))
        .or_else(|| ratio_of(dom, node, dimension_source, natural))
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0);

    // §10.3.2/§10.6.2 auto resolution. The 300×150/2:1 caps are the spec's
    // own last resort for a ratio-less axis.
    let auto_size = |spec_w: Option<f32>, spec_h: Option<f32>| -> Option<(f32, f32)> {
        Some(match (spec_w, spec_h) {
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
            (None, None) => match (ratio_only, natural, ratio) {
                // Rule 3: block-constraint width from the containing block; the
                // height follows the ratio.
                (Some(r), _, _) => {
                    let w = cb_w.unwrap_or(300.0).max(0.0);
                    (w, w / r)
                }
                (None, Some(n), _) => n,
                (None, None, Some(r)) if r > 0.0 => (300.0, 300.0 / r),
                (None, None, _) => return None, // fallback-content representation
            },
        })
    };
    let (w0, h0) = auto_size(spec_w, spec_h)?;

    // §10.4 min/max. Ratio-preserving table when BOTH dimensions were auto
    // and a ratio exists; a specified axis clamps plainly and re-derives the
    // auto one through the ratio.
    //
    // css-sizing-3 #sizing-values: `min-content`, `max-content` and
    // `fit-content` (with or without a limit) in a min/max property are
    // the box's intrinsic sizes. A replaced element's min- and max-content
    // inline sizes are both its auto width given the other axis (CSS 2
    // §10.3.2; css-sizing-3 #intrinsic-sizes), so every keyword clamps to
    // that width; the block-axis keywords are the automatic height. These
    // are content-box sizes: box-sizing applies to lengths only.
    let limit =
        |prop: &str, basis: Option<f32>, content: &dyn Fn(f32) -> f32, auto: Option<f32>| match dom
            .computed_value_resolved(node, prop)
            .and_then(|v| Len::parse(&v, u, vp))?
        {
            Len::MinContent | Len::MaxContent | Len::FitContent | Len::FitContentLimit(_) => auto,
            length => length.resolve(basis).filter(|&v| v >= 0.0).map(content),
        };
    let auto_w = || auto_size(None, spec_h).map(|(w, _)| w);
    let auto_h = || auto_size(spec_w, None).map(|(_, h)| h);
    let min_w = limit("min-width", cb_w, &content_w, auto_w()).unwrap_or(0.0);
    let max_w = limit("max-width", cb_w, &content_w, auto_w())
        .unwrap_or(f32::INFINITY)
        .max(min_w);
    let min_h = limit("min-height", cb_h, &content_h, auto_h()).unwrap_or(0.0);
    let max_h = limit("max-height", cb_h, &content_h, auto_h())
        .unwrap_or(f32::INFINITY)
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
    // CSS2 #min-max-width and CSSOM View #dom-element-getclientrects:
    // zero and fractional used sizes remain zero/fractional CSS pixels.
    // A one-pixel floor here changed inline flow and fabricated image area.
    let (box_w, box_h) = (box_w.max(0.0), box_h.max(0.0));

    // object-fit (css-images-3 §5.5). Meaningful only with a natural size to
    // map; a reserved-but-undecoded box paints blank regardless. `none` maps
    // to `scale-down` (painting the natural size CLIPPED by the box needs
    // sub-image crop offsets the emission model doesn't carry — the
    // documented paint-model approximation; `scale-down` is its ≤-natural
    // half and identical whenever the image doesn't overflow the box).
    Some(apply_fit(dom, node, natural, box_w, box_h))
}

/// HTML Rendering #images-3: an img with no image to show (no source, or
/// an unusable one) and a non-empty alt represents that text, and the user
/// agent does not expect it to change. In no-quirks and limited-quirks
/// documents it is then a non-replaced phrasing element whose content is the
/// text, whatever its dimension attributes or aspect ratio (which applies to
/// no inline box, css-sizing-4 #aspect-ratio). Only a quirks-mode img that
/// already has both dimensions stays a replaced element (holding the text).
pub(crate) fn represents_alt_text(
    dom: &Dom,
    node: NodeId,
    dimension_source: NodeId,
    url: Option<&str>,
    vp: Vp,
) -> bool {
    if url.is_some()
        || dom.tag_name(node) != Some("img")
        || dom
            .attr(node, "alt")
            .is_none_or(|alt| alt.trim().is_empty())
    {
        return false;
    }
    let quirks = dom.owner_document(node).is_some_and(|document| {
        dom.document_mode(document) == html5ever::tree_builder::QuirksMode::Quirks
    });
    if !quirks {
        return true;
    }
    let u = Units::of(dom, node);
    let sized = |prop: &str| {
        if dom.author_declares(node, prop) {
            matches!(
                dom.computed_value_resolved(node, prop)
                    .and_then(|v| Len::parse(&v, u, vp)),
                Some(Len::Val(_))
            )
        } else {
            dom.attr(dimension_source, prop)
                .and_then(html_dimension)
                .is_some()
        }
    };
    !(sized("width") && sized("height"))
}

/// css-images-3 §5.5 `object-fit` over a used box: the paint rect and crop
/// flag. `fill` (initial) stretches to the box; `cover` fills and crops;
/// `contain` letterboxes centered (`object-position` initial 50% 50%);
/// `none` maps to `scale-down` (sub-image crop offsets don't exist in the
/// emission model — the documented paint-model approximation, identical
/// whenever the image doesn't overflow its box).
pub(crate) fn apply_fit(
    dom: &Dom,
    node: NodeId,
    natural: Option<(f32, f32)>,
    box_w: f32,
    box_h: f32,
) -> Replaced {
    let fit = dom
        .computed_value_resolved(node, "object-fit")
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
                out.paint_w = pw.max(0.0);
                out.paint_h = ph.max(0.0);
                out.off_x = (box_w - out.paint_w) / 2.0;
                out.off_y = (box_h - out.paint_h) / 2.0;
            }
            _ => {}
        }
    }
    out
}

/// HTML #maps-to-the-dimension-property: an img dimension attribute as the
/// `width`/`height` value it maps to, a pixel length or a percentage of the
/// containing block (the rules for parsing dimension values accept zero).
pub(crate) fn dimension_attribute(dom: &Dom, dimension_source: NodeId, name: &str) -> Option<Len> {
    match dom.attr(dimension_source, name).and_then(html_dimension)? {
        HtmlDimension::Pixels(px) => (px >= 0.0).then(|| Len::px(px)),
        HtmlDimension::Percentage(percent) => Some(Len::Val(super::value::Node::Lin {
            k: percent / 100.0,
            b: 0.0,
        })),
    }
}

/// HTML #map-to-the-aspect-ratio-property-(using-dimension-rules): two
/// positive pixel dimension attributes give `aspect-ratio: auto w / h`.
pub(crate) fn dimension_attribute_ratio(dom: &Dom, dimension_source: NodeId) -> Option<f32> {
    let attr = |name: &str| match dom.attr(dimension_source, name).and_then(html_dimension) {
        Some(HtmlDimension::Pixels(px)) if px > 0.0 => Some(px),
        _ => None,
    };
    Some(attr("width")? / attr("height")?).filter(|ratio| ratio.is_finite() && *ratio > 0.0)
}

/// SVG 2 intrinsic sizing: the exact `viewBox` ratio of an SVG image that
/// supplies a ratio but no natural width or height.
fn svg_view_box_ratio(url: Option<&str>) -> Option<f32> {
    url.and_then(crate::img::svg_url_ratio_only)
        .or_else(|| url.and_then(crate::img::svg_ratio_only_get))
        .filter(|&r| r > 0.0)
}

/// SVG 2 §8.12: an inline `svg` retains its exact `viewBox` ratio.
fn inline_svg_ratio(dom: &Dom, node: NodeId) -> Option<f32> {
    (dom.tag_name(node) == Some("svg"))
        .then(|| {
            dom.attr(node, "viewBox")
                .and_then(crate::img::view_box_ratio)
        })
        .flatten()
}

/// The natural ratio `size` uses: a ratio-only SVG's `viewBox` before the
/// decoder's default object size, then `ratio_of`.
pub(crate) fn natural_ratio(
    dom: &Dom,
    node: NodeId,
    dimension_source: NodeId,
    url: Option<&str>,
    natural: Option<(f32, f32)>,
) -> Option<f32> {
    svg_view_box_ratio(url)
        .or_else(|| inline_svg_ratio(dom, node))
        .or_else(|| ratio_of(dom, node, dimension_source, natural))
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
}

/// The natural-ratio chain a replaced element sizes through: intrinsic,
/// else the width/height ATTRIBUTE pair (HTML's pre-decode reservation
/// rule), else CSS `aspect-ratio`.
pub(crate) fn ratio_of(
    dom: &Dom,
    node: NodeId,
    dimension_source: NodeId,
    natural: Option<(f32, f32)>,
) -> Option<f32> {
    // HTML #map-to-the-aspect-ratio-property-(using-dimension-rules): only
    // two pixel dimensions form a ratio.
    let attr = |name: &str| match dom.attr(dimension_source, name).and_then(html_dimension) {
        Some(HtmlDimension::Pixels(px)) if px > 0.0 => Some(px),
        _ => None,
    };
    natural
        .map(|(w, h)| w / h.max(1.0))
        .or_else(|| match (attr("width"), attr("height")) {
            (Some(w), Some(h)) if h > 0.0 => Some(w / h),
            _ => None,
        })
        .or_else(|| {
            dom.computed_value_resolved(node, "aspect-ratio")
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
pub(crate) fn parse_ratio(value: &str) -> Option<f32> {
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

enum HtmlDimension {
    Pixels(f32),
    Percentage(f32),
}

/// HTML #rules-for-parsing-dimension-values: leading whitespace, digits with
/// an optional fraction, then `%` for a percentage; anything after is
/// ignored.
fn html_dimension(input: &str) -> Option<HtmlDimension> {
    let input = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let digits = input
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(input.len());
    let number = &input[..digits];
    let number = match number.find('.') {
        // A second '.' ends the number.
        Some(dot) => match number[dot + 1..].find('.') {
            Some(second) => &number[..dot + 1 + second],
            None => number,
        },
        None => number,
    };
    if !number.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let value = number.trim_end_matches('.').parse::<f32>().ok()?;
    Some(if input[number.len()..].starts_with('%') {
        HtmlDimension::Percentage(value)
    } else {
        HtmlDimension::Pixels(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svg_size(
        attributes: &str,
        natural: Option<(f32, f32)>,
        cb_w: Option<f32>,
        cb_h: Option<f32>,
    ) -> (Replaced, usize) {
        let dom = Dom::parse_document(&format!("<!doctype html><img id='sized' {attributes}>"));
        let node = dom.get_by_id("sized").unwrap();
        let before = crate::img::SVG_URL_RATIO_READS.get();
        let result = size(
            &dom,
            node,
            ImageInput {
                dimension_source: node,
                natural,
                url: Some("data:image/svg+xml,%3Csvg%20viewBox='0%200%20300%20100'/%3E"),
            },
            cb_w,
            cb_h,
            Vp {
                w: 960.0,
                h: 1024.0,
            },
        )
        .unwrap();
        (result, crate::img::SVG_URL_RATIO_READS.get() - before)
    }

    #[test]
    fn definite_svg_dimensions_skip_intrinsic_metadata() {
        for attributes in [
            "style='width:240px;height:120px'",
            "width='240' height='120'",
            "style='width:50%;height:50%'",
        ] {
            let (result, reads) = svg_size(attributes, None, Some(480.0), Some(240.0));
            assert_eq!((result.box_w, result.box_h), (240.0, 120.0));
            assert_eq!(reads, 0, "{attributes}");
        }
    }

    #[test]
    fn definite_svg_dimensions_keep_min_max_and_object_fit() {
        let (result, reads) = svg_size(
            "style='width:240px;height:120px;max-width:180px;min-height:150px;object-fit:contain'",
            Some((320.0, 160.0)),
            Some(480.0),
            Some(240.0),
        );
        assert_eq!((result.box_w, result.box_h), (180.0, 150.0));
        // Fitting uses the supplied natural dimensions, not URL metadata.
        assert_eq!((result.paint_w, result.paint_h), (180.0, 90.0));
        assert_eq!((result.off_x, result.off_y), (0.0, 30.0));
        assert!(!result.crop);
        assert_eq!(reads, 0);
    }

    #[test]
    fn auto_svg_axis_still_reads_exact_intrinsic_ratio() {
        for (style, expected) in [
            ("width:240px;height:auto", (240.0, 80.0)),
            ("width:auto;height:120px", (360.0, 120.0)),
            ("width:auto;height:auto", (480.0, 160.0)),
        ] {
            // Raster fallback deliberately differs from the exact SVG ratio.
            let (result, reads) = svg_size(
                &format!("style='{style}'"),
                Some((152.0, 144.0)),
                Some(480.0),
                Some(240.0),
            );
            assert_eq!((result.box_w, result.box_h), expected, "{style}");
            assert_eq!(reads, 1, "{style}");
        }
    }

    #[test]
    fn indefinite_svg_percentage_still_reads_intrinsic_ratio() {
        let (result, reads) = svg_size("style='width:240px;height:50%'", None, Some(480.0), None);
        assert_eq!((result.box_w, result.box_h), (240.0, 80.0));
        assert_eq!(reads, 1);
        // An authored auto overrides an HTML presentational height hint.
        let (result, reads) = svg_size(
            "width='240' height='120' style='height:auto'",
            None,
            Some(480.0),
            Some(240.0),
        );
        assert_eq!((result.box_w, result.box_h), (240.0, 80.0));
        assert_eq!(reads, 1);
    }

    /// Used box of an `<img>` with the given attributes and a decoded natural
    /// size, in a 480×240 containing block.
    fn image_box(attributes: &str, natural: Option<(f32, f32)>) -> (f32, f32) {
        let dom = Dom::parse_document(&format!("<!doctype html><img id='sized' {attributes}>"));
        let node = dom.get_by_id("sized").unwrap();
        let result = size(
            &dom,
            node,
            ImageInput {
                dimension_source: node,
                natural,
                url: None,
            },
            Some(480.0),
            Some(240.0),
            Vp {
                w: 960.0,
                h: 1024.0,
            },
        )
        .unwrap();
        (result.box_w, result.box_h)
    }

    #[test]
    fn zero_dimension_attributes_are_definite_sizes() {
        // HTML #dimRendering: img width/height map to the dimension
        // properties with the rules for parsing dimension values, which
        // accept zero (only the "ignoring zero" variant rejects it).
        let natural = Some((30.0, 30.0));
        assert_eq!(image_box("width=0 height=0", natural), (0.0, 0.0));
        assert_eq!(image_box("width=40 height=0", natural), (40.0, 0.0));
        assert_eq!(image_box("width=0 height=40", natural), (0.0, 40.0));
        assert_eq!(image_box("width='0.0' height='0px'", natural), (0.0, 0.0));
        // A single zero axis is still a specified size; the other axis
        // follows the natural ratio from it.
        assert_eq!(image_box("width=0", natural), (0.0, 0.0));
        assert_eq!(image_box("width=40 height=40", natural), (40.0, 40.0));
    }

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
