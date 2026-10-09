//! Filter Effects 1 (#FilterPrimitivesOverview, #ShorthandEquivalents) and
//! Filter Effects 2 (#BackdropFilterProperty) evaluated on CPU pixels.
//!
//! Local reference: csswg-drafts 81c27f686901 (2026-09-04),
//! `filter-effects-1/Overview.bs` and `filter-effects-2/Overview.bs`.
//!
//! The renderer-neutral [`FilterGraph`] is an SVG `filter` element resolved
//! by graphical paint for one referencing element: lengths, regions and
//! subregions are in that element's local CSS px, inputs refer to earlier
//! primitives by index, and colors are straight sRGB. Both raster backends
//! rasterize the filtered group offscreen with the CPU reference renderer and
//! run the same evaluator here, so backend selection cannot change filter
//! output.
//!
//! Pixels are premultiplied RGBA in `f32`; a buffer is tagged with the color
//! space it holds (sRGB or linear-light sRGB) and converted only when a
//! primitive with another `color-interpolation-filters` value consumes it
//! (#ColorInterpolationFiltersProperty). Work is bounded: callers cap the
//! buffer size (lowering resolution as #FilterPrimitivesOverviewIntro
//! permits), only the primary filter primitive tree is evaluated
//! (#FilterPrimitiveTree), every result is dropped after its last use, and
//! kernel sizes are clamped.

use std::sync::Arc;

use super::{Affine2d, BlendMode, CssFilter, CssRect, PaintColor};

/// The most primitives of one primary tree that are evaluated; further
/// primitives (always earlier ones, since references only point backwards)
/// are treated as transparent black.
const MAX_PRIMITIVES: usize = 256;
/// A blur deviation or morphology radius beyond this many buffer pixels has
/// the same visible effect on a bounded buffer and is clamped.
const MAX_KERNEL_PX: f32 = 2048.0;
/// #feTurbulenceElement permits clamping numOctaves to the color depth.
const MAX_OCTAVES: u32 = 24;

/// An SVG `filter` element resolved for one referencing element
/// (Filter Effects 1 #FilterElement, #FilterEffectsRegion).
#[derive(Clone, Debug, PartialEq)]
pub struct FilterGraph {
    /// The filter region, in the referencing element's local CSS px. It
    /// clips every primitive's inputs (#FilterPrimitiveSubRegion).
    pub region: CssRect,
    /// The filter's children, in document order. Inputs reference only
    /// earlier primitives; the last primitive roots the primary tree.
    pub primitives: Vec<FilterPrimitive>,
    /// Local CSS px to the coordinate system of `primitiveUnits`, where
    /// feTurbulence evaluates its noise function.
    pub primitive_space: Affine2d,
    /// The `fill` and `stroke` of the referencing element (#FillPaint,
    /// #StrokePaint) as straight sRGB, or transparent when not solid.
    pub fill_paint: [f32; 4],
    pub stroke_paint: [f32; 4],
}

impl FilterGraph {
    /// Whether a primitive reads the isolation group's backdrop.
    pub fn reads_background(&self) -> bool {
        self.primitives.iter().any(|primitive| {
            primitive.kind.inputs().iter().any(|input| {
                matches!(
                    input,
                    FilterInput::BackgroundImage | FilterInput::BackgroundAlpha
                )
            })
        })
    }

    /// The `feImage` graphics of the primitives, whose images the paint
    /// that uses this graph requests.
    pub fn images(&self) -> impl Iterator<Item = &FeImage> {
        self.primitives
            .iter()
            .filter_map(|primitive| match &primitive.kind {
                FilterPrimitiveKind::Image(image) => Some(&**image),
                _ => None,
            })
    }

    /// How far (local px) any primitive can move content: the margin a
    /// caller adds around the visible area to render enough of the input.
    pub fn reach(&self) -> f32 {
        self.primitives
            .iter()
            .map(|primitive| primitive.kind.reach())
            .fold(0.0, f32::max)
    }
}

/// One filter primitive (#CommonAttributes).
#[derive(Clone, Debug, PartialEq)]
pub struct FilterPrimitive {
    /// The filter primitive subregion in local CSS px; an empty one disables
    /// the primitive (its result is transparent black).
    pub subregion: CssRect,
    /// `color-interpolation-filters: linearRGB` (the initial value).
    pub linear_rgb: bool,
    pub kind: FilterPrimitiveKind,
}

/// #CommonAttributes `in`: a standard input or an earlier primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterInput {
    SourceGraphic,
    SourceAlpha,
    BackgroundImage,
    BackgroundAlpha,
    FillPaint,
    StrokePaint,
    /// The result of the primitive at this index.
    Result(usize),
}

/// #feGaussianBlurElement `edgeMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EdgeMode {
    #[default]
    None,
    Duplicate,
    Wrap,
    Mirror,
}

/// #feComponentTransferElement transfer functions on straight values.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum TransferFunction {
    #[default]
    Identity,
    Table(Vec<f32>),
    Discrete(Vec<f32>),
    Linear {
        slope: f32,
        intercept: f32,
    },
    Gamma {
        amplitude: f32,
        exponent: f32,
        offset: f32,
    },
}

impl TransferFunction {
    fn apply(&self, c: f32) -> f32 {
        match self {
            Self::Identity => c,
            // An empty table is the identity transfer function.
            Self::Table(values) | Self::Discrete(values) if values.is_empty() => c,
            Self::Table(values) => {
                let n = values.len() - 1;
                if n == 0 || c >= 1.0 {
                    return values[n];
                }
                let position = c.max(0.0) * n as f32;
                let k = (position.floor() as usize).min(n - 1);
                values[k] + (position - k as f32) * (values[k + 1] - values[k])
            }
            Self::Discrete(values) => {
                let n = values.len();
                if c >= 1.0 {
                    return values[n - 1];
                }
                values[((c.max(0.0) * n as f32).floor() as usize).min(n - 1)]
            }
            Self::Linear { slope, intercept } => slope * c + intercept,
            Self::Gamma {
                amplitude,
                exponent,
                offset,
            } => amplitude * c.powf(*exponent) + offset,
        }
    }
}

/// #feCompositeElement `operator`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositeOp {
    Over,
    In,
    Out,
    Atop,
    Xor,
    Lighter,
    Arithmetic { k1: f32, k2: f32, k3: f32, k4: f32 },
}

/// #feTurbulenceElement parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Turbulence {
    pub base_frequency: (f32, f32),
    pub octaves: u32,
    pub seed: f32,
    pub stitch: bool,
    pub fractal_noise: bool,
}

/// #feConvolveMatrixElement parameters, already validated.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvolveMatrix {
    pub order: (usize, usize),
    pub kernel: Vec<f32>,
    pub divisor: f32,
    pub bias: f32,
    pub target: (usize, usize),
    pub edge: EdgeMode,
    pub preserve_alpha: bool,
}

/// #LightSourceDefinitions in local CSS px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightSource {
    Distant {
        azimuth: f32,
        elevation: f32,
    },
    Point {
        x: f32,
        y: f32,
        z: f32,
    },
    Spot {
        x: f32,
        y: f32,
        z: f32,
        at: (f32, f32, f32),
        exponent: f32,
        cone: Option<f32>,
    },
}

/// #feDiffuseLightingElement / #feSpecularLightingElement parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lighting {
    pub surface_scale: f32,
    /// `diffuseConstant`, or `specularConstant`.
    pub constant: f32,
    /// `Some(specularExponent)` for feSpecularLighting.
    pub specular_exponent: Option<f32>,
    /// `lighting-color` as straight sRGB.
    pub color: [f32; 3],
    pub light: LightSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FilterPrimitiveKind {
    /// #feBlendElement: `input` is the source, `input2` the backdrop.
    Blend {
        input: FilterInput,
        input2: FilterInput,
        mode: BlendMode,
        composite: bool,
    },
    /// #feColorMatrixElement; `None` is a pass through filter.
    ColorMatrix {
        input: FilterInput,
        matrix: Option<[f32; 20]>,
    },
    ComponentTransfer {
        input: FilterInput,
        functions: Box<[TransferFunction; 4]>,
    },
    Composite {
        input: FilterInput,
        input2: FilterInput,
        operator: CompositeOp,
    },
    ConvolveMatrix {
        input: FilterInput,
        matrix: Box<ConvolveMatrix>,
    },
    /// #feDisplacementMapElement: `input2` displaces `input`.
    DisplacementMap {
        input: FilterInput,
        input2: FilterInput,
        scale: (f32, f32),
        channels: (usize, usize),
    },
    /// #feDropShadowElement; lengths in local px.
    DropShadow {
        input: FilterInput,
        dx: f32,
        dy: f32,
        std_deviation: (f32, f32),
        color: [f32; 4],
    },
    /// #feFloodElement as straight sRGB.
    Flood {
        color: [f32; 4],
    },
    GaussianBlur {
        input: FilterInput,
        std_deviation: (f32, f32),
        edge: EdgeMode,
    },
    Lighting {
        input: FilterInput,
        lighting: Lighting,
    },
    Merge {
        inputs: Vec<FilterInput>,
    },
    Morphology {
        input: FilterInput,
        dilate: bool,
        radius: (f32, f32),
    },
    Offset {
        input: FilterInput,
        dx: f32,
        dy: f32,
    },
    Tile {
        input: FilterInput,
    },
    Turbulence(Turbulence),
    /// #feImageElement: an image, or a referenced element rendered into
    /// one, placed in the primitive's viewport.
    Image(Box<FeImage>),
    /// A primitive whose result is transparent black: a primitive in error,
    /// or an `feImage` whose reference is unusable.
    Transparent,
}

/// #feImageElement: the graphic an `feImage` primitive draws. An external
/// image is rendered like an `image` element whose viewport is the
/// primitive subregion; a referenced element is rendered (like `use`) into
/// a document image covering the subregion exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct FeImage {
    /// The image request: the external image's URL, or the document image
    /// of the referenced element.
    pub source: Arc<str>,
    pub handle: super::ImageHandle,
    /// The viewport the image is fitted into, in local CSS px.
    pub viewport: CssRect,
    /// `preserveAspectRatio`.
    pub aspect: AspectRatio,
}

/// SVG 2 #PreserveAspectRatioAttribute: `none` (`align: None`) stretches to
/// the viewport; otherwise the image keeps its aspect ratio, scaled to fit
/// (`meet`) or cover (`slice`) the viewport and aligned by the fractions
/// (0, ½ or 1 of the free space) of `align`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AspectRatio {
    pub align: Option<(f32, f32)>,
    pub slice: bool,
}

impl Default for AspectRatio {
    /// `xMidYMid meet`, the initial value.
    fn default() -> Self {
        Self {
            align: Some((0.5, 0.5)),
            slice: false,
        }
    }
}

impl AspectRatio {
    /// Parse `[defer] <align> [<meetOrSlice>]`; `None` when invalid.
    pub fn parse(value: &str) -> Option<Self> {
        let mut words = value.split_ascii_whitespace().peekable();
        if words.peek() == Some(&"defer") {
            words.next();
        }
        let align = match words.next()? {
            "none" => None,
            align => {
                let fraction = |name: &str| match name {
                    "Min" => Some(0.0),
                    "Mid" => Some(0.5),
                    "Max" => Some(1.0),
                    _ => None,
                };
                let rest = align.strip_prefix('x')?;
                let (x, y) = rest.split_at_checked(3)?;
                Some((fraction(x)?, fraction(y.strip_prefix('Y')?)?))
            }
        };
        let slice = match words.next() {
            None | Some("meet") => false,
            Some("slice") => true,
            Some(_) => return None,
        };
        words.next().is_none().then_some(Self { align, slice })
    }

    /// The rectangle an image of `natural` size occupies in `viewport`.
    pub fn place(self, natural: (f32, f32), viewport: CssRect) -> CssRect {
        let Some((ax, ay)) = self.align else {
            return viewport;
        };
        if !(natural.0 > 0.0 && natural.1 > 0.0) {
            return CssRect::new(viewport.x, viewport.y, 0.0, 0.0);
        }
        let (sx, sy) = (viewport.width / natural.0, viewport.height / natural.1);
        let scale = if self.slice { sx.max(sy) } else { sx.min(sy) };
        let (width, height) = (natural.0 * scale, natural.1 * scale);
        CssRect::new(
            viewport.x + (viewport.width - width) * ax,
            viewport.y + (viewport.height - height) * ay,
            width,
            height,
        )
    }
}

/// The pixels of `feImage` primitives' images, from the scene's image
/// store. Both raster backends evaluate filters on CPU with the same source.
pub trait FilterImageSource {
    /// The natural size of the image, once it is available.
    fn natural_size(&self, handle: super::ImageHandle) -> Option<(u32, u32)>;
    /// The image as straight-alpha RGBA8 for drawing into `rect` (local
    /// CSS px) under `to_pixel`: a vector image is rasterized at that size.
    fn pixels(
        &self,
        handle: super::ImageHandle,
        rect: CssRect,
        to_pixel: Affine2d,
    ) -> Option<super::ImageResource>;
}

impl FilterPrimitiveKind {
    fn inputs(&self) -> Vec<FilterInput> {
        match self {
            Self::Blend { input, input2, .. }
            | Self::Composite { input, input2, .. }
            | Self::DisplacementMap { input, input2, .. } => vec![*input, *input2],
            Self::ColorMatrix { input, .. }
            | Self::ComponentTransfer { input, .. }
            | Self::ConvolveMatrix { input, .. }
            | Self::DropShadow { input, .. }
            | Self::GaussianBlur { input, .. }
            | Self::Lighting { input, .. }
            | Self::Morphology { input, .. }
            | Self::Offset { input, .. }
            | Self::Tile { input } => vec![*input],
            Self::Merge { inputs } => inputs.clone(),
            Self::Flood { .. } | Self::Turbulence(_) | Self::Image(_) | Self::Transparent => {
                Vec::new()
            }
        }
    }

    fn reach(&self) -> f32 {
        match self {
            Self::GaussianBlur { std_deviation, .. } => 3.0 * std_deviation.0.max(std_deviation.1),
            Self::DropShadow {
                dx,
                dy,
                std_deviation,
                ..
            } => 3.0 * std_deviation.0.max(std_deviation.1) + dx.abs().max(dy.abs()),
            Self::Offset { dx, dy, .. } => dx.abs().max(dy.abs()),
            Self::Morphology { radius, .. } => radius.0.max(radius.1),
            Self::DisplacementMap { scale, .. } => scale.0.abs().max(scale.1.abs()) / 2.0,
            _ => 0.0,
        }
        .max(0.0)
    }
}

/// How far (local px) a CSS filter list can move content.
pub fn filters_reach(filters: &[CssFilter]) -> f32 {
    filters
        .iter()
        .map(|filter| match filter {
            CssFilter::Blur(deviation) => 3.0 * deviation.max(0.0),
            CssFilter::DropShadow {
                dx,
                dy,
                std_deviation,
                ..
            } => 3.0 * std_deviation.max(0.0) + dx.abs().max(dy.abs()),
            CssFilter::Graph(graph) => graph.reach(),
            CssFilter::ColorMatrix(_) | CssFilter::Reference(_) => 0.0,
        })
        .sum()
}

/// A premultiplied RGBA image. Coordinates are buffer pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 4]>,
}

impl FilterImage {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![[0.0; 4]; width * height],
        }
    }

    /// From premultiplied RGBA8.
    pub fn from_premultiplied_rgba8(width: usize, height: usize, rgba: &[u8]) -> Self {
        let data = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .take(width * height)
            .map(|pixel| pixel.map(|c| f32::from(c) / 255.0))
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    /// To premultiplied RGBA8, keeping every color at or below its alpha.
    pub fn to_premultiplied_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for pixel in &self.data {
            let alpha = (pixel[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            for &c in &pixel[..3] {
                out.push(((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8).min(alpha));
            }
            out.push(alpha);
        }
        out
    }

    fn full(&self) -> PixelRect {
        PixelRect {
            x0: 0,
            y0: 0,
            x1: self.width as i64,
            y1: self.height as i64,
        }
    }

    fn get(&self, x: i64, y: i64) -> [f32; 4] {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            [0.0; 4]
        } else {
            self.data[y as usize * self.width + x as usize]
        }
    }

    /// Zero every pixel outside `rect`.
    fn clip_to(&mut self, rect: PixelRect) {
        let rect = rect.intersect(self.full());
        for y in 0..self.height {
            let row = &mut self.data[y * self.width..(y + 1) * self.width];
            let inside_row = (y as i64) >= rect.y0 && (y as i64) < rect.y1;
            for (x, pixel) in row.iter_mut().enumerate() {
                if !inside_row || (x as i64) < rect.x0 || (x as i64) >= rect.x1 {
                    *pixel = [0.0; 4];
                }
            }
        }
    }
}

/// An integer pixel rectangle `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

impl PixelRect {
    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    fn width(self) -> i64 {
        (self.x1 - self.x0).max(0)
    }

    fn height(self) -> i64 {
        (self.y1 - self.y0).max(0)
    }
}

/// Maps the filtered element's local CSS px to buffer pixels: the
/// #FilterPrimitivesOverviewIntro operating coordinate space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelSpace {
    pub to_pixel: Affine2d,
}

impl PixelSpace {
    /// Buffer pixels per local px along each local axis.
    fn scale(&self) -> (f32, f32) {
        let [a, b, c, d, ..] = self.to_pixel.0;
        (a.hypot(b).max(f32::EPSILON), c.hypot(d).max(f32::EPSILON))
    }

    /// A local offset as a buffer offset.
    fn vector(&self, dx: f32, dy: f32) -> (f32, f32) {
        let [a, b, c, d, ..] = self.to_pixel.0;
        (a * dx + c * dy, b * dx + d * dy)
    }

    /// The pixels a local rectangle touches (#FilterPrimitiveSubRegion:
    /// offscreens include every pixel that partly intersects a region).
    fn rect(&self, rect: CssRect) -> PixelRect {
        if !(rect.width > 0.0 && rect.height > 0.0) {
            return PixelRect {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            };
        }
        let bounds = super::vello_cpu::transformed_bounds(rect, self.to_pixel);
        let clamp = |value: f32| value.clamp(-1.0e9, 1.0e9) as i64;
        PixelRect {
            x0: clamp((bounds.x + 1.0e-3).floor()),
            y0: clamp((bounds.y + 1.0e-3).floor()),
            x1: clamp((bounds.x + bounds.width - 1.0e-3).ceil()),
            y1: clamp((bounds.y + bounds.height - 1.0e-3).ceil()),
        }
    }

    /// The local point at the center of buffer pixel (x, y).
    fn local_point(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let inverse = self.to_pixel.inverse()?;
        let point = inverse.map_point(crate::core::CssPoint::new(x + 0.5, y + 0.5));
        Some((point.x, point.y))
    }
}

/// Evaluation context beyond the source pixels.
#[derive(Clone, Copy, Default)]
pub struct FilterOptions<'a> {
    /// Filter Effects 2 #BackdropFilterProperty: blur() samples mirrored
    /// pixels at the edge of the clipped border box instead of transparency.
    pub blur_edge: EdgeMode,
    /// The isolation group's backdrop (#BackgroundImage), when known.
    pub background: Option<&'a FilterImage>,
    /// The images `feImage` primitives draw; without it they are
    /// transparent black, as for an image that has not loaded.
    pub images: Option<&'a dyn FilterImageSource>,
}

/// Apply a CSS `<filter-value-list>` in order (Filter Effects 1
/// #FilterProperty): each function or reference takes the previous output
/// as its input image; a referenced `filter` element still receives that
/// output as its SourceGraphic.
pub fn apply_filters(
    image: &mut FilterImage,
    filters: &[CssFilter],
    space: &PixelSpace,
    options: FilterOptions<'_>,
) {
    let full = image.full();
    for filter in filters {
        match filter {
            CssFilter::ColorMatrix(matrix) => color_matrix(image, full, matrix),
            CssFilter::Blur(deviation) => {
                let (sx, sy) = space.scale();
                let blurred = gaussian_blur(
                    image,
                    full,
                    full,
                    (deviation * sx, deviation * sy),
                    options.blur_edge,
                );
                *image = blurred;
            }
            CssFilter::DropShadow {
                dx,
                dy,
                std_deviation,
                color,
            } => {
                // #dropshadowEquivalent: feDropShadow in sRGB with
                // edgeMode none (#filter-functions operate in sRGB).
                let (sx, sy) = space.scale();
                let offset = space.vector(*dx, *dy);
                *image = drop_shadow(
                    image,
                    full,
                    offset,
                    (std_deviation * sx, std_deviation * sy),
                    paint_color(*color),
                    false,
                );
            }
            CssFilter::Graph(graph) => {
                *image = evaluate_graph(graph, image, space, options);
            }
            // Graphical paint resolves references before rasterization.
            CssFilter::Reference(_) => {}
        }
    }
}

/// Straight sRGB components of a paint color.
pub fn paint_color(color: PaintColor) -> [f32; 4] {
    match color {
        PaintColor::Rgba(r, g, b, a) => [r, g, b, a].map(|c| f32::from(c) / 255.0),
        _ => [0.0, 0.0, 0.0, 1.0],
    }
}

/// One evaluated primitive result.
struct Evaluated {
    image: FilterImage,
    linear: bool,
    /// The primitive subregion in buffer pixels (feTile's tile).
    subregion: PixelRect,
}

/// #FilterPrimitiveTree: evaluate the primary tree of `graph` with `source`
/// as SourceGraphic, returning its sRGB result clipped to the filter region.
fn evaluate_graph(
    graph: &FilterGraph,
    source: &FilterImage,
    space: &PixelSpace,
    options: FilterOptions<'_>,
) -> FilterImage {
    let full = source.full();
    let region = space.rect(graph.region).intersect(full);
    let mut out = FilterImage::new(source.width, source.height);
    // #FilterPrimitiveTree: a filter without primitives renders nothing.
    let Some(root) = graph.primitives.len().checked_sub(1) else {
        return out;
    };
    if region.is_empty() {
        return out;
    }
    // Only the primary tree contributes; collect its primitives.
    let mut needed = vec![false; graph.primitives.len()];
    needed[root] = true;
    let mut count = 0usize;
    for index in (0..graph.primitives.len()).rev() {
        if !needed[index] {
            continue;
        }
        count += 1;
        if count > MAX_PRIMITIVES {
            needed[index] = false;
            continue;
        }
        for input in graph.primitives[index].kind.inputs() {
            if let FilterInput::Result(previous) = input
                && previous < index
            {
                needed[previous] = true;
            }
        }
    }
    // The last primitive that reads each result.
    let mut last_use = vec![0usize; graph.primitives.len()];
    for (index, primitive) in graph.primitives.iter().enumerate() {
        if !needed[index] {
            continue;
        }
        for input in primitive.kind.inputs() {
            if let FilterInput::Result(previous) = input
                && previous < index
            {
                last_use[previous] = last_use[previous].max(index);
            }
        }
    }
    // #FilterPrimitiveSubRegion: the filter region clips the inputs.
    let mut clipped_source = source.clone();
    clipped_source.clip_to(region);
    let standard = |input: FilterInput| -> Option<FilterImage> {
        let mut image = match input {
            FilterInput::SourceGraphic => clipped_source.clone(),
            FilterInput::SourceAlpha => alpha_only(&clipped_source),
            FilterInput::BackgroundImage => options
                .background
                .cloned()
                .unwrap_or_else(|| FilterImage::new(source.width, source.height)),
            FilterInput::BackgroundAlpha => options
                .background
                .map_or_else(|| FilterImage::new(source.width, source.height), alpha_only),
            FilterInput::FillPaint | FilterInput::StrokePaint => {
                let color = if input == FilterInput::FillPaint {
                    graph.fill_paint
                } else {
                    graph.stroke_paint
                };
                let mut image = FilterImage::new(source.width, source.height);
                let pixel = premultiply(color);
                image.data.fill(pixel);
                image
            }
            FilterInput::Result(_) => return None,
        };
        image.clip_to(region);
        Some(image)
    };
    let mut results: Vec<Option<Evaluated>> = (0..graph.primitives.len()).map(|_| None).collect();
    for (index, primitive) in graph.primitives.iter().enumerate() {
        if !needed[index] {
            continue;
        }
        let subregion = space.rect(primitive.subregion).intersect(region);
        let linear = primitive.linear_rgb;
        // Fetch an input converted into this primitive's color space.
        let input =
            |input: FilterInput, results: &[Option<Evaluated>]| -> (FilterImage, PixelRect) {
                let (mut image, image_linear, extent) = match input {
                    FilterInput::Result(previous) if previous < index => match &results[previous] {
                        Some(result) => (result.image.clone(), result.linear, result.subregion),
                        None => (
                            FilterImage::new(source.width, source.height),
                            linear,
                            subregion,
                        ),
                    },
                    FilterInput::Result(_) => (
                        FilterImage::new(source.width, source.height),
                        linear,
                        subregion,
                    ),
                    standard_input => (
                        standard(standard_input)
                            .unwrap_or_else(|| FilterImage::new(source.width, source.height)),
                        false,
                        region,
                    ),
                };
                if image_linear != linear {
                    convert_space(&mut image, linear);
                }
                (image, extent)
            };
        let mut result = if subregion.is_empty() {
            FilterImage::new(source.width, source.height)
        } else {
            evaluate_primitive(
                &primitive.kind,
                &|i| input(i, &results),
                graph,
                space,
                subregion,
                linear,
                (source.width, source.height),
                options.images,
            )
        };
        // #FilterPrimitiveSubRegion: the subregion clips the result.
        result.clip_to(subregion);
        results[index] = Some(Evaluated {
            image: result,
            linear,
            subregion,
        });
        for (previous, slot) in results.iter_mut().enumerate().take(index) {
            if needed[previous] && last_use[previous] <= index && previous != root {
                *slot = None;
            }
        }
    }
    if let Some(Evaluated {
        mut image, linear, ..
    }) = results[root].take()
    {
        if linear {
            convert_space(&mut image, false);
        }
        image.clip_to(region);
        out = image;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn evaluate_primitive(
    kind: &FilterPrimitiveKind,
    input: &dyn Fn(FilterInput) -> (FilterImage, PixelRect),
    graph: &FilterGraph,
    space: &PixelSpace,
    subregion: PixelRect,
    linear: bool,
    size: (usize, usize),
    images: Option<&dyn FilterImageSource>,
) -> FilterImage {
    let (sx, sy) = space.scale();
    match kind {
        FilterPrimitiveKind::Blend {
            input: a,
            input2: b,
            mode,
            composite,
        } => {
            let (source, _) = input(*a);
            let (backdrop, _) = input(*b);
            blend(&source, &backdrop, subregion, *mode, *composite)
        }
        FilterPrimitiveKind::ColorMatrix { input: a, matrix } => {
            let (mut image, _) = input(*a);
            if let Some(matrix) = matrix {
                color_matrix(&mut image, subregion, matrix);
            }
            image
        }
        FilterPrimitiveKind::ComponentTransfer {
            input: a,
            functions,
        } => {
            let (mut image, _) = input(*a);
            component_transfer(&mut image, subregion, functions);
            image
        }
        FilterPrimitiveKind::Composite {
            input: a,
            input2: b,
            operator,
        } => {
            let (source, _) = input(*a);
            let (destination, _) = input(*b);
            composite(&source, &destination, subregion, *operator)
        }
        FilterPrimitiveKind::ConvolveMatrix { input: a, matrix } => {
            let (image, extent) = input(*a);
            convolve(&image, extent, subregion, matrix)
        }
        FilterPrimitiveKind::DisplacementMap {
            input: a,
            input2: b,
            scale,
            channels,
        } => {
            let (image, _) = input(*a);
            let (map, _) = input(*b);
            displacement_map(&image, &map, subregion, space, *scale, *channels, linear)
        }
        FilterPrimitiveKind::DropShadow {
            input: a,
            dx,
            dy,
            std_deviation,
            color,
        } => {
            let (image, _) = input(*a);
            let mut color = *color;
            if linear {
                color = to_linear_color(color);
            }
            drop_shadow(
                &image,
                subregion,
                space.vector(*dx, *dy),
                (std_deviation.0 * sx, std_deviation.1 * sy),
                color,
                true,
            )
        }
        FilterPrimitiveKind::Flood { color } => {
            let mut image = FilterImage::new(size.0, size.1);
            let color = if linear {
                to_linear_color(*color)
            } else {
                *color
            };
            let pixel = premultiply(color);
            for y in subregion.y0..subregion.y1 {
                for x in subregion.x0..subregion.x1 {
                    image.data[y as usize * size.0 + x as usize] = pixel;
                }
            }
            image
        }
        FilterPrimitiveKind::GaussianBlur {
            input: a,
            std_deviation,
            edge,
        } => {
            let (image, extent) = input(*a);
            gaussian_blur(
                &image,
                extent,
                subregion,
                (std_deviation.0 * sx, std_deviation.1 * sy),
                *edge,
            )
        }
        FilterPrimitiveKind::Lighting { input: a, lighting } => {
            let (image, _) = input(*a);
            light(&image, subregion, space, lighting, linear)
        }
        FilterPrimitiveKind::Merge { inputs } => {
            let mut out = FilterImage::new(size.0, size.1);
            for &node in inputs {
                let (layer, _) = input(node);
                for (dst, src) in out.data.iter_mut().zip(&layer.data) {
                    let inverse = 1.0 - src[3];
                    for c in 0..4 {
                        dst[c] = src[c] + dst[c] * inverse;
                    }
                }
            }
            out
        }
        FilterPrimitiveKind::Morphology {
            input: a,
            dilate,
            radius,
        } => {
            let (image, _) = input(*a);
            // A negative or zero radius disables the effect: the result is
            // the input image.
            if radius.0 <= 0.0 || radius.1 <= 0.0 {
                return image;
            }
            morphology(&image, subregion, (radius.0 * sx, radius.1 * sy), *dilate)
        }
        FilterPrimitiveKind::Offset { input: a, dx, dy } => {
            let (image, _) = input(*a);
            offset(&image, space.vector(*dx, *dy))
        }
        FilterPrimitiveKind::Tile { input: a } => {
            let (image, tile) = input(*a);
            tile_image(&image, tile, subregion)
        }
        FilterPrimitiveKind::Turbulence(turbulence) => {
            turbulence_image(turbulence, graph, space, subregion, size)
        }
        FilterPrimitiveKind::Image(image) => {
            let mut result = FilterImage::new(size.0, size.1);
            if let Some(images) = images {
                draw_image(&mut result, image, images, space, subregion);
            }
            // The image's colors are sRGB.
            if linear {
                convert_space(&mut result, true);
            }
            result
        }
        FilterPrimitiveKind::Transparent => FilterImage::new(size.0, size.1),
    }
}

/// #feImageElement: draw `image` as an `image` element whose viewport is
/// `image.viewport` (SVG 2 #ImageElement: placed by preserveAspectRatio and
/// clipped to the viewport), resampled bilinearly into the buffer pixels of
/// `subregion`.
fn draw_image(
    out: &mut FilterImage,
    image: &FeImage,
    images: &dyn FilterImageSource,
    space: &PixelSpace,
    subregion: PixelRect,
) {
    let Some((width, height)) = images.natural_size(image.handle) else {
        return;
    };
    // An empty image fills the subregion with transparent black.
    let placed = image
        .aspect
        .place((width as f32, height as f32), image.viewport);
    let Some(visible) = super::intersect_css_rect(placed, image.viewport) else {
        return;
    };
    let Some(inverse) = space.to_pixel.inverse() else {
        return;
    };
    let Some(pixels) = images.pixels(image.handle, placed, space.to_pixel) else {
        return;
    };
    let (iw, ih) = (pixels.width as usize, pixels.height as usize);
    if iw == 0 || ih == 0 || pixels.rgba.len() != iw * ih * 4 {
        return;
    }
    let texel = |x: usize, y: usize| -> [f32; 4] {
        let at = (y * iw + x) * 4;
        let [r, g, b, a] = [0, 1, 2, 3].map(|c| f32::from(pixels.rgba[at + c]) / 255.0);
        [r * a, g * a, b * a, a]
    };
    let area = space
        .rect(visible)
        .intersect(subregion)
        .intersect(out.full());
    for y in area.y0..area.y1 {
        for x in area.x0..area.x1 {
            let point =
                inverse.map_point(crate::core::CssPoint::new(x as f32 + 0.5, y as f32 + 0.5));
            if point.x < visible.x
                || point.y < visible.y
                || point.x >= visible.x + visible.width
                || point.y >= visible.y + visible.height
            {
                continue;
            }
            // Texel centers sit at half-integers; clamp at the image edge.
            let u =
                ((point.x - placed.x) / placed.width * iw as f32 - 0.5).clamp(0.0, (iw - 1) as f32);
            let v = ((point.y - placed.y) / placed.height * ih as f32 - 0.5)
                .clamp(0.0, (ih - 1) as f32);
            let (x0, y0) = (u.floor() as usize, v.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(iw - 1), (y0 + 1).min(ih - 1));
            let (fx, fy) = (u - x0 as f32, v - y0 as f32);
            let [a, b, c, d] = [texel(x0, y0), texel(x1, y0), texel(x0, y1), texel(x1, y1)];
            let mut pixel = [0.0; 4];
            for channel in 0..4 {
                let top = a[channel] + (b[channel] - a[channel]) * fx;
                let bottom = c[channel] + (d[channel] - c[channel]) * fx;
                pixel[channel] = top + (bottom - top) * fy;
            }
            out.data[y as usize * out.width + x as usize] = pixel;
        }
    }
}

fn premultiply(color: [f32; 4]) -> [f32; 4] {
    let a = color[3].clamp(0.0, 1.0);
    [color[0] * a, color[1] * a, color[2] * a, a]
}

fn alpha_only(image: &FilterImage) -> FilterImage {
    FilterImage {
        width: image.width,
        height: image.height,
        data: image.data.iter().map(|p| [0.0, 0.0, 0.0, p[3]]).collect(),
    }
}

/// CSS Color 4 #color-conversion-code: the sRGB transfer function.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn to_linear_color(color: [f32; 4]) -> [f32; 4] {
    [
        srgb_to_linear(color[0]),
        srgb_to_linear(color[1]),
        srgb_to_linear(color[2]),
        color[3],
    ]
}

/// Convert premultiplied pixels between sRGB and linear-light sRGB.
fn convert_space(image: &mut FilterImage, to_linear: bool) {
    let convert = if to_linear {
        srgb_to_linear
    } else {
        linear_to_srgb
    };
    for pixel in &mut image.data {
        let a = pixel[3];
        if a <= 0.0 {
            *pixel = [0.0; 4];
            continue;
        }
        for c in &mut pixel[..3] {
            *c = convert((*c / a).clamp(0.0, 1.0)) * a;
        }
    }
}

/// #feColorMatrixElement on non-premultiplied values, clamped.
fn color_matrix(image: &mut FilterImage, rect: PixelRect, matrix: &[f32; 20]) {
    let rect = rect.intersect(image.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let pixel = &mut image.data[y as usize * image.width + x as usize];
            let a = pixel[3];
            let straight = if a > 0.0 {
                [pixel[0] / a, pixel[1] / a, pixel[2] / a, a]
            } else {
                [0.0; 4]
            };
            let out: [f32; 4] = std::array::from_fn(|row| {
                let m = &matrix[row * 5..row * 5 + 5];
                (m[0] * straight[0]
                    + m[1] * straight[1]
                    + m[2] * straight[2]
                    + m[3] * straight[3]
                    + m[4])
                    .clamp(0.0, 1.0)
            });
            *pixel = premultiply(out);
        }
    }
}

fn component_transfer(image: &mut FilterImage, rect: PixelRect, functions: &[TransferFunction; 4]) {
    let rect = rect.intersect(image.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let pixel = &mut image.data[y as usize * image.width + x as usize];
            let a = pixel[3];
            let straight = if a > 0.0 {
                [pixel[0] / a, pixel[1] / a, pixel[2] / a, a]
            } else {
                [0.0; 4]
            };
            let out: [f32; 4] =
                std::array::from_fn(|c| functions[c].apply(straight[c]).clamp(0.0, 1.0));
            *pixel = premultiply(out);
        }
    }
}

/// Compositing 1 #porterduffcompositingoperators on premultiplied pixels,
/// and #feCompositeElement's arithmetic operator.
fn composite(
    source: &FilterImage,
    destination: &FilterImage,
    rect: PixelRect,
    operator: CompositeOp,
) -> FilterImage {
    let mut out = FilterImage::new(source.width, source.height);
    let rect = rect.intersect(source.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let index = y as usize * source.width + x as usize;
            let (s, d) = (source.data[index], destination.data[index]);
            let (sa, da) = (s[3], d[3]);
            out.data[index] = match operator {
                CompositeOp::Over => std::array::from_fn(|c| s[c] + d[c] * (1.0 - sa)),
                CompositeOp::In => std::array::from_fn(|c| s[c] * da),
                CompositeOp::Out => std::array::from_fn(|c| s[c] * (1.0 - da)),
                CompositeOp::Atop => std::array::from_fn(|c| s[c] * da + d[c] * (1.0 - sa)),
                CompositeOp::Xor => std::array::from_fn(|c| s[c] * (1.0 - da) + d[c] * (1.0 - sa)),
                CompositeOp::Lighter => std::array::from_fn(|c| (s[c] + d[c]).min(1.0)),
                CompositeOp::Arithmetic { k1, k2, k3, k4 } => {
                    let alpha = (k1 * sa * da + k2 * sa + k3 * da + k4).clamp(0.0, 1.0);
                    let mut pixel: [f32; 4] = std::array::from_fn(|c| {
                        (k1 * s[c] * d[c] + k2 * s[c] + k3 * d[c] + k4).clamp(0.0, 1.0)
                    });
                    // A premultiplied color never exceeds its alpha.
                    pixel[3] = alpha;
                    for c in &mut pixel[..3] {
                        *c = c.min(alpha);
                    }
                    pixel
                }
            };
        }
    }
    out
}

/// Compositing 1 #blending: B(Cb, Cs) on straight colors.
fn blend_channels(mode: BlendMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    fn separable(mode: BlendMode, cb: f32, cs: f32) -> f32 {
        let multiply = |cb: f32, cs: f32| cb * cs;
        let screen = |cb: f32, cs: f32| cb + cs - cb * cs;
        let hard_light = |cb: f32, cs: f32| {
            if cs <= 0.5 {
                multiply(cb, 2.0 * cs)
            } else {
                screen(cb, 2.0 * cs - 1.0)
            }
        };
        match mode {
            BlendMode::Multiply => multiply(cb, cs),
            BlendMode::Screen => screen(cb, cs),
            BlendMode::Overlay => hard_light(cs, cb),
            BlendMode::Darken => cb.min(cs),
            BlendMode::Lighten => cb.max(cs),
            BlendMode::ColorDodge => {
                if cb == 0.0 {
                    0.0
                } else if cs >= 1.0 {
                    1.0
                } else {
                    (cb / (1.0 - cs)).min(1.0)
                }
            }
            BlendMode::ColorBurn => {
                if cb >= 1.0 {
                    1.0
                } else if cs <= 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - cb) / cs).min(1.0)
                }
            }
            BlendMode::HardLight => hard_light(cb, cs),
            BlendMode::SoftLight => {
                if cs <= 0.5 {
                    cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
                } else {
                    let d = if cb <= 0.25 {
                        ((16.0 * cb - 12.0) * cb + 4.0) * cb
                    } else {
                        cb.sqrt()
                    };
                    cb + (2.0 * cs - 1.0) * (d - cb)
                }
            }
            BlendMode::Difference => (cb - cs).abs(),
            BlendMode::Exclusion => cb + cs - 2.0 * cb * cs,
            _ => cs,
        }
    }
    fn lum(c: [f32; 3]) -> f32 {
        0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
    }
    fn clip_color(c: [f32; 3]) -> [f32; 3] {
        let l = lum(c);
        let n = c[0].min(c[1]).min(c[2]);
        let x = c[0].max(c[1]).max(c[2]);
        let mut c = c;
        if n < 0.0 && l - n > 0.0 {
            c = c.map(|v| l + (v - l) * l / (l - n));
        }
        if x > 1.0 && x - l > 0.0 {
            c = c.map(|v| l + (v - l) * (1.0 - l) / (x - l));
        }
        c
    }
    fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
        let d = l - lum(c);
        clip_color(c.map(|v| v + d))
    }
    fn sat(c: [f32; 3]) -> f32 {
        c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
    }
    fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
        let mut order = [0usize, 1, 2];
        order.sort_by(|&a, &b| c[a].total_cmp(&c[b]));
        let (min, mid, max) = (order[0], order[1], order[2]);
        let mut out = [0.0; 3];
        if c[max] > c[min] {
            out[mid] = (c[mid] - c[min]) * s / (c[max] - c[min]);
            out[max] = s;
        }
        out
    }
    match mode {
        BlendMode::Hue => set_lum(set_sat(cs, sat(cb)), lum(cb)),
        BlendMode::Saturation => set_lum(set_sat(cb, sat(cs)), lum(cb)),
        BlendMode::Color => set_lum(cs, lum(cb)),
        BlendMode::Luminosity => set_lum(cb, lum(cs)),
        _ => std::array::from_fn(|i| separable(mode, cb[i], cs[i]).clamp(0.0, 1.0)),
    }
}

/// #feBlendElement: blend `source` onto `backdrop` and composite with
/// source-over, or (`no-composite`) only mix.
fn blend(
    source: &FilterImage,
    backdrop: &FilterImage,
    rect: PixelRect,
    mode: BlendMode,
    composite: bool,
) -> FilterImage {
    let mut out = FilterImage::new(source.width, source.height);
    let rect = rect.intersect(source.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let index = y as usize * source.width + x as usize;
            let (s, b) = (source.data[index], backdrop.data[index]);
            let (sa, ba) = (s[3], b[3]);
            let cs = if sa > 0.0 {
                [s[0] / sa, s[1] / sa, s[2] / sa]
            } else {
                [0.0; 3]
            };
            let cb = if ba > 0.0 {
                [b[0] / ba, b[1] / ba, b[2] / ba]
            } else {
                [0.0; 3]
            };
            let mixed = if mode == BlendMode::PlusLighter {
                cs
            } else {
                blend_channels(mode, cb, cs)
            };
            // Cr = (1 - αb) x Cs + αb x B(Cb, Cs)
            let cr: [f32; 3] = std::array::from_fn(|c| (1.0 - ba) * cs[c] + ba * mixed[c]);
            out.data[index] = if !composite {
                [cr[0] * sa, cr[1] * sa, cr[2] * sa, sa]
            } else if mode == BlendMode::PlusLighter {
                std::array::from_fn(|c| (s[c] + b[c]).min(1.0))
            } else {
                // Source-over with the blended source color.
                let alpha = sa + ba * (1.0 - sa);
                [
                    sa * cr[0] + b[0] * (1.0 - sa),
                    sa * cr[1] + b[1] * (1.0 - sa),
                    sa * cr[2] + b[2] * (1.0 - sa),
                    alpha,
                ]
            };
        }
    }
    out
}

/// Sample `image` at (x, y) with `edge` applied relative to `extent`.
fn sample_edge(image: &FilterImage, extent: PixelRect, x: i64, y: i64, edge: EdgeMode) -> [f32; 4] {
    let resolve = |v: i64, lo: i64, hi: i64| -> Option<i64> {
        if hi <= lo {
            return None;
        }
        if v >= lo && v < hi {
            return Some(v);
        }
        let len = hi - lo;
        match edge {
            EdgeMode::None => None,
            EdgeMode::Duplicate => Some(v.clamp(lo, hi - 1)),
            EdgeMode::Wrap => Some(lo + (v - lo).rem_euclid(len)),
            EdgeMode::Mirror => {
                let period = 2 * len;
                let m = (v - lo).rem_euclid(period);
                Some(lo + if m < len { m } else { period - 1 - m })
            }
        }
    };
    match (
        resolve(x, extent.x0, extent.x1),
        resolve(y, extent.y0, extent.y1),
    ) {
        (Some(x), Some(y)) => image.get(x, y),
        _ => [0.0; 4],
    }
}

/// #feGaussianBlurElement: a separable Gaussian for small deviations and
/// the three-box-blur approximation for s >= 2.0, sampling outside
/// `extent` with `edge`. Each axis with a zero deviation is left unblurred;
/// with both zero (or negative) the result is the input image.
fn gaussian_blur(
    image: &FilterImage,
    extent: PixelRect,
    output: PixelRect,
    deviation: (f32, f32),
    edge: EdgeMode,
) -> FilterImage {
    let sanitize = |s: f32| {
        if s.is_finite() && s > 0.0 {
            s.min(MAX_KERNEL_PX)
        } else {
            0.0
        }
    };
    let (sx, sy) = (sanitize(deviation.0), sanitize(deviation.1));
    if sx == 0.0 && sy == 0.0 {
        return image.clone();
    }
    // Pixels outside the extent come from the edge mode; materialize the
    // padded extent once so each pass reads a plain buffer.
    let reach = |s: f32| (s * 3.0).ceil() as i64 + 2;
    let (rx, ry) = (reach(sx), reach(sy));
    let full = image.full();
    let work = PixelRect {
        x0: output.x0.min(extent.x0) - rx,
        y0: output.y0.min(extent.y0) - ry,
        x1: output.x1.max(extent.x1) + rx,
        y1: output.y1.max(extent.y1) + ry,
    }
    .intersect(PixelRect {
        x0: full.x0 - rx,
        y0: full.y0 - ry,
        x1: full.x1 + rx,
        y1: full.y1 + ry,
    });
    let (w, h) = (work.width() as usize, work.height() as usize);
    if w == 0 || h == 0 {
        return FilterImage::new(image.width, image.height);
    }
    let mut buffer = FilterImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            buffer.data[y * w + x] =
                sample_edge(image, extent, work.x0 + x as i64, work.y0 + y as i64, edge);
        }
    }
    if sx > 0.0 {
        blur_axis(&mut buffer, sx, true);
    }
    if sy > 0.0 {
        blur_axis(&mut buffer, sy, false);
    }
    let mut out = FilterImage::new(image.width, image.height);
    let copy = output.intersect(full).intersect(work);
    for y in copy.y0..copy.y1 {
        for x in copy.x0..copy.x1 {
            out.data[y as usize * image.width + x as usize] =
                buffer.data[(y - work.y0) as usize * w + (x - work.x0) as usize];
        }
    }
    out
}

/// Blur every row (`horizontal`) or column of `image` in place.
fn blur_axis(image: &mut FilterImage, s: f32, horizontal: bool) {
    let (len, lines) = if horizontal {
        (image.width, image.height)
    } else {
        (image.height, image.width)
    };
    let mut line = vec![[0.0f32; 4]; len];
    let mut scratch = vec![[0.0f32; 4]; len];
    let index = |line: usize, i: usize, width: usize| {
        if horizontal {
            line * width + i
        } else {
            i * width + line
        }
    };
    let width = image.width;
    // #feGaussianBlurElement's box approximation: three box blurs of size
    // d (odd d), or two of size d offset by half a pixel either way and one
    // of size d + 1 (even d).
    let boxes: Vec<(usize, usize)> = if s >= 2.0 {
        let d = (s * 3.0 * (2.0 * std::f32::consts::PI).sqrt() / 4.0 + 0.5).floor() as usize;
        let d = d.max(1);
        if d % 2 == 1 {
            vec![((d - 1) / 2, (d - 1) / 2); 3]
        } else {
            vec![(d / 2, d / 2 - 1), (d / 2 - 1, d / 2), (d / 2, d / 2)]
        }
    } else {
        Vec::new()
    };
    let kernel: Vec<f32> = if boxes.is_empty() {
        let radius = (s * 3.0).ceil() as i64;
        let weights: Vec<f32> = (-radius..=radius)
            .map(|i| (-(i * i) as f32 / (2.0 * s * s)).exp())
            .collect();
        let sum: f32 = weights.iter().sum();
        weights.into_iter().map(|w| w / sum).collect()
    } else {
        Vec::new()
    };
    for l in 0..lines {
        for (i, slot) in line.iter_mut().enumerate() {
            *slot = image.data[index(l, i, width)];
        }
        if boxes.is_empty() {
            let radius = (kernel.len() / 2) as i64;
            for (i, out) in scratch.iter_mut().enumerate() {
                let mut sum = [0.0f32; 4];
                for (k, weight) in kernel.iter().enumerate() {
                    let j = i as i64 + k as i64 - radius;
                    if j >= 0 && (j as usize) < len {
                        let p = line[j as usize];
                        for c in 0..4 {
                            sum[c] += p[c] * weight;
                        }
                    }
                }
                *out = sum;
            }
            std::mem::swap(&mut line, &mut scratch);
        } else {
            for &(left, right) in &boxes {
                box_blur(&line, &mut scratch, left, right);
                std::mem::swap(&mut line, &mut scratch);
            }
        }
        for (i, value) in line.iter().enumerate() {
            image.data[index(l, i, width)] = *value;
        }
    }
}

/// A box blur covering [i - left, i + right], with zeros beyond the line.
fn box_blur(input: &[[f32; 4]], output: &mut [[f32; 4]], left: usize, right: usize) {
    let len = input.len();
    let size = (left + right + 1) as f32;
    let mut sum = [0.0f64; 4];
    // Window for i = 0: [-left, right].
    for p in input.iter().take((right + 1).min(len)) {
        for c in 0..4 {
            sum[c] += f64::from(p[c]);
        }
    }
    for (i, slot) in output.iter_mut().enumerate().take(len) {
        *slot = sum.map(|v| (v / f64::from(size)) as f32);
        let add = i + right + 1;
        if add < len {
            for c in 0..4 {
                sum[c] += f64::from(input[add][c]);
            }
        }
        if i >= left {
            let remove = i - left;
            for c in 0..4 {
                sum[c] -= f64::from(input[remove][c]);
            }
        }
    }
}

/// #feDropShadowElement: the input's alpha blurred, offset and flooded with
/// `color` (already in the working space), merged beneath the input.
fn drop_shadow(
    image: &FilterImage,
    rect: PixelRect,
    offset_px: (f32, f32),
    deviation: (f32, f32),
    color: [f32; 4],
    _primitive: bool,
) -> FilterImage {
    let alpha = alpha_only(image);
    let blurred = gaussian_blur(
        &alpha,
        image.full(),
        image.full(),
        deviation,
        EdgeMode::None,
    );
    let shifted = offset(&blurred, offset_px);
    let color = premultiply(color);
    let mut out = FilterImage::new(image.width, image.height);
    let rect = rect.intersect(image.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let index = y as usize * image.width + x as usize;
            let coverage = shifted.data[index][3];
            let shadow: [f32; 4] = std::array::from_fn(|c| color[c] * coverage);
            let s = image.data[index];
            out.data[index] = std::array::from_fn(|c| s[c] + shadow[c] * (1.0 - s[3]));
        }
    }
    out
}

/// #feOffsetElement, bilinearly interpolating fractional offsets.
fn offset(image: &FilterImage, (dx, dy): (f32, f32)) -> FilterImage {
    let mut out = FilterImage::new(image.width, image.height);
    if !dx.is_finite() || !dy.is_finite() {
        return out;
    }
    let (fx, fy) = (dx.floor(), dy.floor());
    let (tx, ty) = (dx - fx, dy - fy);
    let (ix, iy) = (fx as i64, fy as i64);
    for y in 0..image.height as i64 {
        for x in 0..image.width as i64 {
            // out(x, y) = in(x - dx, y - dy)
            let (sx, sy) = (x - ix, y - iy);
            let p00 = image.get(sx, sy);
            let p10 = image.get(sx - 1, sy);
            let p01 = image.get(sx, sy - 1);
            let p11 = image.get(sx - 1, sy - 1);
            out.data[y as usize * image.width + x as usize] = std::array::from_fn(|c| {
                p00[c] * (1.0 - tx) * (1.0 - ty)
                    + p10[c] * tx * (1.0 - ty)
                    + p01[c] * (1.0 - tx) * ty
                    + p11[c] * tx * ty
            });
        }
    }
    out
}

/// #feMorphologyElement: the component-wise maximum (dilate) or minimum
/// (erode) over a (2rx + 1) × (2ry + 1) window, computed separably.
fn morphology(
    image: &FilterImage,
    rect: PixelRect,
    (rx, ry): (f32, f32),
    dilate: bool,
) -> FilterImage {
    let rx = rx.min(MAX_KERNEL_PX).round().max(0.0) as usize;
    let ry = ry.min(MAX_KERNEL_PX).round().max(0.0) as usize;
    let mut work = image.clone();
    let pick = |a: f32, b: f32| if dilate { a.max(b) } else { a.min(b) };
    for horizontal in [true, false] {
        let radius = if horizontal { rx } else { ry };
        if radius == 0 {
            continue;
        }
        let (len, lines) = if horizontal {
            (work.width, work.height)
        } else {
            (work.height, work.width)
        };
        let width = work.width;
        let at = |line: usize, i: usize| {
            if horizontal {
                line * width + i
            } else {
                i * width + line
            }
        };
        let window = 2 * radius + 1;
        let mut prefix = vec![[0.0f32; 4]; len];
        let mut suffix = vec![[0.0f32; 4]; len];
        let mut source = vec![[0.0f32; 4]; len];
        for l in 0..lines {
            for (i, slot) in source.iter_mut().enumerate() {
                *slot = work.data[at(l, i)];
            }
            // van Herk / Gil-Werman: block prefix and suffix extrema.
            for i in 0..len {
                prefix[i] = if i % window == 0 {
                    source[i]
                } else {
                    std::array::from_fn(|c| pick(prefix[i - 1][c], source[i][c]))
                };
            }
            for i in (0..len).rev() {
                suffix[i] = if i % window == window - 1 || i == len - 1 {
                    source[i]
                } else {
                    std::array::from_fn(|c| pick(suffix[i + 1][c], source[i][c]))
                };
            }
            for i in 0..len {
                // Window [i - radius, i + radius]; beyond the image the
                // input is transparent black.
                let lo = i as i64 - radius as i64;
                let hi = i + radius;
                let value: [f32; 4] = if lo < 0 || hi >= len {
                    // Partially outside: transparent black participates.
                    let mut value = [0.0f32; 4];
                    if dilate {
                        let lo = lo.max(0) as usize;
                        let hi = hi.min(len - 1);
                        for p in &source[lo..=hi] {
                            value = std::array::from_fn(|c| value[c].max(p[c]));
                        }
                    }
                    value
                } else {
                    let lo = lo as usize;
                    let a = suffix[lo];
                    let b = prefix[hi];
                    std::array::from_fn(|c| pick(a[c], b[c]))
                };
                work.data[at(l, i)] = value;
            }
        }
    }
    work.clip_to(rect);
    work
}

/// #feTileElement: replicate the input's subregion across `rect`.
fn tile_image(image: &FilterImage, tile: PixelRect, rect: PixelRect) -> FilterImage {
    let mut out = FilterImage::new(image.width, image.height);
    if tile.is_empty() {
        return out;
    }
    let rect = rect.intersect(image.full());
    for y in rect.y0..rect.y1 {
        let ty = tile.y0 + (y - tile.y0).rem_euclid(tile.height());
        for x in rect.x0..rect.x1 {
            let tx = tile.x0 + (x - tile.x0).rem_euclid(tile.width());
            out.data[y as usize * image.width + x as usize] = image.get(tx, ty);
        }
    }
    out
}

/// #feConvolveMatrixElement on premultiplied values (or straight color with
/// `preserveAlpha`), sampling beyond the input's subregion with `edgeMode`.
fn convolve(
    image: &FilterImage,
    extent: PixelRect,
    rect: PixelRect,
    matrix: &ConvolveMatrix,
) -> FilterImage {
    let mut out = FilterImage::new(image.width, image.height);
    let rect = rect.intersect(image.full());
    let (order_x, order_y) = matrix.order;
    let (target_x, target_y) = matrix.target;
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let mut sum = [0.0f32; 4];
            for j in 0..order_y {
                for i in 0..order_x {
                    let sample = sample_edge(
                        image,
                        extent,
                        x - target_x as i64 + i as i64,
                        y - target_y as i64 + j as i64,
                        matrix.edge,
                    );
                    let sample = if matrix.preserve_alpha && sample[3] > 0.0 {
                        [
                            sample[0] / sample[3],
                            sample[1] / sample[3],
                            sample[2] / sample[3],
                            sample[3],
                        ]
                    } else if matrix.preserve_alpha {
                        [0.0; 4]
                    } else {
                        sample
                    };
                    // The kernel is applied rotated 180 degrees.
                    let weight = matrix.kernel[(order_y - 1 - j) * order_x + (order_x - 1 - i)];
                    for c in 0..4 {
                        sum[c] += sample[c] * weight;
                    }
                }
            }
            let index = y as usize * image.width + x as usize;
            out.data[index] = if matrix.preserve_alpha {
                let alpha = image.data[index][3];
                let color: [f32; 3] = std::array::from_fn(|c| {
                    (sum[c] / matrix.divisor + matrix.bias).clamp(0.0, 1.0)
                });
                [color[0] * alpha, color[1] * alpha, color[2] * alpha, alpha]
            } else {
                let alpha = (sum[3] / matrix.divisor + matrix.bias).clamp(0.0, 1.0);
                let mut pixel: [f32; 4] = std::array::from_fn(|c| {
                    (sum[c] / matrix.divisor + matrix.bias * alpha).clamp(0.0, 1.0)
                });
                pixel[3] = alpha;
                for c in &mut pixel[..3] {
                    *c = c.min(alpha);
                }
                pixel
            };
        }
    }
    out
}

/// #feDisplacementMapElement: P'(x,y) ← P(x + scale * (XC(x,y) - .5),
/// y + scale * (YC(x,y) - .5)), with the map's straight channel values.
fn displacement_map(
    image: &FilterImage,
    map: &FilterImage,
    rect: PixelRect,
    space: &PixelSpace,
    scale: (f32, f32),
    channels: (usize, usize),
    _linear: bool,
) -> FilterImage {
    let mut out = FilterImage::new(image.width, image.height);
    let rect = rect.intersect(image.full());
    let (sx, sy) = space.scale();
    let (scale_x, scale_y) = (scale.0 * sx, scale.1 * sy);
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let index = y as usize * image.width + x as usize;
            let m = map.data[index];
            let straight = |c: usize| {
                if c == 3 {
                    m[3]
                } else if m[3] > 0.0 {
                    m[c] / m[3]
                } else {
                    0.0
                }
            };
            let dx = scale_x * (straight(channels.0.min(3)) - 0.5);
            let dy = scale_y * (straight(channels.1.min(3)) - 0.5);
            out.data[index] = image.get(
                (x as f32 + dx).round() as i64,
                (y as f32 + dy).round() as i64,
            );
        }
    }
    out
}

/// #feDiffuseLightingElement / #feSpecularLightingElement with the Sobel
/// surface normals of #feDiffuseLightingElement's table.
fn light(
    image: &FilterImage,
    rect: PixelRect,
    space: &PixelSpace,
    lighting: &Lighting,
    linear: bool,
) -> FilterImage {
    let mut out = FilterImage::new(image.width, image.height);
    let rect = rect.intersect(image.full());
    if rect.is_empty() {
        return out;
    }
    let alpha = |x: i64, y: i64| image.get(x, y)[3];
    let (sx, sy) = space.scale();
    let scale = (sx + sy) / 2.0;
    let mut color = lighting.color;
    if linear {
        color = color.map(srgb_to_linear);
    }
    let surface = lighting.surface_scale;
    let to_pixel = |x: f32, y: f32| {
        let p = space.to_pixel.map_point(crate::core::CssPoint::new(x, y));
        (p.x, p.y)
    };
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            // #SurfaceNormalCalculations: the Sobel kernels, one-sided at the
            // subregion's edges with FACTOR = 2 / (weights × distance).
            let (left, right) = (
                if x > rect.x0 { x - 1 } else { x },
                if x < rect.x1 - 1 { x + 1 } else { x },
            );
            let (top, bottom) = (
                if y > rect.y0 { y - 1 } else { y },
                if y < rect.y1 - 1 { y + 1 } else { y },
            );
            let rows: Vec<(i64, f32)> = [(y - 1, 1.0), (y, 2.0), (y + 1, 1.0)]
                .into_iter()
                .filter(|(row, _)| (rect.y0..rect.y1).contains(row))
                .collect();
            let columns: Vec<(i64, f32)> = [(x - 1, 1.0), (x, 2.0), (x + 1, 1.0)]
                .into_iter()
                .filter(|(column, _)| (rect.x0..rect.x1).contains(column))
                .collect();
            let gradient = |pairs: &[(i64, f32)], lo: i64, hi: i64, horizontal: bool| {
                let distance = (hi - lo) as f32;
                let weights: f32 = pairs.iter().map(|(_, w)| w).sum();
                if distance <= 0.0 || weights <= 0.0 {
                    return 0.0;
                }
                let sum: f32 = pairs
                    .iter()
                    .map(|&(line, weight)| {
                        weight
                            * if horizontal {
                                alpha(hi, line) - alpha(lo, line)
                            } else {
                                alpha(line, hi) - alpha(line, lo)
                            }
                    })
                    .sum();
                -surface * 2.0 / (weights * distance) * sum
            };
            let nx = gradient(&rows, left, right, true);
            let ny = gradient(&columns, top, bottom, false);
            let normal = normalize([nx, ny, 1.0]);
            let z = surface * alpha(x, y);
            let (light_vector, light_color) = match lighting.light {
                LightSource::Distant { azimuth, elevation } => {
                    let (az, el) = (azimuth.to_radians(), elevation.to_radians());
                    ([az.cos() * el.cos(), az.sin() * el.cos(), el.sin()], color)
                }
                LightSource::Point {
                    x: lx,
                    y: ly,
                    z: lz,
                } => {
                    let (px, py) = to_pixel(lx, ly);
                    (
                        normalize([px - x as f32, py - y as f32, lz * scale - z]),
                        color,
                    )
                }
                LightSource::Spot {
                    x: lx,
                    y: ly,
                    z: lz,
                    at,
                    exponent,
                    cone,
                } => {
                    let (px, py) = to_pixel(lx, ly);
                    let l = normalize([px - x as f32, py - y as f32, lz * scale - z]);
                    let (ax, ay) = to_pixel(at.0, at.1);
                    let s = normalize([ax - px, ay - py, (at.2 - lz) * scale]);
                    let minus_l_dot_s = -(l[0] * s[0] + l[1] * s[1] + l[2] * s[2]);
                    let inside = cone.is_none_or(|cone| minus_l_dot_s >= cone.to_radians().cos());
                    let factor = if inside && minus_l_dot_s > 0.0 {
                        minus_l_dot_s.powf(exponent)
                    } else {
                        0.0
                    };
                    (l, color.map(|c| c * factor))
                }
            };
            let index = y as usize * image.width + x as usize;
            out.data[index] = match lighting.specular_exponent {
                None => {
                    let n_dot_l = (normal[0] * light_vector[0]
                        + normal[1] * light_vector[1]
                        + normal[2] * light_vector[2])
                        .max(0.0);
                    let c: [f32; 3] = std::array::from_fn(|i| {
                        (lighting.constant * n_dot_l * light_color[i]).clamp(0.0, 1.0)
                    });
                    [c[0], c[1], c[2], 1.0]
                }
                Some(exponent) => {
                    let h = normalize([light_vector[0], light_vector[1], light_vector[2] + 1.0]);
                    let n_dot_h = (normal[0] * h[0] + normal[1] * h[1] + normal[2] * h[2]).max(0.0);
                    let factor = lighting.constant * n_dot_h.powf(exponent);
                    let c: [f32; 3] =
                        std::array::from_fn(|i| (factor * light_color[i]).clamp(0.0, 1.0));
                    let a = c[0].max(c[1]).max(c[2]);
                    // Straight (c, a) premultiplied: c * a / a … keep c ≤ a.
                    [c[0].min(a), c[1].min(a), c[2].min(a), a]
                }
            };
        }
    }
    out
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length > 0.0 {
        v.map(|c| c / length)
    } else {
        v
    }
}

/// #feTurbulenceElement's reference noise generator, transcribed from the
/// normative C code (Perlin noise with Park–Miller random numbers).
struct PerlinNoise {
    lattice: [usize; 2 * 256 + 2],
    gradient: Box<[[[f64; 2]; 2 * 256 + 2]; 4]>,
}

const RAND_M: i64 = 2_147_483_647;
const RAND_A: i64 = 16807;
const RAND_Q: i64 = 127_773;
const RAND_R: i64 = 2836;
const BSIZE: usize = 0x100;
const BM: i64 = 0xff;
const PERLIN_N: f64 = 4096.0;

fn setup_seed(mut seed: i64) -> i64 {
    if seed <= 0 {
        seed = -(seed % (RAND_M - 1)) + 1;
    }
    if seed > RAND_M - 1 {
        seed = RAND_M - 1;
    }
    seed
}

fn random(seed: i64) -> i64 {
    let mut result = RAND_A * (seed % RAND_Q) - RAND_R * (seed / RAND_Q);
    if result <= 0 {
        result += RAND_M;
    }
    result
}

impl PerlinNoise {
    fn new(seed: i64) -> Self {
        let mut lattice = [0usize; 2 * BSIZE + 2];
        let mut gradient = Box::new([[[0.0f64; 2]; 2 * BSIZE + 2]; 4]);
        let mut seed = setup_seed(seed);
        for channel in gradient.iter_mut() {
            let mut i = 0usize;
            while i < BSIZE {
                lattice[i] = i;
                loop {
                    for value in &mut channel[i] {
                        seed = random(seed);
                        *value = ((seed % (2 * BSIZE as i64)) - BSIZE as i64) as f64 / BSIZE as f64;
                    }
                    if channel[i][0] != 0.0 || channel[i][1] != 0.0 {
                        break;
                    }
                }
                let s = (channel[i][0] * channel[i][0] + channel[i][1] * channel[i][1]).sqrt();
                if s > 1.0 {
                    // Discard the vector and draw it again.
                    continue;
                }
                channel[i][0] /= s;
                channel[i][1] /= s;
                i += 1;
            }
        }
        let mut i = BSIZE - 1;
        while i > 0 {
            let k = lattice[i];
            seed = random(seed);
            let j = (seed % BSIZE as i64) as usize;
            lattice[i] = lattice[j];
            lattice[j] = k;
            i -= 1;
        }
        for i in 0..BSIZE + 2 {
            lattice[BSIZE + i] = lattice[i];
            for channel in gradient.iter_mut() {
                channel[BSIZE + i] = channel[i];
            }
        }
        Self { lattice, gradient }
    }

    fn noise2(&self, channel: usize, vec: [f64; 2], stitch: Option<&Stitch>) -> f64 {
        let t = vec[0] + PERLIN_N;
        let mut bx0 = t as i64;
        let mut bx1 = bx0 + 1;
        let rx0 = t - (t as i64) as f64;
        let rx1 = rx0 - 1.0;
        let t = vec[1] + PERLIN_N;
        let mut by0 = t as i64;
        let mut by1 = by0 + 1;
        let ry0 = t - (t as i64) as f64;
        let ry1 = ry0 - 1.0;
        if let Some(stitch) = stitch {
            if bx0 >= stitch.wrap_x {
                bx0 -= stitch.width;
            }
            if bx1 >= stitch.wrap_x {
                bx1 -= stitch.width;
            }
            if by0 >= stitch.wrap_y {
                by0 -= stitch.height;
            }
            if by1 >= stitch.wrap_y {
                by1 -= stitch.height;
            }
        }
        let (bx0, bx1, by0, by1) = (
            (bx0 & BM) as usize,
            (bx1 & BM) as usize,
            (by0 & BM) as usize,
            (by1 & BM) as usize,
        );
        let i = self.lattice[bx0];
        let j = self.lattice[bx1];
        let b00 = self.lattice[i + by0];
        let b10 = self.lattice[j + by0];
        let b01 = self.lattice[i + by1];
        let b11 = self.lattice[j + by1];
        let s_curve = |t: f64| t * t * (3.0 - 2.0 * t);
        let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
        let sx = s_curve(rx0);
        let sy = s_curve(ry0);
        let g = &self.gradient[channel];
        let u = rx0 * g[b00][0] + ry0 * g[b00][1];
        let v = rx1 * g[b10][0] + ry0 * g[b10][1];
        let a = lerp(sx, u, v);
        let u = rx0 * g[b01][0] + ry1 * g[b01][1];
        let v = rx1 * g[b11][0] + ry1 * g[b11][1];
        let b = lerp(sx, u, v);
        lerp(sy, a, b)
    }

    #[allow(clippy::too_many_arguments)]
    fn turbulence(
        &self,
        channel: usize,
        point: [f64; 2],
        mut base_x: f64,
        mut base_y: f64,
        octaves: u32,
        fractal_sum: bool,
        tile: Option<[f64; 4]>,
    ) -> f64 {
        let mut stitch = None;
        if let Some([tile_x, tile_y, tile_w, tile_h]) = tile {
            if base_x != 0.0 {
                let lo = (tile_w * base_x).floor() / tile_w;
                let hi = (tile_w * base_x).ceil() / tile_w;
                base_x = if base_x / lo < hi / base_x { lo } else { hi };
            }
            if base_y != 0.0 {
                let lo = (tile_h * base_y).floor() / tile_h;
                let hi = (tile_h * base_y).ceil() / tile_h;
                base_y = if base_y / lo < hi / base_y { lo } else { hi };
            }
            let width = (tile_w * base_x + 0.5) as i64;
            let height = (tile_h * base_y + 0.5) as i64;
            stitch = Some(Stitch {
                width,
                height,
                wrap_x: (tile_x * base_x + PERLIN_N + width as f64) as i64,
                wrap_y: (tile_y * base_y + PERLIN_N + height as f64) as i64,
            });
        }
        let mut sum = 0.0;
        let mut vec = [point[0] * base_x, point[1] * base_y];
        let mut ratio = 1.0;
        for _ in 0..octaves {
            let noise = self.noise2(channel, vec, stitch.as_ref());
            sum += if fractal_sum { noise } else { noise.abs() } / ratio;
            vec = [vec[0] * 2.0, vec[1] * 2.0];
            ratio *= 2.0;
            if let Some(stitch) = &mut stitch {
                stitch.width *= 2;
                stitch.wrap_x = 2 * stitch.wrap_x - PERLIN_N as i64;
                stitch.height *= 2;
                stitch.wrap_y = 2 * stitch.wrap_y - PERLIN_N as i64;
            }
        }
        sum
    }
}

struct Stitch {
    width: i64,
    height: i64,
    wrap_x: i64,
    wrap_y: i64,
}

fn turbulence_image(
    turbulence: &Turbulence,
    graph: &FilterGraph,
    space: &PixelSpace,
    rect: PixelRect,
    size: (usize, usize),
) -> FilterImage {
    let mut out = FilterImage::new(size.0, size.1);
    let (fx, fy) = turbulence.base_frequency;
    // Negative frequencies are unsupported (an error: transparent black).
    if !(fx >= 0.0 && fy >= 0.0) {
        return out;
    }
    // #feTurbulenceElement: the seed is truncated toward zero.
    let noise = PerlinNoise::new(turbulence.seed.trunc() as i64);
    let octaves = turbulence.octaves.min(MAX_OCTAVES);
    let (Some(inverse), Some(_)) = (space.to_pixel.inverse(), Some(())) else {
        return out;
    };
    let to_primitive = graph.primitive_space;
    // The stitching tile is the primitive subregion in primitive units.
    let tile = turbulence.stitch.then(|| {
        let local = CssRect::new(
            rect.x0 as f32,
            rect.y0 as f32,
            rect.width() as f32,
            rect.height() as f32,
        );
        let local = super::vello_cpu::transformed_bounds(local, inverse);
        let units = super::vello_cpu::transformed_bounds(local, to_primitive);
        [
            f64::from(units.x),
            f64::from(units.y),
            f64::from(units.width),
            f64::from(units.height),
        ]
    });
    let rect = rect.intersect(out.full());
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let Some((lx, ly)) = space.local_point(x as f32, y as f32) else {
                continue;
            };
            let point = to_primitive.map_point(crate::core::CssPoint::new(lx, ly));
            let point = [f64::from(point.x), f64::from(point.y)];
            let value: [f32; 4] = std::array::from_fn(|channel| {
                let sum = noise.turbulence(
                    channel,
                    point,
                    f64::from(fx),
                    f64::from(fy),
                    octaves,
                    turbulence.fractal_noise,
                    tile,
                );
                let value = if turbulence.fractal_noise {
                    (sum + 1.0) / 2.0
                } else {
                    sum
                };
                value.clamp(0.0, 1.0) as f32
            });
            out.data[y as usize * size.0 + x as usize] = premultiply(value);
        }
    }
    out
}

/// A shared, immutable filter graph handle.
pub type SharedFilterGraph = Arc<FilterGraph>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_aspect_ratio_places_images_like_svg() {
        // SVG 2 #PreserveAspectRatioAttribute.
        let viewport = CssRect::new(10.0, 20.0, 80.0, 40.0);
        let parse = |value: &str| AspectRatio::parse(value);
        assert_eq!(parse("xMidYMid meet"), Some(AspectRatio::default()));
        assert_eq!(
            parse("defer xMinYMax"),
            Some(AspectRatio {
                align: Some((0.0, 1.0)),
                slice: false,
            })
        );
        assert_eq!(parse("none slice").map(|ratio| ratio.align), Some(None));
        for invalid in [
            "",
            "xMidYmid",
            "xMidYMid meet slice",
            "meet",
            "xMaxYMax cover",
        ] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
        let natural = (10.0, 10.0);
        assert_eq!(
            AspectRatio::default().place(natural, viewport),
            CssRect::new(30.0, 20.0, 40.0, 40.0)
        );
        assert_eq!(
            parse("xMaxYMin slice").unwrap().place(natural, viewport),
            CssRect::new(10.0, 20.0, 80.0, 80.0)
        );
        assert_eq!(parse("none").unwrap().place(natural, viewport), viewport);
        // An empty image places nothing.
        assert_eq!(
            AspectRatio::default().place((0.0, 5.0), viewport).width,
            0.0
        );
    }

    struct OneImage(crate::render::ImageResource);

    impl FilterImageSource for OneImage {
        fn natural_size(&self, _: crate::render::ImageHandle) -> Option<(u32, u32)> {
            Some((self.0.width, self.0.height))
        }

        fn pixels(
            &self,
            _: crate::render::ImageHandle,
            _: CssRect,
            _: Affine2d,
        ) -> Option<crate::render::ImageResource> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn fe_image_results_are_clipped_to_their_subregion_and_converted() {
        // #feImageElement in a linearRGB primitive: the sRGB image is
        // converted like any input, and the subregion clips it.
        let full = CssRect::new(0.0, 0.0, 8.0, 8.0);
        let image = crate::render::ImageResource {
            svg_source: None,
            width: 2,
            height: 2,
            rgba: std::sync::Arc::from([128u8, 0, 0, 255].repeat(4)),
            has_alpha: false,
        };
        let graph = FilterGraph {
            region: full,
            primitives: vec![FilterPrimitive {
                subregion: CssRect::new(0.0, 0.0, 4.0, 8.0),
                linear_rgb: true,
                kind: FilterPrimitiveKind::Image(Box::new(FeImage {
                    source: "image".into(),
                    handle: crate::render::ImageHandle(1),
                    viewport: full,
                    aspect: AspectRatio {
                        align: None,
                        slice: false,
                    },
                })),
            }],
            primitive_space: Affine2d::IDENTITY,
            fill_paint: [0.0; 4],
            stroke_paint: [0.0; 4],
        };
        let mut out = FilterImage::new(8, 8);
        let source = OneImage(image);
        apply_filters(
            &mut out,
            &[CssFilter::Graph(std::sync::Arc::new(graph.clone()))],
            &space(),
            FilterOptions {
                images: Some(&source),
                ..FilterOptions::default()
            },
        );
        let rgba = out.to_premultiplied_rgba8();
        assert_eq!(&rgba[..4], &[128, 0, 0, 255]);
        assert_eq!(&rgba[(4 * 8 + 6) * 4..(4 * 8 + 7) * 4], &[0, 0, 0, 0]);
        // Without image access the primitive is transparent black.
        let mut out = FilterImage::new(8, 8);
        apply_filters(
            &mut out,
            &[CssFilter::Graph(std::sync::Arc::new(graph))],
            &space(),
            FilterOptions::default(),
        );
        assert!(out.to_premultiplied_rgba8().iter().all(|&c| c == 0));
    }

    fn space() -> PixelSpace {
        PixelSpace {
            to_pixel: Affine2d::IDENTITY,
        }
    }

    fn solid(width: usize, height: usize, rect: PixelRect, color: [f32; 4]) -> FilterImage {
        let mut image = FilterImage::new(width, height);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                image.data[y as usize * width + x as usize] = premultiply(color);
            }
        }
        image
    }

    fn graph(region: CssRect, primitives: Vec<FilterPrimitive>) -> FilterGraph {
        FilterGraph {
            region,
            primitives,
            primitive_space: Affine2d::IDENTITY,
            fill_paint: [0.0; 4],
            stroke_paint: [0.0; 4],
        }
    }

    fn primitive(subregion: CssRect, linear: bool, kind: FilterPrimitiveKind) -> FilterPrimitive {
        FilterPrimitive {
            subregion,
            linear_rgb: linear,
            kind,
        }
    }

    fn rect(x0: i64, y0: i64, x1: i64, y1: i64) -> PixelRect {
        PixelRect { x0, y0, x1, y1 }
    }

    #[test]
    fn park_miller_generator_matches_the_specified_check_value() {
        // #feTurbulenceElement: "the algorithm should produce the result
        // 1043618065 as the 10,000th generated number if the original seed
        // is 1."
        let mut seed = setup_seed(1);
        for _ in 0..10_000 {
            seed = random(seed);
        }
        assert_eq!(seed, 1_043_618_065);
    }

    #[test]
    fn transfer_functions_follow_the_table_discrete_and_gamma_formulas() {
        let table = TransferFunction::Table(vec![0.0, 1.0, 0.0]);
        assert!((table.apply(0.25) - 0.5).abs() < 1e-6);
        assert!((table.apply(0.75) - 0.5).abs() < 1e-6);
        assert_eq!(table.apply(1.0), 0.0);
        let discrete = TransferFunction::Discrete(vec![0.2, 0.8]);
        assert_eq!(discrete.apply(0.49), 0.2);
        assert_eq!(discrete.apply(0.5), 0.8);
        assert_eq!(discrete.apply(1.0), 0.8);
        let gamma = TransferFunction::Gamma {
            amplitude: 2.0,
            exponent: 2.0,
            offset: 0.1,
        };
        assert!((gamma.apply(0.5) - 0.6).abs() < 1e-6);
        assert_eq!(TransferFunction::Table(Vec::new()).apply(0.3), 0.3);
    }

    #[test]
    fn flood_composite_and_merge_respect_subregions_and_operators() {
        let full = CssRect::new(0.0, 0.0, 8.0, 8.0);
        let source = solid(8, 8, rect(0, 0, 4, 8), [1.0, 0.0, 0.0, 1.0]);
        let filter = graph(
            full,
            vec![
                primitive(
                    CssRect::new(2.0, 0.0, 6.0, 8.0),
                    false,
                    FilterPrimitiveKind::Flood {
                        color: [0.0, 0.0, 1.0, 1.0],
                    },
                ),
                primitive(
                    full,
                    false,
                    FilterPrimitiveKind::Composite {
                        input: FilterInput::SourceGraphic,
                        input2: FilterInput::Result(0),
                        operator: CompositeOp::In,
                    },
                ),
            ],
        );
        let out = evaluate_graph(&filter, &source, &space(), FilterOptions::default());
        // Red survives only where the flood covers it.
        assert_eq!(out.get(1, 1), [0.0; 4]);
        assert_eq!(out.get(3, 1), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(out.get(5, 1), [0.0; 4]);
        let merged = graph(
            full,
            vec![
                primitive(
                    CssRect::new(2.0, 0.0, 6.0, 8.0),
                    false,
                    FilterPrimitiveKind::Flood {
                        color: [0.0, 0.0, 1.0, 1.0],
                    },
                ),
                primitive(
                    full,
                    false,
                    FilterPrimitiveKind::Merge {
                        inputs: vec![FilterInput::Result(0), FilterInput::SourceGraphic],
                    },
                ),
            ],
        );
        let out = evaluate_graph(&merged, &source, &space(), FilterOptions::default());
        assert_eq!(out.get(1, 1), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(out.get(3, 1), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(out.get(6, 1), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn linear_rgb_primitives_convert_inputs_and_results() {
        // A 50% opacity color matrix in linearRGB leaves an opaque mid gray
        // unchanged (alpha only), and the round trip restores sRGB.
        let full = CssRect::new(0.0, 0.0, 2.0, 2.0);
        let source = solid(2, 2, rect(0, 0, 2, 2), [0.5, 0.5, 0.5, 1.0]);
        let mut matrix = [0.0; 20];
        matrix[0] = 1.0;
        matrix[6] = 1.0;
        matrix[12] = 1.0;
        matrix[18] = 1.0;
        let identity = graph(
            full,
            vec![primitive(
                full,
                true,
                FilterPrimitiveKind::ColorMatrix {
                    input: FilterInput::SourceGraphic,
                    matrix: Some(matrix),
                },
            )],
        );
        let out = evaluate_graph(&identity, &source, &space(), FilterOptions::default());
        assert!((out.get(0, 0)[0] - 0.5).abs() < 1e-4);
        // A linearRGB flood of sRGB 0.5 gray is still 0.5 gray in sRGB.
        let flood = graph(
            full,
            vec![primitive(
                full,
                true,
                FilterPrimitiveKind::Flood {
                    color: [0.5, 0.5, 0.5, 1.0],
                },
            )],
        );
        let out = evaluate_graph(&flood, &source, &space(), FilterOptions::default());
        assert!((out.get(1, 1)[1] - 0.5).abs() < 1e-4);
        // Blending (averaging) black and white differs between spaces: in
        // linearRGB a 50% white over black is brighter in sRGB.
        let half_white = graph(
            full,
            vec![
                primitive(
                    full,
                    true,
                    FilterPrimitiveKind::Flood {
                        color: [0.0, 0.0, 0.0, 1.0],
                    },
                ),
                primitive(
                    full,
                    true,
                    FilterPrimitiveKind::Flood {
                        color: [1.0, 1.0, 1.0, 0.5],
                    },
                ),
                primitive(
                    full,
                    true,
                    FilterPrimitiveKind::Merge {
                        inputs: vec![FilterInput::Result(0), FilterInput::Result(1)],
                    },
                ),
            ],
        );
        let linear = evaluate_graph(&half_white, &source, &space(), FilterOptions::default());
        assert!((linear.get(0, 0)[0] - linear_to_srgb(0.5)).abs() < 1e-3);
    }

    #[test]
    fn empty_filters_and_disabled_subregions_render_nothing() {
        let full = CssRect::new(0.0, 0.0, 4.0, 4.0);
        let source = solid(4, 4, rect(0, 0, 4, 4), [0.0, 1.0, 0.0, 1.0]);
        let empty = graph(full, Vec::new());
        assert!(
            evaluate_graph(&empty, &source, &space(), FilterOptions::default())
                .data
                .iter()
                .all(|p| *p == [0.0; 4])
        );
        let disabled = graph(
            full,
            vec![primitive(
                CssRect::new(0.0, 0.0, 0.0, 4.0),
                false,
                FilterPrimitiveKind::Offset {
                    input: FilterInput::SourceGraphic,
                    dx: 0.0,
                    dy: 0.0,
                },
            )],
        );
        assert!(
            evaluate_graph(&disabled, &source, &space(), FilterOptions::default())
                .data
                .iter()
                .all(|p| *p == [0.0; 4])
        );
        // The filter region clips SourceGraphic.
        let region = graph(
            CssRect::new(1.0, 1.0, 2.0, 2.0),
            vec![primitive(
                CssRect::new(0.0, 0.0, 4.0, 4.0),
                false,
                FilterPrimitiveKind::Offset {
                    input: FilterInput::SourceGraphic,
                    dx: 0.0,
                    dy: 0.0,
                },
            )],
        );
        let out = evaluate_graph(&region, &source, &space(), FilterOptions::default());
        assert_eq!(out.get(0, 0), [0.0; 4]);
        assert_eq!(out.get(1, 1), [0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn gaussian_blur_preserves_mass_and_edge_modes_extend_the_input() {
        let mut image = FilterImage::new(41, 1);
        image.data[20] = [1.0, 1.0, 1.0, 1.0];
        for deviation in [1.0, 4.0] {
            let blurred = gaussian_blur(
                &image,
                image.full(),
                image.full(),
                (deviation, 0.0),
                EdgeMode::None,
            );
            let sum: f32 = blurred.data.iter().map(|p| p[3]).sum();
            assert!((sum - 1.0).abs() < 1e-3, "{deviation}: {sum}");
            assert!(blurred.data[20][3] > blurred.data[23][3]);
            assert!((blurred.data[17][3] - blurred.data[23][3]).abs() < 0.02);
        }
        // Mirror and duplicate keep an opaque field opaque at its edges;
        // edgeMode none fades it.
        let field = solid(10, 10, rect(0, 0, 10, 10), [1.0, 1.0, 1.0, 1.0]);
        let none = gaussian_blur(
            &field,
            field.full(),
            field.full(),
            (3.0, 3.0),
            EdgeMode::None,
        );
        assert!(none.get(0, 0)[3] < 0.6);
        for edge in [EdgeMode::Mirror, EdgeMode::Duplicate, EdgeMode::Wrap] {
            let kept = gaussian_blur(&field, field.full(), field.full(), (3.0, 3.0), edge);
            assert!((kept.get(0, 0)[3] - 1.0).abs() < 1e-3, "{edge:?}");
        }
    }

    #[test]
    fn morphology_dilates_and_erodes_by_the_radius() {
        let source = solid(9, 9, rect(4, 4, 5, 5), [1.0, 1.0, 1.0, 1.0]);
        let dilated = morphology(&source, source.full(), (2.0, 1.0), true);
        assert_eq!(dilated.get(2, 4)[3], 1.0);
        assert_eq!(dilated.get(1, 4)[3], 0.0);
        assert_eq!(dilated.get(4, 3)[3], 1.0);
        assert_eq!(dilated.get(4, 2)[3], 0.0);
        let block = solid(9, 9, rect(2, 2, 7, 7), [1.0, 1.0, 1.0, 1.0]);
        let eroded = morphology(&block, block.full(), (1.0, 1.0), false);
        assert_eq!(eroded.get(2, 2)[3], 0.0);
        assert_eq!(eroded.get(3, 3)[3], 1.0);
        assert_eq!(eroded.get(5, 5)[3], 1.0);
        assert_eq!(eroded.get(6, 6)[3], 0.0);
    }

    #[test]
    fn offsets_shift_by_whole_and_fractional_pixels() {
        let source = solid(4, 4, rect(0, 0, 1, 1), [1.0, 1.0, 1.0, 1.0]);
        let moved = offset(&source, (2.0, 1.0));
        assert_eq!(moved.get(2, 1)[3], 1.0);
        assert_eq!(moved.get(0, 0)[3], 0.0);
        let half = offset(&source, (0.5, 0.0));
        assert!((half.get(0, 0)[3] - 0.5).abs() < 1e-6);
        assert!((half.get(1, 0)[3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn blend_modes_follow_compositing_formulas() {
        let full = rect(0, 0, 1, 1);
        let source = solid(1, 1, full, [0.5, 0.25, 1.0, 1.0]);
        let backdrop = solid(1, 1, full, [0.5, 0.5, 0.5, 1.0]);
        let multiply = blend(&source, &backdrop, full, BlendMode::Multiply, true);
        assert!((multiply.get(0, 0)[0] - 0.25).abs() < 1e-6);
        let screen = blend(&source, &backdrop, full, BlendMode::Screen, true);
        assert!((screen.get(0, 0)[2] - 1.0).abs() < 1e-6);
        // A transparent backdrop leaves the source unblended.
        let empty = FilterImage::new(1, 1);
        let over = blend(&source, &empty, full, BlendMode::Difference, true);
        assert!((over.get(0, 0)[1] - 0.25).abs() < 1e-6);
        let luminosity = blend(&source, &backdrop, full, BlendMode::Luminosity, true);
        let lum = 0.3 * 0.5 + 0.59 * 0.25 + 0.11;
        assert!((luminosity.get(0, 0)[0] - lum).abs() < 1e-4);
    }

    #[test]
    fn arithmetic_composite_clamps_and_keeps_colors_premultiplied() {
        let full = rect(0, 0, 1, 1);
        let a = solid(1, 1, full, [1.0, 0.0, 0.0, 1.0]);
        let b = solid(1, 1, full, [0.0, 0.0, 1.0, 0.5]);
        let out = composite(
            &a,
            &b,
            full,
            CompositeOp::Arithmetic {
                k1: 0.0,
                k2: 0.5,
                k3: 0.5,
                k4: 0.0,
            },
        );
        let p = out.get(0, 0);
        assert!((p[3] - 0.75).abs() < 1e-6);
        assert!((p[0] - 0.5).abs() < 1e-6);
        assert!((p[2] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn turbulence_fills_the_subregion_deterministically() {
        let region = CssRect::new(0.0, 0.0, 16.0, 16.0);
        let noise = graph(
            region,
            vec![primitive(
                CssRect::new(0.0, 0.0, 8.0, 16.0),
                false,
                FilterPrimitiveKind::Turbulence(Turbulence {
                    base_frequency: (0.1, 0.1),
                    octaves: 2,
                    seed: 0.0,
                    stitch: false,
                    fractal_noise: true,
                }),
            )],
        );
        let source = FilterImage::new(16, 16);
        let a = evaluate_graph(&noise, &source, &space(), FilterOptions::default());
        let b = evaluate_graph(&noise, &source, &space(), FilterOptions::default());
        assert_eq!(a, b);
        assert!(a.get(2, 2)[3] > 0.0);
        assert_eq!(a.get(12, 2), [0.0; 4]);
        // Fractal noise centers on 0.5.
        let mean: f32 = (0..8)
            .flat_map(|x| (0..16).map(move |y| (x, y)))
            .map(|(x, y)| a.get(x, y)[3])
            .sum::<f32>()
            / 128.0;
        assert!((0.3..0.7).contains(&mean), "{mean}");
    }

    #[test]
    fn diffuse_lighting_uses_sobel_normals_including_edge_kernels() {
        // #feDiffuseLightingElement: a flat surface lit from straight above
        // reflects kd × the light color; a step in the bump map tilts the
        // normal (N.L < 1). Results are opaque.
        let flat = solid(6, 6, rect(0, 0, 6, 6), [0.0, 0.0, 0.0, 1.0]);
        let lighting = Lighting {
            surface_scale: 5.0,
            constant: 0.5,
            specular_exponent: None,
            color: [1.0, 1.0, 1.0],
            light: LightSource::Distant {
                azimuth: 0.0,
                elevation: 90.0,
            },
        };
        let lit = light(&flat, flat.full(), &space(), &lighting, false);
        for (x, y) in [(0, 0), (5, 0), (2, 3), (5, 5)] {
            let p = lit.get(x, y);
            assert!((p[0] - 0.5).abs() < 1e-5 && p[3] == 1.0, "({x},{y}) {p:?}");
        }
        let step = solid(6, 6, rect(0, 0, 3, 6), [0.0, 0.0, 0.0, 1.0]);
        let lit = light(&step, step.full(), &space(), &lighting, false);
        assert!(lit.get(2, 3)[0] < 0.49, "{:?}", lit.get(2, 3));
        assert!((lit.get(0, 3)[0] - 0.5).abs() < 1e-5);
        // Specular alpha is the largest color component.
        let specular = Lighting {
            specular_exponent: Some(1.0),
            constant: 1.0,
            color: [1.0, 0.5, 0.0],
            ..lighting
        };
        let p = light(&flat, flat.full(), &space(), &specular, false).get(2, 2);
        assert!((p[3] - p[0]).abs() < 1e-6 && p[1] <= p[3]);
    }

    #[test]
    fn convolution_follows_the_rotated_kernel_and_edge_mode() {
        // #feConvolveMatrixElement: the kernel is applied rotated 180°, so a
        // kernel picking the right-hand neighbour moves content left.
        let source = solid(5, 1, rect(2, 0, 3, 1), [1.0, 1.0, 1.0, 1.0]);
        let matrix = ConvolveMatrix {
            order: (3, 1),
            kernel: vec![1.0, 0.0, 0.0],
            divisor: 1.0,
            bias: 0.0,
            target: (1, 0),
            edge: EdgeMode::None,
            preserve_alpha: false,
        };
        let out = convolve(&source, source.full(), source.full(), &matrix);
        assert_eq!(out.get(1, 0)[3], 1.0);
        assert_eq!(out.get(2, 0)[3], 0.0);
    }

    #[test]
    fn css_functions_and_graphs_chain_in_order() {
        // Filter Effects 1 #FilterProperty: blur() then a graph, the graph
        // receiving the blurred output as SourceGraphic.
        let region = CssRect::new(0.0, 0.0, 10.0, 10.0);
        let mut image = solid(10, 10, rect(4, 4, 6, 6), [0.0, 0.0, 0.0, 1.0]);
        let alpha_to_red = Arc::new(graph(
            region,
            vec![primitive(
                region,
                false,
                FilterPrimitiveKind::ColorMatrix {
                    input: FilterInput::SourceGraphic,
                    matrix: Some([
                        0.0, 0.0, 0.0, 0.0, 1.0, //
                        0.0, 0.0, 0.0, 0.0, 0.0, //
                        0.0, 0.0, 0.0, 0.0, 0.0, //
                        0.0, 0.0, 0.0, 1.0, 0.0,
                    ]),
                },
            )],
        ));
        apply_filters(
            &mut image,
            &[CssFilter::Blur(1.0), CssFilter::Graph(alpha_to_red)],
            &space(),
            FilterOptions::default(),
        );
        let edge = image.get(3, 5);
        assert!(edge[3] > 0.0 && edge[3] < 1.0);
        assert!((edge[0] - edge[3]).abs() < 1e-5 && edge[1] == 0.0);
    }
}
