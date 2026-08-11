//! Thin Vello Hybrid/wgpu adapter.
//!
//! Browser, layout, and display-list code never see Vello or wgpu types.  The
//! adapter retains the device, pipelines, glyph/image atlases and surface
//! across frames; only Vello Hybrid's documented per-frame `Scene` packet is
//! rebuilt.  The CPU backend remains the reference and clean fallback.
//!
//! Vello Hybrid 0.2 limitations relevant to this adapter are intentionally
//! quarantined here: mask layers and complex filter graphs can panic, some
//! non-isolated destructive blends are unsupported, glyph-atlas caching is
//! still experimental, and several allocation failures panic instead of
//! returning `RenderError`. TRust does not emit masks/filter graphs today;
//! compositing commands use isolated layers, and the desktop contains a
//! backend panic/error by dropping Hybrid and replaying the unchanged list on
//! CPU. Making those failure paths fallible and stabilizing resource lifetime
//! APIs are good upstream Vello contributions.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vello_common::color::PremulRgba8;
use vello_common::kurbo::{Affine, BezPath, Cap, Diagonal2, Rect, Stroke};
use vello_common::paint::ImageSource;
use vello_common::peniko::{ImageBrush, ImageQuality, ImageSampler};
use vello_hybrid::{Pixmap, RenderSize, RenderTargetConfig, Resources, TextureBindings};
use wgpu::{CurrentSurfaceTexture, SurfaceConfiguration, TextureFormat, TextureView};
use winit::window::Window;

use super::vello_cpu::{
    MAX_REGISTERED_IMAGES, OwnedRgbaFrame, intersect_rect, offset_shape, point_bounds,
    rect_is_visible, rect_path, shape_bounds, shape_is_visible, shape_path, simple_rounded_rect,
    transformed_bounds, vello_affine, vello_blend, vello_color, vello_rect, vello_stops,
    vello_stroke,
};
use super::{
    Affine2d, CssRect, DecorationStyle, DisplayCommand, ImageFit, ImageHandle, ImageResource,
    ImageSampling, PaintBrush, PaintColor, Primitive, RasterBackend, RasterFrame, RendererKind,
    Scene, is_desktop_heart_image_handle,
};
use crate::core::{CssPoint, PhysicalSize};

const GPU_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
// Vello Hybrid 0.2's default image atlas is 4096x4096. Keep uploads within
// that allocation boundary even on devices advertising larger 2D textures.
const HYBRID_IMAGE_ATLAS_LIMIT: u32 = 4096;

struct CachedImage {
    source: ImageSource,
    upload_width: u32,
    upload_height: u32,
    // CSS Images 3 §4: natural dimensions drive concrete object sizing. The
    // backend upload may be downsampled, but these values must remain natural.
    width: u32,
    height: u32,
    revision: u64,
    last_used_frame: u64,
}

struct HeadlessTarget {
    texture: wgpu::Texture,
    view: TextureView,
    readback: wgpu::Buffer,
    bytes_per_row: u32,
    size: PhysicalSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Skipped,
}

/// Retained accelerated renderer. It can either own a presentable winit
/// surface or a reusable off-screen texture for differential tests.
pub struct VelloHybridRenderer {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: vello_hybrid::Renderer,
    resources: Resources,
    texture_bindings: TextureBindings,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<SurfaceConfiguration>,
    window: Option<Arc<Window>>,
    format: TextureFormat,
    size: PhysicalSize,
    images: HashMap<ImageHandle, CachedImage>,
    frame_id: u64,
    headless: Option<HeadlessTarget>,
    rgba: Vec<u8>,
    presented: Vec<u32>,
    device_failure: Arc<Mutex<Option<String>>>,
    adapter_name: String,
}

impl VelloHybridRenderer {
    /// Initialize a GPU renderer compatible with `window`. The future only
    /// covers adapter/device negotiation; rendering remains synchronous with
    /// the winit redraw callback and does not create a polling loop.
    pub async fn new_window(window: Arc<Window>) -> Result<Self, String> {
        let size = physical_size(window.inner_size().width, window.inner_size().height);
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|error| format!("could not create Hybrid surface: {error}"))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                apply_limit_buckets: false,
            })
            .await
            .map_err(|error| format!("no compatible Hybrid adapter: {error}"))?;
        let (device, queue) = request_device(&adapter).await?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = preferred_surface_format(&capabilities.formats)
            .ok_or_else(|| String::from("Hybrid surface reports no texture formats"))?;
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| String::from("Hybrid adapter cannot configure this surface"))?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 2;
        if !size.is_empty() {
            surface.configure(&device, &config);
        }
        Self::from_device(
            instance,
            adapter,
            device,
            queue,
            format,
            size,
            Some(surface),
            Some(config),
            Some(window),
        )
    }

    /// Initialize a window-free renderer. Used only by parity/regression tools;
    /// production Hybrid presentation never performs a GPU-to-CPU readback.
    pub async fn new_headless() -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|error| format!("no headless Hybrid adapter: {error}"))?;
        let (device, queue) = request_device(&adapter).await?;
        Self::from_device(
            instance,
            adapter,
            device,
            queue,
            TextureFormat::Rgba8Unorm,
            PhysicalSize::new(1, 1),
            None,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_device(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: TextureFormat,
        size: PhysicalSize,
        surface: Option<wgpu::Surface<'static>>,
        surface_config: Option<SurfaceConfiguration>,
        window: Option<Arc<Window>>,
    ) -> Result<Self, String> {
        ensure_device_size(&device, size)?;
        let (renderer, resources) = vello_hybrid::Renderer::new(
            &device,
            &RenderTargetConfig {
                format,
                width: size.width.max(1),
                height: size.height.max(1),
            },
        );
        let failure = Arc::new(Mutex::new(None));
        let validation_failure = Arc::clone(&failure);
        device.on_uncaptured_error(Arc::new(move |error| {
            *validation_failure.lock().unwrap() = Some(error.to_string());
        }));
        let lost_failure = Arc::clone(&failure);
        device.set_device_lost_callback(move |reason, message| {
            *lost_failure.lock().unwrap() =
                Some(format!("GPU device lost ({reason:?}): {message}"));
        });
        let adapter_name = adapter.get_info().name;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            renderer,
            resources,
            texture_bindings: TextureBindings::new(),
            surface,
            surface_config,
            window,
            format,
            size,
            images: HashMap::new(),
            frame_id: 0,
            headless: None,
            rgba: Vec::new(),
            presented: Vec::new(),
            device_failure: failure,
            adapter_name,
        })
    }

    pub fn kind(&self) -> RendererKind {
        RendererKind::Hybrid
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub fn resize(&mut self, size: PhysicalSize) -> Result<(), String> {
        self.size = size;
        self.headless = None;
        if size.is_empty() {
            return Ok(());
        }
        ensure_device_size(&self.device, size)?;
        if let (Some(surface), Some(config)) = (&self.surface, &mut self.surface_config)
            && (config.width != size.width || config.height != size.height)
        {
            config.width = size.width;
            config.height = size.height;
            surface.configure(&self.device, config);
        }
        Ok(())
    }

    /// Drop only the platform surface while preserving device, pipelines and
    /// caches. This follows winit's suspend contract on platforms where a
    /// native surface cannot remain alive while suspended.
    pub fn suspend(&mut self) {
        self.surface = None;
    }

    pub fn resume(&mut self, window: Arc<Window>) -> Result<(), String> {
        self.window = Some(Arc::clone(&window));
        let surface = self
            .instance
            .create_surface(window)
            .map_err(|error| format!("could not recreate Hybrid surface: {error}"))?;
        let capabilities = surface.get_capabilities(&self.adapter);
        if !capabilities.formats.contains(&self.format) {
            return Err(format!(
                "resumed surface no longer supports Hybrid format {:?}",
                self.format
            ));
        }
        if let Some(config) = &self.surface_config
            && !self.size.is_empty()
        {
            surface.configure(&self.device, config);
        }
        self.surface = Some(surface);
        Ok(())
    }

    /// Render directly into the swapchain. Surface timeout/occlusion is a
    /// skipped frame, not a renderer failure and not a redraw spin.
    pub fn present(&mut self, scene: &Scene) -> Result<PresentOutcome, String> {
        match catch_unwind(AssertUnwindSafe(|| self.present_inner(scene))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "Vello Hybrid panicked while rendering: {}",
                panic_message(payload)
            )),
        }
    }

    fn present_inner(&mut self, scene: &Scene) -> Result<PresentOutcome, String> {
        self.check_device()?;
        self.resize(scene.viewport.physical)?;
        if self.size.is_empty() {
            return Ok(PresentOutcome::Skipped);
        }
        for attempt in 0..2 {
            let current = self.acquire_surface()?;
            let (texture, reconfigure_after) = match current {
                CurrentSurfaceTexture::Success(texture) => (texture, false),
                CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
                CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                    return Ok(PresentOutcome::Skipped);
                }
                CurrentSurfaceTexture::Outdated => {
                    self.configure_surface()?;
                    if attempt == 0 {
                        continue;
                    }
                    return Err(String::from(
                        "Hybrid surface remained outdated after configure",
                    ));
                }
                CurrentSurfaceTexture::Lost => {
                    self.recreate_surface()?;
                    if attempt == 0 {
                        continue;
                    }
                    return Err(String::from(
                        "Hybrid surface remained lost after recreation",
                    ));
                }
                CurrentSurfaceTexture::Validation => {
                    return Err(self
                        .take_device_failure()
                        .unwrap_or_else(|| String::from("Hybrid surface validation failed")));
                }
            };
            let view = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("TRust Vello Hybrid frame"),
                });
            let hybrid_scene = self.build_scene(scene, &mut encoder)?;
            self.render_to_view(&hybrid_scene, &view, &mut encoder)?;
            self.queue.submit([encoder.finish()]);
            self.queue.present(texture);
            if reconfigure_after {
                self.configure_surface()?;
            }
            self.check_device()?;
            return Ok(PresentOutcome::Presented);
        }
        Err(String::from("could not acquire Hybrid surface"))
    }

    pub fn render_rgba(&mut self, scene: &Scene) -> Result<OwnedRgbaFrame, String> {
        match catch_unwind(AssertUnwindSafe(|| self.render_rgba_inner(scene))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "Vello Hybrid panicked during headless rendering: {}",
                panic_message(payload)
            )),
        }
    }

    fn render_rgba_inner(&mut self, scene: &Scene) -> Result<OwnedRgbaFrame, String> {
        self.check_device()?;
        let size = scene.viewport.physical;
        if size.is_empty() {
            return Err(String::from("cannot render an empty Hybrid framebuffer"));
        }
        ensure_device_size(&self.device, size)?;
        self.ensure_headless_target(size);
        let view = self.headless.as_ref().unwrap().view.clone();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("TRust Vello Hybrid headless frame"),
            });
        let hybrid_scene = self.build_scene(scene, &mut encoder)?;
        self.render_to_view(&hybrid_scene, &view, &mut encoder)?;
        let target = self.headless.as_ref().unwrap();
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.bytes_per_row),
                    rows_per_image: Some(size.height),
                },
            },
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        let submission = self.queue.submit([encoder.finish()]);
        let slice = target.readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result.map_err(|error| error.to_string()));
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(GPU_WAIT_TIMEOUT),
            })
            .map_err(|error| format!("timed out waiting for Hybrid readback: {error}"))?;
        receiver
            .recv_timeout(GPU_WAIT_TIMEOUT)
            .map_err(|error| format!("Hybrid readback callback failed: {error}"))??;
        let mapped = slice
            .get_mapped_range()
            .map_err(|error| format!("could not access Hybrid readback: {error}"))?;
        self.rgba.clear();
        self.rgba
            .reserve(size.width as usize * size.height as usize * 4);
        for row in mapped.chunks_exact(target.bytes_per_row as usize) {
            for pixel in row[..size.width as usize * 4].chunks_exact(4) {
                let alpha = pixel[3];
                let unpremultiply = |component: u8| {
                    if alpha == 0 {
                        0
                    } else {
                        ((u16::from(component) * 255 + u16::from(alpha) / 2) / u16::from(alpha))
                            .min(255) as u8
                    }
                };
                self.rgba.extend_from_slice(&[
                    unpremultiply(pixel[0]),
                    unpremultiply(pixel[1]),
                    unpremultiply(pixel[2]),
                    alpha,
                ]);
            }
        }
        drop(mapped);
        target.readback.unmap();
        self.check_device()?;
        Ok(OwnedRgbaFrame {
            size,
            pixels: self.rgba.clone(),
        })
    }

    fn ensure_headless_target(&mut self, size: PhysicalSize) {
        if self
            .headless
            .as_ref()
            .is_some_and(|target| target.size == size)
        {
            return;
        }
        let bytes_per_row = (size.width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("TRust Hybrid headless target"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("TRust Hybrid readback"),
            size: u64::from(bytes_per_row) * u64::from(size.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.headless = Some(HeadlessTarget {
            texture,
            view,
            readback,
            bytes_per_row,
            size,
        });
    }

    fn render_to_view(
        &mut self,
        scene: &vello_hybrid::Scene,
        view: &TextureView,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(), String> {
        self.renderer
            .render(
                scene,
                &mut self.resources,
                &self.device,
                &self.queue,
                encoder,
                &RenderSize {
                    width: u32::from(scene.width()),
                    height: u32::from(scene.height()),
                },
                view,
                &self.texture_bindings,
            )
            .map_err(|error| format!("Vello Hybrid render failed: {error}"))
    }

    fn build_scene(
        &mut self,
        scene: &Scene,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<vello_hybrid::Scene, String> {
        let size = scene.viewport.physical;
        let width = u16::try_from(size.width)
            .map_err(|_| format!("framebuffer width {} exceeds Vello limit", size.width))?;
        let height = u16::try_from(size.height)
            .map_err(|_| format!("framebuffer height {} exceeds Vello limit", size.height))?;
        self.size = size;
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
                self.renderer
                    .destroy_image(&mut self.resources, encoder, id);
            }
        }

        let mut target = vello_hybrid::Scene::new(width, height);
        let device_transform = Affine::scale(scene.viewport.scale_factor.get());
        let mut transforms = vec![device_transform];
        let mut logical_transforms = vec![Affine2d::IDENTITY];
        let mut visible_clips = vec![CssRect::new(
            0.0,
            0.0,
            scene.viewport.css.width,
            scene.viewport.css.height,
        )];
        target.set_transform(device_transform);

        for command in &scene.primitives {
            match command {
                DisplayCommand::Fill { shape, brush } => {
                    if shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                        0.0,
                    ) {
                        set_brush(&mut target, brush);
                        target.fill_path(&shape_path(shape));
                    }
                }
                DisplayCommand::Stroke {
                    shape,
                    brush,
                    style,
                } => {
                    if shape_is_visible(
                        shape,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                        style.width.max(0.0) / 2.0,
                    ) {
                        set_brush(&mut target, brush);
                        target.set_stroke(vello_stroke(style));
                        target.stroke_path(&shape_path(shape));
                    }
                }
                DisplayCommand::PushClip(shape) => {
                    target.push_clip_path(&shape_path(shape));
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
                    target.pop_clip_path();
                    if visible_clips.len() > 1 {
                        visible_clips.pop();
                    }
                }
                DisplayCommand::PushTransform(transform) => {
                    let next = *transforms.last().unwrap() * vello_affine(*transform);
                    transforms.push(next);
                    target.set_transform(next);
                    let logical = logical_transforms.last().unwrap().then(*transform);
                    logical_transforms.push(logical);
                }
                DisplayCommand::PopTransform => {
                    if transforms.len() > 1 {
                        transforms.pop();
                    }
                    target.set_transform(*transforms.last().unwrap());
                    if logical_transforms.len() > 1 {
                        logical_transforms.pop();
                    }
                }
                DisplayCommand::PushLayer(layer) => target.push_layer(
                    None,
                    Some(vello_blend(layer.blend)),
                    Some(layer.opacity.clamp(0.0, 1.0)),
                    None,
                    None,
                ),
                DisplayCommand::PopLayer => target.pop_layer(),
                DisplayCommand::BeginSticky(_) | DisplayCommand::EndSticky => {}
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
                    target.set_paint(vello_color(*color));
                    if let Some((rect, radius)) = simple_rounded_rect(&shifted) {
                        if *inset {
                            target.push_clip_path(&shape_path(shape));
                        }
                        target.fill_blurred_rounded_rect(
                            &rect,
                            radius,
                            blur_radius.max(0.0) / 2.0,
                            *inset,
                        );
                        if *inset {
                            target.pop_clip_path();
                        }
                    } else {
                        // Both current Vello backends expose direct blur only
                        // for rounded rectangles; arbitrary shadows retain the
                        // same unblurred fallback as the CPU reference.
                        target.fill_path(&shape_path(&shifted));
                    }
                }
                DisplayCommand::HitRegion(_) => {}
                Primitive::FillRect { rect, color } => {
                    if rect_is_visible(
                        *rect,
                        *logical_transforms.last().unwrap(),
                        *visible_clips.last().unwrap(),
                    ) {
                        target.set_paint(vello_color(*color));
                        target.fill_rect(&vello_rect(*rect));
                    }
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
                    target.set_paint(vello_color(*color));
                    target.fill_path(&path);
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
                        target.push_clip_path(&rect_path(*clip));
                    }
                    target.set_paint(vello_color(*color));
                    for run in &shaped.runs {
                        let glyphs = run.glyphs.iter().map(|glyph| glifo::Glyph {
                            id: glyph.id,
                            x: origin.x + glyph.x,
                            y: origin.y + glyph.y,
                        });
                        let mut builder = target
                            .glyph_run(&mut self.resources, run.font.data())
                            .font_size(run.font_size)
                            .normalized_coords(&run.normalized_coords);
                        if run.synth_bold {
                            let amount = f64::from(run.font_size) * 0.025;
                            builder = builder.font_embolden(glifo::FontEmbolden::new(
                                Diagonal2::new(amount, amount),
                            ));
                        }
                        if let Some(degrees) = run.synth_skew_degrees {
                            builder = builder.glyph_transform(Affine::skew(
                                f64::from(degrees).to_radians().tan(),
                                0.0,
                            ));
                        }
                        builder.fill_glyphs(glyphs);
                    }
                    target.set_paint(vello_color(decoration.color));
                    paint_decorations(&mut target, *origin, shaped, decoration.style);
                    if clip.is_some() {
                        target.pop_clip_path();
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
                        target.push_clip_path(&rect_path(*clip));
                    }
                    self.paint_image(scene, &mut target, encoder, *handle, *rect, *fit, *sampling)?;
                    if clip.is_some() {
                        target.pop_clip_path();
                    }
                }
            }
        }
        Ok(target)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_image(
        &mut self,
        scene: &Scene,
        target: &mut vello_hybrid::Scene,
        encoder: &mut wgpu::CommandEncoder,
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
            self.renderer
                .destroy_image(&mut self.resources, encoder, id);
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
                self.renderer
                    .destroy_image(&mut self.resources, encoder, id);
            } else {
                target.set_paint(vello_color(PaintColor::Muted));
                target.fill_rect(&vello_rect(rect));
                return Ok(());
            }
        }
        if !self.images.contains_key(&handle)
            && let Some(revision) = store_revision
            && let Some(image) = scene.image_store.get(handle)
        {
            let upload_limit = HYBRID_IMAGE_ATLAS_LIMIT
                .min(self.device.limits().max_texture_dimension_2d)
                .min(u32::from(u16::MAX));
            let Some((width, height, pixels)) = hybrid_image_pixels(&image, upload_limit) else {
                target.set_paint(vello_color(PaintColor::Muted));
                target.fill_rect(&vello_rect(rect));
                return Ok(());
            };
            let pixmap = Pixmap::from_parts_with_opacity(pixels, width, height, image.has_alpha);
            let id = self.renderer.upload_image(
                &mut self.resources,
                &self.device,
                &self.queue,
                encoder,
                &pixmap,
            );
            self.images.insert(
                handle,
                CachedImage {
                    source: ImageSource::opaque_id_with_transparency_hint(id, image.has_alpha),
                    upload_width: u32::from(width),
                    upload_height: u32::from(height),
                    width: image.width,
                    height: image.height,
                    revision,
                    last_used_frame: self.frame_id,
                },
            );
        }
        let Some(image) = self.images.get_mut(&handle) else {
            target.set_paint(vello_color(PaintColor::Muted));
            target.fill_rect(&vello_rect(rect));
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
        target.set_paint(ImageBrush {
            image: image.source.clone(),
            sampler,
        });
        target.set_paint_transform(
            Affine::translate((f64::from(x), f64::from(y)))
                * Affine::scale_non_uniform(
                    f64::from(drawn_width / image.upload_width as f32),
                    f64::from(drawn_height / image.upload_height as f32),
                ),
        );
        if fit == ImageFit::Cover {
            target.push_clip_path(&rect_path(rect));
        }
        target.fill_rect(&Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + drawn_width),
            f64::from(y + drawn_height),
        ));
        if fit == ImageFit::Cover {
            target.pop_clip_path();
        }
        target.reset_paint_transform();
        Ok(())
    }

    fn acquire_surface(&self) -> Result<CurrentSurfaceTexture, String> {
        self.surface
            .as_ref()
            .map(wgpu::Surface::get_current_texture)
            .ok_or_else(|| String::from("Hybrid surface is suspended"))
    }

    fn configure_surface(&self) -> Result<(), String> {
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| String::from("Hybrid surface is unavailable"))?;
        let config = self
            .surface_config
            .as_ref()
            .ok_or_else(|| String::from("Hybrid surface has no configuration"))?;
        if !self.size.is_empty() {
            surface.configure(&self.device, config);
        }
        Ok(())
    }

    fn recreate_surface(&mut self) -> Result<(), String> {
        let window = self
            .window
            .as_ref()
            .cloned()
            .ok_or_else(|| String::from("Hybrid window is unavailable"))?;
        self.surface = Some(
            self.instance
                .create_surface(window)
                .map_err(|error| format!("could not recreate lost Hybrid surface: {error}"))?,
        );
        self.configure_surface()
    }

    fn check_device(&self) -> Result<(), String> {
        if let Some(error) = self.take_device_failure() {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn take_device_failure(&self) -> Option<String> {
        self.device_failure.lock().unwrap().take()
    }
}

impl RasterBackend for VelloHybridRenderer {
    fn render<'a>(&'a mut self, scene: &Scene) -> Result<RasterFrame<'a>, String> {
        let frame = self.render_rgba(scene)?;
        self.presented.clear();
        self.presented.reserve(frame.pixels.len() / 4);
        self.presented
            .extend(frame.pixels.chunks_exact(4).map(|pixel| {
                u32::from(pixel[0]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[2])
            }));
        Ok(RasterFrame {
            size: frame.size,
            pixels: &self.presented,
        })
    }
}

async fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("TRust Vello Hybrid device"),
            required_features: wgpu::Features::empty(),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("could not create Hybrid device: {error}"))
}

fn preferred_surface_format(formats: &[TextureFormat]) -> Option<TextureFormat> {
    // Hybrid and the CPU reference both produce display-referred sRGB channel
    // values. Prefer a non-sRGB swapchain view to avoid applying a second
    // transfer function; BGRA is the guaranteed native desktop format.
    [TextureFormat::Bgra8Unorm, TextureFormat::Rgba8Unorm]
        .into_iter()
        .find(|format| formats.contains(format))
        .or_else(|| formats.first().copied())
}

fn ensure_device_size(device: &wgpu::Device, size: PhysicalSize) -> Result<(), String> {
    if size.width > u16::MAX as u32 || size.height > u16::MAX as u32 {
        return Err(format!(
            "framebuffer {}x{} exceeds Vello's 16-bit scene limit",
            size.width, size.height
        ));
    }
    let limit = device.limits().max_texture_dimension_2d;
    if size.width > limit || size.height > limit {
        return Err(format!(
            "framebuffer {}x{} exceeds GPU texture limit {limit}",
            size.width, size.height
        ));
    }
    Ok(())
}

fn physical_size(width: u32, height: u32) -> PhysicalSize {
    PhysicalSize::new(width, height)
}

fn bounded_image_dimensions(width: u32, height: u32, limit: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || limit == 0 {
        return None;
    }
    if width <= limit && height <= limit {
        return Some((width, height));
    }
    if width >= height {
        let scaled_height =
            (u64::from(height) * u64::from(limit) + u64::from(width) / 2) / u64::from(width);
        Some((limit, scaled_height.max(1) as u32))
    } else {
        let scaled_width =
            (u64::from(width) * u64::from(limit) + u64::from(height) / 2) / u64::from(height);
        Some((scaled_width.max(1) as u32, limit))
    }
}

fn hybrid_image_pixels(image: &ImageResource, limit: u32) -> Option<(u16, u16, Vec<PremulRgba8>)> {
    let expected = usize::try_from(image.width)
        .ok()
        .and_then(|width| {
            usize::try_from(image.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4));
    if expected != Some(image.rgba.len()) {
        return None;
    }
    let (upload_width, upload_height) = bounded_image_dimensions(image.width, image.height, limit)?;
    let pixels = if (upload_width, upload_height) == (image.width, image.height) {
        premultiply_rgba(&image.rgba)
    } else {
        let source = image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(
            image.width,
            image.height,
            image.rgba.as_ref(),
        )?;
        let resized = image::imageops::resize(
            &source,
            upload_width,
            upload_height,
            image::imageops::FilterType::Triangle,
        );
        premultiply_rgba(resized.as_raw())
    };
    Some((
        u16::try_from(upload_width).ok()?,
        u16::try_from(upload_height).ok()?,
        pixels,
    ))
}

fn premultiply_rgba(rgba: &[u8]) -> Vec<PremulRgba8> {
    rgba.chunks_exact(4)
        .map(|pixel| {
            let alpha = u16::from(pixel[3]);
            let premul = |component| ((u16::from(component) * alpha) / 255) as u8;
            PremulRgba8 {
                r: premul(pixel[0]),
                g: premul(pixel[1]),
                b: premul(pixel[2]),
                a: pixel[3],
            }
        })
        .collect()
}

fn set_brush(target: &mut vello_hybrid::Scene, brush: &PaintBrush) {
    match brush {
        PaintBrush::Solid(color) => target.set_paint(vello_color(*color)),
        PaintBrush::LinearGradient { start, end, stops } => {
            let stops = vello_stops(stops);
            target.set_paint(
                vello_common::peniko::Gradient::new_linear(
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
            target.set_paint(
                vello_common::peniko::Gradient::new_radial(
                    (f64::from(center.x), f64::from(center.y)),
                    *radius,
                )
                .with_stops(stops.as_slice()),
            );
        }
    }
}

fn paint_decorations(
    target: &mut vello_hybrid::Scene,
    origin: CssPoint,
    shaped: &crate::text::ShapedText,
    style: DecorationStyle,
) {
    let thickness = (shaped.line_height / 18.0).max(1.0);
    let paint_line = |target: &mut vello_hybrid::Scene, y: f32| {
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
        target.set_stroke(stroke);
        let mut path = BezPath::new();
        path.move_to((f64::from(origin.x), f64::from(y)));
        path.line_to((f64::from(origin.x + shaped.advance), f64::from(y)));
        target.stroke_path(&path);
        if style == DecorationStyle::Double {
            let mut second = BezPath::new();
            second.move_to((f64::from(origin.x), f64::from(y + thickness * 2.0)));
            second.line_to((
                f64::from(origin.x + shaped.advance),
                f64::from(y + thickness * 2.0),
            ));
            target.stroke_path(&second);
        }
    };
    if shaped.underline {
        paint_line(target, origin.y + shaped.baseline + thickness);
    }
    if shaped.strikethrough {
        paint_line(target, origin.y + shaped.baseline - shaped.ascent * 0.32);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("unknown panic payload"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_format_prefers_non_srgb_reference_channels() {
        assert_eq!(
            preferred_surface_format(&[TextureFormat::Bgra8UnormSrgb, TextureFormat::Bgra8Unorm,]),
            Some(TextureFormat::Bgra8Unorm)
        );
    }

    #[test]
    fn oversized_images_fit_the_hybrid_atlas_without_changing_aspect_ratio() {
        assert_eq!(
            bounded_image_dimensions(3840, 5160, HYBRID_IMAGE_ATLAS_LIMIT),
            Some((3048, 4096))
        );
        assert_eq!(
            bounded_image_dimensions(8192, 1024, HYBRID_IMAGE_ATLAS_LIMIT),
            Some((4096, 512))
        );
        assert_eq!(
            bounded_image_dimensions(320, 240, HYBRID_IMAGE_ATLAS_LIMIT),
            Some((320, 240))
        );
    }

    #[test]
    fn hybrid_upload_resamples_pixels_but_retains_separate_natural_size() {
        let image = ImageResource {
            width: 4,
            height: 2,
            rgba: Arc::from(vec![255; 4 * 2 * 4]),
            has_alpha: false,
        };
        let (width, height, pixels) = hybrid_image_pixels(&image, 2).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(pixels.len(), 2);
        assert_eq!((image.width, image.height), (4, 2));
    }
}
