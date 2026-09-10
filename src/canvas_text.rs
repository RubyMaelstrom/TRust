//! Canvas text uses the browser's font catalog, shaper and glyph renderer.
//! HTML #text-preparation-algorithm / #drawing-text-to-the-bitmap and CSS
//! Inline 3 #baseline-synthesis-em (local September 6, 2026 snapshots).

use crate::text::{ShapedText, TextStyle};
use vello_common::paint::{Image, ImageSource, PaintType, Tint};
use vello_cpu::color::{AlphaColor, Srgb, palette::css::BLACK};
use vello_cpu::kurbo::{Affine, BezPath, Rect, Shape};
use vello_cpu::peniko::BlendMode;

#[derive(Clone)]
pub(crate) struct TextState {
    pub font: String,
    pub style: TextStyle,
    pub align: String,
    pub baseline: String,
    pub direction: String,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            font: "10px sans-serif".into(),
            style: TextStyle {
                size: 10.,
                ..TextStyle::default()
            },
            align: "start".into(),
            baseline: "alphabetic".into(),
            direction: "inherit".into(),
        }
    }
}

impl TextState {
    pub fn retained_bytes(&self) -> usize {
        self.font.capacity()
            + self.style.family.capacity()
            + self.style.language.as_ref().map_or(0, String::capacity)
            + self.align.capacity()
            + self.baseline.capacity()
            + self.direction.capacity()
    }
    pub fn set_font(&mut self, value: &str, units: crate::layout2::Units, weight: f32) {
        // Use the existing SVG/CSS shorthand parser and common CSS length
        // conversion, with the canvas element's computed font environment.
        let Ok(font) = svgtypes::FontShorthand::from_str(value) else {
            return;
        };
        // SVG's legacy length grammar accepts nonzero unitless sizes; CSS
        // font-size is a length-percentage and permits only unitless zero.
        if font.font_size.parse::<f32>().is_ok_and(|n| n != 0.) {
            return;
        }
        let Ok(families) = svgtypes::parse_font_families(font.font_family) else {
            return;
        };
        if families.is_empty() {
            return;
        }
        let size = match font.font_size {
            "xx-small" => Some(9.6),
            "x-small" => Some(12.),
            "small" => Some(16. * 8. / 9.),
            "medium" => Some(16.),
            "large" => Some(19.2),
            "x-large" => Some(24.),
            "xx-large" => Some(32.),
            "xxx-large" => Some(48.),
            "larger" => Some(units.fs * 1.2),
            "smaller" => Some(units.fs / 1.2),
            size if size.ends_with('%') => size[..size.len() - 1]
                .parse::<f32>()
                .ok()
                .map(|n| n * units.fs / 100.),
            size => crate::layout2::css_length_px(size, units),
        };
        let Some(size) = size.filter(|n| n.is_finite() && *n >= 0.) else {
            return;
        };
        let weight = match font.font_weight.unwrap_or("normal") {
            "bolder" => {
                if weight < 350. {
                    400.
                } else if weight < 550. {
                    700.
                } else if weight < 900. {
                    900.
                } else {
                    weight
                }
            }
            "lighter" => {
                if weight < 100. {
                    weight
                } else if weight < 550. {
                    100.
                } else if weight < 750. {
                    400.
                } else {
                    700.
                }
            }
            value => crate::layout2::css_font_weight(value).unwrap_or(400.),
        };
        let family = families
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut parts = Vec::new();
        if let Some(style) = font.font_style {
            parts.push(style.to_string());
        }
        if let Some(variant) = font.font_variant {
            parts.push(variant.to_string());
        }
        if weight != 400. {
            parts.push(if weight == 700. {
                "bold".into()
            } else {
                weight.to_string()
            });
        }
        if let Some(stretch) = font.font_stretch {
            parts.push(stretch.to_string());
        }
        parts.push(format!("{size}px"));
        parts.push(family.clone());
        self.font = parts.join(" ");
        self.style = TextStyle {
            family,
            size,
            weight,
            italic: font.font_style.is_some(),
            ..TextStyle::default()
        };
    }

    pub fn prepare(
        &self,
        text: &str,
        inherited_rtl: bool,
        language: Option<String>,
    ) -> PreparedText {
        let text: String = text
            .chars()
            .map(|c| {
                if matches!(c, '\t' | '\n' | '\r' | '\x0c') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        let mut style = self.style.clone();
        style.language = language;
        let shaped = crate::text::shape_canvas(&text, &style);
        // Space selects the first available font for stable font-box/em metrics,
        // independently of later fallback glyphs in the measured string.
        let primary = crate::text::shape_canvas(" ", &style);
        let ascent = f64::from(primary.ascent);
        let descent = f64::from(primary.descent);
        let em_ascent = if ascent + descent > 0. {
            ascent / (ascent + descent) * f64::from(style.size)
        } else {
            0.
        };
        let em_descent = f64::from(style.size) - em_ascent;
        let hanging = ascent * 0.8;
        let baseline_y = match self.baseline.as_str() {
            "top" => em_ascent,
            "hanging" => hanging,
            "middle" => (em_ascent - em_descent) * 0.5,
            "ideographic" => -descent,
            "bottom" => -em_descent,
            _ => 0.,
        };
        let rtl = self.direction == "rtl" || (self.direction == "inherit" && inherited_rtl);
        let anchor_x = match self.align.as_str() {
            "center" => f64::from(shaped.advance) * 0.5,
            "right" => f64::from(shaped.advance),
            "start" if rtl => f64::from(shaped.advance),
            "end" if !rtl => f64::from(shaped.advance),
            _ => 0.,
        };
        PreparedText {
            shaped,
            anchor_x,
            baseline_y,
            ascent,
            descent,
            em_ascent,
            em_descent,
            hanging,
        }
    }
}

pub(crate) struct PreparedText {
    pub shaped: ShapedText,
    pub anchor_x: f64,
    pub baseline_y: f64,
    pub ascent: f64,
    pub descent: f64,
    pub em_ascent: f64,
    pub em_descent: f64,
    pub hanging: f64,
}

impl PreparedText {
    pub fn metrics(&self) -> [f64; 12] {
        let mut bounds = GlyphBounds::default();
        let mut cache = glifo::GlyphPrepCache::default();
        for run in &self.shaped.runs {
            let backend = GlyphBackend {
                renderer: &mut bounds,
                cache: &mut cache,
            };
            let mut builder = glifo::GlyphRunBuilder::new(
                run.font.data().clone(),
                Affine::IDENTITY,
                Affine::IDENTITY,
                backend,
            )
            .font_size(run.font_size)
            .normalized_coords(&run.normalized_coords)
            .hint(false);
            if run.synth_bold {
                let amount = f64::from(run.font_size) * 0.025;
                builder = builder.font_embolden(glifo::FontEmbolden::new(
                    vello_cpu::kurbo::Diagonal2::new(amount, amount),
                ));
            }
            if let Some(degrees) = run.synth_skew_degrees {
                builder = builder
                    .glyph_transform(Affine::skew(f64::from(degrees).to_radians().tan(), 0.));
            }
            builder.fill_glyphs(run.glyphs.iter().map(|glyph| glifo::Glyph {
                id: glyph.id,
                x: glyph.x,
                y: glyph.y - self.shaped.baseline,
            }));
        }
        let (left, right, top, bottom) = bounds.bounds.map_or((0., 0., 0., 0.), |r| {
            (
                self.anchor_x - r.x0,
                r.x1 - self.anchor_x,
                -r.y0 - self.baseline_y,
                r.y1 + self.baseline_y,
            )
        });
        [
            f64::from(self.shaped.advance),
            left,
            right,
            self.ascent - self.baseline_y,
            self.descent + self.baseline_y,
            top,
            bottom,
            self.em_ascent - self.baseline_y,
            self.em_descent + self.baseline_y,
            self.hanging - self.baseline_y,
            -self.baseline_y,
            -self.descent - self.baseline_y,
        ]
    }
}

/// Measure the SAME glyph geometry consumed by the painters, including variable,
/// synthetic, bitmap and COLR glyphs. Atlas caching is disabled so cache padding
/// never becomes a TextMetrics ink bound. No raster bitmap is allocated here.
#[derive(Default)]
struct GlyphBounds {
    transform: Affine,
    bounds: Option<Rect>,
    clip: Option<Rect>,
    clips: Vec<Option<Rect>>,
}

impl GlyphBounds {
    fn add(&mut self, rect: Rect) {
        let rect = self.clip.map_or(rect, |clip| rect.intersect(clip));
        if rect.width() > 0. && rect.height() > 0. {
            self.bounds = Some(self.bounds.map_or(rect, |old| old.union(rect)));
        }
    }
}

impl glifo::DrawSink for GlyphBounds {
    fn set_transform(&mut self, transform: Affine) {
        self.transform = transform;
    }
    fn set_paint(&mut self, _paint: glifo::AtlasPaint) {}
    fn set_paint_transform(&mut self, _transform: Affine) {}
    fn fill_path(&mut self, path: &BezPath) {
        self.add((self.transform * path).bounding_box());
    }
    fn fill_rect(&mut self, rect: &Rect) {
        self.add(self.transform.transform_rect_bbox(*rect));
    }
    fn push_clip_layer(&mut self, path: &BezPath) {
        self.clips.push(self.clip);
        let clip = (self.transform * path).bounding_box();
        self.clip = Some(self.clip.map_or(clip, |old| old.intersect(clip)));
    }
    fn push_blend_layer(&mut self, _blend: BlendMode) {
        self.clips.push(self.clip);
    }
    fn pop_layer(&mut self) {
        self.clip = self.clips.pop().flatten();
    }
    fn width(&self) -> u16 {
        u16::MAX
    }
    fn height(&self) -> u16 {
        u16::MAX
    }
}

impl glifo::GlyphRenderer for GlyphBounds {
    type SavedState = Affine;
    fn save_state(&mut self) -> Affine {
        self.transform
    }
    fn restore_state(&mut self, state: Affine) {
        self.transform = state;
    }
    fn stroke_path(&mut self, path: &BezPath) {
        glifo::DrawSink::fill_path(self, path);
    }
    fn set_paint_image(&mut self, _image: Image) {}
    fn set_tint(&mut self, _tint: Option<Tint>) {}
    fn get_context_color(&self) -> AlphaColor<Srgb> {
        BLACK
    }
    fn current_paint(&self) -> &PaintType {
        static PAINT: PaintType = PaintType::Solid(BLACK);
        &PAINT
    }
    fn atlas_image_source(&self, _slot: &glifo::AtlasSlot) -> ImageSource {
        unreachable!("measurement disables atlas sampling")
    }
    fn atlas_paint_transform(&self, _slot: &glifo::AtlasSlot) -> Affine {
        unreachable!("measurement disables atlas sampling")
    }
}

struct GlyphBackend<'a, R> {
    renderer: &'a mut R,
    cache: &'a mut glifo::GlyphPrepCache,
}
impl<'a, R: glifo::GlyphRenderer> glifo::GlyphRunBackend<'a> for GlyphBackend<'a, R> {
    fn atlas_cache(self, _enabled: bool) -> Self {
        self
    }
    fn fill_glyphs<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        run: glifo::GlyphRun<'a>,
        glyphs: G,
    ) {
        run.build(glyphs, self.cache.as_mut(), glifo::AtlasCacher::Disabled)
            .fill_glyphs(self.renderer);
    }
    fn stroke_glyphs<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        run: glifo::GlyphRun<'a>,
        glyphs: G,
    ) {
        run.build(glyphs, self.cache.as_mut(), glifo::AtlasCacher::Disabled)
            .stroke_glyphs(self.renderer);
    }
    fn render_decoration<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        _run: glifo::GlyphRun<'a>,
        _glyphs: G,
        _range: std::ops::RangeInclusive<f32>,
        _baseline: f32,
        _offset: f32,
        _size: f32,
        _buffer: f32,
    ) {
        unreachable!("canvas text does not request decorations")
    }
}

/// Glyph outlines are scaled to CSS pixels BEFORE applying Canvas line styles.
/// Otherwise font-unit scaling and maxWidth also scale the line width/dashes.
pub(crate) fn stroke_run(
    context: &mut vello_cpu::RenderContext,
    cache: &mut glifo::GlyphPrepCache,
    run: &crate::text::ShapedRun,
    baseline: f32,
    canvas_transform: Affine,
) {
    let transform = *context.transform();
    let paint_transform = *context.paint_transform();
    let mut renderer = StrokeRenderer {
        context,
        canvas_transform,
    };
    let backend = GlyphBackend {
        renderer: &mut renderer,
        cache,
    };
    let mut builder =
        glifo::GlyphRunBuilder::new(run.font.data().clone(), transform, paint_transform, backend)
            .font_size(run.font_size)
            .normalized_coords(&run.normalized_coords)
            .hint(false);
    if run.synth_bold {
        let amount = f64::from(run.font_size) * 0.025;
        builder = builder.font_embolden(glifo::FontEmbolden::new(
            vello_cpu::kurbo::Diagonal2::new(amount, amount),
        ));
    }
    if let Some(degrees) = run.synth_skew_degrees {
        builder = builder.glyph_transform(Affine::skew(f64::from(degrees).to_radians().tan(), 0.));
    }
    builder.stroke_glyphs(run.glyphs.iter().map(|glyph| glifo::Glyph {
        id: glyph.id,
        x: glyph.x,
        y: glyph.y - baseline,
    }));
}

struct StrokeRenderer<'a> {
    context: &'a mut vello_cpu::RenderContext,
    canvas_transform: Affine,
}

impl glifo::DrawSink for StrokeRenderer<'_> {
    fn set_transform(&mut self, transform: Affine) {
        self.context.set_transform(transform);
    }
    fn set_paint(&mut self, paint: glifo::AtlasPaint) {
        self.context.set_paint(paint);
    }
    fn set_paint_transform(&mut self, transform: Affine) {
        self.context.set_paint_transform(transform);
    }
    fn fill_path(&mut self, path: &BezPath) {
        self.context.fill_path(path);
    }
    fn fill_rect(&mut self, rect: &Rect) {
        self.context.fill_rect(rect);
    }
    fn push_clip_layer(&mut self, path: &BezPath) {
        self.context.push_clip_layer(path);
    }
    fn push_clip_path(&mut self, path: &BezPath) {
        self.context.push_clip_path(path);
    }
    fn push_blend_layer(&mut self, blend: BlendMode) {
        self.context.push_blend_layer(blend);
    }
    fn pop_layer(&mut self) {
        self.context.pop_layer();
    }
    fn pop_clip_path(&mut self) {
        self.context.pop_clip_path();
    }
    fn width(&self) -> u16 {
        self.context.width()
    }
    fn height(&self) -> u16 {
        self.context.height()
    }
}

impl glifo::GlyphRenderer for StrokeRenderer<'_> {
    type SavedState = vello_common::render_state::RenderState;
    fn save_state(&mut self) -> Self::SavedState {
        self.context.save_current_state()
    }
    fn restore_state(&mut self, state: Self::SavedState) {
        self.context.restore_state(state);
    }
    fn stroke_path(&mut self, path: &BezPath) {
        let local = self.canvas_transform.inverse() * *self.context.transform();
        let paint = local * *self.context.paint_transform();
        self.context.set_transform(self.canvas_transform);
        self.context.set_paint_transform(paint);
        self.context.stroke_path(&(local * path));
    }
    fn set_paint_image(&mut self, image: Image) {
        self.context.set_paint(image);
    }
    fn set_tint(&mut self, tint: Option<Tint>) {
        self.context.set_tint(tint);
    }
    fn get_context_color(&self) -> AlphaColor<Srgb> {
        glifo::GlyphRenderer::get_context_color(self.context)
    }
    fn current_paint(&self) -> &PaintType {
        self.context.paint()
    }
    fn atlas_image_source(&self, _slot: &glifo::AtlasSlot) -> ImageSource {
        unreachable!("canvas stroke disables atlas sampling")
    }
    fn atlas_paint_transform(&self, _slot: &glifo::AtlasSlot) -> Affine {
        unreachable!("canvas stroke disables atlas sampling")
    }
}
