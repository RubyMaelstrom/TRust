//! Renderer-neutral graphical display data.
//!
//! Browser/layout code emits logical geometry and simple paint primitives.
//! Backends own device scaling and raster/GPU resources. Vello-specific types
//! are confined to [`vello_cpu`] and [`vello_hybrid`]. Vello CPU is the
//! deterministic reference/fallback; Hybrid consumes the same [`Scene`] and
//! owns wgpu surface/device recovery without feeding capabilities back into
//! DOM, style, layout, hit testing, or paint-order construction.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use crate::core::{BrowserSnapshot, CssPoint, CssSize, PhysicalSize, ViewportMetrics};
use crate::doc::Link;

pub mod documents;
pub mod headless;
pub mod vello_cpu;
pub mod vello_hybrid;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CssRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CssRect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, point: CssPoint) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }

    pub fn translate(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }
}

/// Small initial palette. It remains TRust-owned so backend color APIs do not
/// escape the adapter; arbitrary page colors can extend this display contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintColor {
    Window,
    Chrome,
    Content,
    Surface,
    Muted,
    Foreground,
    Accent,
    Loading,
    Rgba(u8, u8, u8, u8),
}

/// A TRust-owned 2D affine matrix in CSS coordinates. Its six coefficients
/// follow CSS/DOMMatrix order: `[a b c d e f]`, mapping `(x,y)` to
/// `(a*x+c*y+e, b*x+d*y+f)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine2d(pub [f32; 6]);

impl Affine2d {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub const fn translate(x: f32, y: f32) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, x, y])
    }

    pub const fn scale(x: f32, y: f32) -> Self {
        Self([x, 0.0, 0.0, y, 0.0, 0.0])
    }

    /// Matrix product `self * rhs`, matching CSS transform-list
    /// post-multiplication.
    pub fn then(self, rhs: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = rhs.0;
        Self([
            a * g + c * h,
            b * g + d * h,
            a * i + c * j,
            b * i + d * j,
            a * k + c * l + e,
            b * k + d * l + f,
        ])
    }

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    pub fn map_point(self, point: CssPoint) -> CssPoint {
        let [a, b, c, d, e, f] = self.0;
        CssPoint::new(a * point.x + c * point.y + e, b * point.x + d * point.y + f)
    }

    pub fn inverse(self) -> Option<Self> {
        let [a, b, c, d, e, f] = self.0;
        let determinant = a * d - b * c;
        if !determinant.is_finite() || determinant.abs() < 1.0e-8 {
            return None;
        }
        let inverse = determinant.recip();
        Some(Self([
            d * inverse,
            -b * inverse,
            -c * inverse,
            a * inverse,
            (c * f - d * e) * inverse,
            (b * e - a * f) * inverse,
        ]))
    }
}

impl Default for Affine2d {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CornerRadii {
    /// Horizontal and vertical radius for TL, TR, BR, BL.
    pub corners: [(f32, f32); 4],
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathElement {
    MoveTo(CssPoint),
    LineTo(CssPoint),
    QuadTo(CssPoint, CssPoint),
    CurveTo(CssPoint, CssPoint, CssPoint),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaintShape {
    Rect(CssRect),
    RoundedRect { rect: CssRect, radii: CornerRadii },
    Path(Vec<PathElement>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: PaintColor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaintBrush {
    Solid(PaintColor),
    LinearGradient {
        start: CssPoint,
        end: CssPoint,
        stops: Vec<GradientStop>,
    },
    RadialGradient {
        center: CssPoint,
        radius: f32,
        stops: Vec<GradientStop>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub width: f32,
    pub dash: Vec<f32>,
    pub dash_offset: f32,
    pub cap: LineCap,
}

impl StrokeStyle {
    pub fn solid(width: f32) -> Self {
        Self {
            width,
            dash: Vec::new(),
            dash_offset: 0.0,
            cap: LineCap::Butt,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    Difference,
    Exclusion,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositingLayer {
    pub opacity: f32,
    pub blend: BlendMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageHandle(pub u64);

impl ImageHandle {
    /// Deterministic FNV-1a key. The source string is retained once alongside
    /// this key in `ImageRequest`, while draw operations carry only the handle.
    pub fn for_source(source: &str) -> Self {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in source.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(hash)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub handle: ImageHandle,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct ImageResource {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA8 pixels. Conversion to a backend's native storage
    /// happens once in that backend's retained cache.
    pub rgba: Arc<[u8]>,
    pub has_alpha: bool,
}

// The byte ceiling is the primary decoded-image memory bound. A separate,
// higher entry ceiling prevents tiny thumbnail/icon grids from evicting live
// resources merely because a page contains more than 256 images.
const MAX_IMAGE_RESOURCES: usize = 2_048;
const MAX_IMAGE_RESOURCE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Default)]
struct ImageStoreState {
    entries: HashMap<ImageHandle, StoredImageResource>,
    order: VecDeque<ImageHandle>,
    bytes: usize,
    next_revision: u64,
}

#[derive(Debug)]
struct StoredImageResource {
    image: ImageResource,
    /// Monotonic content identity. Animation replaces pixels under the stable
    /// layout handle; retained backends compare this cheap integer and upload
    /// only when a frame actually changes.
    revision: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ImageStore(Arc<RwLock<ImageStoreState>>);

impl ImageStore {
    pub fn insert(&self, handle: ImageHandle, image: ImageResource) -> Vec<ImageHandle> {
        let mut store = self.0.write().expect("image store poisoned");
        if let Some(old) = store.entries.remove(&handle) {
            store.bytes = store.bytes.saturating_sub(old.image.rgba.len());
            store.order.retain(|candidate| *candidate != handle);
        }
        store.next_revision = store.next_revision.wrapping_add(1).max(1);
        let revision = store.next_revision;
        store.bytes = store.bytes.saturating_add(image.rgba.len());
        store
            .entries
            .insert(handle, StoredImageResource { image, revision });
        store.order.push_back(handle);
        let mut evicted = Vec::new();
        while store.entries.len() > MAX_IMAGE_RESOURCES || store.bytes > MAX_IMAGE_RESOURCE_BYTES {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            if let Some(old) = store.entries.remove(&oldest) {
                store.bytes = store.bytes.saturating_sub(old.image.rgba.len());
                evicted.push(oldest);
            }
        }
        evicted
    }

    pub fn get(&self, handle: ImageHandle) -> Option<ImageResource> {
        let mut store = self.0.write().expect("image store poisoned");
        let image = store.entries.get(&handle).map(|entry| entry.image.clone());
        if image.is_some() {
            // Explicit LRU ownership: the shared decoded store is touched by
            // display-list construction/backends, while each backend owns its
            // own independent atlas eviction policy.
            store.order.retain(|candidate| *candidate != handle);
            store.order.push_back(handle);
        }
        image
    }

    pub(crate) fn revision(&self, handle: ImageHandle) -> Option<u64> {
        self.0
            .read()
            .expect("image store poisoned")
            .entries
            .get(&handle)
            .map(|entry| entry.revision)
    }

    /// Monotonic identity for the decoded contents of the whole store.
    ///
    /// A `Scene` intentionally shares its image store with the frontend, so a
    /// retained old scene cannot discover image changes by comparing the two
    /// `ImageStore` handles. Frontends use this generation to reject partial
    /// raster damage whenever an image (including an animation frame) changed
    /// since the last presentation.
    pub fn generation(&self) -> u64 {
        self.0.read().expect("image store poisoned").next_revision
    }

    pub fn contains(&self, handle: ImageHandle) -> bool {
        self.0
            .read()
            .expect("image store poisoned")
            .entries
            .contains_key(&handle)
    }

    pub fn len(&self) -> usize {
        self.0.read().expect("image store poisoned").entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn remove(&self, handle: ImageHandle) -> Option<ImageResource> {
        let mut store = self.0.write().expect("image store poisoned");
        let old = store.entries.remove(&handle)?;
        store.bytes = store.bytes.saturating_sub(old.image.rgba.len());
        store.order.retain(|candidate| *candidate != handle);
        Some(old.image)
    }

    pub fn clear(&self) {
        let mut store = self.0.write().expect("image store poisoned");
        store.entries.clear();
        store.order.clear();
        store.bytes = 0;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSampling {
    Nearest,
    #[default]
    Smooth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFit {
    Fill,
    #[default]
    Contain,
    Cover,
    None,
    ScaleDown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecorationStyle {
    #[default]
    Solid,
    Double,
    Dotted,
    Dashed,
    Wavy,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextDecorationPaint {
    pub color: PaintColor,
    pub style: DecorationStyle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HitRegion {
    pub rect: CssRect,
    pub node: usize,
    /// Live page-actor identity serialized into the presentation DOM. Layout
    /// node ids are deliberately not sent to JavaScript.
    pub actor: Option<usize>,
    pub link: Option<Link>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollContainer {
    pub node: usize,
    pub actor: Option<usize>,
    pub viewport: CssRect,
    pub content: CssSize,
    pub offset: CssPoint,
    pub horizontal: bool,
    pub vertical: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StickyConstraint {
    pub node: usize,
    pub rect: CssRect,
    /// Nearest overflow scroll container; `None` means the page viewport.
    pub container: Option<usize>,
    /// Computed top/right/bottom/left insets in CSS pixels when definite.
    pub insets: [Option<f32>; 4],
}

/// Stable renderer-neutral display-list command. Commands are ordered and
/// stateful; every push is paired with its corresponding pop by the producer.
/// No command contains device pixels, terminal cells, Vello, or winit types.
#[derive(Clone, Debug, PartialEq)]
pub enum DisplayCommand {
    Fill {
        shape: PaintShape,
        brush: PaintBrush,
    },
    Stroke {
        shape: PaintShape,
        brush: PaintBrush,
        style: StrokeStyle,
    },
    PushClip(PaintShape),
    PopClip,
    PushTransform(Affine2d),
    PopTransform,
    PushLayer(CompositingLayer),
    PopLayer,
    /// Layout-owned sticky scope. Scene composition resolves it against the
    /// current viewport/container scroll and replaces it with a plain affine
    /// transform before a raster backend sees the list.
    BeginSticky(StickyConstraint),
    EndSticky,
    /// A renderer may use a native blur/filter, or fall back to an expanded
    /// translucent shape. The geometry and CSS semantics remain TRust-owned.
    Shadow {
        shape: PaintShape,
        color: PaintColor,
        offset: CssPoint,
        blur_radius: f32,
        spread: f32,
        inset: bool,
    },
    HitRegion(HitRegion),
    // Compact chrome operations retained as sugar. Page paint uses `Fill` and
    // `Stroke`; backends treat these exactly like their general equivalents.
    FillRect {
        rect: CssRect,
        color: PaintColor,
    },
    FillPolygon {
        points: Vec<CssPoint>,
        color: PaintColor,
    },
    GlyphRun {
        origin: CssPoint,
        shaped: crate::text::ShapedText,
        color: PaintColor,
        decoration: TextDecorationPaint,
        clip: Option<CssRect>,
        /// DOM/layout identity retained for selection, hit testing, and links.
        node: usize,
        link: Option<Link>,
    },
    Image {
        rect: CssRect,
        handle: ImageHandle,
        /// Source crop in intrinsic image pixels; `None` selects the full image.
        source_rect: Option<CssRect>,
        fit: ImageFit,
        sampling: ImageSampling,
        clip: Option<CssRect>,
        node: usize,
        link: Option<Link>,
    },
}

/// Compatibility name used by the Phase-1 chrome and older tests. It aliases
/// the permanent display command rather than creating a second paint model.
pub type Primitive = DisplayCommand;

/// A canonical line box in document CSS pixels. This is deliberately kept
/// beside the display list: selection and hit testing need the real baseline
/// and typographic extents even when a line has no visible glyphs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaintLine {
    pub rect: CssRect,
    pub baseline: f32,
    pub ascent: f32,
    pub descent: f32,
}

/// Renderer-neutral page display list in document CSS pixels. Device scaling
/// is applied only by the backend; terminal cells never enter this structure.
#[derive(Clone, Debug, Default)]
pub struct PagePaint {
    pub width: f32,
    pub height: f32,
    /// Page canvas color. HTML currently uses the default white canvas while
    /// protocol and VT adapters supply their own frontend-neutral theme.
    pub background: Option<PaintColor>,
    pub lines: Vec<PaintLine>,
    pub primitives: Vec<Primitive>,
    /// Viewport-fixed commands that paint below the scrolling document. These
    /// are still pinned to the viewport, but remain behind later content in
    /// the same stacking context (CSS 2.1 Appendix E §E.2 tree order).
    pub fixed_under_primitives: Vec<Primitive>,
    /// Viewport-fixed commands are composed after the scrolled document list.
    pub fixed_primitives: Vec<Primitive>,
    pub image_requests: Vec<ImageRequest>,
    pub scroll_containers: Vec<ScrollContainer>,
    pub sticky_constraints: Vec<StickyConstraint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlId {
    Find,
    Command,
    VerticalRail,
    HorizontalRail,
    VerticalHeart,
    HorizontalHeart,
}

#[derive(Clone, Debug, Default)]
pub struct EditorVisual {
    pub text: String,
    pub selection: Vec<CssRect>,
    pub caret: Option<CssRect>,
}

#[derive(Clone, Debug, Default)]
pub struct ChromeModel {
    pub command: Option<EditorVisual>,
    pub status: String,
    pub status_label: String,
    pub link_preview: String,
    pub find: Option<EditorVisual>,
    pub find_count: Option<(usize, usize)>,
    pub heart: HeartVisual,
}

/// Dynamic state for the two overlay heart scrollbars. Fractions are visual
/// positions; document scroll remains authoritative in the controller.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeartVisual {
    pub vertical_visible: bool,
    pub vertical_fraction: Option<f32>,
    pub horizontal_fraction: Option<f32>,
    pub energy: f32,
    pub vertical_engaged: bool,
    pub horizontal_engaged: bool,
    pub dragging: Option<ScrollbarAxis>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlRegion {
    pub id: ControlId,
    pub rect: CssRect,
    pub enabled: bool,
}

/// A renderer-neutral frame description in logical coordinates.
#[derive(Clone, Debug)]
pub struct Scene {
    pub viewport: ViewportMetrics,
    pub primitives: Vec<Primitive>,
    pub controls: Vec<ControlRegion>,
    pub content_viewport: CssRect,
    pub image_store: ImageStore,
    pub page_scroll_containers: Vec<ScrollContainer>,
    pub page_size: CssSize,
}

/// A conservative raster-only crop of a complete [`Scene`]. The display list
/// remains complete and is translated beneath a smaller physical target, so
/// normal transform/clip/paint ordering is preserved while backend culling
/// avoids constructing work outside the damaged rectangle.
#[derive(Clone, Debug)]
pub struct RasterDamage {
    pub scene: Scene,
    pub x: u32,
    pub y: u32,
}

/// Result of comparing two renderer-neutral display lists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SceneDamage {
    /// The rasterized commands are identical.
    Unchanged,
    /// Only ordinary paint commands changed within this conservative bound.
    Partial(CssRect),
    /// Stateful paint ordering or viewport geometry changed; repaint fully.
    Full,
}

/// Compute damage for a retained scene without weakening CSS painting order.
///
/// CSS 2.2 Appendix E, CSS Transforms, and CSS Overflow make transforms,
/// clips, and compositing scopes stateful. A change to any such command is
/// therefore a full-frame fallback. When only leaf paint commands differ, we
/// replay both complete lists to recover their active transforms and clips,
/// then union the old and new painted bounds. Removed paint is included so
/// stale pixels are always cleared by the replacement crop.
pub fn scene_damage(old: &Scene, new: &Scene) -> SceneDamage {
    if old.viewport != new.viewport {
        return SceneDamage::Full;
    }
    let prefix = old
        .primitives
        .iter()
        .zip(&new.primitives)
        .take_while(|(old, new)| old == new)
        .count();
    if prefix == old.primitives.len() && prefix == new.primitives.len() {
        return SceneDamage::Unchanged;
    }
    let suffix = old.primitives[prefix..]
        .iter()
        .rev()
        .zip(new.primitives[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let old_changed = prefix..old.primitives.len() - suffix;
    let new_changed = prefix..new.primitives.len() - suffix;
    if old.primitives[old_changed.clone()]
        .iter()
        .chain(&new.primitives[new_changed.clone()])
        .any(command_changes_paint_state)
    {
        return SceneDamage::Full;
    }
    let Some(old_bounds) = changed_command_bounds(old, old_changed) else {
        return SceneDamage::Full;
    };
    let Some(new_bounds) = changed_command_bounds(new, new_changed) else {
        return SceneDamage::Full;
    };
    let bounds = match (old_bounds, new_bounds) {
        (Some(old), Some(new)) => union_rect(old, new),
        (Some(bounds), None) | (None, Some(bounds)) => bounds,
        (None, None) => return SceneDamage::Unchanged,
    };
    // Cover antialiasing, synthetic font skew/emboldening, and backend filter
    // kernels at the integer damage edge. Command-specific effects are already
    // included below; this is the final device-independent safety gutter.
    let bounds = expand_rect(bounds, 2.0);
    intersect_css_rect(
        bounds,
        CssRect::new(0.0, 0.0, new.viewport.css.width, new.viewport.css.height),
    )
    .map_or(SceneDamage::Unchanged, SceneDamage::Partial)
}

/// Quantize a logical damage rectangle outward exactly once and create the
/// smaller scene a raster backend can paint into a retained full-size target.
pub fn raster_damage(scene: &Scene, rect: CssRect) -> Option<RasterDamage> {
    let viewport = CssRect::new(
        0.0,
        0.0,
        scene.viewport.css.width,
        scene.viewport.css.height,
    );
    let rect = intersect_css_rect(rect, viewport)?;
    let scale = scene.viewport.scale_factor.get() as f32;
    let left = (rect.x * scale).floor().max(0.0) as u32;
    let top = (rect.y * scale).floor().max(0.0) as u32;
    let right = ((rect.x + rect.width) * scale)
        .ceil()
        .min(scene.viewport.physical.width as f32) as u32;
    let bottom = ((rect.y + rect.height) * scale)
        .ceil()
        .min(scene.viewport.physical.height as f32) as u32;
    if right <= left || bottom <= top {
        return None;
    }
    let physical = PhysicalSize::new(right - left, bottom - top);
    let metrics = ViewportMetrics::from_physical(physical, scene.viewport.scale_factor);
    let origin = CssPoint::new(left as f32 / scale, top as f32 / scale);
    let mut primitives = Vec::with_capacity(scene.primitives.len() + 2);
    primitives.push(DisplayCommand::PushTransform(Affine2d::translate(
        -origin.x, -origin.y,
    )));
    primitives.extend(scene.primitives.iter().cloned());
    primitives.push(DisplayCommand::PopTransform);
    Some(RasterDamage {
        scene: Scene {
            viewport: metrics,
            primitives,
            controls: Vec::new(),
            content_viewport: scene.content_viewport.translate(-origin.x, -origin.y),
            image_store: scene.image_store.clone(),
            page_scroll_containers: Vec::new(),
            page_size: scene.page_size,
        },
        x: left,
        y: top,
    })
}

fn command_changes_paint_state(command: &DisplayCommand) -> bool {
    matches!(
        command,
        DisplayCommand::PushClip(_)
            | DisplayCommand::PopClip
            | DisplayCommand::PushTransform(_)
            | DisplayCommand::PopTransform
            | DisplayCommand::PushLayer(_)
            | DisplayCommand::PopLayer
            | DisplayCommand::BeginSticky(_)
            | DisplayCommand::EndSticky
    )
}

fn changed_command_bounds(
    scene: &Scene,
    changed: std::ops::Range<usize>,
) -> Option<Option<CssRect>> {
    let mut transforms = vec![Affine2d::IDENTITY];
    let mut clips = vec![CssRect::new(
        0.0,
        0.0,
        scene.viewport.css.width,
        scene.viewport.css.height,
    )];
    let mut bounds = None;
    for (index, command) in scene.primitives.iter().enumerate() {
        let transform = *transforms.last()?;
        let clip = *clips.last()?;
        if changed.contains(&index)
            && let Some(command_bounds) = leaf_command_bounds(command)
        {
            let command_bounds = transformed_css_bounds(command_bounds, transform);
            if let Some(visible) = intersect_css_rect(command_bounds, clip) {
                bounds = Some(bounds.map_or(visible, |old| union_rect(old, visible)));
            }
        }
        match command {
            DisplayCommand::PushTransform(next) => transforms.push(transform.then(*next)),
            DisplayCommand::PopTransform => {
                if transforms.len() == 1 {
                    return None;
                }
                transforms.pop();
            }
            DisplayCommand::PushClip(shape) => {
                let shape = shape_css_bounds(shape)?;
                let transformed = transformed_css_bounds(shape, transform);
                clips.push(intersect_css_rect(clip, transformed).unwrap_or_default());
            }
            DisplayCommand::PopClip => {
                if clips.len() == 1 {
                    return None;
                }
                clips.pop();
            }
            _ => {}
        }
    }
    if transforms.len() != 1 || clips.len() != 1 {
        return None;
    }
    Some(bounds)
}

fn leaf_command_bounds(command: &DisplayCommand) -> Option<CssRect> {
    match command {
        DisplayCommand::Fill { shape, .. } => shape_css_bounds(shape),
        DisplayCommand::Stroke { shape, style, .. } => {
            shape_css_bounds(shape).map(|rect| expand_rect(rect, style.width.max(0.0) / 2.0))
        }
        DisplayCommand::Shadow {
            shape,
            offset,
            blur_radius,
            spread,
            inset,
            ..
        } => shape_css_bounds(shape).map(|rect| {
            if *inset {
                rect
            } else {
                expand_rect(
                    rect.translate(offset.x, offset.y),
                    spread.abs() + blur_radius.max(0.0) * 2.0,
                )
            }
        }),
        DisplayCommand::FillRect { rect, .. } => Some(*rect),
        DisplayCommand::FillPolygon { points, .. } => css_point_bounds(points.iter().copied()),
        DisplayCommand::GlyphRun {
            origin,
            shaped,
            clip,
            ..
        } => {
            let overflow = shaped.line_height.max(1.0);
            let glyphs = CssRect::new(
                origin.x - overflow,
                origin.y - overflow,
                shaped.advance.max(1.0) + overflow * 2.0,
                shaped.line_height.max(1.0) + overflow * 2.0,
            );
            clip.and_then(|clip| intersect_css_rect(glyphs, clip))
                .or_else(|| clip.is_none().then_some(glyphs))
        }
        DisplayCommand::Image { rect, clip, .. } => clip
            .and_then(|clip| intersect_css_rect(*rect, clip))
            .or_else(|| clip.is_none().then_some(*rect)),
        DisplayCommand::HitRegion(_)
        | DisplayCommand::PushClip(_)
        | DisplayCommand::PopClip
        | DisplayCommand::PushTransform(_)
        | DisplayCommand::PopTransform
        | DisplayCommand::PushLayer(_)
        | DisplayCommand::PopLayer
        | DisplayCommand::BeginSticky(_)
        | DisplayCommand::EndSticky => None,
    }
}

fn shape_css_bounds(shape: &PaintShape) -> Option<CssRect> {
    match shape {
        PaintShape::Rect(rect) | PaintShape::RoundedRect { rect, .. } => Some(*rect),
        PaintShape::Path(elements) => css_point_bounds(elements.iter().flat_map(|element| {
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

fn css_point_bounds(points: impl Iterator<Item = CssPoint>) -> Option<CssRect> {
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

fn transformed_css_bounds(rect: CssRect, transform: Affine2d) -> CssRect {
    css_point_bounds(
        [
            CssPoint::new(rect.x, rect.y),
            CssPoint::new(rect.x + rect.width, rect.y),
            CssPoint::new(rect.x, rect.y + rect.height),
            CssPoint::new(rect.x + rect.width, rect.y + rect.height),
        ]
        .into_iter()
        .map(|point| transform.map_point(point)),
    )
    .unwrap_or_default()
}

fn intersect_css_rect(a: CssRect, b: CssRect) -> Option<CssRect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > left && bottom > top).then(|| CssRect::new(left, top, right - left, bottom - top))
}

fn union_rect(a: CssRect, b: CssRect) -> CssRect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    CssRect::new(left, top, right - left, bottom - top)
}

fn expand_rect(rect: CssRect, amount: f32) -> CssRect {
    CssRect::new(
        rect.x - amount,
        rect.y - amount,
        rect.width + amount * 2.0,
        rect.height + amount * 2.0,
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageHit {
    pub rect: CssRect,
    pub node: usize,
    pub actor: Option<usize>,
    pub link: Option<Link>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub command: usize,
    pub byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: TextPosition,
    pub focus: TextPosition,
}

#[derive(Clone, Copy)]
struct PrimitiveState {
    transform: Affine2d,
}

#[derive(Clone)]
struct InteractionState {
    transform: Affine2d,
    clips: Vec<(PaintShape, Affine2d)>,
}

impl Scene {
    pub fn control_at(&self, point: CssPoint) -> Option<ControlId> {
        self.controls
            .iter()
            .rev()
            .find(|control| control.enabled && control.rect.contains(point))
            .map(|control| control.id)
    }

    /// CSS hit testing is based on retained semantic regions, not rasterized
    /// glyph pixels. Commands are interpreted in reverse paint order while
    /// respecting the transform and clip stacks that were active when each
    /// region was emitted (Pointer Events §4.1.3.2 target determination).
    pub fn page_hit_at(&self, point: CssPoint) -> Option<PageHit> {
        let states = interaction_states(&self.primitives);
        self.primitives
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, primitive)| {
                let DisplayCommand::HitRegion(region) = primitive else {
                    return None;
                };
                // Paint emits geometry regions for ordinary boxes as well as
                // actionable content. Hit testing must continue through a
                // topmost decorative/anonymous region to the first semantic
                // target beneath it; filtering only after this method returns
                // made text/pseudo/SVG paint over a button swallow its click.
                if region.link.is_none() && region.actor.is_none() {
                    return None;
                }
                let state = states.get(index)?.as_ref()?;
                point_in_interaction_state(point, state)
                    .then(|| state.transform.inverse())
                    .flatten()
                    .map(|inverse| inverse.map_point(point))
                    .filter(|local| region.rect.contains(*local))
                    .map(|_| PageHit {
                        rect: transformed_bounds(region.rect, state.transform),
                        node: region.node,
                        actor: region.actor,
                        link: region.link.clone(),
                    })
            })
    }

    /// Semantic interactive regions in document paint order. Unlike pointer
    /// hit testing this intentionally retains regions outside the current
    /// viewport clip so keyboard traversal can move focus to them and then
    /// scroll them into view.
    pub fn interactive_hits(&self) -> Vec<PageHit> {
        let states = primitive_states(&self.primitives);
        self.primitives
            .iter()
            .enumerate()
            .filter_map(|(index, primitive)| {
                let DisplayCommand::HitRegion(region) = primitive else {
                    return None;
                };
                (region.link.is_some() || region.actor.is_some()).then(|| PageHit {
                    rect: transformed_bounds(region.rect, states[index].transform),
                    node: region.node,
                    actor: region.actor,
                    link: region.link.clone(),
                })
            })
            .collect()
    }

    /// Return the closest Parley cluster boundary under a pointer.
    pub fn text_position_at(&self, point: CssPoint) -> Option<TextPosition> {
        let states = interaction_states(&self.primitives);
        self.primitives
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, primitive)| {
                let DisplayCommand::GlyphRun { origin, shaped, .. } = primitive else {
                    return None;
                };
                let state = states.get(index)?.as_ref()?;
                if !point_in_interaction_state(point, state) {
                    return None;
                }
                let local = state.transform.inverse()?.map_point(point);
                let bounds = CssRect::new(
                    origin.x,
                    origin.y,
                    shaped.advance.max(1.0),
                    shaped.line_height.max(1.0),
                );
                if !bounds.contains(local) {
                    return None;
                }
                let x = local.x - origin.x;
                let cluster = shaped.clusters.iter().min_by(|left, right| {
                    let left_distance = distance_to_cluster(x, left.x, left.advance);
                    let right_distance = distance_to_cluster(x, right.x, right.advance);
                    left_distance.total_cmp(&right_distance)
                });
                let byte = cluster.map_or(0, |cluster| {
                    let after = x >= cluster.x + cluster.advance / 2.0;
                    match (cluster.rtl, after) {
                        (false, false) | (true, true) => cluster.text_range.start,
                        _ => cluster.text_range.end,
                    }
                });
                Some(TextPosition {
                    command: index,
                    byte,
                })
            })
    }

    pub fn selected_text(&self, selection: TextSelection) -> String {
        let (start, end) = ordered_selection(selection);
        let mut output = String::new();
        let mut previous_y: Option<f32> = None;
        for index in start.command..=end.command.min(self.primitives.len().saturating_sub(1)) {
            let DisplayCommand::GlyphRun { origin, shaped, .. } = &self.primitives[index] else {
                continue;
            };
            let from = if index == start.command {
                start.byte
            } else {
                0
            };
            let to = if index == end.command {
                end.byte
            } else {
                shaped.text.len()
            };
            let Some(text) = shaped
                .text
                .get(from.min(shaped.text.len())..to.min(shaped.text.len()))
            else {
                continue;
            };
            if previous_y.is_some_and(|y| (origin.y - y).abs() > shaped.line_height * 0.5)
                && !output.ends_with('\n')
            {
                output.push('\n');
            }
            output.push_str(text);
            previous_y = Some(origin.y);
        }
        output
    }

    pub fn find_text(&self, query: &str) -> Vec<TextSelection> {
        if query.is_empty() {
            return Vec::new();
        }
        let folded_query = query.to_lowercase();
        let mut matches = Vec::new();
        for (command, primitive) in self.primitives.iter().enumerate() {
            let DisplayCommand::GlyphRun { shaped, .. } = primitive else {
                continue;
            };
            let folded = shaped.text.to_lowercase();
            if folded.len() == shaped.text.len() && folded_query.len() == query.len() {
                for (start, _) in folded.match_indices(&folded_query) {
                    matches.push(TextSelection {
                        anchor: TextPosition {
                            command,
                            byte: start,
                        },
                        focus: TextPosition {
                            command,
                            byte: start + query.len(),
                        },
                    });
                }
            } else {
                for (start, _) in shaped.text.match_indices(query) {
                    matches.push(TextSelection {
                        anchor: TextPosition {
                            command,
                            byte: start,
                        },
                        focus: TextPosition {
                            command,
                            byte: start + query.len(),
                        },
                    });
                }
            }
        }
        matches
    }

    pub fn selection_rects(&self, selection: TextSelection) -> Vec<CssRect> {
        let (start, end) = ordered_selection(selection);
        let states = primitive_states(&self.primitives);
        let mut rects = Vec::new();
        for (index, state) in states
            .iter()
            .enumerate()
            .take(end.command.min(self.primitives.len().saturating_sub(1)) + 1)
            .skip(start.command)
        {
            let DisplayCommand::GlyphRun { origin, shaped, .. } = &self.primitives[index] else {
                continue;
            };
            let from = if index == start.command {
                start.byte
            } else {
                0
            };
            let to = if index == end.command {
                end.byte
            } else {
                shaped.text.len()
            };
            for cluster in &shaped.clusters {
                if cluster.text_range.end <= from || cluster.text_range.start >= to {
                    continue;
                }
                rects.push(transformed_bounds(
                    CssRect::new(
                        origin.x + cluster.x,
                        origin.y,
                        cluster.advance.max(1.0),
                        shaped.line_height.max(1.0),
                    ),
                    state.transform,
                ));
            }
        }
        rects
    }

    pub fn scroll_container_at(&self, point: CssPoint) -> Option<&ScrollContainer> {
        self.page_scroll_containers
            .iter()
            .rev()
            .find(|container| container.viewport.contains(point))
    }

    /// Overlay canonical page paint into the chrome's content viewport.
    pub fn append_page(&mut self, page: &PagePaint, scroll: CssPoint) {
        self.page_size = CssSize::new(page.width, page.height);
        self.page_scroll_containers = page
            .scroll_containers
            .iter()
            .cloned()
            .map(|mut container| {
                container.viewport = container.viewport.translate(
                    self.content_viewport.x - scroll.x,
                    self.content_viewport.y - scroll.y,
                );
                container
            })
            .collect();
        self.primitives.push(Primitive::FillRect {
            rect: self.content_viewport,
            color: page
                .background
                .unwrap_or(PaintColor::Rgba(255, 255, 255, 255)),
        });
        self.primitives
            .push(Primitive::PushClip(PaintShape::Rect(self.content_viewport)));
        // A fixed box is not automatically a topmost overlay. CSS Positioned
        // Layout §2.2 makes it a stacking context, while CSS 2.1 Appendix E
        // §E.2 still orders z-index:auto siblings in tree order. Backdrop
        // fixed boxes therefore paint in a viewport-pinned underlay slot.
        if !page.fixed_under_primitives.is_empty() {
            self.primitives
                .push(Primitive::PushTransform(Affine2d::translate(
                    self.content_viewport.x,
                    self.content_viewport.y,
                )));
            self.append_sticky_commands(&page.fixed_under_primitives, page, CssPoint::default());
            self.primitives.push(Primitive::PopTransform);
        }
        self.primitives
            .push(Primitive::PushTransform(Affine2d::translate(
                self.content_viewport.x - scroll.x,
                self.content_viewport.y - scroll.y,
            )));
        self.append_sticky_commands(&page.primitives, page, scroll);
        self.primitives.push(Primitive::PopTransform);

        // Fixed-position descendants use the viewport as their containing
        // block: chrome offset applies, document scroll deliberately does not.
        if !page.fixed_primitives.is_empty() {
            self.primitives
                .push(Primitive::PushTransform(Affine2d::translate(
                    self.content_viewport.x,
                    self.content_viewport.y,
                )));
            self.append_sticky_commands(&page.fixed_primitives, page, CssPoint::default());
            self.primitives.push(Primitive::PopTransform);
        }
        self.primitives.push(Primitive::PopClip);
    }

    fn append_sticky_commands(
        &mut self,
        commands: &[Primitive],
        page: &PagePaint,
        viewport_scroll: CssPoint,
    ) {
        for command in commands {
            match command {
                Primitive::BeginSticky(constraint) => {
                    let (scroll, viewport) = constraint
                        .container
                        .and_then(|node| {
                            page.scroll_containers
                                .iter()
                                .find(|container| container.node == node)
                                .map(|container| (container.offset, container.viewport))
                        })
                        .unwrap_or((
                            viewport_scroll,
                            CssRect::new(0.0, 0.0, page.width, page.height),
                        ));
                    let dx = constraint.insets[3]
                        .map(|left| (scroll.x + viewport.x + left - constraint.rect.x).max(0.0))
                        .unwrap_or(0.0);
                    let dy = constraint.insets[0]
                        .map(|top| (scroll.y + viewport.y + top - constraint.rect.y).max(0.0))
                        .unwrap_or(0.0);
                    self.primitives
                        .push(Primitive::PushTransform(Affine2d::translate(dx, dy)));
                }
                Primitive::EndSticky => self.primitives.push(Primitive::PopTransform),
                other => self.primitives.push(other.clone()),
            }
        }
    }
}

fn ordered_selection(selection: TextSelection) -> (TextPosition, TextPosition) {
    if selection.anchor <= selection.focus {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    }
}

fn distance_to_cluster(x: f32, start: f32, advance: f32) -> f32 {
    if x < start {
        start - x
    } else if x > start + advance {
        x - start - advance
    } else {
        0.0
    }
}

fn transformed_bounds(rect: CssRect, transform: Affine2d) -> CssRect {
    let points = [
        CssPoint::new(rect.x, rect.y),
        CssPoint::new(rect.x + rect.width, rect.y),
        CssPoint::new(rect.x, rect.y + rect.height),
        CssPoint::new(rect.x + rect.width, rect.y + rect.height),
    ]
    .map(|point| transform.map_point(point));
    let min_x = points.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
    CssRect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

fn primitive_states(primitives: &[Primitive]) -> Vec<PrimitiveState> {
    let mut result = Vec::with_capacity(primitives.len());
    let mut transform = Affine2d::IDENTITY;
    let mut stack = Vec::new();
    for primitive in primitives {
        result.push(PrimitiveState { transform });
        match primitive {
            Primitive::PushTransform(next) => {
                stack.push(transform);
                transform = transform.then(*next);
            }
            Primitive::PopTransform => transform = stack.pop().unwrap_or(Affine2d::IDENTITY),
            _ => {}
        }
    }
    result
}

/// Resolve transform/clip state in one forward pass and retain it only for
/// commands that can participate in pointer or text hit testing. The previous
/// implementation rescanned the entire display-list prefix for every candidate
/// hit region, making pointer motion quadratic on image grids and long pages.
fn interaction_states(primitives: &[Primitive]) -> Vec<Option<InteractionState>> {
    let mut result = Vec::with_capacity(primitives.len());
    let mut transform = Affine2d::IDENTITY;
    let mut transforms = Vec::new();
    let mut clips: Vec<(PaintShape, Affine2d)> = Vec::new();
    for primitive in primitives {
        result.push(
            matches!(
                primitive,
                Primitive::HitRegion(_) | Primitive::GlyphRun { .. }
            )
            .then(|| InteractionState {
                transform,
                clips: clips.clone(),
            }),
        );
        match primitive {
            Primitive::PushTransform(next) => {
                transforms.push(transform);
                transform = transform.then(*next);
            }
            Primitive::PopTransform => transform = transforms.pop().unwrap_or_default(),
            Primitive::PushClip(shape) => clips.push((shape.clone(), transform)),
            Primitive::PopClip => {
                let _ = clips.pop();
            }
            _ => {}
        }
    }
    result
}

fn point_in_interaction_state(point: CssPoint, state: &InteractionState) -> bool {
    state.clips.iter().all(|(shape, transform)| {
        transform
            .inverse()
            .map(|inverse| shape_contains(shape, inverse.map_point(point)))
            .unwrap_or(false)
    })
}

fn shape_contains(shape: &PaintShape, point: CssPoint) -> bool {
    match shape {
        PaintShape::Rect(rect) | PaintShape::RoundedRect { rect, .. } => rect.contains(point),
        PaintShape::Path(elements) => {
            let mut min_x = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for point in elements.iter().flat_map(|element| match element {
                PathElement::MoveTo(a) | PathElement::LineTo(a) => vec![*a],
                PathElement::QuadTo(a, b) => vec![*a, *b],
                PathElement::CurveTo(a, b, c) => vec![*a, *b, *c],
                PathElement::Close => Vec::new(),
            }) {
                min_x = min_x.min(point.x);
                max_x = max_x.max(point.x);
                min_y = min_y.min(point.y);
                max_y = max_y.max(point.y);
            }
            CssRect::new(min_x, min_y, max_x - min_x, max_y - min_y).contains(point)
        }
    }
}

pub const COMMAND_PANEL_HEIGHT: f32 = 122.0;
pub const FIND_PANEL_HEIGHT: f32 = 64.0;
const HEART_SIZE: f32 = 30.0;
const HEART_HIT_SIZE: f32 = 34.0;
const HEART_EDGE_INSET: f32 = 17.0;
const HEART_IDLE_SOURCE: &str = "trust:ui/heart/idle-v1";
const HEART_ACTIVE_SOURCE: &str = "trust:ui/heart/active-v1";

const fn theme_color(rgb: crate::theme::Rgb) -> PaintColor {
    PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
}

const UI_BG: PaintColor = theme_color(crate::theme::BG);
const UI_PINK: PaintColor = theme_color(crate::theme::NEON_PINK);
const UI_CYAN: PaintColor = theme_color(crate::theme::NEON_CYAN);
const UI_AMBER: PaintColor = theme_color(crate::theme::AMBER);
const UI_DIM: PaintColor = theme_color(crate::theme::DIM);
const HEART_DEPTH: PaintColor = PaintColor::Rgba(98, 17, 104, 255);
const HEART_LIGHT: PaintColor = PaintColor::Rgba(255, 232, 246, 255);

/// Stable retained-image identity for the two embedded desktop heart states.
/// The animation clock selects a state and changes only its destination rect;
/// neither backend decodes or uploads pixels per frame.
pub fn desktop_heart_image_handle(active: bool) -> ImageHandle {
    ImageHandle::for_source(if active {
        HEART_ACTIVE_SOURCE
    } else {
        HEART_IDLE_SOURCE
    })
}

/// The two-frame desktop jewel is intentionally retained across scene frames.
/// A heartbeat displays only one handle at a time, so treating the other as a
/// stale page image would destroy and re-upload the pair on every state change.
pub(crate) fn is_desktop_heart_image_handle(handle: ImageHandle) -> bool {
    handle == desktop_heart_image_handle(false) || handle == desktop_heart_image_handle(true)
}

/// Build the chrome-free desktop surface. Browse mode gives the page the full
/// client area; COMMAND/FIND reserve only their solid bottom instrument panel.
pub fn desktop_shell(viewport: ViewportMetrics, browser: &BrowserSnapshot) -> Scene {
    desktop_chrome(
        viewport,
        browser,
        &ChromeModel {
            status: browser.status.clone(),
            ..ChromeModel::default()
        },
    )
}

pub fn desktop_chrome(
    viewport: ViewportMetrics,
    _browser: &BrowserSnapshot,
    model: &ChromeModel,
) -> Scene {
    let panel_height = if model.command.is_some() {
        COMMAND_PANEL_HEIGHT
    } else if model.find.is_some() {
        FIND_PANEL_HEIGHT
    } else {
        0.0
    }
    .min(viewport.css.height);
    let content = CssRect::new(
        0.0,
        0.0,
        viewport.css.width,
        (viewport.css.height - panel_height).max(0.0),
    );
    Scene {
        viewport,
        primitives: vec![Primitive::FillRect {
            rect: CssRect::new(0.0, 0.0, viewport.css.width, viewport.css.height),
            color: UI_BG,
        }],
        controls: Vec::new(),
        content_viewport: content,
        image_store: ImageStore::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    }
}

/// Paint the lightweight TRust-owned overlay after the page display list. The
/// hearts intentionally derive their range from the same `page_size`, viewport,
/// and CSS-pixel scroll used by [`Scene::append_page`]. CSSOM View §4 clamps
/// viewport scrolling to scrolling-area size minus viewport size.
pub fn paint_desktop_overlay(scene: &mut Scene, _browser: &BrowserSnapshot, model: &ChromeModel) {
    if let Some(command) = &model.command {
        paint_command_panel(scene, command, model);
    } else if let Some(find) = &model.find {
        paint_find_panel(scene, find, model.find_count);
    } else {
        paint_browse_hints(scene, model);
    }
    paint_heart_scrollbars(scene, model.heart);
}

/// Normalized scrollbar position over the actual scrollable range. A fixed-size
/// jewel can use this fraction without pretending its size is the viewport to
/// document ratio.
pub fn scrollbar_fraction(position: f32, content: f32, viewport: f32) -> Option<f32> {
    let range = (content - viewport).max(0.0);
    (range > f32::EPSILON).then(|| (position / range).clamp(0.0, 1.0))
}

/// Inverse of [`scrollbar_fraction`], used by heart dragging.
pub fn scrollbar_position(fraction: f32, content: f32, viewport: f32) -> f32 {
    fraction.clamp(0.0, 1.0) * (content - viewport).max(0.0)
}

/// Convert a thumb-center or rail-click coordinate into the same normalized
/// range used for scroll state. CSSOM View's eventual position clamp is
/// mirrored here so clicks outside the two end centers land exactly at an end.
pub fn scrollbar_track_fraction(coordinate: f32, start: f32, end: f32) -> f32 {
    if end > start {
        ((coordinate - start) / (end - start)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn paint_command_panel(scene: &mut Scene, editor: &EditorVisual, model: &ChromeModel) {
    let panel = CssRect::new(
        0.0,
        scene.content_viewport.y + scene.content_viewport.height,
        scene.viewport.css.width,
        COMMAND_PANEL_HEIGHT.min(scene.viewport.css.height),
    );
    scene.primitives.push(Primitive::FillRect {
        rect: panel,
        color: UI_BG,
    });
    let border_y = panel.y + 9.5;
    scene.primitives.push(Primitive::Stroke {
        shape: PaintShape::Path(vec![
            PathElement::MoveTo(CssPoint::new(8.0, border_y)),
            PathElement::LineTo(CssPoint::new(18.0, border_y)),
            PathElement::MoveTo(CssPoint::new(118.0, border_y)),
            PathElement::LineTo(CssPoint::new((panel.width - 8.0).max(118.0), border_y)),
        ]),
        brush: PaintBrush::Solid(UI_CYAN),
        style: StrokeStyle::solid(1.0),
    });
    let plate = PaintShape::Path(vec![
        PathElement::MoveTo(CssPoint::new(18.0, panel.y + 1.0)),
        PathElement::LineTo(CssPoint::new(111.0, panel.y + 1.0)),
        PathElement::LineTo(CssPoint::new(118.0, panel.y + 8.0)),
        PathElement::LineTo(CssPoint::new(111.0, panel.y + 19.0)),
        PathElement::LineTo(CssPoint::new(18.0, panel.y + 19.0)),
        PathElement::Close,
    ]);
    scene.primitives.push(Primitive::Fill {
        shape: plate,
        brush: PaintBrush::Solid(UI_PINK),
    });
    paint_ui_text(
        &mut scene.primitives,
        "COMMAND",
        CssPoint::new(29.0, panel.y + 2.5),
        UI_BG,
        80.0,
        command_text_style(),
    );

    let input_y = panel.y + 27.0;
    paint_ui_text(
        &mut scene.primitives,
        "trust>",
        CssPoint::new(20.0, input_y + 3.0),
        UI_CYAN,
        58.0,
        command_text_style(),
    );
    let input = CssRect::new(79.0, input_y, (panel.width - 95.0).max(1.0), 25.0);
    scene.controls.push(ControlRegion {
        id: ControlId::Command,
        rect: CssRect::new(14.0, input_y - 3.0, (panel.width - 28.0).max(1.0), 31.0),
        enabled: true,
    });
    paint_command_editor(&mut scene.primitives, editor, input);

    let status_y = panel.y + 61.0;
    let label = if model.status_label.is_empty() {
        "TRUST"
    } else {
        model.status_label.as_str()
    };
    let label_width = (label.chars().count() as f32 * 9.5 + 10.0).clamp(46.0, 150.0);
    scene.primitives.push(Primitive::FillRect {
        rect: CssRect::new(18.0, status_y - 2.0, label_width, 21.0),
        color: UI_PINK,
    });
    paint_ui_text(
        &mut scene.primitives,
        label,
        CssPoint::new(23.0, status_y),
        UI_BG,
        label_width - 8.0,
        command_text_style(),
    );
    paint_ui_text(
        &mut scene.primitives,
        &model.status,
        CssPoint::new(27.0 + label_width, status_y),
        UI_CYAN,
        (panel.width - label_width - 48.0).max(1.0),
        command_text_style(),
    );

    let keys_y = panel.y + 92.0;
    paint_ui_text(
        &mut scene.primitives,
        "[KEYS]",
        CssPoint::new(20.0, keys_y),
        UI_AMBER,
        62.0,
        command_text_style(),
    );
    paint_ui_text(
        &mut scene.primitives,
        "TAB close  ENTER run/open  ↑↓ history  ESC stop  help reference",
        CssPoint::new(83.0, keys_y),
        UI_AMBER,
        (panel.width - 100.0).max(1.0),
        command_text_style(),
    );
}

fn paint_find_panel(scene: &mut Scene, editor: &EditorVisual, count: Option<(usize, usize)>) {
    let panel = CssRect::new(
        0.0,
        scene.content_viewport.y + scene.content_viewport.height,
        scene.viewport.css.width,
        FIND_PANEL_HEIGHT.min(scene.viewport.css.height),
    );
    scene.primitives.push(Primitive::FillRect {
        rect: panel,
        color: UI_BG,
    });
    scene.primitives.push(Primitive::FillRect {
        rect: CssRect::new(0.0, panel.y, panel.width, 1.0),
        color: UI_CYAN,
    });
    paint_ui_text(
        &mut scene.primitives,
        "FIND>",
        CssPoint::new(20.0, panel.y + 13.0),
        UI_PINK,
        55.0,
        command_text_style(),
    );
    let input = CssRect::new(76.0, panel.y + 10.0, (panel.width - 160.0).max(1.0), 25.0);
    scene.controls.push(ControlRegion {
        id: ControlId::Find,
        rect: CssRect::new(14.0, panel.y + 7.0, (panel.width - 28.0).max(1.0), 31.0),
        enabled: true,
    });
    paint_command_editor(&mut scene.primitives, editor, input);
    let count = count.map_or_else(
        || String::from("0/0"),
        |(at, total)| format!("{at}/{total}"),
    );
    paint_ui_text(
        &mut scene.primitives,
        &count,
        CssPoint::new((panel.width - 68.0).max(8.0), panel.y + 13.0),
        UI_CYAN,
        60.0,
        command_text_style(),
    );
    paint_ui_text(
        &mut scene.primitives,
        "ENTER/↓ next  SHIFT+ENTER/↑ previous  TAB command  ESC stop",
        CssPoint::new(20.0, panel.y + 39.0),
        UI_AMBER,
        (panel.width - 40.0).max(1.0),
        command_text_style(),
    );
}

fn paint_browse_hints(scene: &mut Scene, model: &ChromeModel) {
    let bottom = scene.content_viewport.y + scene.content_viewport.height;
    if !model.link_preview.is_empty() {
        let style = command_text_style_with_size(12.0);
        let shaped = crate::text::shape(&model.link_preview, &style);
        let width = (shaped.advance + 18.0).min((scene.viewport.css.width - 24.0).max(1.0));
        let rect = CssRect::new(6.0, (bottom - 27.0).max(0.0), width, 23.0);
        scene
            .primitives
            .push(Primitive::FillRect { rect, color: UI_BG });
        scene.primitives.push(Primitive::FillRect {
            rect: CssRect::new(rect.x, rect.y, 2.0, rect.height),
            color: UI_CYAN,
        });
        paint_ui_text(
            &mut scene.primitives,
            &model.link_preview,
            CssPoint::new(rect.x + 9.0, rect.y + 3.0),
            UI_CYAN,
            (rect.width - 13.0).max(1.0),
            style,
        );
    }
    if model.link_preview.is_empty() {
        paint_ui_text(
            &mut scene.primitives,
            "TAB · COMMAND",
            CssPoint::new(10.0, (bottom - 14.0).max(0.0)),
            UI_DIM,
            108.0,
            command_text_style_with_size(9.0),
        );
    }
}

fn paint_heart_scrollbars(scene: &mut Scene, heart: HeartVisual) {
    let viewport = scene.content_viewport;
    let vertical_range =
        scrollbar_fraction(0.0, scene.page_size.height, scene.content_viewport.height);
    let horizontal_range =
        scrollbar_fraction(0.0, scene.page_size.width, scene.content_viewport.width);
    if heart.vertical_visible || vertical_range.is_some() {
        let actual = vertical_range.unwrap_or(0.0);
        let fraction = heart.vertical_fraction.unwrap_or(actual).clamp(0.0, 1.0);
        let (start, end) = vertical_heart_track(viewport, horizontal_range.is_some());
        let center = CssPoint::new(
            viewport.x + viewport.width - HEART_EDGE_INSET,
            start + (end - start) * fraction,
        );
        let engaged = heart.vertical_engaged || heart.dragging == Some(ScrollbarAxis::Vertical);
        if vertical_range.is_some() {
            if engaged {
                scene.primitives.push(Primitive::Stroke {
                    shape: PaintShape::Path(vec![
                        PathElement::MoveTo(CssPoint::new(center.x, start)),
                        PathElement::LineTo(CssPoint::new(center.x, end)),
                    ]),
                    brush: PaintBrush::Solid(if heart.dragging == Some(ScrollbarAxis::Vertical) {
                        UI_CYAN
                    } else {
                        UI_DIM
                    }),
                    style: StrokeStyle::solid(1.0),
                });
            }
            scene.controls.push(ControlRegion {
                id: ControlId::VerticalRail,
                rect: CssRect::new(
                    center.x - HEART_HIT_SIZE / 2.0,
                    start,
                    HEART_HIT_SIZE,
                    (end - start).max(1.0),
                ),
                enabled: true,
            });
        }
        paint_heart(
            &mut scene.primitives,
            &scene.image_store,
            center,
            HEART_SIZE
                * (1.0 + heart.energy.clamp(0.0, 1.0) * 0.08)
                * if engaged { 1.04 } else { 1.0 },
            heart.energy.max(if engaged { 0.65 } else { 0.0 }),
        );
        scene.controls.push(ControlRegion {
            id: ControlId::VerticalHeart,
            rect: CssRect::new(
                center.x - HEART_HIT_SIZE / 2.0,
                center.y - HEART_HIT_SIZE / 2.0,
                HEART_HIT_SIZE,
                HEART_HIT_SIZE,
            ),
            // Even a non-scrolling document owns the visible overlay pixels;
            // it simply cannot begin a drag until a real range exists.
            enabled: true,
        });
    }
    if let Some(actual) = horizontal_range {
        let fraction = heart.horizontal_fraction.unwrap_or(actual).clamp(0.0, 1.0);
        let (start, end) =
            horizontal_heart_track(viewport, heart.vertical_visible || vertical_range.is_some());
        let center = CssPoint::new(
            start + (end - start) * fraction,
            viewport.y + viewport.height - HEART_EDGE_INSET,
        );
        let engaged = heart.horizontal_engaged || heart.dragging == Some(ScrollbarAxis::Horizontal);
        if engaged {
            scene.primitives.push(Primitive::Stroke {
                shape: PaintShape::Path(vec![
                    PathElement::MoveTo(CssPoint::new(start, center.y)),
                    PathElement::LineTo(CssPoint::new(end, center.y)),
                ]),
                brush: PaintBrush::Solid(if heart.dragging == Some(ScrollbarAxis::Horizontal) {
                    UI_CYAN
                } else {
                    UI_DIM
                }),
                style: StrokeStyle::solid(1.0),
            });
        }
        scene.controls.push(ControlRegion {
            id: ControlId::HorizontalRail,
            rect: CssRect::new(
                start,
                center.y - HEART_HIT_SIZE / 2.0,
                (end - start).max(1.0),
                HEART_HIT_SIZE,
            ),
            enabled: true,
        });
        paint_heart(
            &mut scene.primitives,
            &scene.image_store,
            center,
            HEART_SIZE
                * (1.0 + heart.energy.clamp(0.0, 1.0) * 0.08)
                * if engaged { 1.04 } else { 1.0 },
            heart.energy.max(if engaged { 0.65 } else { 0.0 }),
        );
        scene.controls.push(ControlRegion {
            id: ControlId::HorizontalHeart,
            rect: CssRect::new(
                center.x - HEART_HIT_SIZE / 2.0,
                center.y - HEART_HIT_SIZE / 2.0,
                HEART_HIT_SIZE,
                HEART_HIT_SIZE,
            ),
            enabled: true,
        });
    }
}

pub fn vertical_heart_track(viewport: CssRect, reserve_corner: bool) -> (f32, f32) {
    let start = viewport.y + HEART_HIT_SIZE / 2.0;
    let end = (viewport.y + viewport.height
        - HEART_HIT_SIZE / 2.0
        - if reserve_corner {
            HEART_HIT_SIZE + 2.0
        } else {
            0.0
        })
    .max(start);
    (start, end)
}

pub fn horizontal_heart_track(viewport: CssRect, reserve_corner: bool) -> (f32, f32) {
    let start = viewport.x + HEART_HIT_SIZE / 2.0;
    let end = (viewport.x + viewport.width
        - HEART_HIT_SIZE / 2.0
        - if reserve_corner {
            HEART_HIT_SIZE + 2.0
        } else {
            0.0
        })
    .max(start);
    (start, end)
}

fn paint_heart(
    primitives: &mut Vec<Primitive>,
    image_store: &ImageStore,
    center: CssPoint,
    size: f32,
    energy: f32,
) {
    let handle = desktop_heart_image_handle(energy > 0.45);
    if image_store.contains(handle) {
        primitives.push(Primitive::Image {
            rect: CssRect::new(center.x - size / 2.0, center.y - size / 2.0, size, size),
            handle,
            source_rect: None,
            fit: ImageFit::Fill,
            sampling: ImageSampling::Smooth,
            clip: None,
            node: 0,
            link: None,
        });
    } else {
        // An embedded-asset decode failure must not remove the scrollbar. Keep
        // the original tiny vector as a dependency-free degraded path.
        paint_fallback_heart(primitives, center, size * (14.0 / HEART_SIZE), energy);
    }
}

fn paint_fallback_heart(primitives: &mut Vec<Primitive>, center: CssPoint, size: f32, energy: f32) {
    let scale = size / 14.0;
    let point = |x: f32, y: f32| CssPoint::new(center.x + x * scale, center.y + y * scale);
    let outer = PaintShape::Path(vec![
        PathElement::MoveTo(point(0.0, -2.5)),
        PathElement::LineTo(point(-3.6, -6.6)),
        PathElement::LineTo(point(-6.8, -4.7)),
        PathElement::LineTo(point(-6.8, -1.0)),
        PathElement::LineTo(point(0.0, 6.8)),
        PathElement::LineTo(point(6.8, -1.0)),
        PathElement::LineTo(point(6.8, -4.7)),
        PathElement::LineTo(point(3.6, -6.6)),
        PathElement::Close,
    ]);
    primitives.push(Primitive::Fill {
        shape: outer.clone(),
        brush: PaintBrush::Solid(UI_PINK),
    });
    primitives.push(Primitive::Fill {
        shape: PaintShape::Path(vec![
            PathElement::MoveTo(point(-6.8, -1.0)),
            PathElement::LineTo(point(0.0, 6.8)),
            PathElement::LineTo(point(0.0, -2.5)),
            PathElement::Close,
        ]),
        brush: PaintBrush::Solid(HEART_DEPTH),
    });
    primitives.push(Primitive::Fill {
        shape: PaintShape::Path(vec![
            PathElement::MoveTo(point(0.0, -2.5)),
            PathElement::LineTo(point(3.6, -6.6)),
            PathElement::LineTo(point(6.8, -4.7)),
            PathElement::LineTo(point(4.6, -1.0)),
            PathElement::Close,
        ]),
        brush: PaintBrush::Solid(if energy > 0.45 { UI_CYAN } else { UI_PINK }),
    });
    primitives.push(Primitive::Stroke {
        shape: outer,
        brush: PaintBrush::Solid(UI_CYAN),
        style: StrokeStyle::solid((0.75 + energy.clamp(0.0, 1.0) * 0.45) * scale),
    });
    primitives.push(Primitive::Fill {
        shape: PaintShape::Path(vec![
            PathElement::MoveTo(point(-4.5, -4.4)),
            PathElement::LineTo(point(-3.1, -5.2)),
            PathElement::LineTo(point(-2.3, -3.9)),
            PathElement::Close,
        ]),
        brush: PaintBrush::Solid(HEART_LIGHT),
    });
}

pub fn paint_text_editor(
    primitives: &mut Vec<Primitive>,
    editor: &EditorVisual,
    rect: CssRect,
    color: PaintColor,
) {
    let origin = CssPoint::new(rect.x + 9.0, rect.y + 8.0);
    primitives.push(Primitive::PushClip(PaintShape::Rect(rect)));
    for selection in &editor.selection {
        primitives.push(Primitive::FillRect {
            rect: selection.translate(origin.x, origin.y),
            color: PaintColor::Rgba(88, 148, 255, 90),
        });
    }
    paint_chrome_text(
        primitives,
        &editor.text,
        origin,
        15.0,
        color,
        (rect.width - 18.0).max(1.0),
    );
    if let Some(caret) = editor.caret {
        primitives.push(Primitive::FillRect {
            rect: caret.translate(origin.x, origin.y),
            color,
        });
    }
    primitives.push(Primitive::PopClip);
}

fn paint_command_editor(primitives: &mut Vec<Primitive>, editor: &EditorVisual, rect: CssRect) {
    let origin = CssPoint::new(rect.x, rect.y + 3.0);
    primitives.push(Primitive::PushClip(PaintShape::Rect(rect)));
    for selection in &editor.selection {
        primitives.push(Primitive::FillRect {
            rect: selection.translate(origin.x, origin.y),
            color: UI_CYAN,
        });
    }
    paint_ui_text(
        primitives,
        &editor.text,
        origin,
        UI_PINK,
        rect.width.max(1.0),
        command_text_style(),
    );
    if let Some(caret) = editor.caret {
        primitives.push(Primitive::FillRect {
            rect: caret.translate(origin.x, origin.y),
            color: HEART_LIGHT,
        });
    }
    primitives.push(Primitive::PopClip);
}

fn paint_chrome_text(
    primitives: &mut Vec<Primitive>,
    text: &str,
    origin: CssPoint,
    size: f32,
    color: PaintColor,
    width: f32,
) {
    if text.is_empty() || width <= 0.0 {
        return;
    }
    let style = crate::text::TextStyle {
        size,
        ..crate::text::TextStyle::default()
    };
    let end = crate::text::first_line_end(
        text,
        &style,
        width,
        crate::text::TextBreakStyle {
            wrap: false,
            ..crate::text::TextBreakStyle::default()
        },
    );
    let shaped = crate::text::shape(text.get(..end).unwrap_or(text), &style);
    primitives.push(Primitive::GlyphRun {
        origin,
        shaped,
        color,
        decoration: TextDecorationPaint {
            color,
            style: DecorationStyle::Solid,
        },
        clip: None,
        node: 0,
        link: None,
    });
}

fn paint_ui_text(
    primitives: &mut Vec<Primitive>,
    text: &str,
    origin: CssPoint,
    color: PaintColor,
    width: f32,
    style: crate::text::TextStyle,
) {
    if text.is_empty() || width <= 0.0 {
        return;
    }
    let end = crate::text::first_line_end(
        text,
        &style,
        width,
        crate::text::TextBreakStyle {
            wrap: false,
            ..crate::text::TextBreakStyle::default()
        },
    );
    let shaped = crate::text::shape(text.get(..end).unwrap_or(text), &style);
    primitives.push(Primitive::GlyphRun {
        origin,
        shaped,
        color,
        decoration: TextDecorationPaint {
            color,
            style: DecorationStyle::Solid,
        },
        clip: None,
        node: 0,
        link: None,
    });
}

fn command_text_style() -> crate::text::TextStyle {
    command_text_style_with_size(crate::theme::TERMINAL_FONT_SIZE_CSS_PX)
}

fn command_text_style_with_size(size: f32) -> crate::text::TextStyle {
    crate::text::TextStyle {
        family: crate::theme::TERMINAL_FONT_FAMILY.to_string(),
        size,
        weight: crate::theme::TERMINAL_FONT_WEIGHT,
        ..crate::text::TextStyle::default()
    }
}

#[derive(Debug)]
pub struct RasterFrame<'a> {
    pub size: PhysicalSize,
    /// Softbuffer-compatible `0x00RRGGBB` pixels.
    pub pixels: &'a [u32],
}

/// Window-free backend contract used by the CPU reference, Hybrid
/// differential tests, snapshots, and diagnostics. Native Hybrid presentation
/// uses the same [`Scene`] but writes directly to its retained wgpu surface.
pub trait RasterBackend {
    fn render<'a>(&'a mut self, scene: &Scene) -> Result<RasterFrame<'a>, String>;
}

/// Renderer requested by the desktop command line. `Auto` is deliberately a
/// frontend policy: it probes Hybrid once and retains the CPU backend if GPU
/// initialization is unavailable. Browser/layout behavior never depends on
/// this choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RendererPreference {
    Cpu,
    Hybrid,
    #[default]
    Auto,
}

impl std::str::FromStr for RendererPreference {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Ok(Self::Cpu),
            "hybrid" => Ok(Self::Hybrid),
            "auto" => Ok(Self::Auto),
            _ => Err(format!(
                "unknown renderer {value:?}; expected cpu, hybrid, or auto"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererKind {
    Cpu,
    Hybrid,
}

impl RendererKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Hybrid => "hybrid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BrowserSnapshot, CssSize, ScaleFactor};

    fn snapshot() -> BrowserSnapshot {
        BrowserSnapshot {
            address: String::from("https://example.com/"),
            status: String::from("Ready"),
            loading: false,
            can_go_back: true,
            can_go_forward: false,
            focused: false,
            viewport: CssSize::new(800.0, 600.0),
            page_revision: 0,
        }
    }

    #[test]
    fn leaf_paint_change_produces_transformed_clipped_damage() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(200, 120), ScaleFactor::default());
        let mut old = desktop_shell(viewport, &snapshot());
        old.primitives
            .push(DisplayCommand::PushClip(PaintShape::Rect(CssRect::new(
                20.0, 10.0, 40.0, 40.0,
            ))));
        old.primitives
            .push(DisplayCommand::PushTransform(Affine2d::translate(
                10.0, 5.0,
            )));
        old.primitives.push(DisplayCommand::FillRect {
            rect: CssRect::new(15.0, 10.0, 50.0, 20.0),
            color: PaintColor::Rgba(10, 20, 30, 255),
        });
        old.primitives.push(DisplayCommand::PopTransform);
        old.primitives.push(DisplayCommand::PopClip);
        let mut new = old.clone();
        let DisplayCommand::FillRect { color, .. } = &mut new.primitives[3] else {
            panic!("expected test fill")
        };
        *color = PaintColor::Rgba(30, 20, 10, 255);

        assert_eq!(
            scene_damage(&old, &new),
            SceneDamage::Partial(CssRect::new(23.0, 13.0, 39.0, 24.0))
        );
    }

    #[test]
    fn stateful_display_list_change_requires_full_damage() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(200, 120), ScaleFactor::default());
        let mut old = desktop_shell(viewport, &snapshot());
        old.primitives
            .push(DisplayCommand::PushTransform(Affine2d::translate(
                10.0, 5.0,
            )));
        old.primitives.push(DisplayCommand::FillRect {
            rect: CssRect::new(0.0, 0.0, 20.0, 20.0),
            color: PaintColor::Accent,
        });
        old.primitives.push(DisplayCommand::PopTransform);
        let mut new = old.clone();
        new.primitives[1] = DisplayCommand::PushTransform(Affine2d::translate(11.0, 5.0));
        assert_eq!(scene_damage(&old, &new), SceneDamage::Full);
    }

    #[test]
    fn raster_damage_quantizes_outward_at_device_scale() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(400, 240), ScaleFactor::new(2.0));
        let scene = desktop_shell(viewport, &snapshot());
        let damage = raster_damage(&scene, CssRect::new(10.25, 5.25, 20.1, 10.1)).unwrap();
        assert_eq!((damage.x, damage.y), (20, 10));
        assert_eq!(damage.scene.viewport.physical, PhysicalSize::new(41, 21));
        assert!(matches!(
            damage.scene.primitives.first(),
            Some(DisplayCommand::PushTransform(transform))
                if *transform == Affine2d::translate(-10.0, -5.0)
        ));
    }

    #[test]
    fn image_store_generation_changes_with_replaced_pixels() {
        let store = ImageStore::default();
        let handle = ImageHandle(7);
        assert_eq!(store.generation(), 0);
        for value in [1, 2] {
            store.insert(
                handle,
                ImageResource {
                    width: 1,
                    height: 1,
                    rgba: Arc::from([value, value, value, 255]),
                    has_alpha: false,
                },
            );
        }
        assert_eq!(store.generation(), 2);
    }

    #[test]
    fn browse_mode_has_no_placeholder_chrome_at_any_device_scale() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(1600, 1200), ScaleFactor::new(2.0));
        let scene = desktop_shell(viewport, &snapshot());
        assert_eq!(scene.content_viewport, CssRect::new(0.0, 0.0, 800.0, 600.0));
        assert!(scene.controls.is_empty());
    }

    #[test]
    fn command_mode_reserves_only_its_bottom_panel() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let model = ChromeModel {
            command: Some(EditorVisual::default()),
            ..ChromeModel::default()
        };
        let mut scene = desktop_chrome(viewport, &snapshot(), &model);
        assert_eq!(scene.content_viewport.y, 0.0);
        assert_eq!(scene.content_viewport.height, 600.0 - COMMAND_PANEL_HEIGHT);
        paint_desktop_overlay(&mut scene, &snapshot(), &model);
        assert_eq!(
            scene.control_at(CssPoint::new(300.0, 600.0 - COMMAND_PANEL_HEIGHT + 30.0)),
            Some(ControlId::Command)
        );
    }

    #[test]
    fn heart_and_clickable_rail_map_to_the_scroll_range() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let mut scene = desktop_shell(viewport, &snapshot());
        scene.page_size = CssSize::new(800.0, 1_800.0);
        for active in [false, true] {
            scene.image_store.insert(
                desktop_heart_image_handle(active),
                ImageResource {
                    width: 30,
                    height: 30,
                    rgba: Arc::from(vec![0; 30 * 30 * 4]),
                    has_alpha: true,
                },
            );
        }
        let model = ChromeModel {
            heart: HeartVisual {
                vertical_visible: true,
                vertical_fraction: Some(0.5),
                ..HeartVisual::default()
            },
            ..ChromeModel::default()
        };
        paint_desktop_overlay(&mut scene, &snapshot(), &model);
        let heart = scene
            .controls
            .iter()
            .find(|control| control.id == ControlId::VerticalHeart)
            .expect("scrollable pages expose the heart thumb");
        assert_eq!(HEART_SIZE, 30.0);
        assert_eq!((heart.rect.width, heart.rect.height), (34.0, 34.0));
        assert_eq!(heart.rect.y + heart.rect.height / 2.0, 300.0);
        assert!(scene.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Image { handle, rect, .. }
                if *handle == desktop_heart_image_handle(false)
                    && rect.width == 30.0
                    && rect.height == 30.0
        )));
        let active_model = ChromeModel {
            heart: HeartVisual {
                vertical_visible: true,
                vertical_fraction: Some(0.5),
                energy: 1.0,
                ..HeartVisual::default()
            },
            ..ChromeModel::default()
        };
        paint_desktop_overlay(&mut scene, &snapshot(), &active_model);
        assert!(scene.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Image { handle, rect, .. }
                if *handle == desktop_heart_image_handle(true)
                    && (rect.width - 32.4).abs() < 0.01
                    && (rect.height - 32.4).abs() < 0.01
        )));
        assert_eq!(
            scene.control_at(CssPoint::new(788.0, 100.0)),
            Some(ControlId::VerticalRail)
        );
        assert_eq!(
            scene.control_at(CssPoint::new(788.0, 300.0)),
            Some(ControlId::VerticalHeart),
            "the jewel must win hit testing over its rail"
        );
        assert_eq!(scrollbar_fraction(600.0, 1_800.0, 600.0), Some(0.5));
        assert_eq!(scrollbar_position(0.5, 1_800.0, 600.0), 600.0);
        assert_eq!(scrollbar_track_fraction(300.0, 17.0, 583.0), 0.5);
        assert_eq!(scrollbar_track_fraction(-20.0, 17.0, 583.0), 0.0);
        assert_eq!(scrollbar_track_fraction(900.0, 17.0, 583.0), 1.0);
        assert!(
            !scene
                .controls
                .iter()
                .any(|control| control.id == ControlId::HorizontalHeart),
            "a fitting page must not grow a horizontal scrollbar"
        );
    }

    #[test]
    fn genuine_horizontal_overflow_gets_a_clickable_rail_and_heart() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let mut scene = desktop_shell(viewport, &snapshot());
        scene.page_size = CssSize::new(1_600.0, 1_800.0);
        let model = ChromeModel {
            heart: HeartVisual {
                vertical_visible: true,
                vertical_fraction: Some(0.5),
                horizontal_fraction: Some(0.5),
                ..HeartVisual::default()
            },
            ..ChromeModel::default()
        };
        paint_desktop_overlay(&mut scene, &snapshot(), &model);
        assert_eq!(
            scene.control_at(CssPoint::new(100.0, 588.0)),
            Some(ControlId::HorizontalRail)
        );
        assert!(
            scene
                .controls
                .iter()
                .any(|control| control.id == ControlId::HorizontalHeart)
        );
    }

    #[test]
    fn semantic_hit_testing_obeys_page_transforms_and_clips() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let mut scene = desktop_shell(viewport, &snapshot());
        let mut page = PagePaint {
            width: 800.0,
            height: 1000.0,
            ..PagePaint::default()
        };
        page.primitives
            .push(DisplayCommand::PushClip(PaintShape::Rect(CssRect::new(
                0.0, 0.0, 50.0, 50.0,
            ))));
        page.primitives
            .push(DisplayCommand::PushTransform(Affine2d::translate(
                20.0, 10.0,
            )));
        page.primitives.push(DisplayCommand::HitRegion(HitRegion {
            rect: CssRect::new(0.0, 0.0, 100.0, 20.0),
            node: 7,
            actor: Some(42),
            link: None,
        }));
        page.primitives.push(DisplayCommand::PopTransform);
        page.primitives.push(DisplayCommand::PopClip);
        scene.append_page(&page, CssPoint::default());

        let hit = scene
            .page_hit_at(CssPoint::new(25.0, scene.content_viewport.y + 15.0))
            .expect("transformed region should be interactive");
        assert_eq!((hit.node, hit.actor), (7, Some(42)));
        assert!(
            scene
                .page_hit_at(CssPoint::new(60.0, scene.content_viewport.y + 15.0))
                .is_none(),
            "the active page clip must constrain hit testing"
        );
    }

    #[test]
    fn decorative_hit_region_does_not_mask_the_button_beneath_it() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let mut scene = desktop_shell(viewport, &snapshot());
        let mut page = PagePaint::default();
        let rect = CssRect::new(10.0, 10.0, 40.0, 30.0);
        page.primitives.push(DisplayCommand::HitRegion(HitRegion {
            rect,
            node: 7,
            actor: Some(42),
            link: None,
        }));
        // A later-painted anonymous glyph/SVG box has geometry but no semantic
        // identity. The button remains the pointer target through that paint.
        page.primitives.push(DisplayCommand::HitRegion(HitRegion {
            rect,
            node: crate::layout2::NO_NODE,
            actor: None,
            link: None,
        }));
        scene.append_page(&page, CssPoint::default());

        let point = CssPoint::new(20.0, scene.content_viewport.y + 20.0);
        let hit = scene
            .page_hit_at(point)
            .expect("button should remain hittable");
        assert_eq!((hit.node, hit.actor), (7, Some(42)));
    }

    #[test]
    fn selection_retains_logical_text_at_parley_cluster_boundaries() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(800, 600), ScaleFactor::default());
        let mut scene = desktop_shell(viewport, &snapshot());
        let shaped = crate::text::shape("a👨‍👩‍👧‍👦b", &crate::text::TextStyle::default());
        let family = shaped
            .clusters
            .iter()
            .find(|cluster| cluster.text_range.start == 1)
            .expect("emoji family should have a shaped cluster")
            .text_range
            .clone();
        let mut page = PagePaint::default();
        page.primitives.push(DisplayCommand::GlyphRun {
            origin: CssPoint::new(0.0, 0.0),
            shaped,
            color: PaintColor::Foreground,
            decoration: TextDecorationPaint {
                color: PaintColor::Foreground,
                style: DecorationStyle::Solid,
            },
            clip: None,
            node: 1,
            link: None,
        });
        scene.append_page(&page, CssPoint::default());
        let command = scene
            .primitives
            .iter()
            .position(|primitive| matches!(primitive, DisplayCommand::GlyphRun { shaped, .. } if shaped.text.starts_with('a')))
            .unwrap();
        let selection = TextSelection {
            anchor: TextPosition {
                command,
                byte: family.start,
            },
            focus: TextPosition {
                command,
                byte: family.end,
            },
        };
        assert_eq!(scene.selected_text(selection), "👨‍👩‍👧‍👦");
        assert!(!scene.selection_rects(selection).is_empty());
    }

    #[test]
    fn image_store_is_bounded_and_evicts_least_recently_used_resources() {
        let store = ImageStore::default();
        for index in 0..MAX_IMAGE_RESOURCES {
            store.insert(
                ImageHandle(index as u64),
                ImageResource {
                    width: 1,
                    height: 1,
                    rgba: Arc::from([index as u8, 0, 0, 255]),
                    has_alpha: false,
                },
            );
        }
        assert!(store.get(ImageHandle(0)).is_some(), "get touches the LRU");
        store.insert(
            ImageHandle(MAX_IMAGE_RESOURCES as u64),
            ImageResource {
                width: 1,
                height: 1,
                rgba: Arc::from([0, 0, 0, 255]),
                has_alpha: false,
            },
        );
        assert_eq!(store.len(), MAX_IMAGE_RESOURCES);
        assert!(store.contains(ImageHandle(0)));
        assert!(!store.contains(ImageHandle(1)));
        assert!(store.contains(ImageHandle(MAX_IMAGE_RESOURCES as u64)));
    }

    #[test]
    fn replacing_pixels_under_a_stable_image_handle_advances_its_revision() {
        let store = ImageStore::default();
        let handle = ImageHandle(42);
        let image = |red| ImageResource {
            width: 1,
            height: 1,
            rgba: Arc::from([red, 0, 0, 255]),
            has_alpha: false,
        };
        store.insert(handle, image(10));
        let first = store.revision(handle).unwrap();
        assert!(store.get(handle).is_some());
        assert_eq!(
            store.revision(handle),
            Some(first),
            "an LRU touch is not a mutation"
        );
        store.insert(handle, image(20));
        assert_ne!(store.revision(handle), Some(first));
        assert_eq!(store.remove(handle).unwrap().rgba[0], 20);
        assert_eq!(store.revision(handle), None);
    }
}
