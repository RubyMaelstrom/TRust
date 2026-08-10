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
    entries: HashMap<ImageHandle, ImageResource>,
    order: VecDeque<ImageHandle>,
    bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ImageStore(Arc<RwLock<ImageStoreState>>);

impl ImageStore {
    pub fn insert(&self, handle: ImageHandle, image: ImageResource) {
        let mut store = self.0.write().expect("image store poisoned");
        if let Some(old) = store.entries.remove(&handle) {
            store.bytes = store.bytes.saturating_sub(old.rgba.len());
            store.order.retain(|candidate| *candidate != handle);
        }
        store.bytes = store.bytes.saturating_add(image.rgba.len());
        store.entries.insert(handle, image);
        store.order.push_back(handle);
        while store.entries.len() > MAX_IMAGE_RESOURCES || store.bytes > MAX_IMAGE_RESOURCE_BYTES {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            if let Some(old) = store.entries.remove(&oldest) {
                store.bytes = store.bytes.saturating_sub(old.rgba.len());
            }
        }
    }

    pub fn get(&self, handle: ImageHandle) -> Option<ImageResource> {
        let mut store = self.0.write().expect("image store poisoned");
        let image = store.entries.get(&handle).cloned();
        if image.is_some() {
            // Explicit LRU ownership: the shared decoded store is touched by
            // display-list construction/backends, while each backend owns its
            // own independent atlas eviction policy.
            store.order.retain(|candidate| *candidate != handle);
            store.order.push_back(handle);
        }
        image
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
    /// Viewport-fixed commands are composed after the scrolled document list.
    pub fixed_primitives: Vec<Primitive>,
    pub image_requests: Vec<ImageRequest>,
    pub scroll_containers: Vec<ScrollContainer>,
    pub sticky_constraints: Vec<StickyConstraint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlId {
    Back,
    Forward,
    Reload,
    Stop,
    Address,
    Console,
    Find,
    FindPrevious,
    FindNext,
    FindClose,
}

#[derive(Clone, Debug, Default)]
pub struct EditorVisual {
    pub text: String,
    pub selection: Vec<CssRect>,
    pub caret: Option<CssRect>,
}

#[derive(Clone, Debug, Default)]
pub struct ChromeModel {
    pub address: EditorVisual,
    pub address_focused: bool,
    pub title: String,
    pub status: String,
    pub link_preview: String,
    pub find: Option<EditorVisual>,
    pub find_count: Option<(usize, usize)>,
    pub console: Option<EditorVisual>,
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

/// Build the permanent minimal desktop shell. This is graphical chrome—not a
/// Ratatui frame. The desktop frontend appends the canonical page display list
/// into the returned content viewport.
pub fn desktop_shell(viewport: ViewportMetrics, browser: &BrowserSnapshot) -> Scene {
    desktop_chrome(
        viewport,
        browser,
        &ChromeModel {
            address: EditorVisual {
                text: browser.address.clone(),
                ..EditorVisual::default()
            },
            address_focused: browser.focused,
            status: browser.status.clone(),
            ..ChromeModel::default()
        },
    )
}

/// Practical renderer-neutral desktop chrome. Every label and editable value
/// is a retained Parley glyph run, consumed through the same display list and
/// Vello/Glifo path as page text.
pub fn desktop_chrome(
    viewport: ViewportMetrics,
    browser: &BrowserSnapshot,
    model: &ChromeModel,
) -> Scene {
    const BUTTON: f32 = 36.0;
    const GAP: f32 = 8.0;
    const TOP: f32 = 8.0;

    let extra = if model.find.is_some() || model.console.is_some() {
        38.0
    } else {
        0.0
    };
    let chrome_height: f32 = 82.0 + extra;

    let width = viewport.css.width;
    let height = viewport.css.height;
    let mut primitives = Vec::with_capacity(48);
    let whole = CssRect::new(0.0, 0.0, width, height);
    let chrome = CssRect::new(0.0, 0.0, width, chrome_height.min(height));
    let content = CssRect::new(
        0.0,
        chrome_height.min(height),
        width,
        (height - chrome_height).max(0.0),
    );
    primitives.push(Primitive::FillRect {
        rect: whole,
        color: PaintColor::Window,
    });
    primitives.push(Primitive::FillRect {
        rect: chrome,
        color: PaintColor::Chrome,
    });
    primitives.push(Primitive::FillRect {
        rect: content,
        color: PaintColor::Content,
    });

    let ids = [
        (ControlId::Back, browser.can_go_back),
        (ControlId::Forward, browser.can_go_forward),
        (
            ControlId::Reload,
            !browser.loading && !browser.address.is_empty(),
        ),
        (ControlId::Stop, browser.loading),
        (ControlId::Console, true),
        (ControlId::Find, true),
    ];
    let mut controls = Vec::with_capacity(12);
    for (index, (id, enabled)) in ids.into_iter().enumerate() {
        let rect = CssRect::new(GAP + index as f32 * (BUTTON + GAP), TOP, BUTTON, BUTTON);
        controls.push(ControlRegion { id, rect, enabled });
        primitives.push(Primitive::FillRect {
            rect,
            color: if enabled {
                PaintColor::Surface
            } else {
                PaintColor::Muted
            },
        });
        add_control_icon(&mut primitives, id, rect, enabled);
    }

    let address_x = GAP + 6.0 * (BUTTON + GAP);
    let address = CssRect::new(address_x, TOP, (width - address_x - GAP).max(0.0), BUTTON);
    controls.push(ControlRegion {
        id: ControlId::Address,
        rect: address,
        enabled: true,
    });
    primitives.push(Primitive::FillRect {
        rect: address,
        color: if model.address_focused {
            PaintColor::Foreground
        } else {
            PaintColor::Surface
        },
    });
    let inset = 2.0;
    primitives.push(Primitive::FillRect {
        rect: CssRect::new(
            address.x + inset,
            address.y + inset,
            (address.width - inset * 2.0).max(0.0),
            (address.height - inset * 2.0).max(0.0),
        ),
        color: PaintColor::Window,
    });

    paint_text_editor(
        &mut primitives,
        &model.address,
        address,
        PaintColor::Foreground,
    );

    if browser.loading {
        primitives.push(Primitive::FillRect {
            rect: CssRect::new(0.0, chrome.height - 3.0, (width * 0.32).max(24.0), 3.0),
            color: PaintColor::Loading,
        });
    }

    let title = if model.title.is_empty() {
        "TRust Desktop"
    } else {
        model.title.as_str()
    };
    paint_chrome_text(
        &mut primitives,
        title,
        CssPoint::new(GAP, 52.0),
        13.0,
        PaintColor::Foreground,
        (width * 0.45).max(20.0),
    );
    let status = if model.link_preview.is_empty() {
        model.status.as_str()
    } else {
        model.link_preview.as_str()
    };
    paint_chrome_text(
        &mut primitives,
        status,
        CssPoint::new((width * 0.46).max(GAP), 52.0),
        13.0,
        PaintColor::Muted,
        (width * 0.53 - GAP).max(20.0),
    );

    let overlay_y = 82.0;
    if let Some(find) = &model.find {
        let field = CssRect::new(GAP, overlay_y, (width - 174.0).max(80.0), 30.0);
        controls.push(ControlRegion {
            id: ControlId::Find,
            rect: field,
            enabled: true,
        });
        primitives.push(Primitive::FillRect {
            rect: field,
            color: PaintColor::Window,
        });
        paint_text_editor(&mut primitives, find, field, PaintColor::Foreground);
        for (index, id) in [
            ControlId::FindPrevious,
            ControlId::FindNext,
            ControlId::FindClose,
        ]
        .into_iter()
        .enumerate()
        {
            let rect = CssRect::new(width - 158.0 + index as f32 * 50.0, overlay_y, 44.0, 30.0);
            controls.push(ControlRegion {
                id,
                rect,
                enabled: true,
            });
            primitives.push(Primitive::FillRect {
                rect,
                color: PaintColor::Surface,
            });
            paint_chrome_text(
                &mut primitives,
                match id {
                    ControlId::FindPrevious => "↑",
                    ControlId::FindNext => "↓",
                    _ => "×",
                },
                CssPoint::new(rect.x + 15.0, rect.y + 6.0),
                15.0,
                PaintColor::Foreground,
                rect.width - 8.0,
            );
        }
        if let Some((current, total)) = model.find_count {
            paint_chrome_text(
                &mut primitives,
                &format!("{current}/{total}"),
                CssPoint::new((field.x + field.width - 60.0).max(field.x), field.y + 7.0),
                12.0,
                PaintColor::Muted,
                55.0,
            );
        }
    } else if let Some(console) = &model.console {
        let field = CssRect::new(GAP, overlay_y, (width - GAP * 2.0).max(80.0), 30.0);
        controls.push(ControlRegion {
            id: ControlId::Console,
            rect: field,
            enabled: true,
        });
        primitives.push(Primitive::FillRect {
            rect: field,
            color: PaintColor::Window,
        });
        paint_chrome_text(
            &mut primitives,
            ":",
            CssPoint::new(field.x + 7.0, field.y + 6.0),
            15.0,
            PaintColor::Accent,
            12.0,
        );
        let input = CssRect::new(field.x + 16.0, field.y, field.width - 16.0, field.height);
        paint_text_editor(&mut primitives, console, input, PaintColor::Foreground);
    }

    Scene {
        viewport,
        primitives,
        controls,
        content_viewport: content,
        image_store: ImageStore::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    }
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

fn add_control_icon(primitives: &mut Vec<Primitive>, id: ControlId, rect: CssRect, enabled: bool) {
    let color = if enabled {
        PaintColor::Foreground
    } else {
        PaintColor::Chrome
    };
    let cx = rect.x + rect.width / 2.0;
    let cy = rect.y + rect.height / 2.0;
    match id {
        ControlId::Back => primitives.push(Primitive::FillPolygon {
            points: vec![
                CssPoint::new(cx - 8.0, cy),
                CssPoint::new(cx + 5.0, cy - 9.0),
                CssPoint::new(cx + 5.0, cy + 9.0),
            ],
            color,
        }),
        ControlId::Forward => primitives.push(Primitive::FillPolygon {
            points: vec![
                CssPoint::new(cx + 8.0, cy),
                CssPoint::new(cx - 5.0, cy - 9.0),
                CssPoint::new(cx - 5.0, cy + 9.0),
            ],
            color,
        }),
        ControlId::Reload => {
            primitives.push(Primitive::FillRect {
                rect: CssRect::new(cx - 8.0, cy - 8.0, 16.0, 5.0),
                color,
            });
            primitives.push(Primitive::FillRect {
                rect: CssRect::new(cx + 3.0, cy - 8.0, 5.0, 16.0),
                color,
            });
            primitives.push(Primitive::FillPolygon {
                points: vec![
                    CssPoint::new(cx + 9.0, cy + 8.0),
                    CssPoint::new(cx + 1.0, cy + 8.0),
                    CssPoint::new(cx + 9.0, cy + 1.0),
                ],
                color,
            });
        }
        ControlId::Stop => primitives.push(Primitive::FillRect {
            rect: CssRect::new(cx - 7.0, cy - 7.0, 14.0, 14.0),
            color,
        }),
        ControlId::Console => {
            primitives.push(Primitive::FillRect {
                rect: CssRect::new(cx - 9.0, cy - 8.0, 18.0, 16.0),
                color,
            });
            primitives.push(Primitive::FillRect {
                rect: CssRect::new(cx - 7.0, cy - 6.0, 14.0, 12.0),
                color: PaintColor::Surface,
            });
        }
        ControlId::Find => {
            primitives.push(Primitive::Stroke {
                shape: PaintShape::RoundedRect {
                    rect: CssRect::new(cx - 8.0, cy - 8.0, 12.0, 12.0),
                    radii: CornerRadii {
                        corners: [(6.0, 6.0); 4],
                    },
                },
                brush: PaintBrush::Solid(color),
                style: StrokeStyle::solid(2.0),
            });
            primitives.push(Primitive::FillRect {
                rect: CssRect::new(cx + 3.0, cy + 3.0, 8.0, 2.0),
                color,
            });
        }
        ControlId::Address
        | ControlId::FindPrevious
        | ControlId::FindNext
        | ControlId::FindClose => {}
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
    fn chrome_hit_testing_uses_css_coordinates_at_any_device_scale() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(1600, 1200), ScaleFactor::new(2.0));
        let scene = desktop_shell(viewport, &snapshot());
        assert_eq!(scene.content_viewport.y, 82.0);
        assert_eq!(
            scene.control_at(CssPoint::new(20.0, 20.0)),
            Some(ControlId::Back)
        );
        assert_eq!(scene.control_at(CssPoint::new(64.0, 20.0)), None);
        assert_eq!(
            scene.control_at(CssPoint::new(300.0, 20.0)),
            Some(ControlId::Address)
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
}
