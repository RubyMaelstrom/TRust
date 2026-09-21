//! Glyph geometry for CSS Backgrounds 4 #background-clip. Use the same font
//! face, glyph IDs, size and normalized variation location as normal painting.

use super::{ShapedText, TextStyle};
use crate::core::CssPoint;
use crate::render::{DecorationStyle, PathElement};
use skrifa::{
    FontRef, MetadataProvider,
    instance::{NormalizedCoord, Size},
    outline::{DrawSettings, OutlinePen},
};

pub(crate) fn x_height(style: &TextStyle) -> f32 {
    let shaped = super::shape("x", style);
    let height = shaped.runs.first().and_then(|run| {
        let font =
            FontRef::from_index(run.font.data().data.as_ref(), run.font.collection_index()).ok()?;
        let coords: Vec<_> = run
            .normalized_coords
            .iter()
            .copied()
            .map(NormalizedCoord::from_bits)
            .collect();
        font.metrics(Size::new(run.font_size), coords.as_slice())
            .x_height
    });
    height
        .filter(|h| h.is_finite() && *h > 0.)
        .unwrap_or(style.size * 0.5)
}

struct Pen<'a> {
    path: &'a mut Vec<PathElement>,
    origin: CssPoint,
    skew: f32,
}

impl Pen<'_> {
    fn point(&self, x: f32, y: f32) -> CssPoint {
        CssPoint::new(self.origin.x + x + y * self.skew, self.origin.y - y)
    }
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.push(PathElement::MoveTo(self.point(x, y)));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.push(PathElement::LineTo(self.point(x, y)));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path
            .push(PathElement::QuadTo(self.point(x1, y1), self.point(x, y)));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path.push(PathElement::CurveTo(
            self.point(x1, y1),
            self.point(x2, y2),
            self.point(x, y),
        ));
    }
    fn close(&mut self) {
        self.path.push(PathElement::Close);
    }
}

pub(crate) fn append_glyph_path(
    path: &mut Vec<PathElement>,
    shaped: &ShapedText,
    origin: CssPoint,
) {
    for run in &shaped.runs {
        let Ok(font) =
            FontRef::from_index(run.font.data().data.as_ref(), run.font.collection_index())
        else {
            continue;
        };
        let outlines = font.outline_glyphs();
        let coords: Vec<_> = run
            .normalized_coords
            .iter()
            .copied()
            .map(NormalizedCoord::from_bits)
            .collect();
        for glyph in &run.glyphs {
            let Some(outline) = outlines.get(skrifa::GlyphId::new(glyph.id)) else {
                continue;
            };
            let start = path.len();
            let mut pen = Pen {
                path,
                origin: CssPoint::new(origin.x + glyph.x, origin.y + glyph.y),
                skew: run.synth_skew_degrees.unwrap_or(0.).to_radians().tan(),
            };
            if outline
                .draw(
                    DrawSettings::unhinted(Size::new(run.font_size), coords.as_slice()),
                    &mut pen,
                )
                .is_err()
            {
                path.truncate(start);
            }
        }
    }
}

pub(crate) fn append_text_path(
    path: &mut Vec<PathElement>,
    shaped: &ShapedText,
    origin: CssPoint,
    decoration: DecorationStyle,
) {
    append_glyph_path(path, shaped, origin);
    // Match the retained text painter's decoration geometry. Decorations
    // participate even when the text's foreground is transparent.
    let thickness = (shaped.line_height / 18.).max(1.);
    let mut line = |y: f32| {
        let step = match decoration {
            DecorationStyle::Dotted => thickness * 2.,
            DecorationStyle::Dashed => thickness * 5.,
            _ => shaped.advance.max(1.),
        };
        let width = match decoration {
            DecorationStyle::Dotted => thickness,
            DecorationStyle::Dashed => thickness * 3.,
            _ => shaped.advance,
        };
        let copies = if decoration == DecorationStyle::Double {
            2
        } else {
            1
        };
        for copy in 0..copies {
            let mut x = 0.;
            while x < shaped.advance {
                let left = origin.x + x;
                let right = left + width.min(shaped.advance - x);
                let top = y - thickness / 2. + copy as f32 * thickness * 2.;
                path.extend([
                    PathElement::MoveTo(CssPoint::new(left, top)),
                    PathElement::LineTo(CssPoint::new(left, top + thickness)),
                    PathElement::LineTo(CssPoint::new(right, top + thickness)),
                    PathElement::LineTo(CssPoint::new(right, top)),
                    PathElement::Close,
                ]);
                x += step;
            }
        }
    };
    if shaped.underline {
        line(origin.y + shaped.baseline + thickness);
    }
    if shaped.strikethrough {
        line(origin.y + shaped.baseline - shaped.ascent * 0.32);
    }
}
