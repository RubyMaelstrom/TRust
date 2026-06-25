//! Image decoding and terminal-graphics encoding for the viewer panel.
//!
//! Decoding sniffs the format from the bytes (servers lie about
//! content types) and guards dimensions against decompression bombs.
//! Encoding goes through ratatui-image's `Picker` — sixel where the
//! terminal answered the startup query for it (foot does), unicode
//! half-blocks anywhere else. Both steps are CPU-bound and run on
//! blocking tasks, never the UI thread.

use std::borrow::Cow;
use std::io::Read as _;
use std::sync::{Arc, OnceLock};

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FilterType, Resize};

/// Hard ceiling on decoded raster dimensions: a small download can still claim
/// to be a gigapixel image.
const MAX_DIMENSION: u32 = 12_000;
/// SVG and SVGZ are text formats whose compressed representation can be tiny.
/// Bound the expanded XML before usvg sees it.
const MAX_SVG_BYTES: usize = 16 * 1024 * 1024;
/// A resvg pixmap is four bytes per pixel. Sixteen megapixels caps one SVG
/// rasterization at 64 MiB even if a terminal or document requests a huge box.
const MAX_SVG_PIXELS: u64 = 16 * 1024 * 1024;
const SVG_MIME: &str = "image/svg+xml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub mime: &'static str,
}

/// How an SVG is recolored to match the UI. We deliberately do NOT honor an
/// SVG's own colors (the same call as not honoring HTML/CSS color — see the
/// cascade notes): a vector is rendered as a SILHOUETTE — its coverage painted
/// in `fg` over `bg` — so a black-on-transparent icon designed for a light page
/// reads cleanly on the cyberpunk canvas instead of vanishing. `fg` is the
/// element's role color (link accent vs. body text); `bg` is the UI background.
/// Only SVG is tinted; raster images keep their pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SvgTint {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

/// Replace every pixel's color with the tint, keeping the artwork's coverage:
/// `out = bg·(1-α) + fg·α`, fully opaque. The anti-aliased edges blend into the
/// UI background exactly as the on-screen background does, and the result is a
/// flat duotone (one accent color), never the source art's clashing palette.
fn apply_silhouette(image: DynamicImage, tint: SvgTint) -> DynamicImage {
    let mut rgba = image.to_rgba8();
    for px in rgba.pixels_mut() {
        let a = px[3] as f32 / 255.0;
        for c in 0..3 {
            px[c] = (tint.bg[c] as f32 * (1.0 - a) + tint.fg[c] as f32 * a).round() as u8;
        }
        px[3] = 255;
    }
    DynamicImage::ImageRgba8(rgba)
}

/// Wrap serialized SVG markup as a self-contained `data:` URL. Inline `<svg>`
/// elements are rewritten to `<img src=…>` carrying this so they reuse the
/// whole `<img>` decode/cache/reflow/tint pipeline (an inline vector has no URL
/// of its own). base64 keeps the payload safe inside an HTML `src` attribute
/// (the markup is full of `"`/`<`/`>`).
pub(crate) fn svg_data_url(svg: &str) -> String {
    format!(
        "data:image/svg+xml;base64,{}",
        base64_encode(svg.as_bytes())
    )
}

/// The raw bytes of a `data:` URL — base64 or percent/plain payload. Lets the
/// image loader render inline SVG (and any `data:image/*`) without a fetch.
pub(crate) fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        base64_decode(payload)
    } else {
        Some(percent_decode(payload))
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        let mut pad = 0;
        for i in 0..4 {
            n <<= 6;
            match chunk.get(i) {
                Some(b'=') | None => pad += 1,
                Some(&c) => n |= val(c)?,
            }
        }
        out.push((n >> 16 & 0xff) as u8);
        if pad < 2 {
            out.push((n >> 8 & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    out
}

/// Sniff the image format from magic bytes or a bounded SVG/XML prologue.
/// This remains deliberately cheap because HTTP uses it on the UI thread for
/// application/octet-stream responses; full XML validation happens off-thread.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    image::guess_format(bytes)
        .ok()
        .map(|f| f.to_mime_type())
        .or_else(|| looks_like_svg(bytes).then_some(SVG_MIME))
}

/// Return intrinsic image metadata without exposing an SVG renderer tree to the
/// rest of the app. Raster images retain the existing decode-first behavior;
/// SVG is parsed in secure static mode and reports its CSS-pixel viewport.
pub fn info(bytes: &[u8]) -> Result<ImageInfo, String> {
    if image::guess_format(bytes).is_ok() {
        let (image, mime) = decode_raster(bytes)?;
        return Ok(ImageInfo {
            width: image.width(),
            height: image.height(),
            mime,
        });
    }

    let svg = parse_svg(bytes)?;
    Ok(svg.info)
}

/// Decode raw bytes into pixels, returning the detected MIME type too.
/// Animated raster formats decode to their first frame. SVG uses its intrinsic
/// viewport, reduced when necessary to stay inside the pixmap allocation cap.
/// Viewer and inline-image callers should prefer encode_bytes so SVG is
/// rasterized at the actual terminal box instead of this intrinsic fallback.
#[cfg(test)]
pub fn decode(bytes: &[u8]) -> Result<(DynamicImage, &'static str), String> {
    if image::guess_format(bytes).is_ok() {
        return decode_raster(bytes);
    }

    let svg = parse_svg(bytes)?;
    let image = rasterize_svg(&svg.tree, svg.info.width, svg.info.height, false)?;
    Ok((image, SVG_MIME))
}

fn decode_raster(bytes: &[u8]) -> Result<(DynamicImage, &'static str), String> {
    let format =
        image::guess_format(bytes).map_err(|_| String::from("unrecognized image format"))?;
    let mime = format.to_mime_type();
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| format!("decode: {e}"))?;
    Ok((image, mime))
}

/// Parse a top-level SVG in the SVG 2 secure static processing mode used for
/// image resources: scripting/animation are absent in usvg, and our resolver
/// permits embedded data URLs but rejects every external string reference.
/// This is also used for the deliberately static standalone image viewer.
struct SvgImage {
    tree: resvg::usvg::Tree,
    info: ImageInfo,
}

fn parse_svg(bytes: &[u8]) -> Result<SvgImage, String> {
    let data = bounded_svg_data(bytes)?;
    let text = std::str::from_utf8(&data).map_err(|_| String::from("SVG is not UTF-8"))?;
    let xml =
        resvg::usvg::roxmltree::Document::parse(text).map_err(|e| format!("svg XML parse: {e}"))?;
    let root = xml.root_element();
    if root.tag_name().name() != "svg" {
        return Err(String::from("SVG document root is not <svg>"));
    }

    // CSS Images default sizing for a replaced image: natural dimensions come
    // from definite root width/height; a viewBox contributes a natural ratio;
    // missing dimensions use the 300x150 default object size constrained by
    // that ratio. Wrapping the original root in that concrete viewport also
    // prevents usvg's no-viewBox fallback from shrinking to the artwork bbox.
    let width = root.attribute("width").and_then(svg_length_px);
    let height = root.attribute("height").and_then(svg_length_px);
    let ratio = root
        .attribute("viewBox")
        .and_then(view_box_ratio)
        .or_else(|| Some(width? / height?));
    let (width, height) = concrete_object_size(width, height, ratio)?;
    let original_root = &text[root.range()];
    let wrapped = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">{original_root}</svg>"#
    );
    let tree = resvg::usvg::Tree::from_str(&wrapped, &secure_svg_options())
        .map_err(|e| format!("svg parse: {e}"))?;
    Ok(SvgImage {
        tree,
        info: ImageInfo {
            width: css_pixels(width),
            height: css_pixels(height),
            mime: SVG_MIME,
        },
    })
}

fn svg_length_px(value: &str) -> Option<f32> {
    use svgtypes::LengthUnit as Unit;

    let length: svgtypes::Length = value.trim().parse().ok()?;
    let number = length.number as f32;
    let px = match length.unit {
        Unit::None | Unit::Px => number,
        Unit::Em => number * 16.0,
        Unit::Ex => number * 8.0,
        Unit::In => number * 96.0,
        Unit::Cm => number * (96.0 / 2.54),
        Unit::Mm => number * (96.0 / 25.4),
        Unit::Pt => number * (96.0 / 72.0),
        Unit::Pc => number * 16.0,
        Unit::Percent => return None,
    };
    (px.is_finite() && px > 0.0).then_some(px)
}

fn view_box_ratio(value: &str) -> Option<f32> {
    let values: Vec<f32> = value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|v| !v.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() != 4 || values[2] <= 0.0 || values[3] <= 0.0 {
        return None;
    }
    let ratio = values[2] / values[3];
    ratio.is_finite().then_some(ratio)
}

fn concrete_object_size(
    width: Option<f32>,
    height: Option<f32>,
    ratio: Option<f32>,
) -> Result<(f32, f32), String> {
    let (width, height) = match (width, height, ratio.filter(|r| *r > 0.0)) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(r)) => (w, w / r),
        (None, Some(h), Some(r)) => (h * r, h),
        (Some(w), None, None) => (w, 150.0),
        (None, Some(h), None) => (300.0, h),
        (None, None, Some(r)) if r >= 2.0 => (300.0, 300.0 / r),
        (None, None, Some(r)) => (150.0 * r, 150.0),
        (None, None, None) => (300.0, 150.0),
    };
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(String::from("invalid SVG intrinsic size"));
    }
    Ok((width, height))
}

fn secure_svg_options() -> resvg::usvg::Options<'static> {
    let mut options = resvg::usvg::Options::default();
    // The HTML/SVG default object size when an image has no intrinsic width or
    // height. usvg's tool-oriented default is 100x100, so set the browser value.
    options.default_size = resvg::usvg::Size::from_wh(300.0, 150.0).unwrap();
    options.font_size = 16.0;
    options.resources_dir = None;
    options.fontdb = svg_fontdb().clone();
    options.image_href_resolver = resvg::usvg::ImageHrefResolver {
        resolve_data: Box::new(|mime, data, options| {
            if data.len() > MAX_SVG_BYTES {
                return None;
            }
            let nested_svg =
                mime == SVG_MIME || (mime == "text/plain" && looks_like_svg(data.as_slice()));
            if nested_svg {
                // Apply the same SVGZ expansion cap recursively to data: SVGs.
                let xml = bounded_svg_data(data.as_slice()).ok()?.into_owned();
                return (resvg::usvg::ImageHrefResolver::default_data_resolver())(
                    SVG_MIME,
                    Arc::new(xml),
                    options,
                );
            }
            (resvg::usvg::ImageHrefResolver::default_data_resolver())(mime, data, options)
        }),
        // The usvg default treats arbitrary strings as local paths. Browser
        // image resources must not read files or fetch external subresources.
        resolve_string: Box::new(|_, _| None),
    };
    options
}

fn svg_fontdb() -> &'static Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
}

fn bounded_svg_data(bytes: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    bounded_svg_data_with_limit(bytes, MAX_SVG_BYTES)
}

fn bounded_svg_data_with_limit(bytes: &[u8], limit: usize) -> Result<Cow<'_, [u8]>, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut limited = decoder.take((limit + 1) as u64);
        let mut out = Vec::new();
        limited
            .read_to_end(&mut out)
            .map_err(|e| format!("svgz decode: {e}"))?;
        if out.len() > limit {
            return Err(format!("svgz expands beyond {limit} bytes"));
        }
        Ok(Cow::Owned(out))
    } else if bytes.len() > limit {
        Err(format!("svg exceeds {limit} bytes"))
    } else {
        Ok(Cow::Borrowed(bytes))
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        // Only inflate enough to inspect the XML prologue. Full decoding, when
        // selected, uses bounded_svg_data and its stricter expanded-size cap.
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut limited = decoder.take(64 * 1024);
        let mut prefix = Vec::new();
        return limited.read_to_end(&mut prefix).is_ok() && looks_like_svg_xml(&prefix);
    }
    looks_like_svg_xml(bytes)
}

fn looks_like_svg_xml(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text).trim_start();

    loop {
        if rest.starts_with("<?") {
            let Some(end) = rest.find("?>") else {
                return false;
            };
            rest = rest[end + 2..].trim_start();
        } else if rest.starts_with("<!--") {
            let Some(end) = rest.find("-->") else {
                return false;
            };
            rest = rest[end + 3..].trim_start();
        } else if rest.starts_with("<!") {
            let Some(end) = rest.find('>') else {
                return false;
            };
            rest = rest[end + 1..].trim_start();
        } else {
            break;
        }
    }

    let Some(after) = rest.strip_prefix('<') else {
        return false;
    };
    let name = after
        .split(|c: char| c.is_ascii_whitespace() || matches!(c, '/' | '>'))
        .next()
        .unwrap_or("");
    name.rsplit(':').next() == Some("svg")
}

fn css_pixels(value: f32) -> u32 {
    value.ceil().max(1.0) as u32
}

fn bounded_svg_size(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = (width.max(1) as f64, height.max(1) as f64);
    let dimension_scale = (MAX_DIMENSION as f64 / width)
        .min(MAX_DIMENSION as f64 / height)
        .min(1.0);
    let pixel_scale = ((MAX_SVG_PIXELS as f64 / (width * height)).sqrt()).min(1.0);
    let scale = dimension_scale.min(pixel_scale);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}

fn rasterize_svg(
    tree: &resvg::usvg::Tree,
    target_width: u32,
    target_height: u32,
    crop: bool,
) -> Result<DynamicImage, String> {
    let (width, height) = bounded_svg_size(target_width, target_height);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| String::from("svg target is too large"))?;
    let source = tree.size();
    let sx = width as f32 / source.width();
    let sy = height as f32 / source.height();
    let scale = if crop { sx.max(sy) } else { sx.min(sy) };
    let tx = (width as f32 - source.width() * scale) / 2.0;
    let ty = (height as f32 - source.height() * scale) / 2.0;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);
    resvg::render(tree, transform, &mut pixmap.as_mut());

    let pixels = pixmap.take_demultiplied();
    let image = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| String::from("invalid SVG raster buffer"))?;
    Ok(DynamicImage::ImageRgba8(image))
}

fn decode_for_box(
    bytes: &[u8],
    picker: &Picker,
    size: Size,
    crop: bool,
    tint: Option<SvgTint>,
) -> Result<(DynamicImage, ImageInfo, bool), String> {
    if image::guess_format(bytes).is_ok() {
        let (image, mime) = decode_raster(bytes)?;
        let info = ImageInfo {
            width: image.width(),
            height: image.height(),
            mime,
        };
        return Ok((image, info, false));
    }

    let svg = parse_svg(bytes)?;
    let font = picker.font_size();
    let width = u32::from(size.width.max(1)) * u32::from(font.width.max(1));
    let height = u32::from(size.height.max(1)) * u32::from(font.height.max(1));
    let image = rasterize_svg(&svg.tree, width, height, crop)?;
    // Recolor to the UI palette (silhouette), unless a caller asked for the
    // raw render. Raster images never reach here with a tint that matters.
    let image = match tint {
        Some(t) => apply_silhouette(image, t),
        None => image,
    };
    Ok((image, svg.info, true))
}

/// Decode and encode an image for a fixed terminal-cell box. SVG is rendered
/// directly into that box, preserving vector quality at every viewer resize.
pub fn encode_bytes(
    picker: &Picker,
    bytes: &[u8],
    size: Size,
    crop: bool,
    tint: Option<SvgTint>,
) -> Result<(Protocol, ImageInfo), String> {
    let (image, info, svg_fitted) = decode_for_box(bytes, picker, size, crop, tint)?;
    encode(picker, image, size, crop && !svg_fitted).map(|protocol| (protocol, info))
}

/// Decode and encode an inline image once for its scroll-independent cell box.
pub fn encode_sliced_bytes(
    picker: &Picker,
    bytes: &[u8],
    size: Size,
    crop: bool,
    tint: Option<SvgTint>,
) -> Result<(SlicedProtocol, ImageInfo), String> {
    let (image, info, svg_fitted) = decode_for_box(bytes, picker, size, crop, tint)?;
    encode_sliced(picker, image, size, crop && !svg_fitted).map(|protocol| (protocol, info))
}

/// Encode an image to fill a panel of `size` cells. `crop` selects the CSS
/// `object-fit` behaviour: `false` → `Resize::Fit` (contain — scale to fit,
/// preserving aspect, letterboxing); `true` → `Resize::Crop` (cover — fill the
/// box, clipping overflow). The result is a fixed `Protocol` for the stateless
/// `Image` widget; re-encode when the panel size, crop mode, or protocol type
/// changes.
pub fn encode(
    picker: &Picker,
    image: DynamicImage,
    size: Size,
    crop: bool,
) -> Result<Protocol, String> {
    let resize = if crop {
        Resize::Crop(None)
    } else {
        Resize::Fit(Some(FilterType::Lanczos3))
    };
    picker
        .new_protocol(image, size, resize)
        .map_err(|e| e.to_string())
}

/// Encode an image ONCE into a `SlicedProtocol` for a `size`-cell box. The
/// returned protocol is scroll-independent: the renderer (`ratatui_image::sliced
/// ::SlicedImage`) clips it to any vertical slice at draw time — for sixel it
/// strips the format's six-pixel "bands", so scrolling a tall inline image past
/// the viewport edge never re-encodes it (the old per-slice `encode_slice` did,
/// which both re-decoded per line and made a partly-visible image render at a
/// different scale than a fully-visible one). `crop` selects the CSS
/// `object-fit`: `false` → contain (fit preserving aspect, never upscaling,
/// transparent letterbox); `true` → cover (fill the box preserving aspect,
/// cropping the overflow). Re-encode only when the cell box or crop mode changes.
pub fn encode_sliced(
    picker: &Picker,
    image: DynamicImage,
    size: Size,
    crop: bool,
) -> Result<SlicedProtocol, String> {
    if crop {
        // object-fit: cover — scale to fill the box preserving aspect and crop
        // the overflow, then slice 1:1 (the image already matches the box, so
        // `Resize::Fit(None)` neither rescales nor pads).
        let f = picker.font_size();
        let (fw, fh) = (
            u32::from(size.width).max(1) * u32::from(f.width).max(1),
            u32::from(size.height).max(1) * u32::from(f.height).max(1),
        );
        let filled = image.resize_to_fill(fw, fh, FilterType::Lanczos3);
        SlicedProtocol::new_with_resize(picker, filled, size, Resize::Fit(None))
            .map_err(|e| e.to_string())
    } else {
        // object-fit: contain — the library fits (preserving aspect, never
        // upscaling) and pads the slack transparently.
        SlicedProtocol::new_with_resize(
            picker,
            image,
            size,
            Resize::Fit(Some(FilterType::Lanczos3)),
        )
        .map_err(|e| e.to_string())
    }
}

/// A tiny PNG made with the same crate that decodes it (test fixture,
/// also used by the app-level viewer tests).
#[cfg(test)]
pub(crate) fn red_png() -> Vec<u8> {
    let pixels = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView as _;

    fn sample_svg() -> Vec<u8> {
        br##"<?xml version="1.0"?>
            <svg xmlns="http://www.w3.org/2000/svg"
                 width="80" height="32" viewBox="0 0 80 32"
                 onload="this.setAttribute('width','1')">
              <script>document.documentElement.setAttribute('height', '1')</script>
              <rect width="80" height="32" fill="#ff0000"/>
            </svg>"##
            .to_vec()
    }

    #[test]
    fn static_svg_is_sniffed_sized_and_rasterized_for_the_terminal_box() {
        let svg = sample_svg();
        assert_eq!(sniff(&svg), Some(SVG_MIME));
        let metadata = info(&svg).unwrap();
        assert_eq!(
            metadata,
            ImageInfo {
                width: 80,
                height: 32,
                mime: SVG_MIME
            }
        );

        // Script and event attributes are inert in usvg's static tree: the red
        // shape renders at the declared viewport rather than either scripted 1px
        // mutation taking effect.
        let (intrinsic, mime) = decode(&svg).unwrap();
        assert_eq!(mime, SVG_MIME);
        assert_eq!(intrinsic.dimensions(), (80, 32));
        assert_eq!(intrinsic.to_rgba8().get_pixel(40, 16).0, [255, 0, 0, 255]);

        // Unlike the intrinsic fallback, production renders SVG at the actual
        // terminal box so a tiny vector remains sharp when CSS/viewer size grows.
        let picker = Picker::halfblocks();
        let cells = Size::new(20, 4);
        let (scaled, scaled_info, svg_fitted) =
            decode_for_box(&svg, &picker, cells, false, None).unwrap();
        let font = picker.font_size();
        assert!(svg_fitted);
        assert_eq!(scaled_info, metadata);
        assert_eq!(
            scaled.dimensions(),
            (
                u32::from(cells.width) * u32::from(font.width),
                u32::from(cells.height) * u32::from(font.height)
            )
        );
        let (protocol, protocol_info) = encode_bytes(&picker, &svg, cells, false, None).unwrap();
        assert_eq!(protocol_info, metadata);
        assert!(protocol.size().width <= cells.width);
        assert!(protocol.size().height <= cells.height);
    }

    #[test]
    fn svg_silhouette_recolors_to_the_tint_over_the_background() {
        // A red rect SVG, tinted with a cyan-on-near-black silhouette: the
        // covered pixels become the tint fg (NOT the source red), transparent
        // ones become the bg, and the result is fully opaque.
        let svg = sample_svg();
        let picker = Picker::halfblocks();
        let cells = Size::new(20, 8);
        let tint = SvgTint {
            fg: [0x00, 0xff, 0xf9],
            bg: [0x0b, 0x02, 0x21],
        };
        let (image, _, fitted) = decode_for_box(&svg, &picker, cells, false, Some(tint)).unwrap();
        assert!(fitted);
        let rgba = image.to_rgba8();
        // Center is inside the (letterboxed) rect → fully-covered → tint fg.
        let (cx, cy) = (rgba.width() / 2, rgba.height() / 2);
        assert_eq!(rgba.get_pixel(cx, cy).0, [0x00, 0xff, 0xf9, 0xff]);
        // The raw render keeps the source red — proving the recolor is the tint,
        // not a coincidence.
        let (raw, _, _) = decode_for_box(&svg, &picker, cells, false, None).unwrap();
        assert_eq!(raw.to_rgba8().get_pixel(cx, cy).0[0], 0xff); // red channel hot
        assert_eq!(raw.to_rgba8().get_pixel(cx, cy).0[1], 0x00); // green cold
    }

    #[test]
    fn base64_and_data_urls_round_trip() {
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"hello, world",
            &[0u8, 255, 1, 254, 127, 128],
        ] {
            assert_eq!(base64_decode(&base64_encode(sample)).unwrap(), sample);
        }
        // An inline-SVG data URL decodes back to the exact markup, and a
        // percent-encoded (non-base64) data URL is handled too.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0h4v4z"/></svg>"#;
        let url = svg_data_url(svg);
        assert!(url.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(decode_data_url(&url).unwrap(), svg.as_bytes());
        assert_eq!(
            decode_data_url("data:image/svg+xml,%3Csvg%3E%3C/svg%3E").unwrap(),
            b"<svg></svg>"
        );
    }

    #[test]
    fn secure_static_mode_blocks_external_references_but_keeps_data_images() {
        let options = secure_svg_options();
        assert!(
            (options.image_href_resolver.resolve_string)("Cargo.toml", &options).is_none(),
            "relative paths must never reach usvg's file resolver"
        );
        assert!(
            (options.image_href_resolver.resolve_string)(
                "https://example.com/tracker.png",
                &options
            )
            .is_none(),
            "SVG image rendering must not start its own network fetches"
        );
        assert!(
            (options.image_href_resolver.resolve_data)("image/png", Arc::new(red_png()), &options)
                .is_some(),
            "embedded data: images are permitted in secure static mode"
        );
    }

    #[test]
    fn svgz_and_default_object_size_are_supported_with_an_expansion_cap() {
        use std::io::Write as _;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&sample_svg()).unwrap();
        let svgz = encoder.finish().unwrap();
        assert_eq!(sniff(&svgz), Some(SVG_MIME));
        assert_eq!(info(&svgz).unwrap().width, 80);

        let defaulted =
            br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#;
        let defaulted = info(defaulted).unwrap();
        assert_eq!((defaulted.width, defaulted.height), (300, 150));

        let expanded = vec![b'x'; 257];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&expanded).unwrap();
        let oversized_svgz = encoder.finish().unwrap();
        assert!(bounded_svg_data_with_limit(&oversized_svgz, 256).is_err());
        assert!(bounded_svg_data_with_limit(&expanded, 256).is_err());
    }

    #[test]
    fn decodes_sniffed_images_and_rejects_garbage() {
        let (image, mime) = decode(&red_png()).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!((image.width(), image.height()), (4, 4));

        assert!(sniff(b"<html>not pixels</html>").is_none());
        assert!(decode(b"<html>not pixels</html>").is_err());
    }

    #[test]
    fn encodes_to_fit_with_halfblocks() {
        let (image, _) = decode(&red_png()).unwrap();
        let picker = Picker::halfblocks();
        let protocol = encode(&picker, image, Size::new(20, 10), false).unwrap();
        let size = protocol.size();
        assert!(size.width > 0 && size.width <= 20);
        assert!(size.height > 0 && size.height <= 10);
    }

    #[test]
    fn encode_sliced_encodes_once_for_the_whole_box() {
        // One scroll-independent encode for the whole box; the renderer slices
        // it at draw time (so partial visibility never re-encodes or rescales).
        let (image, _) = decode(&red_png()).unwrap();
        let picker = Picker::halfblocks();
        let proto = encode_sliced(&picker, image, Size::new(20, 10), false).unwrap();
        let size = proto.size();
        assert!(size.width > 0 && size.width <= 20);
        assert!(size.height > 0 && size.height <= 10);
    }

    /// Tall photographic-ish test image: a gradient plus per-pixel variation so
    /// the sixel payload is dense (a flat fill compresses to almost nothing and
    /// would understate the cost).
    #[cfg(test)]
    fn tall_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        // Smooth two-axis gradient (banner/screenshot-like), no high-frequency
        // noise — a realistic lower bound on sixel density.
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            let b = (((x + y) * 255) / (w + h)) as u8;
            *px = image::Rgb([r, g, b]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// Manual: `cargo test --release image_scroll_bench -- --ignored --nocapture`.
    /// Compares the per-frame *main-thread draw cost* of scrolling a tall inline
    /// image three ways, all forced to sixel (foot's protocol):
    ///   A. current `SlicedImage` (encode once, slice the cached sixel per frame),
    ///   B. old static `Image` blit (render the whole pre-encoded protocol — the
    ///      pre-partial "only draw when fully visible" path),
    ///   C. a #2-style per-scroll re-encode (crop the visible pixel rect + encode
    ///      a fresh sixel each frame — the hand-rolled slice decoder).
    /// Reports one-time encode cost, per-frame draw cost, and emitted sixel bytes.
    #[test]
    #[ignore = "manual perf measurement; run with --release --nocapture"]
    fn image_scroll_bench() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        use ratatui_image::picker::ProtocolType;
        use ratatui_image::sliced::{SignedPosition, SlicedImage};
        use std::time::Instant;

        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Sixel);
        let f = picker.font_size();
        // A tall banner: full terminal width, several screens tall.
        let cols: u16 = 60;
        let rows: u16 = 150;
        let vh: u16 = 40; // viewport height in cells
        let (iw, ih) = (
            u32::from(cols) * u32::from(f.width),
            u32::from(rows) * u32::from(f.height),
        );
        let png = tall_png(iw, ih);
        let (image, _) = decode(&png).unwrap();
        let box_size = Size::new(cols, rows);
        eprintln!(
            "image {iw}x{ih}px -> {cols}x{rows} cells, font {}x{}, viewport {vh} rows",
            f.width, f.height
        );

        // --- one-time encode costs ---
        let t = Instant::now();
        let sliced = encode_sliced(&picker, image.clone(), box_size, false).unwrap();
        eprintln!("A encode_sliced (once): {:?}", t.elapsed());

        let t = Instant::now();
        let proto = encode(&picker, image.clone(), box_size, false).unwrap();
        eprintln!("B encode (once):        {:?}", t.elapsed());

        let scrolls: Vec<i16> = (0..=(rows - vh) as i16).collect();
        let reps = 20;
        let area = Rect::new(0, 0, cols, vh);

        // --- A: current SlicedImage, per-frame slice of the cached sixel ---
        let mut payload_a = 0usize;
        let t = Instant::now();
        for _ in 0..reps {
            for &s in &scrolls {
                let mut buf = Buffer::empty(area);
                let pos = SignedPosition::from((0, -s));
                SlicedImage::new(&sliced, pos).render(area, &mut buf);
                payload_a = buf[(0, 0)].symbol().len().max(payload_a);
            }
        }
        let a_total = t.elapsed();
        let a_frames = reps * scrolls.len();
        eprintln!(
            "A SlicedImage draw:  {:>8.3} ms/frame  (peak {payload_a} sixel bytes/frame)",
            a_total.as_secs_f64() * 1000.0 / a_frames as f64
        );

        // --- B: old static blit of the whole pre-encoded protocol ---
        // The pre-partial path only drew a fully-visible image; per scroll it
        // re-blits the whole protocol string into the buffer (no slicing).
        let full = Rect::new(0, 0, cols, rows);
        let t = Instant::now();
        for _ in 0..reps {
            for _ in &scrolls {
                let mut buf = Buffer::empty(full);
                ratatui_image::Image::new(&proto).render(full, &mut buf);
            }
        }
        let b_total = t.elapsed();
        eprintln!(
            "B static Image blit: {:>8.3} ms/frame",
            b_total.as_secs_f64() * 1000.0 / (reps * scrolls.len()) as f64
        );

        // --- C: #2-style per-scroll re-encode of the visible pixel slice ---
        let fh = u32::from(f.height);
        let mut payload_c = 0usize;
        let t = Instant::now();
        for &s in &scrolls {
            let y0 = (s.max(0) as u32) * fh;
            let slice_h = u32::from(vh) * fh;
            let cropped = image.crop_imm(
                0,
                y0.min(ih.saturating_sub(1)),
                iw,
                slice_h.min(ih - y0.min(ih - 1)),
            );
            let p = encode(&picker, cropped, Size::new(cols, vh), false).unwrap();
            let mut buf = Buffer::empty(area);
            ratatui_image::Image::new(&p).render(area, &mut buf);
            payload_c = buf[(0, 0)].symbol().len().max(payload_c);
        }
        let c_total = t.elapsed();
        eprintln!(
            "C #2 re-encode/frame: {:>8.3} ms/frame  (peak {payload_c} sixel bytes/frame)",
            c_total.as_secs_f64() * 1000.0 / scrolls.len() as f64
        );
    }

    #[test]
    fn encodes_with_crop_to_cover() {
        // object-fit: cover crops to fill the box (here a wide box from a
        // square source) rather than letterboxing.
        let (image, _) = decode(&red_png()).unwrap();
        let picker = Picker::halfblocks();
        let protocol = encode(&picker, image, Size::new(20, 4), true).unwrap();
        let size = protocol.size();
        assert!(size.width > 0 && size.height > 0);
    }
}
