//! Thin Vello CPU adapter.
//!
//! This is the only module that knows Vello/Glifo/Peniko/Kurbo types. Render
//! contexts, glyph resources, decoded-image registrations and target pixmaps
//! survive across frames; layout and browser code only see TRust commands.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use vello_cpu::color::palette::css::{
    BLACK, BLUE, CYAN, DARK_GRAY, GRAY, LIGHT_GRAY, WHITE, YELLOW,
};
use vello_cpu::kurbo::{Affine, BezPath, Cap, Diagonal2, Rect, Stroke};
use vello_cpu::peniko::{
    ColorStop, Compose, Gradient, ImageBrush, ImageQuality, ImageSampler, Mix,
};
use vello_cpu::{ImageSource, Pixmap, RenderContext, Resources};

use super::{
    Affine2d, BlendMode, CssRect, DecorationStyle, DisplayCommand, ImageFit, ImageHandle,
    ImageSampling, LineCap, PaintBrush, PaintColor, PaintShape, PathElement, Primitive,
    RasterBackend, RasterFrame, Scene, StrokeStyle, is_desktop_heart_image_handle,
};
use crate::core::PhysicalSize;

pub(super) const MAX_REGISTERED_IMAGES: usize = 256;

struct CachedImage {
    source: ImageSource,
    width: u32,
    height: u32,
    revision: u64,
    last_used_frame: u64,
}

pub struct VelloCpuRenderer {
    context: RenderContext,
    resources: Resources,
    pixmap: Pixmap,
    presented: Vec<u32>,
    rgba: Vec<u8>,
    size: PhysicalSize,
    images: HashMap<ImageHandle, CachedImage>,
    frame_id: u64,
}

impl VelloCpuRenderer {
    pub fn new() -> Self {
        Self {
            context: RenderContext::new(1, 1),
            resources: Resources::new(),
            pixmap: Pixmap::new(1, 1),
            presented: vec![0],
            rgba: vec![0; 4],
            size: PhysicalSize::new(1, 1),
            images: HashMap::new(),
            frame_id: 0,
        }
    }

    fn prepare(&mut self, size: PhysicalSize) -> Result<(), String> {
        if size.is_empty() {
            return Err(String::from("cannot render an empty framebuffer"));
        }
        let width = u16::try_from(size.width)
            .map_err(|_| format!("framebuffer width {} exceeds Vello CPU limit", size.width))?;
        let height = u16::try_from(size.height)
            .map_err(|_| format!("framebuffer height {} exceeds Vello CPU limit", size.height))?;
        if self.size != size {
            self.context.reset_and_resize(width, height);
            self.pixmap.resize(width, height);
            self.presented
                .resize(size.width as usize * size.height as usize, 0);
            self.rgba
                .resize(size.width as usize * size.height as usize * 4, 0);
            self.size = size;
        } else {
            self.context.reset();
        }
        Ok(())
    }

    /// Deterministic headless output using the same retained backend as the
    /// window. Pixels are straight-alpha RGBA8 in row-major order.
    pub fn render_rgba(&mut self, scene: &Scene) -> Result<OwnedRgbaFrame, String> {
        self.rasterize(scene)?;
        Ok(OwnedRgbaFrame {
            size: self.size,
            pixels: self.rgba.clone(),
        })
    }

    fn rasterize(&mut self, scene: &Scene) -> Result<(), String> {
        self.prepare(scene.viewport.physical)?;
        self.frame_id = self.frame_id.wrapping_add(1);
        let live_images: HashSet<_> = scene
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::Image { handle, .. } => Some(*handle),
                _ => None,
            })
            .collect();
        let stale: Vec<_> = self
            .images
            .keys()
            .copied()
            .filter(|handle| {
                !live_images.contains(handle) && !is_desktop_heart_image_handle(*handle)
            })
            .collect();
        for handle in stale {
            if let Some(CachedImage {
                source: ImageSource::OpaqueId { id, .. },
                ..
            }) = self.images.remove(&handle)
            {
                self.resources.destroy_image(id);
            }
        }
        let device = Affine::scale(scene.viewport.scale_factor.get());
        let mut transforms = vec![device];
        let mut logical_transforms = vec![Affine2d::IDENTITY];
        let mut visible_clips = vec![CssRect::new(
            0.0,
            0.0,
            scene.viewport.css.width,
            scene.viewport.css.height,
        )];
        self.context.set_transform(device);
        for command in &scene.primitives {
            match command {
                DisplayCommand::Fill { shape, brush } => {
                    if !shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                        0.0,
                    ) {
                        continue;
                    }
                    self.set_brush(brush);
                    self.context.fill_path(&shape_path(shape));
                }
                DisplayCommand::Stroke {
                    shape,
                    brush,
                    style,
                } => {
                    if !shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                        style.width.max(0.0) / 2.0,
                    ) {
                        continue;
                    }
                    self.set_brush(brush);
                    self.context.set_stroke(vello_stroke(style));
                    self.context.stroke_path(&shape_path(shape));
                }
                DisplayCommand::PushClip(shape) => {
                    self.context.push_clip_path(&shape_path(shape));
                    let current = *visible_clips.last().unwrap();
                    let next = shape_bounds(shape)
                        .map(|bounds| {
                            transformed_bounds(bounds, *logical_transforms.last().unwrap())
                        })
                        .and_then(|bounds| intersect_rect(current, bounds))
                        .unwrap_or_default();
                    visible_clips.push(next);
                }
                DisplayCommand::PopClip => {
                    self.context.pop_clip_path();
                    if visible_clips.len() > 1 {
                        visible_clips.pop();
                    }
                }
                DisplayCommand::PushTransform(transform) => {
                    let next = *transforms.last().unwrap() * vello_affine(*transform);
                    transforms.push(next);
                    self.context.set_transform(next);
                    let logical = logical_transforms.last().unwrap().then(*transform);
                    logical_transforms.push(logical);
                }
                DisplayCommand::PopTransform => {
                    if transforms.len() > 1 {
                        transforms.pop();
                    }
                    self.context.set_transform(*transforms.last().unwrap());
                    if logical_transforms.len() > 1 {
                        logical_transforms.pop();
                    }
                }
                DisplayCommand::PushLayer(layer) => self.context.push_layer(
                    None,
                    Some(vello_blend(layer.blend)),
                    Some(layer.opacity.clamp(0.0, 1.0)),
                    None,
                    None,
                ),
                DisplayCommand::PopLayer => self.context.pop_layer(),
                DisplayCommand::BeginSticky(_) | DisplayCommand::EndSticky => {
                    // Scene composition resolves these to Push/PopTransform.
                }
                DisplayCommand::Shadow {
                    shape,
                    color,
                    offset,
                    blur_radius,
                    spread,
                    inset,
                } => {
                    let expansion = spread.max(0.0) + blur_radius.max(0.0) * 2.0;
                    let shifted = offset_shape(shape, offset.x, offset.y, *spread);
                    if !shape_is_visible(
                        &shifted,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                        expansion,
                    ) {
                        continue;
                    }
                    self.context.set_paint(vello_color(*color));
                    if let Some((rect, radius)) = simple_rounded_rect(&shifted) {
                        if *inset {
                            self.context.push_clip_path(&shape_path(shape));
                        }
                        self.context.fill_blurred_rounded_rect(
                            &rect,
                            radius,
                            blur_radius.max(0.0) / 2.0,
                            *inset,
                        );
                        if *inset {
                            self.context.pop_clip_path();
                        }
                    } else {
                        // Vello CPU's direct blur primitive currently accepts
                        // rounded rectangles, not arbitrary paths.
                        self.context.fill_path(&shape_path(&shifted));
                    }
                }
                DisplayCommand::HitRegion(_) => {}
                Primitive::FillRect { rect, color } => {
                    if !rect_is_visible(
                        *rect,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                    ) {
                        continue;
                    }
                    self.context.set_paint(vello_color(*color));
                    self.context.fill_rect(&vello_rect(*rect));
                }
                Primitive::FillPolygon { points, color } => {
                    let Some(first) = points.first() else {
                        continue;
                    };
                    let bounds = point_bounds(points.iter().copied()).unwrap_or_default();
                    if !rect_is_visible(
                        bounds,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                    ) {
                        continue;
                    }
                    let mut path = BezPath::new();
                    path.move_to((f64::from(first.x), f64::from(first.y)));
                    for point in &points[1..] {
                        path.line_to((f64::from(point.x), f64::from(point.y)));
                    }
                    path.close_path();
                    self.context.set_paint(vello_color(*color));
                    self.context.fill_path(&path);
                }
                Primitive::GlyphRun {
                    origin,
                    shaped,
                    color,
                    decoration,
                    clip,
                    ..
                } => {
                    if !rect_is_visible(
                        CssRect::new(
                            origin.x,
                            origin.y,
                            shaped.advance.max(1.0),
                            shaped.line_height.max(1.0),
                        ),
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                    ) {
                        continue;
                    }
                    if let Some(clip) = clip {
                        self.context.push_clip_path(&rect_path(*clip));
                    }
                    self.context.set_paint(vello_color(*color));
                    for run in &shaped.runs {
                        let glyphs: Vec<vello_cpu::Glyph> = run
                            .glyphs
                            .iter()
                            .map(|glyph| vello_cpu::Glyph {
                                id: glyph.id,
                                x: origin.x + glyph.x,
                                y: origin.y + glyph.y,
                            })
                            .collect();
                        let mut glyphs_builder = self
                            .context
                            .glyph_run(&mut self.resources, run.font.data())
                            .font_size(run.font_size)
                            .normalized_coords(&run.normalized_coords);
                        if run.synth_bold {
                            let amount = f64::from(run.font_size) * 0.025;
                            glyphs_builder = glyphs_builder.font_embolden(
                                glifo::FontEmbolden::new(Diagonal2::new(amount, amount)),
                            );
                        }
                        if let Some(degrees) = run.synth_skew_degrees {
                            glyphs_builder = glyphs_builder.glyph_transform(Affine::skew(
                                f64::from(degrees).to_radians().tan(),
                                0.0,
                            ));
                        }
                        glyphs_builder.fill_glyphs(glyphs.into_iter());
                    }
                    self.context.set_paint(vello_color(decoration.color));
                    paint_decorations(&mut self.context, *origin, shaped, decoration.style);
                    if clip.is_some() {
                        self.context.pop_clip_path();
                    }
                }
                Primitive::Image {
                    rect,
                    handle,
                    fit,
                    sampling,
                    clip,
                    ..
                } => {
                    if !rect_is_visible(
                        *rect,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                    ) {
                        continue;
                    }
                    if let Some(clip) = clip {
                        self.context.push_clip_path(&rect_path(*clip));
                    }
                    self.paint_image(scene, *handle, *rect, *fit, *sampling)?;
                    if clip.is_some() {
                        self.context.pop_clip_path();
                    }
                }
            }
        }
        self.context.flush();
        self.context.render(&mut self.pixmap, &mut self.resources);
        for ((target, rgba), source) in self
            .presented
            .iter_mut()
            .zip(self.rgba.chunks_exact_mut(4))
            .zip(self.pixmap.data())
        {
            *target = u32::from(source.r) << 16 | u32::from(source.g) << 8 | u32::from(source.b);
            if source.a == 0 {
                rgba.copy_from_slice(&[0, 0, 0, 0]);
            } else {
                let unpremultiply = |component: u8| {
                    ((u16::from(component) * 255 + u16::from(source.a) / 2) / u16::from(source.a))
                        .min(255) as u8
                };
                rgba.copy_from_slice(&[
                    unpremultiply(source.r),
                    unpremultiply(source.g),
                    unpremultiply(source.b),
                    source.a,
                ]);
            }
        }
        Ok(())
    }

    fn set_brush(&mut self, brush: &PaintBrush) {
        match brush {
            PaintBrush::Solid(color) => self.context.set_paint(vello_color(*color)),
            PaintBrush::LinearGradient { start, end, stops } => {
                let stops = vello_stops(stops);
                self.context.set_paint(
                    Gradient::new_linear(
                        (f64::from(start.x), f64::from(start.y)),
                        (f64::from(end.x), f64::from(end.y)),
                    )
                    .with_stops(stops.as_slice()),
                );
            }
            PaintBrush::RadialGradient {
                center,
                radius,
                stops,
            } => {
                let stops = vello_stops(stops);
                self.context.set_paint(
                    Gradient::new_radial((f64::from(center.x), f64::from(center.y)), *radius)
                        .with_stops(stops.as_slice()),
                );
            }
        }
    }

    fn paint_image(
        &mut self,
        scene: &Scene,
        handle: ImageHandle,
        rect: CssRect,
        fit: ImageFit,
        sampling: ImageSampling,
    ) -> Result<(), String> {
        let store_revision = scene.image_store.revision(handle);
        let changed = self
            .images
            .get(&handle)
            .is_some_and(|image| store_revision.is_some_and(|revision| revision != image.revision));
        if changed
            && let Some(CachedImage {
                source: ImageSource::OpaqueId { id, .. },
                ..
            }) = self.images.remove(&handle)
        {
            self.resources.destroy_image(id);
        }
        if !self.images.contains_key(&handle) && self.images.len() >= MAX_REGISTERED_IMAGES {
            let victim = self
                .images
                .iter()
                .filter(|(handle, image)| {
                    !is_desktop_heart_image_handle(**handle)
                        && image.last_used_frame != self.frame_id
                })
                .min_by_key(|(_, image)| image.last_used_frame)
                .map(|(handle, _)| *handle);
            if let Some(victim) = victim
                && let Some(CachedImage {
                    source: ImageSource::OpaqueId { id, .. },
                    ..
                }) = self.images.remove(&victim)
            {
                self.resources.destroy_image(id);
            } else {
                self.context.set_paint(LIGHT_GRAY);
                self.context.fill_rect(&vello_rect(rect));
                return Ok(());
            }
        }
        if !self.images.contains_key(&handle)
            && let Some(revision) = store_revision
            && let Some(image) = scene.image_store.get(handle)
        {
            let expected = usize::try_from(image.width)
                .ok()
                .and_then(|width| {
                    usize::try_from(image.height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixels| pixels.checked_mul(4));
            let (Ok(width), Ok(height)) = (u16::try_from(image.width), u16::try_from(image.height))
            else {
                self.context.set_paint(LIGHT_GRAY);
                self.context.fill_rect(&vello_rect(rect));
                return Ok(());
            };
            if width == 0 || height == 0 || expected != Some(image.rgba.len()) {
                self.context.set_paint(LIGHT_GRAY);
                self.context.fill_rect(&vello_rect(rect));
                return Ok(());
            }
            let pixels = image
                .rgba
                .chunks_exact(4)
                .map(|pixel| {
                    let alpha = u16::from(pixel[3]);
                    let premul = |component| ((u16::from(component) * alpha) / 255) as u8;
                    vello_cpu::color::PremulRgba8 {
                        r: premul(pixel[0]),
                        g: premul(pixel[1]),
                        b: premul(pixel[2]),
                        a: pixel[3],
                    }
                })
                .collect();
            let pixmap = Arc::new(Pixmap::from_parts_with_opacity(
                pixels,
                width,
                height,
                image.has_alpha,
            ));
            let id = self.resources.register_image(pixmap);
            self.images.insert(
                handle,
                CachedImage {
                    source: ImageSource::opaque_id_with_transparency_hint(id, image.has_alpha),
                    width: image.width,
                    height: image.height,
                    revision,
                    last_used_frame: self.frame_id,
                },
            );
        }
        let Some(image) = self.images.get_mut(&handle) else {
            // Missing resources are represented by a neutral checkerless box;
            // the command remains stable and will paint pixels after wakeup.
            self.context.set_paint(LIGHT_GRAY);
            self.context.fill_rect(&vello_rect(rect));
            return Ok(());
        };
        image.last_used_frame = self.frame_id;
        let iw = image.width as f32;
        let ih = image.height as f32;
        let sx = rect.width / iw;
        let sy = rect.height / ih;
        let scale = match fit {
            ImageFit::Fill => None,
            ImageFit::Contain => Some(sx.min(sy)),
            ImageFit::Cover => Some(sx.max(sy)),
            ImageFit::None => Some(1.0),
            ImageFit::ScaleDown => Some(1.0f32.min(sx.min(sy))),
        };
        let (scale_x, scale_y) = scale.map_or((sx, sy), |scale| (scale, scale));
        let drawn_width = iw * scale_x;
        let drawn_height = ih * scale_y;
        let x = rect.x + (rect.width - drawn_width) / 2.0;
        let y = rect.y + (rect.height - drawn_height) / 2.0;
        let sampler = ImageSampler::new().with_quality(match sampling {
            ImageSampling::Nearest => ImageQuality::Low,
            ImageSampling::Smooth => ImageQuality::Medium,
        });
        self.context.set_paint(ImageBrush {
            image: image.source.clone(),
            sampler,
        });
        self.context.set_paint_transform(
            Affine::translate((f64::from(x), f64::from(y)))
                * Affine::scale_non_uniform(f64::from(scale_x), f64::from(scale_y)),
        );
        if fit == ImageFit::Cover {
            self.context.push_clip_path(&rect_path(rect));
        }
        self.context.fill_rect(&Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + drawn_width),
            f64::from(y + drawn_height),
        ));
        if fit == ImageFit::Cover {
            self.context.pop_clip_path();
        }
        self.context.reset_paint_transform();
        Ok(())
    }
}

impl Default for VelloCpuRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl RasterBackend for VelloCpuRenderer {
    fn render<'a>(&'a mut self, scene: &Scene) -> Result<RasterFrame<'a>, String> {
        self.rasterize(scene)?;
        Ok(RasterFrame {
            size: self.size,
            pixels: &self.presented,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedRgbaFrame {
    pub size: PhysicalSize,
    pub pixels: Vec<u8>,
}

fn paint_decorations(
    context: &mut RenderContext,
    origin: crate::core::CssPoint,
    shaped: &crate::text::ShapedText,
    style: DecorationStyle,
) {
    let thickness = (shaped.line_height / 18.0).max(1.0);
    let paint_line = |context: &mut RenderContext, y: f32| {
        let mut stroke = Stroke::new(f64::from(thickness));
        match style {
            DecorationStyle::Dotted => {
                stroke = stroke
                    .with_caps(Cap::Round)
                    .with_dashes(0.0, [0.0, f64::from(thickness * 2.0)]);
            }
            DecorationStyle::Dashed => {
                stroke = stroke.with_dashes(
                    0.0,
                    [f64::from(thickness * 3.0), f64::from(thickness * 2.0)],
                );
            }
            _ => {}
        }
        context.set_stroke(stroke);
        let mut path = BezPath::new();
        path.move_to((f64::from(origin.x), f64::from(y)));
        path.line_to((f64::from(origin.x + shaped.advance), f64::from(y)));
        context.stroke_path(&path);
        if style == DecorationStyle::Double {
            let mut second = BezPath::new();
            second.move_to((f64::from(origin.x), f64::from(y + thickness * 2.0)));
            second.line_to((
                f64::from(origin.x + shaped.advance),
                f64::from(y + thickness * 2.0),
            ));
            context.stroke_path(&second);
        }
    };
    if shaped.underline {
        paint_line(context, origin.y + shaped.baseline + thickness);
    }
    if shaped.strikethrough {
        paint_line(context, origin.y + shaped.baseline - shaped.ascent * 0.32);
    }
}

pub(super) fn vello_stops(stops: &[super::GradientStop]) -> Vec<ColorStop> {
    stops
        .iter()
        .map(|stop| ColorStop {
            offset: stop.offset,
            color: vello_cpu::color::DynamicColor::from_alpha_color(vello_color(stop.color)),
        })
        .collect()
}

pub(super) fn vello_stroke(style: &StrokeStyle) -> Stroke {
    let cap = match style.cap {
        LineCap::Butt => Cap::Butt,
        LineCap::Round => Cap::Round,
        LineCap::Square => Cap::Square,
    };
    Stroke::new(f64::from(style.width.max(0.0)))
        .with_caps(cap)
        .with_dashes(
            f64::from(style.dash_offset),
            style.dash.iter().map(|value| f64::from(*value)),
        )
}

pub(super) fn vello_blend(mode: BlendMode) -> vello_cpu::peniko::BlendMode {
    let mix = match mode {
        BlendMode::Normal => Mix::Normal,
        BlendMode::Multiply => Mix::Multiply,
        BlendMode::Screen => Mix::Screen,
        BlendMode::Overlay => Mix::Overlay,
        BlendMode::Darken => Mix::Darken,
        BlendMode::Lighten => Mix::Lighten,
        BlendMode::Difference => Mix::Difference,
        BlendMode::Exclusion => Mix::Exclusion,
    };
    vello_cpu::peniko::BlendMode::new(mix, Compose::SrcOver)
}

pub(super) fn vello_affine(affine: Affine2d) -> Affine {
    Affine::new(affine.0.map(f64::from))
}

pub(super) fn vello_color(
    color: PaintColor,
) -> vello_cpu::color::AlphaColor<vello_cpu::color::Srgb> {
    match color {
        PaintColor::Window => BLACK,
        PaintColor::Chrome => DARK_GRAY,
        PaintColor::Content => BLUE,
        PaintColor::Surface => GRAY,
        PaintColor::Muted => LIGHT_GRAY,
        PaintColor::Foreground => WHITE,
        PaintColor::Accent => CYAN,
        PaintColor::Loading => YELLOW,
        PaintColor::Rgba(r, g, b, a) => vello_cpu::color::AlphaColor::from_rgba8(r, g, b, a),
    }
}

pub(super) fn vello_rect(rect: CssRect) -> Rect {
    Rect::new(
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.x + rect.width),
        f64::from(rect.y + rect.height),
    )
}

pub(super) fn rect_path(rect: CssRect) -> BezPath {
    shape_path(&PaintShape::Rect(rect))
}

pub(super) fn shape_path(shape: &PaintShape) -> BezPath {
    match shape {
        PaintShape::Rect(rect) => {
            let mut path = BezPath::new();
            path.move_to((f64::from(rect.x), f64::from(rect.y)));
            path.line_to((f64::from(rect.x + rect.width), f64::from(rect.y)));
            path.line_to((
                f64::from(rect.x + rect.width),
                f64::from(rect.y + rect.height),
            ));
            path.line_to((f64::from(rect.x), f64::from(rect.y + rect.height)));
            path.close_path();
            path
        }
        PaintShape::RoundedRect { rect, radii } => rounded_rect_path(*rect, *radii),
        PaintShape::Path(elements) => {
            let mut path = BezPath::new();
            for element in elements {
                match element {
                    PathElement::MoveTo(point) => {
                        path.move_to((f64::from(point.x), f64::from(point.y)))
                    }
                    PathElement::LineTo(point) => {
                        path.line_to((f64::from(point.x), f64::from(point.y)))
                    }
                    PathElement::QuadTo(control, point) => path.quad_to(
                        (f64::from(control.x), f64::from(control.y)),
                        (f64::from(point.x), f64::from(point.y)),
                    ),
                    PathElement::CurveTo(a, b, point) => path.curve_to(
                        (f64::from(a.x), f64::from(a.y)),
                        (f64::from(b.x), f64::from(b.y)),
                        (f64::from(point.x), f64::from(point.y)),
                    ),
                    PathElement::Close => path.close_path(),
                }
            }
            path
        }
    }
}

fn rounded_rect_path(rect: CssRect, radii: super::CornerRadii) -> BezPath {
    const K: f32 = 0.552_284_8;
    let [(tlx, tly), (trx, try_), (brx, bry), (blx, bly)] = radii.corners;
    let x0 = rect.x;
    let y0 = rect.y;
    let x1 = rect.x + rect.width;
    let y1 = rect.y + rect.height;
    let mut p = BezPath::new();
    p.move_to((f64::from(x0 + tlx), f64::from(y0)));
    p.line_to((f64::from(x1 - trx), f64::from(y0)));
    p.curve_to(
        (f64::from(x1 - trx + trx * K), f64::from(y0)),
        (f64::from(x1), f64::from(y0 + try_ - try_ * K)),
        (f64::from(x1), f64::from(y0 + try_)),
    );
    p.line_to((f64::from(x1), f64::from(y1 - bry)));
    p.curve_to(
        (f64::from(x1), f64::from(y1 - bry + bry * K)),
        (f64::from(x1 - brx + brx * K), f64::from(y1)),
        (f64::from(x1 - brx), f64::from(y1)),
    );
    p.line_to((f64::from(x0 + blx), f64::from(y1)));
    p.curve_to(
        (f64::from(x0 + blx - blx * K), f64::from(y1)),
        (f64::from(x0), f64::from(y1 - bly + bly * K)),
        (f64::from(x0), f64::from(y1 - bly)),
    );
    p.line_to((f64::from(x0), f64::from(y0 + tly)));
    p.curve_to(
        (f64::from(x0), f64::from(y0 + tly - tly * K)),
        (f64::from(x0 + tlx - tlx * K), f64::from(y0)),
        (f64::from(x0 + tlx), f64::from(y0)),
    );
    p.close_path();
    p
}

pub(super) fn offset_shape(shape: &PaintShape, dx: f32, dy: f32, spread: f32) -> PaintShape {
    match shape {
        PaintShape::Rect(rect) => PaintShape::Rect(CssRect::new(
            rect.x + dx - spread,
            rect.y + dy - spread,
            (rect.width + spread * 2.0).max(0.0),
            (rect.height + spread * 2.0).max(0.0),
        )),
        PaintShape::RoundedRect { rect, radii } => PaintShape::RoundedRect {
            rect: CssRect::new(
                rect.x + dx - spread,
                rect.y + dy - spread,
                (rect.width + spread * 2.0).max(0.0),
                (rect.height + spread * 2.0).max(0.0),
            ),
            radii: super::CornerRadii {
                corners: radii
                    .corners
                    .map(|(x, y)| ((x + spread).max(0.0), (y + spread).max(0.0))),
            },
        },
        PaintShape::Path(elements) => PaintShape::Path(
            elements
                .iter()
                .map(|element| match element {
                    PathElement::MoveTo(p) => {
                        PathElement::MoveTo(crate::core::CssPoint::new(p.x + dx, p.y + dy))
                    }
                    PathElement::LineTo(p) => {
                        PathElement::LineTo(crate::core::CssPoint::new(p.x + dx, p.y + dy))
                    }
                    PathElement::QuadTo(a, p) => PathElement::QuadTo(
                        crate::core::CssPoint::new(a.x + dx, a.y + dy),
                        crate::core::CssPoint::new(p.x + dx, p.y + dy),
                    ),
                    PathElement::CurveTo(a, b, p) => PathElement::CurveTo(
                        crate::core::CssPoint::new(a.x + dx, a.y + dy),
                        crate::core::CssPoint::new(b.x + dx, b.y + dy),
                        crate::core::CssPoint::new(p.x + dx, p.y + dy),
                    ),
                    PathElement::Close => PathElement::Close,
                })
                .collect(),
        ),
    }
}

pub(super) fn simple_rounded_rect(shape: &PaintShape) -> Option<(Rect, f32)> {
    match shape {
        PaintShape::Rect(rect) => Some((vello_rect(*rect), 0.0)),
        PaintShape::RoundedRect { rect, radii }
            if radii
                .corners
                .iter()
                .all(|&(x, y)| (x - y).abs() < 0.01 && (x - radii.corners[0].0).abs() < 0.01) =>
        {
            Some((vello_rect(*rect), radii.corners[0].0))
        }
        _ => None,
    }
}

/// Conservative screen-space culling. The renderer-neutral display list stays
/// complete for hit testing, selection, accessibility, and future backends;
/// the CPU raster adapter simply avoids constructing glyph/image work that
/// cannot intersect the framebuffer's active clip.
pub(super) fn shape_is_visible(
    shape: &PaintShape,
    transform: Affine2d,
    clip: CssRect,
    expansion: f32,
) -> bool {
    let Some(mut bounds) = shape_bounds(shape) else {
        return false;
    };
    let expansion = expansion.max(0.0);
    bounds.x -= expansion;
    bounds.y -= expansion;
    bounds.width += expansion * 2.0;
    bounds.height += expansion * 2.0;
    rect_is_visible(bounds, transform, clip)
}

pub(super) fn rect_is_visible(rect: CssRect, transform: Affine2d, clip: CssRect) -> bool {
    rect.width > 0.0
        && rect.height > 0.0
        && intersect_rect(transformed_bounds(rect, transform), clip).is_some()
}

pub(super) fn shape_bounds(shape: &PaintShape) -> Option<CssRect> {
    match shape {
        PaintShape::Rect(rect) | PaintShape::RoundedRect { rect, .. } => Some(*rect),
        PaintShape::Path(elements) => point_bounds(elements.iter().flat_map(|element| {
            match element {
                PathElement::MoveTo(point) | PathElement::LineTo(point) => {
                    [Some(*point), None, None]
                }
                PathElement::QuadTo(a, b) => [Some(*a), Some(*b), None],
                PathElement::CurveTo(a, b, c) => [Some(*a), Some(*b), Some(*c)],
                PathElement::Close => [None, None, None],
            }
            .into_iter()
            .flatten()
        })),
    }
}

pub(super) fn point_bounds(points: impl Iterator<Item = crate::core::CssPoint>) -> Option<CssRect> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut any = false;
    for point in points {
        any = true;
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    any.then(|| CssRect::new(min_x, min_y, max_x - min_x, max_y - min_y))
}

pub(super) fn transformed_bounds(rect: CssRect, transform: Affine2d) -> CssRect {
    let points = [
        crate::core::CssPoint::new(rect.x, rect.y),
        crate::core::CssPoint::new(rect.x + rect.width, rect.y),
        crate::core::CssPoint::new(rect.x, rect.y + rect.height),
        crate::core::CssPoint::new(rect.x + rect.width, rect.y + rect.height),
    ]
    .map(|point| transform.map_point(point));
    point_bounds(points.into_iter()).unwrap_or_default()
}

pub(super) fn intersect_rect(a: CssRect, b: CssRect) -> Option<CssRect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > left && bottom > top).then(|| CssRect::new(left, top, right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BrowserSnapshot, CssSize, ScaleFactor, ViewportMetrics};
    use crate::render::{ImageResource, desktop_shell};

    #[test]
    fn adapter_rasterizes_and_reuses_then_resizes_its_cpu_context() {
        let snapshot = BrowserSnapshot {
            address: String::from("https://example.com/"),
            status: String::from("Ready"),
            loading: true,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(160.0, 100.0),
            page_revision: 0,
        };
        let mut renderer = VelloCpuRenderer::new();
        let first_scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(320, 200), ScaleFactor::new(2.0)),
            &snapshot,
        );
        let first = renderer.render(&first_scene).unwrap();
        assert_eq!(first.pixels.len(), 320 * 200);
        assert!(first.pixels.iter().any(|pixel| *pixel != 0));
        assert_eq!(
            renderer.render(&first_scene).unwrap().size,
            PhysicalSize::new(320, 200)
        );
        let resized_scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(400, 240), ScaleFactor::new(2.0)),
            &snapshot,
        );
        let resized = renderer.render(&resized_scene).unwrap();
        assert_eq!(resized.size, PhysicalSize::new(400, 240));
        assert_eq!(resized.pixels.len(), 400 * 240);
    }

    #[test]
    fn offscreen_images_are_not_registered_with_the_cpu_backend() {
        let snapshot = BrowserSnapshot {
            address: String::new(),
            status: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(160.0, 100.0),
            page_revision: 0,
        };
        let mut scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(160, 100), ScaleFactor::new(1.0)),
            &snapshot,
        );
        let handle = ImageHandle(9);
        scene.image_store.insert(
            handle,
            ImageResource {
                width: 2,
                height: 2,
                rgba: Arc::from([255u8; 16]),
                has_alpha: false,
            },
        );
        scene.primitives.push(DisplayCommand::Image {
            rect: CssRect::new(0.0, 10_000.0, 20.0, 20.0),
            handle,
            source_rect: None,
            fit: ImageFit::Contain,
            sampling: ImageSampling::Smooth,
            clip: None,
            node: 1,
            link: None,
        });

        let mut renderer = VelloCpuRenderer::new();
        renderer.render(&scene).unwrap();
        assert!(
            renderer.images.is_empty(),
            "an offscreen display-list image must not allocate a Vello resource"
        );

        if let Some(DisplayCommand::Image { rect, .. }) = scene.primitives.last_mut() {
            *rect = CssRect::new(2.0, 2.0, 20.0, 20.0);
        }
        renderer.render(&scene).unwrap();
        assert_eq!(renderer.images.len(), 1);
    }

    #[test]
    fn stable_image_handle_reuploads_only_after_store_content_changes() {
        let snapshot = BrowserSnapshot {
            address: String::new(),
            status: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(40.0, 40.0),
            page_revision: 0,
        };
        let mut scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(40, 40), ScaleFactor::new(1.0)),
            &snapshot,
        );
        let handle = ImageHandle(101);
        let resource = |rgba| ImageResource {
            width: 1,
            height: 1,
            rgba: Arc::from(rgba),
            has_alpha: false,
        };
        scene.image_store.insert(handle, resource([255, 0, 0, 255]));
        scene.primitives.push(DisplayCommand::Image {
            rect: CssRect::new(1.0, 1.0, 10.0, 10.0),
            handle,
            source_rect: None,
            fit: ImageFit::Fill,
            sampling: ImageSampling::Nearest,
            clip: None,
            node: 1,
            link: None,
        });

        let mut renderer = VelloCpuRenderer::new();
        renderer.render(&scene).unwrap();
        let first = renderer.images[&handle].revision;
        renderer.render(&scene).unwrap();
        assert_eq!(renderer.images[&handle].revision, first);

        scene.image_store.insert(handle, resource([0, 0, 255, 255]));
        renderer.render(&scene).unwrap();
        assert_ne!(renderer.images[&handle].revision, first);
        assert_eq!(
            renderer.images[&handle].revision,
            scene.image_store.revision(handle).unwrap()
        );
    }

    #[test]
    fn malformed_image_resource_degrades_without_panicking_or_poisoning_cache() {
        let snapshot = BrowserSnapshot {
            address: String::new(),
            status: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(80.0, 60.0),
            page_revision: 0,
        };
        let mut scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(80, 60), ScaleFactor::new(1.0)),
            &snapshot,
        );
        let handle = ImageHandle(17);
        scene.image_store.insert(
            handle,
            ImageResource {
                width: 8,
                height: 8,
                rgba: Arc::from([255, 0, 0, 255]),
                has_alpha: false,
            },
        );
        scene.primitives.push(DisplayCommand::Image {
            rect: CssRect::new(2.0, 2.0, 20.0, 20.0),
            handle,
            source_rect: None,
            fit: ImageFit::Contain,
            sampling: ImageSampling::Smooth,
            clip: None,
            node: 1,
            link: None,
        });
        let mut renderer = VelloCpuRenderer::new();
        assert!(renderer.render(&scene).is_ok());
        assert!(renderer.images.is_empty());
    }

    #[test]
    fn alternating_desktop_heart_frames_remain_resident() {
        let snapshot = BrowserSnapshot {
            address: String::new(),
            status: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(80.0, 60.0),
            page_revision: 0,
        };
        let mut scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(80, 60), ScaleFactor::new(1.0)),
            &snapshot,
        );
        let idle = crate::render::desktop_heart_image_handle(false);
        let active = crate::render::desktop_heart_image_handle(true);
        for handle in [idle, active] {
            scene.image_store.insert(
                handle,
                ImageResource {
                    width: 2,
                    height: 2,
                    rgba: Arc::from([255u8; 16]),
                    has_alpha: false,
                },
            );
        }
        scene.primitives.push(DisplayCommand::Image {
            rect: CssRect::new(2.0, 2.0, 20.0, 20.0),
            handle: idle,
            source_rect: None,
            fit: ImageFit::Fill,
            sampling: ImageSampling::Smooth,
            clip: None,
            node: 0,
            link: None,
        });

        let mut renderer = VelloCpuRenderer::new();
        renderer.render(&scene).unwrap();
        assert!(renderer.images.contains_key(&idle));
        if let Some(DisplayCommand::Image { handle, .. }) = scene.primitives.last_mut() {
            *handle = active;
        }
        renderer.render(&scene).unwrap();
        assert!(renderer.images.contains_key(&idle));
        assert!(renderer.images.contains_key(&active));
    }
}
