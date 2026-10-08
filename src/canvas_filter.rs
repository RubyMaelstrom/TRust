//! HTML #dom-context-2d-filter and #drawing-model, with Filter Effects 1
//! #FilterProperty, #ShorthandEquivalents and #feGaussianBlurElement (local
//! WHATWG snapshot e5071a20 and CSSWG snapshot 81c27f68, 2026-09-06).
//!
//! The current filter is kept as the string the author gave plus its parsed
//! steps. Lengths are resolved when the attribute is set ("relative to the
//! computed value of 'font-size' ... at the time that the attribute is set");
//! `url()` references are resolved for each drawing operation, so a changed
//! SVG `<filter>` is used by the next draw.
//!
//! Drawing model step 4 takes the infinite image A as input. Evaluation pulls
//! each step's output for a requested device rectangle, asking its input for
//! exactly the rectangle it reads (a blur's kernel margin, a drop shadow's
//! offset source), down to rendering A itself for a rectangle. Nothing outside
//! the canvas is retained; transient buffers are bounded below.

use std::sync::Arc;

use resvg::tiny_skia as sk;
use vello_cpu::kurbo::{Affine, Rect};

use crate::render::{CssFilter, PaintColor};

/// The largest standard deviation a filter blur uses. HTML permits the same
/// implementation limit for shadows; filters need it for the same reason:
/// the kernel's reach determines how much of A must be rendered.
const MAX_SIGMA: f32 = 256.;
/// Transient filter buffers are bounded like canvas bitmaps.
const MAX_REGION_BYTES: usize = 256 * 1024 * 1024;

/// One `<filter-value-list>` entry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    Function(CssFilter),
    /// A `url()` filter reference, as written.
    Reference(String),
}

/// A parsed current filter other than "none".
#[derive(Debug, PartialEq)]
pub(crate) struct Filter {
    /// The string the setter was given, returned by the getter.
    pub text: String,
    pub steps: Vec<Step>,
}

/// A filter step ready for evaluation: references are resolved to the SVG
/// markup containing their `<filter>` element.
pub(crate) enum Resolved {
    Function(CssFilter),
    Svg { markup: String, id: String },
}

/// HTML #dom-context-2d-filter setter step 2: parse `value` as a
/// `<filter-value-list>`. `length` resolves a `<length>` in the font style
/// source object's environment; `current_color` is drop-shadow()'s default.
pub(crate) fn parse(
    value: &str,
    length: &dyn Fn(&str) -> Option<f32>,
    current_color: PaintColor,
) -> Option<Filter> {
    use cssparser::{Parser, Token};
    let mut parser = Parser::new(value);
    let mut steps = Vec::new();
    while !parser.is_exhausted() {
        let start = parser.position();
        let token = parser.next().ok()?.clone();
        match token {
            Token::UnquotedUrl(url) => steps.push(Step::Reference(url.to_string())),
            Token::Function(name) if name.eq_ignore_ascii_case("url") => {
                let url = parser
                    .parse_nested_block(|p| -> Result<String, cssparser::ParseError<()>> {
                        let url = p.expect_string()?.to_string();
                        p.expect_exhausted()?;
                        Ok(url)
                    })
                    .ok()?;
                steps.push(Step::Reference(url));
            }
            Token::Function(_) => {
                parser
                    .parse_nested_block(|p| -> Result<(), cssparser::ParseError<()>> {
                        while p.next().is_ok() {}
                        Ok(())
                    })
                    .ok()?;
                // One filter function, parsed by the CSS `filter` grammar.
                let function = parser.slice_from(start);
                let mut parsed = crate::layout2::filter::filters(function, length, current_color)?;
                if parsed.len() != 1 {
                    return None;
                }
                steps.push(Step::Function(parsed.remove(0)));
            }
            // `none` is handled by the setter; CSS-wide keywords and any
            // other token make the whole value fail to parse.
            _ => return None,
        }
    }
    (!steps.is_empty()).then(|| Filter {
        text: value.to_string(),
        steps,
    })
}

/// An integer device rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Region {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn new(x: i64, y: i64, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn inflate(self, margin: u32) -> Self {
        Self {
            x: self.x - i64::from(margin),
            y: self.y - i64::from(margin),
            width: self.width.saturating_add(margin.saturating_mul(2)),
            height: self.height.saturating_add(margin.saturating_mul(2)),
        }
    }

    fn bytes(self) -> Option<usize> {
        (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(4)
    }

    fn intersects(self, rect: Rect) -> bool {
        rect.x1 > self.x as f64
            && rect.y1 > self.y as f64
            && rect.x0 < (self.x + i64::from(self.width)) as f64
            && rect.y0 < (self.y + i64::from(self.height)) as f64
    }

    /// The smallest region containing `rect`.
    fn covering(rect: Rect) -> Option<Self> {
        if !(rect.x0.is_finite()
            && rect.y0.is_finite()
            && rect.x1.is_finite()
            && rect.y1.is_finite())
        {
            return None;
        }
        let x = rect.x0.floor();
        let y = rect.y0.floor();
        let width = (rect.x1.ceil() - x).max(0.);
        let height = (rect.y1.ceil() - y).max(0.);
        (width < f64::from(u32::MAX) && height < f64::from(u32::MAX))
            .then(|| Self::new(x as i64, y as i64, width as u32, height as u32))
    }
}

fn pixmap(region: Region) -> Option<sk::Pixmap> {
    if region.bytes()? > MAX_REGION_BYTES {
        return None;
    }
    sk::Pixmap::new(region.width.max(1), region.height.max(1))
}

/// Render the image A into a layer: `(layer, shift)` maps A's device
/// coordinates through `shift` into the layer, as for shadows.
pub(crate) type Source<'a> = dyn FnMut(&mut sk::Pixmap, Affine) + 'a;

struct Evaluation<'a, 'b> {
    source: &'a mut Source<'b>,
    /// A's ink bounds in device space; A is transparent black elsewhere.
    bounds: Option<Rect>,
    canvas: Region,
}

impl Evaluation<'_, '_> {
    /// The output of `steps` (applied in order to A) over `region`.
    fn eval(&mut self, steps: &[Resolved], region: Region) -> Option<sk::Pixmap> {
        let Some((last, rest)) = steps.split_last() else {
            let mut layer = pixmap(region)?;
            if self.bounds.is_none_or(|bounds| region.intersects(bounds)) {
                (self.source)(
                    &mut layer,
                    Affine::translate((-region.x as f64, -region.y as f64)),
                );
            }
            return Some(layer);
        };
        match last {
            Resolved::Function(CssFilter::ColorMatrix(matrix)) => {
                let mut layer = self.eval(rest, region)?;
                color_matrix(&mut layer, matrix);
                Some(layer)
            }
            Resolved::Function(CssFilter::Blur(sigma)) => {
                let sigma = sigma.min(MAX_SIGMA);
                if sigma <= 0. {
                    return self.eval(rest, region);
                }
                let margin = blur_margin(sigma);
                let input = self.eval(rest, region.inflate(margin))?;
                let (width, height) = (input.width(), input.height());
                let mut data = input.take();
                blur(&mut data, width as usize, height as usize, 4, sigma);
                crop(&data, width, margin, region)
            }
            Resolved::Function(CssFilter::DropShadow {
                dx,
                dy,
                std_deviation,
                color,
            }) => {
                let input = self.eval(rest, region)?;
                let sigma = std_deviation.clamp(0., MAX_SIGMA);
                let margin = if sigma > 0. { blur_margin(sigma) } else { 0 } + 1;
                // The shadow at p is the blurred alpha at p - (dx, dy).
                let source = Region::new(
                    region.x - f64::from(*dx).ceil() as i64,
                    region.y - f64::from(*dy).ceil() as i64,
                    region.width.saturating_add(1),
                    region.height.saturating_add(1),
                )
                .inflate(margin);
                let shadow_input = self.eval(rest, source)?;
                let (sw, sh) = (
                    shadow_input.width() as usize,
                    shadow_input.height() as usize,
                );
                let mut alpha: Vec<u8> = shadow_input
                    .data()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|pixel| pixel[3])
                    .collect();
                if sigma > 0. {
                    blur(&mut alpha, sw, sh, 1, sigma);
                }
                let PaintColor::Rgba(r, g, b, a) = *color else {
                    return Some(input);
                };
                let mut output = input;
                let width = region.width as usize;
                for (index, pixel) in output
                    .data_mut()
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    let x = (index % width) as f64 + (region.x - source.x) as f64 - f64::from(*dx);
                    let y = (index / width) as f64 + (region.y - source.y) as f64 - f64::from(*dy);
                    let coverage = sample(&alpha, sw, sh, x, y) / 255.;
                    let shadow_alpha = f32::from(a) / 255. * coverage;
                    if shadow_alpha <= 0. {
                        continue;
                    }
                    // feFlood (premultiplied) "in" the offset blur, under the
                    // input: source-over in sRGB.
                    let inverse = 1. - f32::from(pixel[3]) / 255.;
                    for (channel, component) in pixel.iter_mut().zip([r, g, b, 255]) {
                        let shadow = f32::from(component) * shadow_alpha;
                        *channel = (f32::from(*channel) + shadow * inverse).round().min(255.) as u8;
                    }
                }
                Some(output)
            }
            Resolved::Svg { markup, id } => {
                // SVG filters take the canvas as their reference box: their
                // percentages and default region are relative to it.
                let input = self.eval(rest, self.canvas)?;
                let output = svg_filter(&input, markup, id).unwrap_or(input);
                if region == self.canvas {
                    return Some(output);
                }
                let mut layer = pixmap(region)?;
                layer.draw_pixmap(
                    (self.canvas.x - region.x) as i32,
                    (self.canvas.y - region.y) as i32,
                    output.as_ref(),
                    &sk::PixmapPaint {
                        blend_mode: sk::BlendMode::Source,
                        ..Default::default()
                    },
                    sk::Transform::identity(),
                    None,
                );
                Some(layer)
            }
        }
    }
}

/// HTML #drawing-model steps 4-7 for a filtered drawing: B is the filter's
/// output for the canvas; a drawn shadow is rendered from B; both are
/// composited within the clipping region with the current operator.
pub(crate) fn draw(
    bitmap: &mut sk::Pixmap,
    state: &crate::canvas::State,
    chain: &[Resolved],
    bounds: Option<Rect>,
    source: &mut Source<'_>,
) {
    let canvas = Region::new(0, 0, bitmap.width(), bitmap.height());
    let mut evaluation = Evaluation {
        source,
        bounds,
        canvas,
    };
    if state.shadow.drawn() {
        crate::canvas_shadow::paint(bitmap, state, |layer, shift| {
            // The layer holds B mapped through `shift` (a translation).
            let [_, _, _, _, tx, ty] = shift.as_coeffs();
            let Some(region) = Region::covering(Rect::new(
                -tx,
                -ty,
                -tx + f64::from(layer.width()),
                -ty + f64::from(layer.height()),
            )) else {
                return;
            };
            if let Some(b) = evaluation.eval(chain, region) {
                layer.draw_pixmap(
                    0,
                    0,
                    b.as_ref(),
                    &sk::PixmapPaint {
                        quality: sk::FilterQuality::Bilinear,
                        ..Default::default()
                    },
                    sk::Transform::from_translate(
                        (tx + region.x as f64) as f32,
                        (ty + region.y as f64) as f32,
                    ),
                    None,
                );
            }
        });
    }
    let Some(b) = evaluation.eval(chain, canvas) else {
        return;
    };
    let mode = crate::canvas::blend(&state.composite).unwrap_or_default();
    crate::canvas::composite_region(bitmap, &b, 0, 0, mode, state.clip.as_deref());
}

/// The reach of the blur kernel `blur()` uses for `sigma`.
fn blur_margin(sigma: f32) -> u32 {
    if sigma >= 2. {
        let d = box_size(sigma);
        // Three boxes reach at most 3d/2 on either side.
        (3 * d).div_ceil(2) + 1
    } else {
        (sigma * 3.).ceil() as u32
    }
}

/// Filter Effects 1 #feGaussianBlurElement: the box size of the three-box
/// approximation, `floor(s * 3 * sqrt(2 * pi) / 4 + 0.5)`.
fn box_size(sigma: f32) -> u32 {
    (f64::from(sigma) * 3. * (2. * std::f64::consts::PI).sqrt() / 4. + 0.5).floor() as u32
}

/// A Gaussian blur of interleaved `channels`, extended with transparent black
/// (edgeMode none, which the `filter` property uses).
fn blur(data: &mut [u8], width: usize, height: usize, channels: usize, sigma: f32) {
    if sigma >= 2. {
        // "If d is odd, use three box-blurs of size d, centered on the output
        // pixel. If d is even, two box-blurs of size d (centered on the pixel
        // boundary to the left, then to the right) and one of size d+1."
        let d = box_size(sigma) as usize;
        let passes = if d % 2 == 1 {
            [(d / 2, d / 2); 3]
        } else {
            [(d / 2, d / 2 - 1), (d / 2 - 1, d / 2), (d / 2, d / 2)]
        };
        for (left, right) in passes {
            box_pass(data, width, height, channels, left, right, true);
        }
        for (left, right) in passes {
            box_pass(data, width, height, channels, left, right, false);
        }
    } else {
        let radius = (sigma * 3.).ceil() as usize;
        let mut weights: Vec<f32> = (0..=2 * radius)
            .map(|i| {
                let x = i as f32 - radius as f32;
                (-x * x / (2. * sigma * sigma)).exp()
            })
            .collect();
        let total: f32 = weights.iter().sum();
        for weight in &mut weights {
            *weight /= total;
        }
        kernel_pass(data, width, height, channels, &weights, true);
        kernel_pass(data, width, height, channels, &weights, false);
    }
}

/// One box blur covering `left` pixels before and `right` after each output
/// pixel, along rows (`horizontal`) or columns.
fn box_pass(
    data: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    left: usize,
    right: usize,
    horizontal: bool,
) {
    let (lines, length, step, line_step) = if horizontal {
        (height, width, channels, width * channels)
    } else {
        (width, height, width * channels, channels)
    };
    let size = (left + right + 1) as u32;
    let mut line = vec![0u8; length];
    for index in 0..lines {
        for channel in 0..channels {
            let base = index * line_step + channel;
            for (i, value) in line.iter_mut().enumerate() {
                *value = data[base + i * step];
            }
            // Running sum over [i - left, i + right], zero outside the line.
            let mut sum: u32 = line[..right.min(length)]
                .iter()
                .map(|&v| u32::from(v))
                .sum();
            for i in 0..length {
                if i + right < length {
                    sum += u32::from(line[i + right]);
                }
                data[base + i * step] = ((sum + size / 2) / size) as u8;
                if i >= left {
                    sum -= u32::from(line[i - left]);
                }
            }
        }
    }
}

fn kernel_pass(
    data: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    weights: &[f32],
    horizontal: bool,
) {
    let (lines, length, step, line_step) = if horizontal {
        (height, width, channels, width * channels)
    } else {
        (width, height, width * channels, channels)
    };
    let radius = weights.len() / 2;
    let mut line = vec![0u8; length];
    for index in 0..lines {
        for channel in 0..channels {
            let base = index * line_step + channel;
            for (i, value) in line.iter_mut().enumerate() {
                *value = data[base + i * step];
            }
            for i in 0..length {
                let mut sum = 0.;
                for (k, weight) in weights.iter().enumerate() {
                    if let Some(j) = (i + k).checked_sub(radius)
                        && j < length
                    {
                        sum += f32::from(line[j]) * weight;
                    }
                }
                data[base + i * step] = sum.round().clamp(0., 255.) as u8;
            }
        }
    }
}

/// The inner `region` of a buffer `margin` pixels larger on every side.
fn crop(data: &[u8], width: u32, margin: u32, region: Region) -> Option<sk::Pixmap> {
    let mut out = pixmap(region)?;
    let stride = width as usize * 4;
    let row = region.width as usize * 4;
    for y in 0..region.height as usize {
        let start = (y + margin as usize) * stride + margin as usize * 4;
        out.data_mut()[y * row..(y + 1) * row].copy_from_slice(&data[start..start + row]);
    }
    Some(out)
}

/// Bilinear sample of an alpha plane, transparent outside it.
fn sample(alpha: &[u8], width: usize, height: usize, x: f64, y: f64) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
    let at = |x: f64, y: f64| -> f32 {
        if x < 0. || y < 0. || x >= width as f64 || y >= height as f64 {
            0.
        } else {
            f32::from(alpha[y as usize * width + x as usize])
        }
    };
    let top = at(x0, y0) * (1. - fx) + at(x0 + 1., y0) * fx;
    let bottom = at(x0, y0 + 1.) * (1. - fx) + at(x0 + 1., y0 + 1.) * fx;
    top * (1. - fy) + bottom * fy
}

/// Filter Effects 1 feColorMatrix on non-premultiplied sRGB, each result
/// clamped to [0, 1] (filter functions operate in sRGB).
fn color_matrix(layer: &mut sk::Pixmap, m: &[f32; 20]) {
    for pixel in layer.data_mut().as_chunks_mut::<4>().0 {
        let alpha = f32::from(pixel[3]) / 255.;
        let straight = |c: u8| {
            if alpha > 0. {
                (f32::from(c) / 255. / alpha).min(1.)
            } else {
                0.
            }
        };
        let input = [
            straight(pixel[0]),
            straight(pixel[1]),
            straight(pixel[2]),
            alpha,
        ];
        let mut out = [0f32; 4];
        for (row, value) in out.iter_mut().enumerate() {
            let r = &m[row * 5..row * 5 + 5];
            *value = (r[0] * input[0] + r[1] * input[1] + r[2] * input[2] + r[3] * input[3] + r[4])
                .clamp(0., 1.);
        }
        for channel in 0..3 {
            pixel[channel] = (out[channel] * out[3] * 255.).round() as u8;
        }
        pixel[3] = (out[3] * 255.).round() as u8;
    }
}

/// Apply an SVG `<filter>` (`id` within `markup`) to `input`, which covers the
/// canvas, "in the same manner as SVG": the canvas image is the filtered
/// element (SourceGraphic) and one pixel is one user unit.
fn svg_filter(input: &sk::Pixmap, markup: &str, id: &str) -> Option<sk::Pixmap> {
    use image::ImageEncoder as _;
    let (width, height) = (input.width(), input.height());
    let mut straight = Vec::with_capacity(input.data().len());
    for pixel in input.pixels() {
        let color = pixel.demultiply();
        straight.extend([color.red(), color.green(), color.blue(), color.alpha()]);
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Uncompressed,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&straight, width, height, image::ExtendedColorType::Rgba8)
    .ok()?;
    let png = Arc::new(png);
    // svgtypes reads `url(#id)` up to a space or parenthesis, without
    // escapes: such ids cannot be named, so their reference is unresolved.
    if id.is_empty()
        || id
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\'' | '\\' | '<' | '&'))
    {
        return None;
    }
    let document = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><defs>{markup}</defs><image href="trust-canvas-filter-source" x="0" y="0" width="{width}" height="{height}" preserveAspectRatio="none" image-rendering="optimizeSpeed" filter="url(#{id})"/></svg>"#
    );
    let options = resvg::usvg::Options {
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(move |href, _| {
                (href == "trust-canvas-filter-source")
                    .then(|| resvg::usvg::ImageKind::PNG(png.clone()))
            }),
        },
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_str(&document, &options).ok()?;
    let mut output = sk::Pixmap::new(width, height)?;
    resvg::render(&tree, sk::Transform::identity(), &mut output.as_mut());
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(value: &str) -> Option<f32> {
        crate::layout2::css_length_px(value, crate::layout2::Units::default())
    }

    #[test]
    fn filter_value_lists_parse_functions_and_references_in_order() {
        let black = PaintColor::Rgba(0, 0, 0, 255);
        let filter = parse("blur(2px) url(#a) url('b.svg#c') opacity(50%)", &px, black).unwrap();
        assert_eq!(filter.text, "blur(2px) url(#a) url('b.svg#c') opacity(50%)");
        assert_eq!(filter.steps.len(), 4);
        assert_eq!(filter.steps[0], Step::Function(CssFilter::Blur(2.)));
        assert_eq!(filter.steps[1], Step::Reference("#a".into()));
        assert_eq!(filter.steps[2], Step::Reference("b.svg#c".into()));
        for invalid in [
            "",
            "none",
            "inherit",
            "initial",
            "blur(-1px)",
            "blur(1px) junk",
            "url(#a) 3",
        ] {
            assert!(parse(invalid, &px, black).is_none(), "{invalid}");
        }
        // em resolves against the font style source environment.
        let em = |value: &str| {
            crate::layout2::css_length_px(
                value,
                crate::layout2::Units {
                    fs: 20.,
                    ..crate::layout2::Units::default()
                },
            )
        };
        assert_eq!(
            parse("blur(0.5em)", &em, black).unwrap().steps[0],
            Step::Function(CssFilter::Blur(10.))
        );
    }

    #[test]
    fn three_box_blur_is_normalized_and_close_to_gaussian() {
        // One opaque pixel in a field: the blurred mass is preserved within
        // the rounding of six passes, and its profile is symmetric.
        let (width, height) = (61usize, 61usize);
        let mut alpha = vec![0u8; width * height];
        for y in 25..36 {
            for x in 25..36 {
                alpha[y * width + x] = 255;
            }
        }
        let before: u32 = alpha.iter().map(|&a| u32::from(a)).sum();
        blur(&mut alpha, width, height, 1, 3.);
        let after: u32 = alpha.iter().map(|&a| u32::from(a)).sum();
        assert!((before as f64 - after as f64).abs() / (before as f64) < 0.02);
        for offset in 1..20 {
            assert_eq!(
                alpha[30 * width + 30 - offset],
                alpha[30 * width + 30 + offset]
            );
        }
        assert!(alpha[30 * width + 30] > 200 && alpha[30 * width + 5] == 0);
        // Small sigma uses the sampled kernel.
        let mut point = vec![0u8; 11];
        point[5] = 255;
        blur(&mut point, 11, 1, 1, 1.);
        assert!(point[5] > point[4] && point[4] > point[3] && point[4] == point[6]);
    }

    #[test]
    fn color_matrices_apply_to_unpremultiplied_values() {
        let mut layer = sk::Pixmap::new(1, 1).unwrap();
        layer.fill(sk::Color::from_rgba8(255, 0, 0, 128));
        // invert(1)
        let mut m = [0f32; 20];
        for c in 0..3 {
            m[c * 5 + c] = -1.;
            m[c * 5 + 4] = 1.;
        }
        m[18] = 1.;
        color_matrix(&mut layer, &m);
        let pixel = layer.pixel(0, 0).unwrap().demultiply();
        assert_eq!(
            (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
            (0, 255, 255, 128)
        );
    }

    #[test]
    fn drop_shadow_pulls_its_offset_source_from_outside_the_region() {
        // A 4x4 square at x = -10 (outside the 8x8 canvas) casts a shadow
        // 12px right: drawing model step 1's A is infinite.
        let chain = [Resolved::Function(CssFilter::DropShadow {
            dx: 12.,
            dy: 0.,
            std_deviation: 0.,
            color: PaintColor::Rgba(0, 0, 255, 255),
        })];
        let mut source = |layer: &mut sk::Pixmap, shift: Affine| {
            let mut paint = sk::Paint::default();
            paint.set_color(sk::Color::from_rgba8(255, 0, 0, 255));
            let [_, _, _, _, x, y] = shift.as_coeffs();
            if let Some(rect) = sk::Rect::from_xywh((x - 10.) as f32, (y + 2.) as f32, 4., 4.) {
                layer.fill_rect(rect, &paint, sk::Transform::identity(), None);
            }
        };
        let mut evaluation = Evaluation {
            source: &mut source,
            bounds: Some(Rect::new(-10., 2., -6., 6.)),
            canvas: Region::new(0, 0, 8, 8),
        };
        let b = evaluation.eval(&chain, Region::new(0, 0, 8, 8)).unwrap();
        let shadow = b.pixel(3, 3).unwrap();
        assert_eq!((shadow.red(), shadow.blue(), shadow.alpha()), (0, 255, 255));
        assert_eq!(b.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(b.pixel(1, 3).unwrap().alpha(), 0);
    }

    #[test]
    fn svg_filters_render_through_the_svg_filter_engine() {
        let mut input = sk::Pixmap::new(4, 4).unwrap();
        input.fill(sk::Color::from_rgba8(255, 0, 0, 255));
        let markup = r#"<svg><filter id="f" x="0" y="0" width="1" height="1"><feColorMatrix type="matrix" values="0 0 0 0 0  0 0 0 0 1  0 0 0 0 0  0 0 0 1 0" color-interpolation-filters="sRGB"/></filter></svg>"#;
        let output = svg_filter(&input, markup, "f").unwrap();
        let pixel = output.pixel(1, 1).unwrap();
        assert_eq!((pixel.red(), pixel.green(), pixel.alpha()), (0, 255, 255));
    }
}
