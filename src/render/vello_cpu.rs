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
use vello_cpu::kurbo::{Affine, BezPath, Cap, Ellipse, Join, Rect, Shape as _, Stroke};
use vello_cpu::peniko::{
    ColorStop, Compose, Gradient, ImageAlphaType, ImageBrush, ImageQuality, ImageSampler, Mix,
};
use vello_cpu::{ImageSource, PixelMetadata, Pixmap, RenderContext, Resources};

use super::{
    Affine2d, BlendMode, CompositeOperator, CssFilter, CssRect, DecorationStyle, DisplayCommand,
    ImageFit, ImageHandle, ImageSampling, LineCap, LineJoin, PaintBrush, PaintColor, PaintShape,
    PathElement, Primitive, RasterBackend, RasterFrame, Scene, StrokeStyle,
    is_desktop_heart_image_handle,
};
use crate::core::{CssPoint, PhysicalSize};

pub(super) const MAX_REGISTERED_IMAGES: usize = 256;

/// CSS Images 3 §4.2 and SVG 2 §8.3 size each use of an SVG separately.
/// A frame may reference several sizes of the same resource: replacing its
/// registration while building that frame invalidates earlier image paints.
/// Count all variants against the normal cache limit and pin each used entry
/// until the frame finishes. Translation leaves this identity unchanged.
/// https://drafts.csswg.org/css-images-3/#object-negotiation
/// https://www.w3.org/TR/SVG2/coords.html#ViewportSpace
/// Local snapshots: csswg-drafts 81c27f686901, svgwg c403ca46ad04.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ImageCacheKey {
    pub handle: ImageHandle,
    svg: Option<(u64, [u32; 4])>,
}

impl ImageCacheKey {
    pub fn new(
        handle: ImageHandle,
        revision: u64,
        size: Option<crate::img::SvgRasterSize>,
    ) -> Self {
        Self {
            handle,
            svg: size.map(|size| (revision, size.cache_key())),
        }
    }
}

impl From<ImageHandle> for ImageCacheKey {
    fn from(handle: ImageHandle) -> Self {
        Self { handle, svg: None }
    }
}

// CSS Fonts 4 §2.5 scales the font's em; §5.2 allows raster-size tolerance.
// Keep CSS geometry fractional, and use slight vertical hinting in both
// painters. Native TrueType instructions can round 11pt (14.667px) to 15ppem
// and stretch JetBrains Mono capitals from 10 to 12 pixels. FreeType's light
// target preserves their proportions without changing the shaper's advances.
// https://drafts.csswg.org/css-fonts-4/#font-size-prop
// https://freetype.org/freetype2/docs/reference/ft2-glyph_retrieval.html#ft_load_target_xxx
pub(super) const TEXT_HINTING_MODE: glifo::HintingMode = glifo::HintingMode::Light;

struct CachedImage {
    upload_width: u32,
    upload_height: u32,
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
    images: HashMap<ImageCacheKey, CachedImage>,
    frame_id: u64,
    #[cfg(test)]
    eager_clips: bool,
    #[cfg(test)]
    rasterized_clips: usize,
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
            #[cfg(test)]
            eager_clips: false,
            #[cfg(test)]
            rasterized_clips: 0,
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
            .filter(|key| {
                !live_images.contains(&key.handle) && !is_desktop_heart_image_handle(key.handle)
            })
            .collect();
        for key in stale {
            if let Some(CachedImage {
                source: ImageSource::OpaqueId { id, .. },
                ..
            }) = self.images.remove(&key)
            {
                self.resources.destroy_image(id);
            }
        }
        let device = Affine::scale(scene.viewport.scale_factor.get());
        let mut transforms = vec![device];
        let mut logical_transforms = vec![Affine2d::IDENTITY];
        let mut clips = RasterClips::new(CssRect::new(
            0.0,
            0.0,
            scene.viewport.css.width,
            scene.viewport.css.height,
        ));
        self.context.set_transform(device);
        let mut layer_filters = Vec::new();
        let mut filter_clip_layers = Vec::new();
        let mut skip_until = 0;
        for (index, command) in scene.primitives.iter().enumerate() {
            if index < skip_until {
                continue;
            }
            match command {
                DisplayCommand::Fill { shape, brush } => {
                    if !shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        clips.bounds(),
                        0.0,
                    ) {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    self.set_brush(brush);
                    self.context.set_fill_rule(shape_fill(shape));
                    self.context.fill_path(&shape_path(shape));
                    self.context.set_fill_rule(vello_cpu::peniko::Fill::NonZero);
                    self.context.reset_paint_transform();
                }
                DisplayCommand::Stroke {
                    shape,
                    brush,
                    style,
                } => {
                    if !shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        clips.bounds(),
                        style.width.max(0.0) / 2.0,
                    ) {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    self.set_brush(brush);
                    self.context.set_stroke(vello_stroke(style));
                    self.context.stroke_path(&shape_path(shape));
                    self.context.reset_paint_transform();
                }
                DisplayCommand::PushClip(shape) => {
                    clips.push(
                        shape,
                        *logical_transforms.last().unwrap(),
                        *transforms.last().unwrap(),
                    );
                    #[cfg(test)]
                    if self.eager_clips {
                        apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    }
                }
                DisplayCommand::PopClip => {
                    if clips.pop() {
                        self.context.pop_clip();
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
                DisplayCommand::PushLayer(layer) => {
                    let cull = {
                        #[cfg(test)]
                        {
                            !self.eager_clips
                        }
                        #[cfg(not(test))]
                        {
                            true
                        }
                    };
                    // A layer's own paint clip bounds even destructive
                    // operators: outside it the backdrop is untouched.
                    let hidden = clips.bounds().width <= 0.
                        || clips.bounds().height <= 0.
                        || layer.clip.is_some_and(|clip| {
                            !rect_is_visible(
                                clip,
                                *logical_transforms.last().unwrap(),
                                clips.bounds(),
                            )
                        });
                    if cull
                        && hidden
                        && let Some(end) = super::clipped_layer_end(&scene.primitives, index)
                    {
                        skip_until = end + 1;
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    // CSS Filter Effects 1 §4: a filter's output, including a
                    // blur spreading past the content, is clipped by ancestor
                    // overflow clips. Vello crops a filter layer's composite
                    // only to enclosing clip *layers*, not to the clip-path
                    // stack, so repeat the active clips as layers around it.
                    let mut clip_layers = 0;
                    // Only an isolated source-over group composites the same
                    // inside an extra layer; masks and blend modes need the
                    // real backdrop.
                    if !layer.filters.is_empty()
                        && layer.compose == CompositeOperator::SourceOver
                        && layer.blend == BlendMode::Normal
                    {
                        for (shape, at) in clips.active() {
                            self.context.set_transform(at);
                            self.context.set_fill_rule(shape_fill(shape));
                            self.context.push_layer(
                                Some(&shape_path(shape)),
                                None,
                                None,
                                None,
                                None,
                            );
                            clip_layers += 1;
                        }
                        if clip_layers > 0 {
                            self.context.set_fill_rule(vello_cpu::peniko::Fill::NonZero);
                            self.context.set_transform(*transforms.last().unwrap());
                        }
                    }
                    filter_clip_layers.push(clip_layers);
                    self.context.push_layer(
                        layer.clip.map(rect_path).as_ref(),
                        Some(vello_blend(layer.blend, layer.compose)),
                        Some(layer.opacity.clamp(0.0, 1.0)),
                        None,
                        None,
                    );
                    // Nest in reverse so the first CSS function runs first.
                    // Each layer clamps separately before the next operation.
                    for filter in layer.filters.iter().rev() {
                        self.context.push_layer(
                            None,
                            None,
                            None,
                            None,
                            Some(vello_css_filter(filter)),
                        );
                    }
                    layer_filters.push(layer.filters.len());
                }
                DisplayCommand::PopLayer => {
                    for _ in 0..layer_filters.pop().unwrap_or(0) {
                        self.context.pop_layer();
                    }
                    self.context.pop_layer();
                    for _ in 0..filter_clip_layers.pop().unwrap_or(0) {
                        self.context.pop_layer();
                    }
                }
                DisplayCommand::BeginSticky(_)
                | DisplayCommand::EndSticky
                | DisplayCommand::BeginScroll(_)
                | DisplayCommand::EndScroll
                | DisplayCommand::BeginFixed
                | DisplayCommand::EndFixed
                | DisplayCommand::BeginCssAnimation(_)
                | DisplayCommand::EndCssAnimation
                | DisplayCommand::BeginMarquee(_)
                | DisplayCommand::EndMarquee => {
                    // Scene composition resolves these to Push/PopTransform.
                }
                DisplayCommand::Shadow {
                    shape,
                    color,
                    offset,
                    blur_radius,
                    spread,
                    inset: true,
                } => {
                    // `shape` is the padding box, which clips the shadow.
                    if !shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        clips.bounds(),
                        0.0,
                    ) {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    let hole = inset_shadow_hole(shape, *offset, *spread);
                    let std_dev = blur_radius.max(0.0) / 2.0;
                    let padding = shape_path(shape);
                    self.context.set_paint(vello_color(*color));
                    if let Some((rect, radius)) =
                        simple_rounded_rect(&hole).filter(|_| std_dev > 0.0)
                    {
                        self.context
                            .push_layer(Some(&padding), None, None, None, None);
                        self.context.fill_path(&padding);
                        self.context
                            .push_layer(None, Some(erase_blend()), None, None, None);
                        self.context.set_paint(BLACK);
                        self.context
                            .fill_blurred_rounded_rect(&rect, radius, std_dev, false);
                        self.context.pop_layer();
                        self.context.pop_layer();
                    } else {
                        // Vello CPU's direct blur primitive currently accepts
                        // rounded rectangles, not arbitrary paths.
                        self.context.push_clip_path(&padding);
                        self.context.set_fill_rule(vello_cpu::peniko::Fill::EvenOdd);
                        self.context.fill_path(&inset_shadow_ring(shape, &hole));
                        self.context.set_fill_rule(vello_cpu::peniko::Fill::NonZero);
                        self.context.pop_clip();
                    }
                }
                DisplayCommand::Shadow {
                    shape,
                    color,
                    offset,
                    blur_radius,
                    spread,
                    inset: false,
                } => {
                    let expansion = spread.max(0.0) + blur_radius.max(0.0) * 2.0;
                    let shifted = offset_shape(shape, offset.x, offset.y, *spread);
                    if !shape_is_visible(
                        &shifted,
                        *logical_transforms.last().unwrap(),
                        clips.bounds(),
                        expansion,
                    ) {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    self.context.set_paint(vello_color(*color));
                    self.context.set_fill_rule(vello_cpu::peniko::Fill::EvenOdd);
                    self.context
                        .push_clip_path(&outside_shape_path(shape, &shifted, expansion));
                    self.context.set_fill_rule(vello_cpu::peniko::Fill::NonZero);
                    if let Some((rect, radius)) = simple_rounded_rect(&shifted) {
                        self.context.fill_blurred_rounded_rect(
                            &rect,
                            radius,
                            blur_radius.max(0.0) / 2.0,
                            false,
                        );
                    } else {
                        // Vello CPU's direct blur primitive currently accepts
                        // rounded rectangles, not arbitrary paths.
                        self.context.fill_path(&shape_path(&shifted));
                    }
                    self.context.pop_clip();
                }
                DisplayCommand::HitRegion(_) => {}
                Primitive::FillRect { rect, color } => {
                    if !rect_is_visible(*rect, *logical_transforms.last().unwrap(), clips.bounds())
                    {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    self.context.set_paint(vello_color(*color));
                    self.context.fill_rect(&vello_rect(*rect));
                }
                Primitive::FillPolygon { points, color } => {
                    let Some(first) = points.first() else {
                        continue;
                    };
                    let bounds = point_bounds(points.iter().copied()).unwrap_or_default();
                    if !rect_is_visible(bounds, *logical_transforms.last().unwrap(), clips.bounds())
                    {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
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
                    shadows,
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
                        clips.bounds(),
                    ) {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    if let Some(clip) = clip {
                        self.context.push_clip_path(&rect_path(*clip));
                    }
                    // CSS Text Decoration 4 §4 paints shadow layers below the
                    // decorated text, with the first authored shadow on top.
                    // A blurred shadow is drawn into a bounded blur layer.
                    for shadow in shadows.iter().rev() {
                        let shadow_origin =
                            CssPoint::new(origin.x + shadow.offset.x, origin.y + shadow.offset.y);
                        let blurred = text_shadow_blur(shadow);
                        if let Some(filter) = &blurred {
                            self.context
                                .push_layer(None, None, None, None, Some(filter.clone()));
                        }
                        self.context.set_paint(vello_color(shadow.color));
                        paint_glyphs(
                            &mut self.context,
                            &mut self.resources,
                            shadow_origin,
                            shaped,
                            None,
                        );
                        paint_decorations(
                            &mut self.context,
                            shadow_origin,
                            shaped,
                            decoration,
                            *transforms.last().unwrap(),
                        );
                        if blurred.is_some() {
                            self.context.pop_layer();
                        }
                    }
                    self.context.set_paint(vello_color(*color));
                    paint_glyphs(
                        &mut self.context,
                        &mut self.resources,
                        *origin,
                        shaped,
                        Some(*color),
                    );
                    self.context.set_paint(vello_color(decoration.color));
                    paint_decorations(
                        &mut self.context,
                        *origin,
                        shaped,
                        decoration,
                        *transforms.last().unwrap(),
                    );
                    if clip.is_some() {
                        self.context.pop_clip();
                    }
                }
                Primitive::Image {
                    rect,
                    handle,
                    source_rect,
                    fit,
                    sampling,
                    clip,
                    ..
                } => {
                    if !rect_is_visible(*rect, *logical_transforms.last().unwrap(), clips.bounds())
                    {
                        continue;
                    }
                    apply_clips(&mut self.context, &mut clips, *transforms.last().unwrap());
                    if let Some(clip) = clip {
                        self.context.push_clip_path(&rect_path(*clip));
                    }
                    self.paint_image(
                        scene,
                        *handle,
                        *rect,
                        *source_rect,
                        *fit,
                        *sampling,
                        transforms.last().unwrap().as_coeffs(),
                    )?;
                    if clip.is_some() {
                        self.context.pop_clip();
                    }
                }
            }
        }
        #[cfg(test)]
        {
            self.rasterized_clips = clips.applied_total;
        }
        self.context.flush();
        self.context.render(&mut self.pixmap, &mut self.resources);
        for ((target, rgba), source) in self
            .presented
            .iter_mut()
            .zip(self.rgba.as_chunks_mut::<4>().0.iter_mut())
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
            PaintBrush::LinearGradient {
                start,
                end,
                stops,
                interpolation,
                repeat,
            } => {
                let stops = vello_stops(stops);
                self.context.set_paint(
                    Gradient::new_linear(
                        (f64::from(start.x), f64::from(start.y)),
                        (f64::from(end.x), f64::from(end.y)),
                    )
                    .with_extend(gradient_extend(*repeat))
                    .with_interpolation_cs(interpolation.space)
                    .with_hue_direction(interpolation.hue)
                    .with_stops(stops.as_slice()),
                );
            }
            PaintBrush::RadialGradient {
                center,
                start_radius,
                radius,
                aspect,
                stops,
                interpolation,
                repeat,
            } => {
                let stops = vello_stops(stops);
                self.context
                    .set_paint_transform(radial_aspect_transform(*center, *aspect));
                self.context.set_paint(
                    radial_gradient(*center, *start_radius, *radius)
                        .with_extend(gradient_extend(*repeat))
                        .with_interpolation_cs(interpolation.space)
                        .with_hue_direction(interpolation.hue)
                        .with_stops(stops.as_slice()),
                );
            }
            PaintBrush::ConicGradient {
                center,
                rotation,
                start_angle,
                end_angle,
                stops,
                interpolation,
                repeat,
            } => {
                let stops = vello_stops(stops);
                self.context
                    .set_paint_transform(conic_transform(*center, *rotation));
                self.context.set_paint(
                    Gradient::new_sweep(
                        (f64::from(center.x), f64::from(center.y)),
                        *start_angle,
                        *end_angle,
                    )
                    .with_extend(gradient_extend(*repeat))
                    .with_interpolation_cs(interpolation.space)
                    .with_hue_direction(interpolation.hue)
                    .with_stops(stops.as_slice()),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_image(
        &mut self,
        scene: &Scene,
        handle: ImageHandle,
        rect: CssRect,
        source_rect: Option<CssRect>,
        fit: ImageFit,
        sampling: ImageSampling,
        transform: [f64; 6],
    ) -> Result<(), String> {
        let store_revision = scene.image_revision(handle);
        let resource = scene.image(handle);
        let svg_size = resource
            .as_ref()
            .filter(|image| image.svg_source.is_some())
            .map(|image| {
                crate::img::SvgRasterSize::new(image.width, image.height, rect, fit, transform)
            });
        let key = ImageCacheKey::new(handle, store_revision.unwrap_or(0), svg_size);
        let changed = self
            .images
            .get(&key)
            .is_some_and(|image| store_revision.is_some_and(|revision| revision != image.revision));
        if changed
            && let Some(CachedImage {
                source: ImageSource::OpaqueId { id, .. },
                ..
            }) = self.images.remove(&key)
        {
            self.resources.destroy_image(id);
        }
        if !self.images.contains_key(&key) && self.images.len() >= MAX_REGISTERED_IMAGES {
            let victim = self
                .images
                .iter()
                .filter(|(key, image)| {
                    !is_desktop_heart_image_handle(key.handle)
                        && image.last_used_frame != self.frame_id
                })
                .min_by_key(|(_, image)| image.last_used_frame)
                .map(|(key, _)| *key);
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
        if !self.images.contains_key(&key)
            && let Some(revision) = store_revision
            && let Some(image) = resource
        {
            let natural = (image.width, image.height);
            let raster = svg_size
                .zip(image.svg_source.as_ref())
                .and_then(|(size, source)| size.rasterize(source).ok());
            let image = raster.unwrap_or(image);
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
            let pixmap = Arc::new(premultiplied_pixmap(
                premultiply_rgba(&image.rgba),
                width,
                height,
                image.has_alpha,
            ));
            let id = self.resources.register_image(pixmap);
            self.images.insert(
                key,
                CachedImage {
                    upload_width: image.width,
                    upload_height: image.height,
                    source: ImageSource::opaque_id_with_transparency_hint(id, image.has_alpha),
                    width: natural.0,
                    height: natural.1,
                    revision,
                    last_used_frame: self.frame_id,
                },
            );
        }
        let Some(image) = self.images.get_mut(&key) else {
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
        let mut drawn_width = iw * scale_x;
        let mut drawn_height = ih * scale_y;
        let mut x = rect.x + (rect.width - drawn_width) / 2.0;
        let mut y = rect.y + (rect.height - drawn_height) / 2.0;
        let mut painted = Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + drawn_width),
            f64::from(y + drawn_height),
        );
        // A source crop (in natural image pixels) is stretched over `rect`:
        // border-image slices and sprite regions.
        if let Some(source) = source_rect.filter(|source| source.width > 0.0 && source.height > 0.0)
        {
            let (sx, sy) = (rect.width / source.width, rect.height / source.height);
            drawn_width = iw * sx;
            drawn_height = ih * sy;
            x = rect.x - source.x * sx;
            y = rect.y - source.y * sy;
            painted = vello_rect(rect);
        }
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
                * Affine::scale_non_uniform(
                    f64::from(drawn_width / image.upload_width as f32),
                    f64::from(drawn_height / image.upload_height as f32),
                ),
        );
        if fit == ImageFit::Cover {
            self.context.push_clip_path(&rect_path(rect));
        }
        self.context.fill_rect(&painted);
        if fit == ImageFit::Cover {
            self.context.pop_clip();
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

fn paint_glyphs(
    context: &mut RenderContext,
    resources: &mut Resources,
    origin: crate::core::CssPoint,
    shaped: &crate::text::ShapedText,
    color: Option<PaintColor>,
) {
    for run in &shaped.runs {
        if let Some(default) = color {
            context.set_paint(vello_color(
                run.color
                    .map_or(default, |[r, g, b]| PaintColor::Rgba(r, g, b, 255)),
            ));
        }
        let glyphs: Vec<vello_cpu::Glyph> = run
            .glyphs
            .iter()
            .map(|glyph| vello_cpu::Glyph {
                id: glyph.id,
                x: origin.x + glyph.x,
                y: origin.y + glyph.y,
            })
            .collect();
        let skew = run.synthetic_oblique_skew();
        let bold = run.synth_bold.then(|| glyphs.clone());
        let mut fill = context
            .glyph_run(resources, run.font.data())
            .font_size(run.font_size)
            .hinting_mode(TEXT_HINTING_MODE)
            .normalized_coords(&run.normalized_coords);
        if let Some(skew) = skew {
            fill = fill.glyph_transform(Affine::skew(skew, 0.0));
        }
        // Unrenderable glyphs are skipped; the rest of the run paints.
        let _ = fill.fill_glyphs(glyphs.into_iter());
        if let Some(glyphs) = bold {
            context.set_stroke(synthetic_bold_stroke(run.font_size));
            let mut stroke = context
                .glyph_run(resources, run.font.data())
                .font_size(run.font_size)
                .hinting_mode(TEXT_HINTING_MODE)
                .normalized_coords(&run.normalized_coords);
            if let Some(skew) = skew {
                stroke = stroke.glyph_transform(Affine::skew(skew, 0.0));
            }
            let _ = stroke.stroke_glyphs(glyphs.into_iter());
        }
    }
}

/// Synthetic bold (CSS Fonts 4 #font-synthesis-weight) as a fill plus an
/// outline stroke, as Skia draws it. Expanding the outline itself instead
/// threw long spikes off sharp corners of some CFF outlines.
pub(super) fn synthetic_bold_stroke(font_size: f32) -> Stroke {
    Stroke::new(f64::from(font_size) * 0.05)
        .with_join(vello_cpu::kurbo::Join::Miter)
        .with_miter_limit(4.0)
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
    decoration: &super::TextDecorationPaint,
    transform: Affine,
) {
    for (stroke, path) in decoration_strokes(origin, shaped, decoration, transform) {
        if let Some(stroke) = stroke {
            context.set_stroke(stroke);
            context.stroke_path(&path);
        } else {
            context.fill_path(&path);
        }
    }
}

/// The ink of a glyph run's underline and line-through (CSS Text
/// Decoration 4): an authored `text-decoration-thickness`, else one
/// eighteenth of the line height; an underline sits `text-underline-offset`
/// below the alphabetic baseline (by default, one thickness). `wavy` is a
/// smooth wave of period six thicknesses. Each path is stroked, or filled
/// when it has no stroke. `transform` maps CSS pixels to the device pixels
/// the paths are drawn on.
pub(super) fn decoration_strokes(
    origin: CssPoint,
    shaped: &crate::text::ShapedText,
    decoration: &super::TextDecorationPaint,
    transform: Affine,
) -> Vec<(Option<Stroke>, BezPath)> {
    let style = decoration.style;
    let thickness = decoration
        .thickness
        .filter(|thickness| *thickness > 0.0)
        .unwrap_or((shaped.line_height / 18.0).max(1.0));
    let (left, right) = (f64::from(origin.x), f64::from(origin.x + shaped.advance));
    let mut strokes = Vec::new();
    let mut line = |y: f32| {
        if style == DecorationStyle::Dotted {
            strokes.push((
                None,
                decoration_dots(left, right, f64::from(y), thickness, transform),
            ));
            return;
        }
        let mut stroke = Stroke::new(f64::from(thickness));
        if style == DecorationStyle::Dashed {
            stroke = stroke.with_dashes(
                0.0,
                [f64::from(thickness * 3.0), f64::from(thickness * 2.0)],
            );
        }
        let y = f64::from(y);
        let mut path = BezPath::new();
        if style == DecorationStyle::Wavy {
            let amplitude = f64::from(thickness.max(1.0));
            let half = f64::from((thickness * 3.0).max(3.0));
            path.move_to((left, y));
            let mut x = left;
            let mut up = true;
            while x < right {
                let end = (x + half).min(right);
                let peak = if up {
                    y - amplitude * 2.0
                } else {
                    y + amplitude * 2.0
                };
                path.quad_to(((x + end) / 2.0, peak), (end, y));
                x = end;
                up = !up;
            }
        } else {
            path.move_to((left, y));
            path.line_to((right, y));
        }
        strokes.push((Some(stroke.clone()), path));
        if style == DecorationStyle::Double {
            let second_y = y + f64::from(thickness * 2.0);
            let mut second = BezPath::new();
            second.move_to((left, second_y));
            second.line_to((right, second_y));
            strokes.push((Some(stroke), second));
        }
    };
    if shaped.underline {
        let offset = decoration.underline_offset.unwrap_or(thickness / 2.0);
        line(origin.y + shaped.baseline + offset + thickness / 2.0);
    }
    if shaped.strikethrough {
        line(origin.y + shaped.baseline - shaped.ascent * 0.32);
    }
    strokes
}

/// A `dotted` decoration line from `left` to `right` centered on `y`: dots
/// one thickness across, one thickness apart, starting at `left`, laid out
/// on the device pixels of `transform` (the CSS-to-device transform the
/// path is drawn with). The thickness rounds to whole device pixels, at
/// least one (CSS Text Decoration 4 #text-decoration-thickness-property),
/// and the dots, the gaps between them and the line's edges fall on pixel
/// boundaries. As for dotted borders, dots at most two device pixels across
/// are squares, since so small a circle only rasterizes as a blur. Under a
/// transform that rotates or skews, CSS pixels stand in for device pixels.
fn decoration_dots(left: f64, right: f64, y: f64, thickness: f32, transform: Affine) -> BezPath {
    let [a, b, c, d, e, f] = transform.as_coeffs();
    let aligned =
        b == 0. && c == 0. && a != 0. && d != 0. && [a, d, e, f].iter().all(|v| v.is_finite());
    // Device coordinate = scale * CSS coordinate + offset, along each axis.
    let ((scale_x, offset_x), (scale_y, offset_y)) = if aligned {
        ((a, e), (d, f))
    } else {
        ((1., 0.), (1., 0.))
    };
    let thickness = f64::from(thickness);
    let size_x = (thickness * scale_x.abs()).round().max(1.);
    let size_y = (thickness * scale_y.abs()).round().max(1.);
    let thin = size_y <= 2.;
    let (start, end) = (left * scale_x + offset_x, right * scale_x + offset_x);
    let (start, end) = (start.min(end).round(), start.max(end).round());
    let top = (y * scale_y + offset_y - size_y / 2.).round();
    let css_x = |x: f64| (x - offset_x) / scale_x;
    let (y0, y1) = (
        (top - offset_y) / scale_y,
        (top + size_y - offset_y) / scale_y,
    );
    let mut path = BezPath::new();
    let mut x = start;
    let mut count = 0usize;
    while x + size_x <= end && count < 1 << 16 {
        let dot = Rect::new(css_x(x), y0, css_x(x + size_x), y1).abs();
        if thin {
            path.extend(dot.path_elements(0.1));
        } else {
            path.extend(Ellipse::from_rect(dot).path_elements(0.1));
        }
        count += 1;
        x = start + size_x * 2. * count as f64;
    }
    path
}

pub(super) fn vello_stops(stops: &[super::GradientStop]) -> Vec<ColorStop> {
    stops
        .iter()
        .map(|stop| ColorStop {
            offset: stop.offset,
            color: stop.color,
        })
        .collect()
}

pub(super) fn vello_stroke(style: &StrokeStyle) -> Stroke {
    let cap = match style.cap {
        LineCap::Butt => Cap::Butt,
        LineCap::Round => Cap::Round,
        LineCap::Square => Cap::Square,
    };
    let join = match style.join {
        LineJoin::Miter => Join::Miter,
        LineJoin::Round => Join::Round,
        LineJoin::Bevel => Join::Bevel,
    };
    Stroke::new(f64::from(style.width.max(0.0)))
        .with_caps(cap)
        .with_join(join)
        .with_dashes(
            f64::from(style.dash_offset),
            style.dash.iter().map(|value| f64::from(*value)),
        )
}

/// A group's CSS Compositing 1 blending (#blending) and Porter-Duff
/// compositing (#porterduffcompositingoperators) as one Vello blend mode.
pub(super) fn vello_blend(
    mode: BlendMode,
    compose: CompositeOperator,
) -> vello_cpu::peniko::BlendMode {
    let mix = match mode {
        BlendMode::Normal => Mix::Normal,
        BlendMode::Multiply => Mix::Multiply,
        BlendMode::Screen => Mix::Screen,
        BlendMode::Overlay => Mix::Overlay,
        BlendMode::Darken => Mix::Darken,
        BlendMode::Lighten => Mix::Lighten,
        BlendMode::ColorDodge => Mix::ColorDodge,
        BlendMode::ColorBurn => Mix::ColorBurn,
        BlendMode::HardLight => Mix::HardLight,
        BlendMode::SoftLight => Mix::SoftLight,
        BlendMode::Difference => Mix::Difference,
        BlendMode::Exclusion => Mix::Exclusion,
        BlendMode::Hue => Mix::Hue,
        BlendMode::Saturation => Mix::Saturation,
        BlendMode::Color => Mix::Color,
        BlendMode::Luminosity => Mix::Luminosity,
    };
    let compose = match compose {
        CompositeOperator::SourceOver => Compose::SrcOver,
        CompositeOperator::SourceIn => Compose::SrcIn,
        CompositeOperator::SourceOut => Compose::SrcOut,
        CompositeOperator::DestinationIn => Compose::DestIn,
        CompositeOperator::Xor => Compose::Xor,
    };
    vello_cpu::peniko::BlendMode::new(mix, compose)
}

/// CSS Text Decoration 4 §4 and CSS Backgrounds 3 #shadow-blur: a blurred
/// text shadow is the glyph shadow under a Gaussian blur whose standard
/// deviation is half the blur radius. The filter layer stays unclipped:
/// Vello builds a filter layer's clip in its shifted source viewport, which
/// cut glows within three deviations of the viewport's left and top edges.
pub(super) fn text_shadow_blur(
    shadow: &crate::render::TextShadowPaint,
) -> Option<vello_common::filter_effects::Filter> {
    use vello_common::filter_effects::{EdgeMode, Filter, FilterPrimitive};
    let blur = shadow.blur_radius;
    (blur > 0.0 && blur.is_finite()).then(|| {
        Filter::from_primitive(FilterPrimitive::GaussianBlur {
            std_deviation: blur / 2.0,
            edge_mode: EdgeMode::None,
        })
    })
}

/// A CSS filter function as a filter layer: nested layers apply a list in
/// order, the first function innermost.
pub(super) fn vello_css_filter(filter: &CssFilter) -> vello_common::filter_effects::Filter {
    use vello_common::filter_effects::{EdgeMode, Filter, FilterPrimitive};
    Filter::from_primitive(match *filter {
        CssFilter::ColorMatrix(matrix) => FilterPrimitive::ColorMatrix { matrix },
        CssFilter::Blur(std_deviation) => FilterPrimitive::GaussianBlur {
            std_deviation,
            edge_mode: EdgeMode::None,
        },
        CssFilter::DropShadow {
            dx,
            dy,
            std_deviation,
            color,
        } => FilterPrimitive::DropShadow {
            dx,
            dy,
            std_deviation,
            color: vello_color(color),
            edge_mode: EdgeMode::None,
        },
    })
}

/// Premultiply straight RGBA8 pixels, truncating each scaled component.
pub(super) fn premultiply_rgba(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let alpha = u16::from(pixel[3]);
            let premul = |component| ((u16::from(component) * alpha) / 255) as u8;
            [
                premul(pixel[0]),
                premul(pixel[1]),
                premul(pixel[2]),
                pixel[3],
            ]
        })
        .collect()
}

/// A Vello pixmap from already premultiplied RGBA8 bytes. `may_have_transparency`
/// is the caller's opacity hint; the pixels are not rescanned.
pub(super) fn premultiplied_pixmap(
    premultiplied: Vec<u8>,
    width: u16,
    height: u16,
    may_have_transparency: bool,
) -> Pixmap {
    Pixmap::from_parts(
        premultiplied,
        width,
        height,
        PixelMetadata::new(ImageAlphaType::AlphaPremultiplied, may_have_transparency),
    )
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

pub(super) fn gradient_extend(repeat: bool) -> vello_cpu::peniko::Extend {
    if repeat {
        vello_cpu::peniko::Extend::Repeat
    } else {
        vello_cpu::peniko::Extend::Pad
    }
}

/// A circular gradient between two radii about one center.
pub(super) fn radial_gradient(center: CssPoint, start: f32, end: f32) -> Gradient {
    let center = (f64::from(center.x), f64::from(center.y));
    Gradient::new_two_point_radial(center, start.max(0.0), center, end.max(f32::EPSILON))
}

/// CSS Images 4 #conic-color-stops: 0deg points up and angles increase
/// clockwise. A Vello sweep starts on the positive x axis, also clockwise in
/// y-down space, so its paint turns about the center by the gradient's
/// rotation less a quarter turn.
pub(super) fn conic_transform(center: CssPoint, rotation: f32) -> Affine {
    Affine::rotate_about(
        f64::from(rotation) - std::f64::consts::FRAC_PI_2,
        (f64::from(center.x), f64::from(center.y)),
    )
}

/// Scales a circular gradient's paint vertically about its center into the
/// elliptical ending shape.
pub(super) fn radial_aspect_transform(center: CssPoint, aspect: f32) -> Affine {
    if (aspect - 1.0).abs() < f32::EPSILON || !aspect.is_finite() || aspect <= 0.0 {
        return Affine::IDENTITY;
    }
    let (x, y) = (f64::from(center.x), f64::from(center.y));
    Affine::translate((x, y))
        * Affine::scale_non_uniform(1.0, f64::from(aspect))
        * Affine::translate((-x, -y))
}

pub(super) fn shape_fill(shape: &PaintShape) -> vello_cpu::peniko::Fill {
    if matches!(shape, PaintShape::Polygon { evenodd: true, .. }) {
        vello_cpu::peniko::Fill::EvenOdd
    } else {
        vello_cpu::peniko::Fill::NonZero
    }
}

pub(super) fn shape_path(shape: &PaintShape) -> BezPath {
    match shape {
        PaintShape::Polygon { points, .. } => {
            let mut path = BezPath::new();
            if let Some(first) = points.first() {
                path.move_to((f64::from(first.x), f64::from(first.y)));
                for point in &points[1..] {
                    path.line_to((f64::from(point.x), f64::from(point.y)));
                }
                path.close_path();
            }
            path
        }
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
        PaintShape::Polygon { points, evenodd } => PaintShape::Polygon {
            points: points
                .iter()
                .map(|p| crate::core::CssPoint::new(p.x + dx, p.y + dy))
                .collect(),
            evenodd: *evenodd,
        },
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

/// Retain clip geometry until a visible draw needs its raster mask. Long pages
/// can repeat the same ancestor clip around thousands of offscreen fragments;
/// eagerly rasterizing those clips defeats leaf-command culling.
///
/// CSS Masking 1 §5 (https://drafts.csswg.org/css-masking-1/#clipping-paths)
/// defines clipping as the cumulative intersection, without changing geometry.
/// Keep that conservative intersection immediately, but apply each exact shape
/// in its original device transform only when needed. The complete scene stays
/// available to hit testing, selection, accessibility, and subsequent scrolls.
pub(super) struct RasterClips<'a> {
    viewport: CssRect,
    entries: Vec<DeferredClip<'a>>,
    applied: usize,
    #[cfg(test)]
    applied_total: usize,
}

struct DeferredClip<'a> {
    shape: &'a PaintShape,
    transform: Affine,
    bounds: CssRect,
}

impl<'a> RasterClips<'a> {
    pub(super) fn new(viewport: CssRect) -> Self {
        Self {
            viewport,
            entries: Vec::new(),
            applied: 0,
            #[cfg(test)]
            applied_total: 0,
        }
    }

    pub(super) fn bounds(&self) -> CssRect {
        self.entries
            .last()
            .map_or(self.viewport, |clip| clip.bounds)
    }

    pub(super) fn push(&mut self, shape: &'a PaintShape, logical: Affine2d, device: Affine) {
        let bounds = shape_bounds(shape)
            .map(|bounds| transformed_bounds(bounds, logical))
            .and_then(|bounds| intersect_rect(self.bounds(), bounds))
            .unwrap_or_default();
        self.entries.push(DeferredClip {
            shape,
            transform: device,
            bounds,
        });
    }

    /// Whether the backend must pop a mask; unapplied clips cost no raster work.
    pub(super) fn pop(&mut self) -> bool {
        self.entries.pop();
        if self.applied > self.entries.len() {
            self.applied -= 1;
            true
        } else {
            false
        }
    }

    /// Every active clip, outermost first, with its device transform.
    pub(super) fn active(&self) -> impl Iterator<Item = (&'a PaintShape, Affine)> + '_ {
        self.entries.iter().map(|clip| (clip.shape, clip.transform))
    }

    /// Apply the pending suffix in order, before paint or a compositing layer.
    /// The caller restores its current draw transform when this returns true.
    pub(super) fn apply(&mut self, mut push: impl FnMut(&PaintShape, Affine)) -> bool {
        let changed = self.applied < self.entries.len();
        for clip in &self.entries[self.applied..] {
            push(clip.shape, clip.transform);
        }
        #[cfg(test)]
        {
            self.applied_total += self.entries.len() - self.applied;
        }
        self.applied = self.entries.len();
        changed
    }
}

fn apply_clips(context: &mut RenderContext, clips: &mut RasterClips<'_>, transform: Affine) {
    if clips.apply(|shape, at| {
        context.set_transform(at);
        context.set_fill_rule(shape_fill(shape));
        context.push_clip_path(&shape_path(shape));
    }) {
        context.set_fill_rule(vello_cpu::peniko::Fill::NonZero);
        context.set_transform(transform);
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

/// CSS Backgrounds 3 #shadow-shape: an outer shadow is drawn outside the
/// border box only, as if the box were opaque. Clipping to this path with
/// the even-odd rule leaves the box itself, and only it, unpainted.
pub(super) fn outside_shape_path(shape: &PaintShape, shadow: &PaintShape, reach: f32) -> BezPath {
    let bounds = [shape, shadow]
        .into_iter()
        .filter_map(shape_bounds)
        .reduce(|a, b| {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            CssRect::new(
                x,
                y,
                (a.x + a.width).max(b.x + b.width) - x,
                (a.y + a.height).max(b.y + b.height) - y,
            )
        })
        .unwrap_or_default();
    let margin = reach.max(0.0) * 2.0 + 2.0;
    let mut path = rect_path(CssRect::new(
        bounds.x - margin,
        bounds.y - margin,
        bounds.width + 2.0 * margin,
        bounds.height + 2.0 * margin,
    ));
    path.extend(shape_path(shape));
    path
}

/// CSS Backgrounds 3 #shadow-shape: an inner shadow is cast as if everything
/// outside the padding edge (`shape`) were opaque. Its perimeter is the
/// padding box shifted by the offset and contracted by the spread, flooring
/// its size and corner radii at zero.
pub(super) fn inset_shadow_hole(shape: &PaintShape, offset: CssPoint, spread: f32) -> PaintShape {
    offset_shape(shape, offset.x, offset.y, -spread)
}

/// The unblurred inner shadow: the padding box minus the shadow perimeter,
/// filled with the even-odd rule inside a clip to the padding box.
pub(super) fn inset_shadow_ring(shape: &PaintShape, hole: &PaintShape) -> BezPath {
    let mut path = shape_path(shape);
    path.extend(shape_path(hole));
    path
}

/// Vello's inverted blurred rectangle rasterizes only the perimeter's blur
/// extent, though its paint is opaque everywhere outside it. A blurred inner
/// shadow instead fills the padding box and erases the blurred perimeter
/// from that layer with this mode.
pub(super) fn erase_blend() -> vello_cpu::peniko::BlendMode {
    vello_cpu::peniko::BlendMode::new(Mix::Normal, Compose::DestOut)
}

pub(super) fn shape_bounds(shape: &PaintShape) -> Option<CssRect> {
    match shape {
        PaintShape::Polygon { points, .. } => point_bounds(points.iter().copied()),
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
    fn svg_cache_variants_obey_budget_and_frame_lifetime_cpu() {
        let mut renderer = VelloCpuRenderer::new();
        super::super::image_raster_tests::check_svg_cache_budget(|scene| {
            let frame = renderer.render_rgba(scene).unwrap();
            (frame, renderer.images.len())
        });
    }

    fn clip_test_scene(scale: f64) -> Scene {
        Scene {
            viewport: ViewportMetrics::from_physical(
                PhysicalSize::new((96.0 * scale) as u32, (64.0 * scale) as u32),
                ScaleFactor::new(scale),
            ),
            primitives: vec![DisplayCommand::FillRect {
                rect: CssRect::new(0.0, 0.0, 96.0, 64.0),
                color: PaintColor::Rgba(240, 240, 240, 255),
            }],
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 96.0, 64.0),
            image_store: Default::default(),
            canvas_images: Default::default(),
            page_scroll_containers: Vec::new(),
            page_size: crate::core::CssSize::new(96.0, 64.0),
        }
    }

    #[test]
    fn deferred_clips_do_not_rasterize_ancestors_of_offscreen_content() {
        let mut scene = clip_test_scene(1.0);
        for row in 0..1_024 {
            // A long overflow container intersects the viewport even when the
            // individual descendant wrapped in its clip is far below it.
            scene.primitives.extend([
                DisplayCommand::PushClip(PaintShape::Rect(CssRect::new(0.0, 0.0, 96.0, 100_000.0))),
                DisplayCommand::PushTransform(Affine2d::translate(
                    0.0,
                    1_000.0 + row as f32 * 40.0,
                )),
                DisplayCommand::FillRect {
                    rect: CssRect::new(0.0, 0.0, 80.0, 30.0),
                    color: PaintColor::Rgba(240, 0, 0, 255),
                },
                DisplayCommand::PopTransform,
                DisplayCommand::PopClip,
            ]);
        }
        scene.primitives.extend([
            DisplayCommand::PushClip(PaintShape::Rect(CssRect::new(8.0, 8.0, 20.0, 20.0))),
            DisplayCommand::FillRect {
                rect: CssRect::new(0.0, 0.0, 96.0, 64.0),
                color: PaintColor::Rgba(0, 0, 240, 255),
            },
            DisplayCommand::PopClip,
        ]);
        let mut deferred = VelloCpuRenderer::new();
        let mut eager = VelloCpuRenderer::new();
        eager.eager_clips = true;
        assert_eq!(
            deferred.render_rgba(&scene).unwrap().pixels,
            eager.render_rgba(&scene).unwrap().pixels,
        );
        assert_eq!(deferred.rasterized_clips, 1);
        assert_eq!(eager.rasterized_clips, 1_025);

        // Reusing the renderer and scrolling those descendants into view must
        // restore their clips rather than treating the earlier cull as final.
        scene.primitives.insert(
            1,
            DisplayCommand::PushTransform(Affine2d::translate(0.0, -1_000.0)),
        );
        scene.primitives.push(DisplayCommand::PopTransform);
        assert_eq!(
            deferred.render_rgba(&scene).unwrap().pixels,
            eager.render_rgba(&scene).unwrap().pixels,
        );
        assert_eq!(deferred.rasterized_clips, 2);
    }

    #[test]
    fn clipped_compositing_groups_skip_backend_work_and_reappear_when_scrolled() {
        use super::super::{BlendMode, CompositingLayer};
        let mut scene = clip_test_scene(1.25);
        scene
            .primitives
            .push(DisplayCommand::PushTransform(Affine2d::IDENTITY));
        for row in 0..64 {
            scene.primitives.extend([
                DisplayCommand::PushTransform(Affine2d::translate(0., 1_000. + row as f32 * 40.)),
                DisplayCommand::PushClip(PaintShape::Rect(CssRect::new(0., 0., 90., 30.))),
                DisplayCommand::PushLayer(CompositingLayer::new(
                    0.6,
                    BlendMode::Multiply,
                    Arc::from([]),
                )),
                DisplayCommand::PushTransform(Affine2d::translate(3., 2.)),
                DisplayCommand::FillRect {
                    rect: CssRect::new(0., 0., 90., 30.),
                    color: PaintColor::Rgba(240, 0, 0, 255),
                },
                DisplayCommand::PopTransform,
                DisplayCommand::PopLayer,
                DisplayCommand::PopClip,
                DisplayCommand::PopTransform,
            ]);
        }
        scene.primitives.push(DisplayCommand::PopTransform);
        let mut optimized = VelloCpuRenderer::new();
        let mut reference = VelloCpuRenderer::new();
        reference.eager_clips = true;
        let before = optimized.render_rgba(&scene).unwrap();
        assert_eq!(before.pixels, reference.render_rgba(&scene).unwrap().pixels);
        assert_eq!(optimized.rasterized_clips, 0);
        assert_eq!(reference.rasterized_clips, 64);
        scene.primitives[1] = DisplayCommand::PushTransform(Affine2d::translate(0., -1_000.));
        let scrolled = optimized.render_rgba(&scene).unwrap();
        assert_eq!(
            scrolled.pixels,
            reference.render_rgba(&scene).unwrap().pixels
        );
        assert_ne!(scrolled.pixels, before.pixels);
        assert!(optimized.rasterized_clips < 4);
    }

    #[test]
    fn deferred_clips_preserve_transforms_intersections_and_compositing() {
        // CSS Masking §5 fixes each clip in the coordinate system where it
        // was established. Later transforms, empty clips, nested rounded and
        // even-odd clips, and opacity/blend groups must retain eager pixels.
        for scale in [1.0, 1.5, 2.0] {
            let mut scene = clip_test_scene(scale);
            scene.primitives.extend([
                DisplayCommand::PushTransform(Affine2d::translate(20.25, 8.5)),
                DisplayCommand::PushClip(PaintShape::RoundedRect {
                    rect: CssRect::new(0.0, 0.0, 40.0, 36.0),
                    radii: super::super::CornerRadii {
                        corners: [(7.0, 5.0); 4],
                    },
                }),
                DisplayCommand::PopTransform,
                DisplayCommand::PushLayer(super::super::CompositingLayer::new(
                    0.6,
                    BlendMode::Multiply,
                    Default::default(),
                )),
                DisplayCommand::PushTransform(Affine2d([0.9, 0.2, -0.1, 0.8, -3.5, 6.25])),
                DisplayCommand::PushClip(PaintShape::Polygon {
                    points: vec![
                        CssPoint::new(0.0, 0.0),
                        CssPoint::new(80.0, 50.0),
                        CssPoint::new(0.0, 50.0),
                        CssPoint::new(80.0, 0.0),
                    ],
                    evenodd: true,
                }),
                DisplayCommand::FillRect {
                    rect: CssRect::new(-30.0, -30.0, 150.0, 150.0),
                    color: PaintColor::Rgba(240, 0, 0, 255),
                },
                DisplayCommand::PushClip(PaintShape::Rect(CssRect::default())),
                DisplayCommand::PushTransform(Affine2d::translate(30.0, 10.0)),
                DisplayCommand::FillRect {
                    rect: CssRect::new(0.0, 0.0, 96.0, 64.0),
                    color: PaintColor::Rgba(0, 240, 0, 255),
                },
                DisplayCommand::PopTransform,
                DisplayCommand::PopClip,
                DisplayCommand::PopClip,
                DisplayCommand::PopTransform,
                DisplayCommand::PopLayer,
                DisplayCommand::PopClip,
                DisplayCommand::FillRect {
                    rect: CssRect::new(70.0, 6.0, 20.0, 20.0),
                    color: PaintColor::Rgba(0, 0, 240, 255),
                },
            ]);
            let mut deferred = VelloCpuRenderer::new();
            let mut eager = VelloCpuRenderer::new();
            eager.eager_clips = true;
            let actual = deferred.render_rgba(&scene).unwrap();
            assert_eq!(actual.pixels, eager.render_rgba(&scene).unwrap().pixels);
            assert!(actual.pixels.as_chunks::<4>().0.iter().any(|p| p[0] > p[1]));
            assert!(actual.pixels.as_chunks::<4>().0.contains(&[0, 0, 240, 255]));
            assert_eq!(deferred.rasterized_clips, 2);
        }
    }

    #[test]
    fn svg_chart_lines_stay_sharp_after_resize_and_device_scale_changes() {
        // A one-user-unit line in a 1200-unit chart vanishes when the SVG is
        // first reduced to the default 300x150 bitmap and then enlarged.
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 600"><rect width="1200" height="600" fill="white"/><rect x="600" width="1" height="600" fill="black"/></svg>"#;
        let image = crate::img::decode_graphical(source).unwrap();
        assert_eq!((image.width, image.height), (300, 150));
        let mut scene = clip_test_scene(1.);
        scene.primitives.clear();
        let handle = ImageHandle(42);
        scene.image_store.insert(handle, image);
        let mut renderer = VelloCpuRenderer::new();
        for (width, scale) in [(1200., 1.), (600., 2.), (1200., 2.), (1200., 1.)] {
            let physical = PhysicalSize::new((width * scale) as u32, (width * scale / 2.) as u32);
            scene.viewport = ViewportMetrics::from_physical(physical, ScaleFactor::new(scale));
            scene.primitives = vec![DisplayCommand::Image {
                rect: CssRect::new(0., 0., width as f32, width as f32 / 2.),
                handle,
                source_rect: None,
                fit: ImageFit::Fill,
                sampling: ImageSampling::Smooth,
                clip: None,
                node: 1,
                link: None,
            }];
            let frame = renderer.render_rgba(&scene).unwrap();
            let x = physical.width as usize / 2;
            let y = physical.height as usize / 2;
            let offset = (y * physical.width as usize + x) * 4;
            assert_eq!(
                &frame.pixels[offset..offset + 4],
                &[0, 0, 0, 255],
                "{width} at {scale}x"
            );
            assert_eq!(&frame.pixels[offset - 4..offset], &[255, 255, 255, 255]);
            let (&key, cached) = renderer
                .images
                .iter()
                .find(|(_, image)| image.last_used_frame == renderer.frame_id)
                .unwrap();
            assert_eq!((cached.width, cached.height), (300, 150));
            assert_eq!(
                (cached.upload_width, cached.upload_height),
                (physical.width, physical.height)
            );
            let ImageSource::OpaqueId { id: uploaded, .. } = cached.source else {
                unreachable!()
            };
            renderer.render_rgba(&scene).unwrap();
            let ImageSource::OpaqueId { id: reused, .. } = renderer.images[&key].source else {
                unreachable!()
            };
            assert_eq!(reused, uploaded, "unchanged frames reuse their SVG raster");
        }
    }

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
                svg_source: None,
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
            svg_source: None,
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
        let first = renderer.images[&handle.into()].revision;
        renderer.render(&scene).unwrap();
        assert_eq!(renderer.images[&handle.into()].revision, first);

        scene.image_store.insert(handle, resource([0, 0, 255, 255]));
        renderer.render(&scene).unwrap();
        assert_ne!(renderer.images[&handle.into()].revision, first);
        assert_eq!(
            renderer.images[&handle.into()].revision,
            scene.image_store.revision(handle).unwrap()
        );
    }

    #[test]
    fn decoration_strokes_follow_thickness_offset_and_style() {
        use vello_cpu::kurbo::Shape;
        let style = crate::text::TextStyle {
            size: 20.0,
            ..Default::default()
        };
        let mut shaped = crate::text::shape("underlined", &style);
        shaped.underline = true;
        let origin = CssPoint::new(10.0, 50.0);
        let decoration = |style, thickness, underline_offset| crate::render::TextDecorationPaint {
            color: PaintColor::Rgba(0, 0, 0, 255),
            style,
            thickness,
            underline_offset,
        };
        // CSS Text Decoration 4: `text-underline-offset` moves the line's
        // top edge from the alphabetic baseline; the thickness is the width.
        let strokes = decoration_strokes(
            origin,
            &shaped,
            &decoration(DecorationStyle::Solid, Some(3.0), Some(2.0)),
            Affine::IDENTITY,
        );
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].0.as_ref().map(|stroke| stroke.width), Some(3.0));
        let bounds = strokes[0].1.bounding_box();
        let center = f64::from(origin.y + shaped.baseline) + 2.0 + 1.5;
        assert!((bounds.y0 - center).abs() < 0.01 && (bounds.y1 - center).abs() < 0.01);
        // A wavy line oscillates around that center; double draws two lines.
        let wavy = decoration_strokes(
            origin,
            &shaped,
            &decoration(DecorationStyle::Wavy, Some(1.0), Some(0.0)),
            Affine::IDENTITY,
        );
        assert!(wavy[0].1.bounding_box().height() > 1.0);
        let double = decoration_strokes(
            origin,
            &shaped,
            &decoration(DecorationStyle::Double, None, None),
            Affine::IDENTITY,
        );
        assert_eq!(double.len(), 2);
        // CSS Text Decoration 4 #text-decoration-style: `dotted` is a row of
        // dots one thickness across and apart, filled rather than stroked.
        let dotted = decoration_strokes(
            origin,
            &shaped,
            &decoration(DecorationStyle::Dotted, Some(3.0), Some(0.0)),
            Affine::IDENTITY,
        );
        assert_eq!(dotted.len(), 1);
        assert!(dotted[0].0.is_none());
        let dots = dotted[0]
            .1
            .elements()
            .iter()
            .filter(|element| matches!(element, vello_cpu::kurbo::PathEl::MoveTo(_)))
            .count();
        assert_eq!(dots, ((shaped.advance + 3.0) / 6.0).floor() as usize);
        let bounds = dotted[0].1.bounding_box();
        assert!((bounds.height() - 3.0).abs() < 0.01, "{bounds:?}");
    }

    #[test]
    fn dotted_decorations_paint_every_dot() {
        // Each dot of a dotted underline used to be a zero-length dash with
        // round caps, which the stroker drops, so only stray dots remained.
        let mut context = RenderContext::new(200, 20);
        context.set_paint(vello_color(PaintColor::Rgba(0, 0, 255, 255)));
        let mut shaped = crate::text::shape(
            " ",
            &crate::text::TextStyle {
                size: 10.0,
                ..Default::default()
            },
        );
        shaped.underline = true;
        shaped.advance = 180.0;
        let decoration = crate::render::TextDecorationPaint {
            color: PaintColor::Rgba(0, 0, 255, 255),
            style: DecorationStyle::Dotted,
            thickness: Some(4.0),
            underline_offset: Some(0.0),
        };
        paint_decorations(
            &mut context,
            CssPoint::new(10.0, 0.0),
            &shaped,
            &decoration,
            Affine::IDENTITY,
        );
        let mut pixmap = Pixmap::new(200, 20);
        context.flush();
        context.render(&mut pixmap, &mut Resources::new());
        let row = (shaped.baseline + 2.0) as usize;
        let data = pixmap.data();
        let blue = |x: usize| data[row * 200 + x].b > 128;
        // Dots 4px across every 8px from x = 10: one centered on 12 + 8k.
        for dot in 0..22 {
            assert!(blue(12 + dot * 8), "dot {dot}");
            assert!(!blue(16 + dot * 8), "gap {dot}");
        }
    }

    #[test]
    fn thin_dotted_decorations_fill_whole_device_pixels() {
        // CSS Text Decoration 4 #text-decoration-thickness-property: the
        // thickness should round to whole device pixels, at least one; and
        // `dotted` means what it does for borders, whose thin dots are
        // squares on the pixel grid. A default-thickness dotted underline
        // (about 1.03px at 16px) used to be judged thin in CSS rather than
        // device pixels and left off the grid, so it rasterized as a gray
        // smear instead of dots.
        let mut shaped = crate::text::shape(
            " ",
            &crate::text::TextStyle {
                size: 16.0,
                ..Default::default()
            },
        );
        shaped.underline = true;
        shaped.advance = 100.3;
        let decoration = crate::render::TextDecorationPaint {
            color: PaintColor::Rgba(0, 0, 255, 255),
            style: DecorationStyle::Dotted,
            thickness: None,
            underline_offset: None,
        };
        let thickness = shaped.line_height / 18.0;
        assert!((0.9..1.2).contains(&thickness), "{thickness}");
        for (scale, size) in [(1.0, 1), (1.5, 2), (2.0, 2)] {
            let (width, height) = ((140.0 * scale) as u16, (40.0 * scale) as u16);
            let transform = Affine::scale(scale);
            let mut context = RenderContext::new(width, height);
            context.set_transform(transform);
            context.set_paint(vello_color(decoration.color));
            paint_decorations(
                &mut context,
                CssPoint::new(10.3, 0.4),
                &shaped,
                &decoration,
                transform,
            );
            let mut pixmap = Pixmap::new(width, height);
            context.flush();
            context.render(&mut pixmap, &mut Resources::new());
            let (width, height) = (usize::from(width), usize::from(height));
            let alpha = |x: usize, y: usize| pixmap.data()[y * width + x].a;
            // Every pixel is fully inked or untouched.
            for y in 0..height {
                for x in 0..width {
                    assert!(
                        matches!(alpha(x, y), 0 | 255),
                        "{scale}x: ({x}, {y}) {}",
                        alpha(x, y)
                    );
                }
            }
            // `size` rows of `size`-pixel dots and gaps from the start.
            let rows = (0..height)
                .filter(|&y| (0..width).any(|x| alpha(x, y) > 0))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), size, "{scale}x: {rows:?}");
            let mut runs = Vec::new();
            let mut x = 0;
            while x < width {
                if alpha(x, rows[0]) == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < width && alpha(x, rows[0]) > 0 {
                    x += 1;
                }
                runs.push((start, x));
            }
            assert_eq!(runs[0].0, (10.3 * scale).round() as usize, "{scale}x");
            assert!(runs.len() > 20, "{scale}x: {runs:?}");
            for pair in runs.windows(2) {
                assert_eq!(pair[0].1 - pair[0].0, size, "{scale}x: {runs:?}");
                assert_eq!(pair[1].0 - pair[0].1, size, "{scale}x: {runs:?}");
            }
        }
    }

    #[test]
    fn cpu_raster_paints_retained_text_shadow_ink() {
        let snapshot = BrowserSnapshot {
            address: String::new(),
            status: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(120.0, 80.0),
            page_revision: 0,
        };
        let mut scene = desktop_shell(
            ViewportMetrics::from_physical(PhysicalSize::new(120, 80), ScaleFactor::new(1.0)),
            &snapshot,
        );
        let style = crate::text::TextStyle {
            size: 28.0,
            ..Default::default()
        };
        scene.primitives.push(DisplayCommand::GlyphRun {
            origin: CssPoint::new(12.0, 38.0),
            shaped: crate::text::shape("M", &style),
            color: PaintColor::Rgba(0, 0, 0, 255),
            decoration: crate::render::TextDecorationPaint {
                color: PaintColor::Rgba(0, 0, 0, 255),
                style: DecorationStyle::Solid,
                thickness: None,
                underline_offset: None,
            },
            shadows: Vec::new(),
            clip: None,
            node: 1,
            link: None,
        });

        let mut renderer = VelloCpuRenderer::new();
        let without_shadow = renderer.render(&scene).unwrap().pixels.to_vec();
        let Some(DisplayCommand::GlyphRun { shadows, .. }) = scene.primitives.last_mut() else {
            unreachable!()
        };
        shadows.push(crate::render::TextShadowPaint {
            color: PaintColor::Rgba(255, 0, 255, 255),
            offset: CssPoint::new(24.0, 0.0),
            blur_radius: 0.0,
            spread: 0.0,
            inset: false,
        });
        let with_shadow = renderer.render(&scene).unwrap().pixels.to_vec();
        assert!(
            without_shadow
                .iter()
                .zip(&with_shadow)
                .any(|(before, after)| before != after),
            "a retained text-shadow layer must reach the raster output"
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
                svg_source: None,
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
                    svg_source: None,
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
        assert!(renderer.images.contains_key(&idle.into()));
        if let Some(DisplayCommand::Image { handle, .. }) = scene.primitives.last_mut() {
            *handle = active;
        }
        renderer.render(&scene).unwrap();
        assert!(renderer.images.contains_key(&idle.into()));
        assert!(renderer.images.contains_key(&active.into()));
    }
}
