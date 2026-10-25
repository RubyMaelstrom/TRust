//! Paint-backed Largest Contentful Paint (LCP).
//!
//! W3C local snapshots largest-contentful-paint@f79fc4c8 (2026-09-06),
//! #sec-report-largest-contentful-paint / #sec-effective-visual-size, and
//! paint-timing@053f575b #sec-recording-paint-timing. Only a completed rendering
//! update calls `report`: geometry reads do not manufacture paints. The
//! presentation timestamp is unavailable, as Paint Timing explicitly permits;
//! the entry uses the rendering-update end timestamp instead.
//!
//! Paint Timing allows nested browsing contexts to opt out. Currently only
//! the top-level Document reports; the display list's child-frame content is
//! deliberately not attributed to its parent.

use super::*;
use crate::core::{CssPoint, CssSize, ViewportMetrics};
use crate::render::{Affine2d, CssRect, DisplayCommand, ImageHandle, PaintColor, Scene};
use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Clone, Debug)]
struct ImageData {
    width: u32,
    height: u32,
    bytes: usize,
    loaded_at: f64,
    request_key: String,
}

#[derive(Clone, Debug)]
struct Candidate {
    node: usize,
    size: f64,
    width: f32,
    height: f32,
    url: String,
    loaded_at: f64,
    id: String,
    request_key: String,
}

fn transformed(rect: CssRect, transform: Affine2d) -> CssRect {
    let points = [
        CssPoint::new(rect.x, rect.y),
        CssPoint::new(rect.x + rect.width, rect.y),
        CssPoint::new(rect.x, rect.y + rect.height),
        CssPoint::new(rect.x + rect.width, rect.y + rect.height),
    ]
    .map(|point| transform.map_point(point));
    let left = points
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let top = points
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let right = points
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let bottom = points
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    CssRect::new(left, top, (right - left).max(0.), (bottom - top).max(0.))
}

fn exposed(dom: &Dom, node: usize) -> bool {
    if !dom.is_valid(node) || dom.tag_name(node).is_none() {
        return false;
    }
    let mut root = node;
    while let Some(parent) = dom.node(root).parent {
        root = parent;
    }
    root == dom.document() && dom.owner_document(node) == Some(root)
}

/// CSS 2.2 §10.1: inline text's containing block is its nearest block
/// container ancestor. Inline wrappers do not split a paragraph's candidate.
fn text_owner(dom: &Dom, mut text: usize) -> Option<usize> {
    loop {
        text = dom.node(text).parent?;
        if dom.tag_name(text).is_none() {
            continue;
        }
        let display = dom.effective_display(text);
        let inline = display.as_deref().is_some_and(|display| {
            matches!(
                display,
                "inline" | "contents" | "ruby" | "ruby-base" | "ruby-text"
            )
        });
        if !inline {
            return Some(text);
        }
    }
}

fn nontransparent(color: PaintColor) -> bool {
    !matches!(color, PaintColor::Rgba(_, _, _, 0))
}

fn text_stroke_is_painted(dom: &Dom, text: usize) -> bool {
    let Some(element) = dom.node(text).parent else {
        return false;
    };
    let width = dom
        .computed_value_resolved(element, "-webkit-text-stroke-width")
        .and_then(|value| {
            crate::dom::text_stroke_width_px(
                &value,
                crate::layout2::Units::of(dom, element),
                dom.viewport_px(),
            )
        });
    if width.is_none_or(|width| width <= 0.) {
        return false;
    }
    dom.computed_value_resolved(element, "-webkit-text-stroke-color")
        .and_then(|value| {
            let color = dom
                .computed_value_resolved(element, "color")
                .unwrap_or_else(|| "black".into());
            PaintColor::parse_css(&crate::dom::replace_css_current_color(&value, &color))
        })
        .is_some_and(nontransparent)
}

/// LCP's effective visual size algorithm. Dimensions are rounded up before
/// area comparison; upscaling discounts area but leaves width/height intact
/// for the subsequent three-CSS-pixel candidate replacement rule.
fn effective_size(
    visible: CssRect,
    concrete: CssRect,
    viewport: CssRect,
    image: Option<&ImageData>,
) -> Option<(f64, f32, f32)> {
    let width = visible.width.ceil();
    let height = visible.height.ceil();
    let mut size = f64::from(width) * f64::from(height);
    if !size.is_finite()
        || size <= 0.
        || size == f64::from(viewport.width) * f64::from(viewport.height)
    {
        return None;
    }
    if let Some(image) = image {
        if (image.bytes as f64) < size * 0.004 || image.width == 0 || image.height == 0 {
            return None;
        }
        let scale = f64::from(concrete.width) * f64::from(concrete.height)
            / (f64::from(image.width) * f64::from(image.height));
        if scale > 1. {
            size /= scale;
        }
    }
    Some((size, width, height))
}

fn eligible_image(dom: &Dom, node: usize, source: &str, base: &url::Url) -> bool {
    if !exposed(dom, node) {
        return false;
    }
    if matches!(dom.tag_name(node), Some("img" | "image" | "video")) {
        return true;
    }
    dom.computed_value_resolved(node, "background-image")
        .is_some_and(|value| {
            crate::dom::css_urls(&value)
                .iter()
                .any(|url| base.join(url).is_ok_and(|url| url.as_str() == source) || url == source)
        })
}

fn svg_image_is_painted(dom: &Dom, node: usize, outer: usize) -> bool {
    if !exposed(dom, node) || dom.visibility_hidden(node) {
        return false;
    }
    let mut current = Some(node);
    while let Some(node) = current {
        if dom.computed_value_resolved(node, "display").as_deref() == Some("none")
            || dom
                .computed_value_resolved(node, "opacity")
                .and_then(|v| v.parse::<f32>().ok())
                .is_some_and(|v| v <= 0.)
            || matches!(
                dom.tag_name(node),
                Some("defs" | "symbol" | "clipPath" | "mask" | "marker" | "pattern" | "filter")
            )
        {
            return false;
        }
        if node == outer {
            return true;
        }
        current = dom.node(node).parent;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn collect(
    dom: &Dom,
    paint: &crate::render::PagePaint,
    viewport: crate::layout2::Viewport,
    scroll: CssPoint,
    elapsed: f32,
    base: &url::Url,
    mut image_data: impl FnMut(usize, &str) -> Option<ImageData>,
) -> Vec<Candidate> {
    let viewport_rect = CssRect::new(0., 0., viewport.width, viewport.height);
    // The normal frontend composition resolves scroll, fixed/sticky offsets,
    // transformed descendants and animation scopes before collecting bounds.
    let mut scene = Scene {
        viewport: ViewportMetrics {
            css: CssSize::new(viewport.width, viewport.height),
            ..Default::default()
        },
        primitives: Vec::new(),
        controls: Vec::new(),
        content_viewport: viewport_rect,
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    };
    scene.append_page_at(paint, scroll, elapsed);
    let mut sources: HashMap<_, Cow<'_, str>> = paint
        .image_requests
        .iter()
        .map(|request| (request.handle, Cow::Borrowed(request.source.as_str())))
        .collect();
    let mut transforms = vec![Affine2d::IDENTITY];
    let mut clips = vec![viewport_rect];
    let mut layers = vec![(1.0_f32, viewport_rect)];
    let mut text: Vec<(usize, CssRect)> = Vec::new();
    let mut images: Vec<(usize, ImageHandle, CssRect, CssRect)> = Vec::new();
    let mut text_indices = HashMap::new();
    let mut image_indices = HashMap::new();
    let mut svg_image_data = HashMap::new();
    for command in &scene.primitives {
        let transform = *transforms.last().unwrap();
        let clip = *clips.last().unwrap();
        let (opacity, layer_clip) = *layers.last().unwrap();
        match command {
            DisplayCommand::PushTransform(next) => transforms.push(transform.then(*next)),
            DisplayCommand::PopTransform if transforms.len() > 1 => {
                transforms.pop();
            }
            DisplayCommand::PushClip(shape) => {
                let next = crate::render::shape_css_bounds(shape)
                    .and_then(|rect| {
                        crate::render::intersect_css_rect(clip, transformed(rect, transform))
                    })
                    .unwrap_or_default();
                clips.push(next);
            }
            DisplayCommand::PopClip if clips.len() > 1 => {
                clips.pop();
            }
            DisplayCommand::PushLayer(layer) => layers.push((
                opacity * layer.opacity,
                layer.clip.map_or(layer_clip, |rect| {
                    crate::render::intersect_css_rect(layer_clip, transformed(rect, transform))
                        .unwrap_or_default()
                }),
            )),
            DisplayCommand::PopLayer if layers.len() > 1 => {
                layers.pop();
            }
            DisplayCommand::GlyphRun {
                origin,
                shaped,
                color,
                shadows,
                clip: own_clip,
                selection,
                node,
                ..
            } if opacity > 0.
                && !shaped
                    .text
                    .chars()
                    .all(|ch| matches!(ch, '\t' | '\n' | '\u{000c}' | '\r' | ' ')) =>
            {
                // Generated/control labels have no DOM Text sources. Font
                // block-period glyphs are absent from the painted display list.
                let source_nodes: Vec<_> = match selection
                    .sources
                    .as_ref()
                    .or(selection.paint_timing_sources.as_ref())
                {
                    Some(sources) => sources.iter().map(|source| source.node).collect(),
                    None if dom.is_valid(*node)
                        && matches!(
                            &dom.node(*node).data,
                            crate::dom::NodeData::Text(_) | crate::dom::NodeData::CData(_)
                        ) =>
                    {
                        vec![*node]
                    }
                    None => continue,
                };
                if !nontransparent(*color)
                    && !shadows.iter().any(|shadow| nontransparent(shadow.color))
                    && !source_nodes
                        .iter()
                        .any(|source| text_stroke_is_painted(dom, *source))
                {
                    continue;
                }
                let rect = CssRect::new(origin.x, origin.y, shaped.advance, shaped.line_height);
                let rect = match own_clip {
                    Some(own) => crate::render::intersect_css_rect(rect, *own),
                    None => Some(rect),
                };
                let Some(rect) = rect
                    .map(|rect| transformed(rect, transform))
                    .and_then(|rect| crate::render::intersect_css_rect(rect, clip))
                    .and_then(|rect| crate::render::intersect_css_rect(rect, layer_clip))
                else {
                    continue;
                };
                for source in source_nodes {
                    let Some(owner) = text_owner(dom, source).filter(|node| exposed(dom, *node))
                    else {
                        continue;
                    };
                    if let Some(&index) = text_indices.get(&owner) {
                        let (_, old) = &mut text[index];
                        *old = crate::render::union_css_rects(*old, rect);
                    } else {
                        text_indices.insert(owner, text.len());
                        text.push((owner, rect));
                    }
                }
            }
            DisplayCommand::Image {
                rect,
                handle,
                clip: own_clip,
                node,
                ..
            } if opacity > 0. => {
                // Inline SVG paints as a retained raster snapshot. Attribute
                // its rendered <image> resources to their own DOM elements,
                // never to that implementation-owned data URL. SVG 2
                // #ImageElement / #BoundingBoxes / getScreenCTM, c403ca46.
                if dom.is_valid(*node)
                    && dom.tag_name(*node) == Some("svg")
                    && exposed(dom, *node)
                    && sources
                        .get(handle)
                        .is_some_and(|source| crate::img::is_document_svg_source(source))
                {
                    let outer_rect = transformed(*rect, transform);
                    let viewport_clip = crate::render::intersect_css_rect(outer_rect, clip)
                        .and_then(|rect| crate::render::intersect_css_rect(rect, layer_clip));
                    let viewport_clip = match own_clip {
                        Some(own) => viewport_clip.and_then(|rect| {
                            crate::render::intersect_css_rect(rect, transformed(*own, transform))
                        }),
                        None => viewport_clip,
                    };
                    let Some(viewport_clip) = viewport_clip else {
                        continue;
                    };
                    let outer = (f64::from(rect.width), f64::from(rect.height));
                    let origin = transform.then(Affine2d::translate(rect.x, rect.y));
                    for image in dom
                        .descendants(*node)
                        .filter(|image| dom.tag_name(*image) == Some("image"))
                    {
                        if !svg_image_is_painted(dom, image, *node) {
                            continue;
                        }
                        let Some(href) = dom
                            .attr(image, "href")
                            .or_else(|| dom.attr(image, "xlink:href"))
                        else {
                            continue;
                        };
                        let Ok(source) = dom.resource_base_url(image, base).join(href.trim())
                        else {
                            continue;
                        };
                        let source = source.to_string();
                        let Some(data) = image_data(image, &source) else {
                            continue;
                        };
                        let Some(matrix) = dom.svg_ctm(image, true, outer) else {
                            continue;
                        };
                        let matrix = origin.then(Affine2d(matrix.map(|v| v as f32)));
                        let bbox = dom.svg_bbox(
                            image,
                            crate::dom::svg_geometry::BoundingBoxOptions::OBJECT,
                            outer,
                        );
                        let concrete = transformed(
                            CssRect::new(
                                bbox[0] as f32,
                                bbox[1] as f32,
                                bbox[2] as f32,
                                bbox[3] as f32,
                            ),
                            matrix,
                        );
                        let Some(visible) =
                            crate::render::intersect_css_rect(concrete, viewport_clip)
                        else {
                            continue;
                        };
                        let handle = ImageHandle::for_source(&source);
                        sources.insert(handle, Cow::Owned(source));
                        svg_image_data.insert((image, handle), data);
                        if let Some(&index) = image_indices.get(&(image, handle)) {
                            let (_, _, old, _) = &mut images[index];
                            *old = crate::render::union_css_rects(*old, visible);
                        } else {
                            image_indices.insert((image, handle), images.len());
                            images.push((image, handle, visible, concrete));
                        }
                    }
                    continue;
                }
                let Some(source) = sources.get(handle) else {
                    continue;
                };
                if !eligible_image(dom, *node, source, base) {
                    continue;
                }
                let concrete = transformed(*rect, transform);
                let rect = match own_clip {
                    Some(own) => crate::render::intersect_css_rect(*rect, *own),
                    None => Some(*rect),
                };
                let Some(rect) = rect
                    .map(|rect| transformed(rect, transform))
                    .and_then(|rect| crate::render::intersect_css_rect(rect, clip))
                    .and_then(|rect| crate::render::intersect_css_rect(rect, layer_clip))
                else {
                    continue;
                };
                if let Some(&index) = image_indices.get(&(*node, *handle)) {
                    let (_, _, visible, _) = &mut images[index];
                    *visible = crate::render::union_css_rects(*visible, rect);
                } else {
                    image_indices.insert((*node, *handle), images.len());
                    images.push((*node, *handle, rect, concrete));
                }
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    // The specification examines images before text and retains the first
    // entry on equal sizes. Keep stable paint order within each category.
    for (node, handle, visible, concrete) in images {
        let source = sources[&handle].as_ref();
        let Some(image) = svg_image_data
            .remove(&(node, handle))
            .or_else(|| image_data(node, source))
        else {
            continue;
        };
        let (size, width, height) =
            effective_size(visible, concrete, viewport_rect, Some(&image)).unwrap_or((0., 0., 0.));
        result.push(Candidate {
            node,
            size,
            width,
            height,
            url: source.into(),
            loaded_at: image.loaded_at,
            id: dom.attr(node, "id").unwrap_or_default().into(),
            request_key: image.request_key,
        });
    }
    for (node, visible) in text {
        let (size, width, height) =
            effective_size(visible, visible, viewport_rect, None).unwrap_or((0., 0., 0.));
        result.push(Candidate {
            node,
            size,
            width,
            height,
            url: String::new(),
            loaded_at: 0.,
            id: dom.attr(node, "id").unwrap_or_default().into(),
            request_key: "text".into(),
        });
    }
    result
}

/// Report the end of an actual page rendering update. The private JS control
/// owns per-Document first-paint sets, candidate selection and observer queues.
pub(super) fn report(engine: &mut trust_js::Engine, paint: &crate::render::PagePaint) {
    let scroll = engine.scope(|cx| {
        let wanted = host_call_trust(cx, "wantsLargestContentfulPaint", &[])
            .is_ok_and(|value| matches!(value.kind(), ValueKind::Bool(true)));
        if !wanted {
            return None;
        }
        let mut scroll = |name| {
            host_call_trust(cx, name, &[])
                .ok()
                .and_then(|v| v.as_num_opt())
                .unwrap_or(0.) as f32
        };
        let x = scroll("windowScrollX");
        Some(CssPoint::new(x, scroll("windowScrollY")))
    });
    let Some(scroll) = scroll else {
        return;
    };
    let now = crate::performance::now_ms();
    let Some(state) = engine.host_mut::<HostState>() else {
        return;
    };
    let dom = state.dom.borrow();
    let document = dom.document();
    let viewport = state.viewport.get();
    let images = state.images.borrow();
    let store = state.network.as_ref().map(|network| network.cache.images());
    let candidates = collect(
        &dom,
        paint,
        viewport,
        scroll,
        ((now - paint.timeline_origin_ms.unwrap_or(now)) / 1000.) as f32,
        &state.base,
        |node, source| {
            let (width, height) = images.get(source).copied().or_else(|| {
                source
                    .starts_with("data:")
                    .then(|| {
                        crate::img::decode_data_url(source)
                            .and_then(|bytes| crate::img::natural_size(&bytes).ok())
                    })
                    .flatten()
            })?;
            if let Some((token, payload)) = state
                .element_image_payloads
                .get(&node)
                .and_then(|token| {
                    state
                        .image_payloads
                        .get(token)
                        .map(|payload| (*token, payload))
                })
                .filter(|(_, payload)| payload.source_handle == ImageHandle::for_source(source))
            {
                return Some(ImageData {
                    width,
                    height,
                    bytes: payload.len(),
                    loaded_at: payload.loaded_at,
                    request_key: format!("{:x}:{token}", ImageHandle::for_source(source).0),
                });
            }
            if source.starts_with("data:") {
                let bytes = crate::img::decode_data_url(source)?;
                return Some(ImageData {
                    width,
                    height,
                    bytes: bytes.len(),
                    loaded_at: now,
                    request_key: format!("{:x}", ImageHandle::for_source(source).0),
                });
            }
            let response = store.as_ref()?.ready_for_paint_timing(source)?;
            Some(ImageData {
                width,
                height,
                bytes: response.body.len(),
                loaded_at: response
                    .timing
                    .as_ref()
                    .filter(|timing| timing.response_end > 0.)
                    .map_or(now, |timing| timing.response_end),
                request_key: format!(
                    "{:x}:{:p}",
                    ImageHandle::for_source(source).0,
                    Arc::as_ptr(&response)
                ),
            })
        },
    );
    drop(images);
    drop(dom);
    engine.scope(|ctx| {
        let values = ctx.make_array(
            candidates
                .into_iter()
                .map(|candidate| {
                    ctx.make_array(vec![
                        Value::Num(candidate.node as f64),
                        Value::Num(candidate.size),
                        Value::Num(f64::from(candidate.width)),
                        Value::Num(f64::from(candidate.height)),
                        ctx.string(candidate.url),
                        Value::Num(candidate.loaded_at),
                        ctx.string(candidate.id),
                        Value::Num(document as f64),
                        ctx.string(candidate.request_key),
                    ])
                })
                .collect(),
        );
        let _ = host_call_trust(
            ctx,
            "recordLargestContentfulPaint",
            &[values, Value::Num(now), Value::Num(document as f64)],
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcp_effective_size_rounds_clips_discounts_upscaling_and_filters_placeholders() {
        let viewport = CssRect::new(0., 0., 500., 400.);
        let concrete = CssRect::new(0., 0., 200., 100.);
        let visible = CssRect::new(0., 0., 99.1, 49.1);
        let mut image = ImageData {
            width: 100,
            height: 50,
            bytes: 1000,
            loaded_at: 1.,
            request_key: "test".into(),
        };
        assert_eq!(
            effective_size(visible, concrete, viewport, Some(&image)),
            Some((1250., 100., 50.))
        );
        image.bytes = 19;
        assert!(effective_size(visible, concrete, viewport, Some(&image)).is_none());
        assert!(effective_size(viewport, viewport, viewport, None).is_none());
        image.bytes = 1000;
        image.width = 0;
        assert!(effective_size(visible, concrete, viewport, Some(&image)).is_none());
    }

    #[test]
    fn lcp_real_paint_aggregates_unselectable_inline_text_and_excludes_hidden_content() {
        let base = url::Url::parse("https://example.test/").unwrap();
        let dom = Dom::parse_document(
            "<style>body{margin:0}p{margin:0;font-size:20px;user-select:none}#hidden{opacity:0}</style><p id=p>alpha <span>beta</span></p><p id=hidden>invisible text</p>",
        );
        let viewport = crate::layout2::Viewport::new(500., 400.);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            viewport,
            &[],
            &Default::default(),
            &Default::default(),
        );
        let candidates = collect(
            &dom,
            &layout.paint,
            viewport,
            CssPoint::default(),
            0.,
            &base,
            |_, _| None,
        );
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert_eq!(candidates[0].id, "p");
        assert!(candidates[0].size > 100.);
        assert_eq!(candidates[0].loaded_at, 0.);
        assert!(candidates[0].url.is_empty());
    }

    #[test]
    fn lcp_painted_images_wait_for_resource_and_apply_transform_and_overflow_clip() {
        let base = url::Url::parse("https://example.test/").unwrap();
        let dom = Dom::parse_document(
            "<style>body{margin:0}#clip{width:100px;height:50px;overflow:hidden}img{display:block;width:100px;height:50px;transform:scale(2);transform-origin:top left}</style><div id=clip><img id=hero src=/photo.png></div>",
        );
        let viewport = crate::layout2::Viewport::new(500., 400.);
        let sizes = HashMap::from([("https://example.test/photo.png".into(), (100, 50))]);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            viewport,
            &[],
            &Default::default(),
            &sizes,
        );
        assert!(
            collect(
                &dom,
                &layout.paint,
                viewport,
                CssPoint::default(),
                0.,
                &base,
                |_, _| None
            )
            .is_empty()
        );
        let candidates = collect(
            &dom,
            &layout.paint,
            viewport,
            CssPoint::default(),
            0.,
            &base,
            |_, _| {
                Some(ImageData {
                    width: 100,
                    height: 50,
                    bytes: 1000,
                    loaded_at: 42.,
                    request_key: "test".into(),
                })
            },
        );
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert_eq!(candidates[0].id, "hero");
        assert_eq!(candidates[0].url, "https://example.test/photo.png");
        assert_eq!(candidates[0].loaded_at, 42.);
        assert_eq!(candidates[0].size, 1250.);
    }

    #[test]
    fn lcp_transparent_text_counts_only_with_visible_shadow_or_stroke() {
        let base = url::Url::parse("https://example.test/").unwrap();
        let dom = Dom::parse_document(
            "<style>p{color:transparent;font:20px sans-serif;margin:0}#shadow{text-shadow:1px 1px black}#stroke{-webkit-text-stroke:1px black}</style><p id=clear>clear</p><p id=shadow>shadow</p><p id=stroke>stroke</p>",
        );
        let viewport = crate::layout2::Viewport::new(500., 400.);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            viewport,
            &[],
            &Default::default(),
            &Default::default(),
        );
        let candidates = collect(
            &dom,
            &layout.paint,
            viewport,
            CssPoint::default(),
            0.,
            &base,
            |_, _| None,
        );
        let ids: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect();
        assert_eq!(ids, ["shadow", "stroke"]);
    }

    #[test]
    fn lcp_inline_svg_image_uses_the_element_resource_and_viewport_transform() {
        let base = url::Url::parse("https://example.test/").unwrap();
        let source = "data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%22100%22%20height%3D%2250%22%3E%3Crect%20width%3D%22100%22%20height%3D%2250%22%20fill%3D%22blue%22%2F%3E%3C%2Fsvg%3E";
        let dom = Dom::parse_document(&format!(
            "<style>body{{margin:0}}</style><svg xmlns='http://www.w3.org/2000/svg' width='200' height='120' viewBox='0 0 100 60'><defs><image id='defined' href='{source}' width='90' height='50'/></defs><g transform='translate(5,10)'><image id='inner' href='{source}' x='10' y='5' width='50' height='25'/></g><image id='hidden' opacity='0' href='{source}' width='90' height='50'/></svg>"
        ));
        let viewport = crate::layout2::Viewport::new(500., 400.);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            viewport,
            &[],
            &Default::default(),
            &Default::default(),
        );
        let candidates = collect(
            &dom,
            &layout.paint,
            viewport,
            CssPoint::default(),
            0.,
            &base,
            |_, _| {
                Some(ImageData {
                    width: 100,
                    height: 50,
                    bytes: 500,
                    loaded_at: 42.,
                    request_key: "svg-image".into(),
                })
            },
        );
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert_eq!(candidates[0].id, "inner");
        assert_eq!(candidates[0].url, source);
        assert_eq!(candidates[0].size, 5000.);
    }
}
