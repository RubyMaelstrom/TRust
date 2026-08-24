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
use std::sync::Arc;
use std::time::Duration;

use image::{AnimationDecoder as _, DynamicImage, ImageDecoder as _};
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
    /// Whether the decoded raster carries any non-opaque pixel (a real
    /// transparency, not merely an alpha channel that is fully 255). Layout's
    /// overlap compositor (LAYOUT_OVERHAUL_PLAN.md P8) reads this to decide
    /// whether an image painted OVER another must be alpha-composited into one
    /// emission (transparent → the lower image shows through its holes) or can
    /// stay a separate, cheap opaque overwrite. SVG is silhouette-tinted to a
    /// fully opaque duotone, so it reports `false`.
    pub has_alpha: bool,
}

/// Compressed source for a raster animation.  The desktop keeps this bounded
/// separately from its one-current-frame decoded image cache, and constructs a
/// sequential decoder only while the image is visible.  Keeping the encoded
/// source instead of every full-canvas frame is important for GIF/APNG/WebP:
/// their disposal algorithms can make every returned frame canvas-sized even
/// when the source changes only a few pixels.
#[derive(Clone, Debug)]
pub struct GraphicalAnimation {
    bytes: Arc<[u8]>,
    format: AnimatedRasterFormat,
    loop_count_override: Option<AnimationLoopCount>,
    canvas_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnimatedRasterFormat {
    Gif,
    Png,
    WebP,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationLoopCount {
    Infinite,
    Finite(u32),
}

impl GraphicalAnimation {
    pub fn encoded_len(&self) -> usize {
        self.bytes.len()
    }

    /// Conservative persistent decoder-canvas estimate. GIF/APNG disposal can
    /// require a current and previous canvas in addition to the emitted frame.
    pub fn decoder_working_set_bytes(&self) -> usize {
        self.canvas_bytes.saturating_mul(3)
    }

    /// Start one play at its first frame.  Format decoders implement the
    /// normative logical-canvas blend/disposal algorithms; callers only own
    /// presentation timing and loop restarts.
    pub fn decoder(&self) -> Result<GraphicalAnimationDecoder, String> {
        animation_decoder(
            self.format,
            Arc::clone(&self.bytes),
            self.loop_count_override,
        )
    }
}

/// The first display frame and, for a genuine multi-frame resource, its
/// restartable compressed animation source.
#[derive(Debug)]
pub struct DecodedGraphicalImage {
    pub image: crate::render::ImageResource,
    pub animation: Option<GraphicalAnimation>,
}

pub struct GraphicalAnimationFrame {
    pub image: crate::render::ImageResource,
    /// How long this frame is displayed before advancing to the next one.
    pub delay: Duration,
}

/// A single sequential play. `image::Frames` deliberately erases `Send`; the
/// desktop constructs and owns this value exclusively on its one animation
/// worker thread.
pub struct GraphicalAnimationDecoder {
    frames: image::Frames<'static>,
    loop_count: AnimationLoopCount,
}

impl GraphicalAnimationDecoder {
    pub fn loop_count(&self) -> AnimationLoopCount {
        self.loop_count
    }

    pub fn next_frame(&mut self) -> Result<Option<GraphicalAnimationFrame>, String> {
        let Some(frame) = self.frames.next() else {
            return Ok(None);
        };
        let frame = frame.map_err(|error| format!("animation decode: {error}"))?;
        let delay = frame.delay().into();
        let rgba = frame.into_buffer();
        let has_alpha = rgba.pixels().any(|pixel| pixel[3] < 255);
        Ok(Some(GraphicalAnimationFrame {
            image: crate::render::ImageResource {
                width: rgba.width(),
                height: rgba.height(),
                rgba: Arc::from(rgba.into_raw()),
                has_alpha,
            },
            delay,
        }))
    }
}

/// Whether `image` has any genuinely transparent pixel. A color type without an
/// alpha channel is opaque outright (no scan); otherwise the alpha channel is
/// scanned with an early exit on the first non-opaque pixel — so an opaque RGBA
/// PNG costs a scan that a browser would also pay, and a badge with transparent
/// corners exits almost immediately.
fn image_has_alpha(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgba8(buf) => buf.pixels().any(|p| p[3] < 255),
        DynamicImage::ImageLumaA8(buf) => buf.pixels().any(|p| p[1] < 255),
        DynamicImage::ImageRgba16(buf) => buf.pixels().any(|p| p[3] < u16::MAX),
        DynamicImage::ImageLumaA16(buf) => buf.pixels().any(|p| p[1] < u16::MAX),
        DynamicImage::ImageRgba32F(buf) => buf.pixels().any(|p| p[3] < 1.0),
        // Any other variant has no alpha channel — opaque by construction.
        other => other.color().has_alpha() && other.to_rgba8().pixels().any(|p| p[3] < 255),
    }
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

pub(crate) fn base64_encode(data: &[u8]) -> String {
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
        // A `data:` URL body is percent-decoded only (RFC 2397 / WHATWG URL
        // "data: URL processing"); `+` is a literal plus, NOT a space. The
        // `+`→space rule belongs to application/x-www-form-urlencoded query
        // strings, not here — converting it corrupts JS (`+=`, `'+x'`) and any
        // SVG markup with a literal `+`.
        out.push(bytes[i]);
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
            has_alpha: image_has_alpha(&image),
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
pub fn decode(bytes: &[u8]) -> Result<(DynamicImage, &'static str), String> {
    if image::guess_format(bytes).is_ok() {
        return decode_raster(bytes);
    }

    let svg = parse_svg(bytes)?;
    let image = rasterize_svg(&svg.tree, svg.info.width, svg.info.height, false)?;
    Ok((image, SVG_MIME))
}

/// Decode an HTML/CSS image into the renderer-neutral resource format. Raster
/// formats keep their decoded alpha; SVG remains on the existing secure resvg
/// static-image path and is rasterized at its CSS intrinsic size. No terminal
/// tinting, cell sizing, or graphics protocol enters this path.
pub fn decode_graphical(bytes: &[u8]) -> Result<crate::render::ImageResource, String> {
    let (image, _) = decode(bytes)?;
    let has_alpha = image_has_alpha(&image);
    let rgba = image.to_rgba8();
    Ok(crate::render::ImageResource {
        width: rgba.width(),
        height: rgba.height(),
        rgba: Arc::from(rgba.into_raw()),
        has_alpha,
    })
}

/// Decode a fetched HTML/CSS image and publish the intrinsic metadata that
/// layout cannot obtain from an external resource URL alone.
///
/// SVG 2 §8.12 requires an outer `viewBox` to provide an intrinsic aspect
/// ratio while `auto`/percentage root dimensions provide no intrinsic width
/// or height. Keeping that distinction beside the decoded resource prevents
/// the SVG renderer's concrete fallback bitmap size from masquerading as an
/// intrinsic size during the subsequent CSS replaced-element layout pass.
pub fn decode_graphical_for_source(
    source: &str,
    bytes: &[u8],
) -> Result<crate::render::ImageResource, String> {
    let image = decode_graphical(bytes)?;
    record_svg_intrinsic_metadata(source, bytes);
    Ok(image)
}

/// Decode the first presentation frame and retain a restartable source when
/// the resource has at least two frames. This is the desktop `<img>` path;
/// terminal protocols intentionally continue to use the static first frame
/// because updating sixel/Kitty encodings would require a different renderer
/// contract.
///
/// HTML §4.8.4.3 says image resources should honor their animation. GIF89a
/// §23, PNG 3 §11.3.6, and the WebP Container Specification's “Animation” and
/// “Canvas Assembly from Frames” sections define per-frame delay and logical
/// canvas disposal. The `image` format decoders implement that assembly and
/// yield complete straight-alpha canvases, which keeps this layer from
/// accidentally reinterpreting format-specific disposal state.
pub fn decode_graphical_image_for_source(
    source: &str,
    bytes: Vec<u8>,
) -> Result<DecodedGraphicalImage, String> {
    let bytes: Arc<[u8]> = Arc::from(bytes);
    let animated_format = image::guess_format(&bytes)
        .ok()
        .and_then(animated_raster_format);
    if let Some(format) = animated_format {
        let mut animation = GraphicalAnimation {
            bytes: Arc::clone(&bytes),
            format,
            // GIF89a ends at its Trailer and defines no looping control. The
            // widespread NETSCAPE application extension is optional; in its
            // absence the standards-correct play count is one. `image` cannot
            // distinguish absent from its zero/infinite sentinel, so retain
            // the distinction from the parsed data stream here.
            loop_count_override: (format == AnimatedRasterFormat::Gif)
                .then(|| gif_loop_count(bytes.as_ref())),
            canvas_bytes: 0,
        };
        if animation_declared(format, Arc::clone(&animation.bytes))?
            && let Ok(mut decoder) = animation.decoder()
            && let Ok(Some(first)) = decoder.next_frame()
        {
            // A one-frame GIF/APNG is still a useful static image, but keeping
            // its compressed source and waking the scheduler can never change
            // the presentation. Peek once and retain only genuine animations.
            let animation = match decoder.next_frame() {
                Ok(Some(_)) => {
                    animation.canvas_bytes = first.image.rgba.len();
                    Some(animation)
                }
                Ok(None) | Err(_) => None,
            };
            record_svg_intrinsic_metadata(source, &bytes);
            return Ok(DecodedGraphicalImage {
                image: first.image,
                animation,
            });
        }
    }

    let image = decode_graphical_for_source(source, bytes.as_ref())?;
    Ok(DecodedGraphicalImage {
        image,
        animation: None,
    })
}

fn animated_raster_format(format: image::ImageFormat) -> Option<AnimatedRasterFormat> {
    match format {
        image::ImageFormat::Gif => Some(AnimatedRasterFormat::Gif),
        image::ImageFormat::Png => Some(AnimatedRasterFormat::Png),
        image::ImageFormat::WebP => Some(AnimatedRasterFormat::WebP),
        _ => None,
    }
}

fn raster_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits
}

fn animation_declared(format: AnimatedRasterFormat, bytes: Arc<[u8]>) -> Result<bool, String> {
    let cursor = std::io::Cursor::new(bytes);
    match format {
        // GIF89a has no stream-level animation bit. Multiple table-based image
        // blocks are the animation, so the bounded frame peek above decides.
        AnimatedRasterFormat::Gif => Ok(true),
        AnimatedRasterFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::with_limits(cursor, raster_limits())
                .map_err(|error| format!("PNG animation header: {error}"))?;
            decoder
                .is_apng()
                .map_err(|error| format!("PNG animation header: {error}"))
        }
        AnimatedRasterFormat::WebP => {
            let decoder = image::codecs::webp::WebPDecoder::new(cursor)
                .map_err(|error| format!("WebP animation header: {error}"))?;
            let (width, height) = decoder.dimensions();
            if width > MAX_DIMENSION || height > MAX_DIMENSION {
                return Err(format!(
                    "image dimensions {width}x{height} exceed {MAX_DIMENSION}px cap"
                ));
            }
            Ok(decoder.has_animation())
        }
    }
}

fn animation_loop_count(loop_count: image::metadata::LoopCount) -> AnimationLoopCount {
    match loop_count {
        image::metadata::LoopCount::Infinite => AnimationLoopCount::Infinite,
        image::metadata::LoopCount::Finite(count) => AnimationLoopCount::Finite(count.get()),
    }
}

/// Read the de-facto NETSCAPE/ANIMEXTS loop application extension without
/// searching arbitrary compressed image bytes for a magic substring. GIF89a
/// §26 makes application data a properly framed extension; walking those
/// blocks also lets absence retain the base format's one-pass behavior.
fn gif_loop_count(bytes: &[u8]) -> AnimationLoopCount {
    fn skip_sub_blocks(bytes: &[u8], position: &mut usize) -> bool {
        loop {
            let Some(&length) = bytes.get(*position) else {
                return false;
            };
            *position += 1;
            if length == 0 {
                return true;
            }
            let Some(next) = position.checked_add(usize::from(length)) else {
                return false;
            };
            if next > bytes.len() {
                return false;
            }
            *position = next;
        }
    }

    if bytes.len() < 13 || !matches!(&bytes[..6], b"GIF87a" | b"GIF89a") {
        return AnimationLoopCount::Finite(1);
    }
    let packed = bytes[10];
    let global_table = if packed & 0x80 != 0 {
        3usize << (usize::from(packed & 0x07) + 1)
    } else {
        0
    };
    let Some(mut position) = 13usize.checked_add(global_table) else {
        return AnimationLoopCount::Finite(1);
    };

    while let Some(&marker) = bytes.get(position) {
        position += 1;
        match marker {
            0x3b => break, // Trailer
            0x2c => {
                // Image Descriptor, optional local color table, LZW code size,
                // then data sub-blocks.
                let Some(descriptor_end) = position.checked_add(9) else {
                    break;
                };
                if descriptor_end > bytes.len() {
                    break;
                }
                let image_packed = bytes[position + 8];
                position = descriptor_end;
                if image_packed & 0x80 != 0 {
                    let table = 3usize << (usize::from(image_packed & 0x07) + 1);
                    let Some(next) = position.checked_add(table) else {
                        break;
                    };
                    position = next;
                }
                if position >= bytes.len() {
                    break;
                }
                position += 1;
                if !skip_sub_blocks(bytes, &mut position) {
                    break;
                }
            }
            0x21 => {
                let Some(&label) = bytes.get(position) else {
                    break;
                };
                position += 1;
                if label != 0xff {
                    if !skip_sub_blocks(bytes, &mut position) {
                        break;
                    }
                    continue;
                }

                let Some(&identifier_len) = bytes.get(position) else {
                    break;
                };
                position += 1;
                let Some(identifier_end) = position.checked_add(usize::from(identifier_len)) else {
                    break;
                };
                let Some(identifier) = bytes.get(position..identifier_end) else {
                    break;
                };
                position = identifier_end;
                let recognized = matches!(identifier, b"NETSCAPE2.0" | b"ANIMEXTS1.0");
                while let Some(&length) = bytes.get(position) {
                    position += 1;
                    if length == 0 {
                        break;
                    }
                    let Some(end) = position.checked_add(usize::from(length)) else {
                        return AnimationLoopCount::Finite(1);
                    };
                    let Some(data) = bytes.get(position..end) else {
                        return AnimationLoopCount::Finite(1);
                    };
                    if recognized && data.len() >= 3 && data[0] == 1 {
                        let count = u16::from_le_bytes([data[1], data[2]]);
                        return if count == 0 {
                            AnimationLoopCount::Infinite
                        } else {
                            AnimationLoopCount::Finite(u32::from(count))
                        };
                    }
                    position = end;
                }
            }
            _ => break,
        }
    }
    AnimationLoopCount::Finite(1)
}

fn animation_decoder(
    format: AnimatedRasterFormat,
    bytes: Arc<[u8]>,
    loop_count_override: Option<AnimationLoopCount>,
) -> Result<GraphicalAnimationDecoder, String> {
    match format {
        AnimatedRasterFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
                .map_err(|error| format!("GIF animation header: {error}"))?;
            decoder
                .set_limits(raster_limits())
                .map_err(|error| format!("GIF animation limits: {error}"))?;
            let loop_count =
                loop_count_override.unwrap_or_else(|| animation_loop_count(decoder.loop_count()));
            Ok(GraphicalAnimationDecoder {
                frames: decoder.into_frames(),
                loop_count,
            })
        }
        AnimatedRasterFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::with_limits(
                std::io::Cursor::new(bytes),
                raster_limits(),
            )
            .map_err(|error| format!("PNG animation header: {error}"))?;
            let decoder = decoder
                .apng()
                .map_err(|error| format!("PNG animation: {error}"))?;
            let loop_count = animation_loop_count(decoder.loop_count());
            Ok(GraphicalAnimationDecoder {
                frames: decoder.into_frames(),
                loop_count,
            })
        }
        AnimatedRasterFormat::WebP => {
            let decoder = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes))
                .map_err(|error| format!("WebP animation header: {error}"))?;
            let (width, height) = decoder.dimensions();
            if width > MAX_DIMENSION || height > MAX_DIMENSION {
                return Err(format!(
                    "image dimensions {width}x{height} exceed {MAX_DIMENSION}px cap"
                ));
            }
            let loop_count = animation_loop_count(decoder.loop_count());
            Ok(GraphicalAnimationDecoder {
                frames: decoder.into_frames(),
                loop_count,
            })
        }
    }
}

fn decode_raster(bytes: &[u8]) -> Result<(DynamicImage, &'static str), String> {
    let format =
        image::guess_format(bytes).map_err(|_| String::from("unrecognized image format"))?;
    let mime = format.to_mime_type();
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    reader.limits(raster_limits());
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
    let text = svg_without_external_doctype(text)?;
    let xml = resvg::usvg::roxmltree::Document::parse(text.as_ref())
        .map_err(|e| format!("svg XML parse: {e}"))?;
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
            // SVG rasterizes to an opaque silhouette (`apply_silhouette` forces
            // α=255), so it never composites as a transparent overlay.
            has_alpha: false,
        },
    })
}

/// Return XML markup with an external-only document type declaration removed.
///
/// XML 1.0 Fifth Edition, §2.8 permits a `doctypedecl`, while §5.1 allows a
/// non-validating processor not to read its external subset. SVG 1.1 assets in
/// the wild still commonly carry the published W3C external identifier. We do
/// exactly that here: validate the declaration, never resolve its system ID,
/// and pass the remaining document to the secure static SVG parser.
///
/// Internal subsets stay disabled. Besides not being needed for the external
/// SVG DTD marker, accepting them would enable document-authored entity
/// expansion before usvg sees the already bounded but otherwise untrusted XML.
fn svg_without_external_doctype(text: &str) -> Result<Cow<'_, str>, String> {
    let Some((start, end)) = external_doctype_range(text)? else {
        return Ok(Cow::Borrowed(text));
    };

    // Validate the original declaration with the XML parser before removing
    // it. `roxmltree` recognizes but does not fetch an external subset.
    let options = resvg::usvg::roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    resvg::usvg::roxmltree::Document::parse_with_options(text, options)
        .map_err(|e| format!("svg XML parse: {e}"))?;

    let mut stripped = String::with_capacity(text.len() - (end - start) + 1);
    stripped.push_str(&text[..start]);
    // Keep prologue and root tokens separated even for compact input such as
    // `<!DOCTYPE svg><svg ...>`.
    stripped.push('\n');
    stripped.push_str(&text[end..]);
    Ok(Cow::Owned(stripped))
}

/// Locate an external-only XML document type declaration in the prologue.
/// Returns the byte range including its closing `>`. A `[` outside a quoted
/// public/system identifier starts an internal subset and is rejected.
fn external_doctype_range(text: &str) -> Result<Option<(usize, usize)>, String> {
    let bytes = text.as_bytes();
    let mut cursor = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8();

    loop {
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }

        let tail = &text[cursor..];
        if let Some(after_open) = tail.strip_prefix("<!--") {
            let Some(close) = after_open.find("-->") else {
                return Ok(None); // the regular XML parser reports the syntax error
            };
            cursor += 4 + close + 3;
            continue;
        }
        if let Some(after_open) = tail.strip_prefix("<?") {
            let Some(close) = after_open.find("?>") else {
                return Ok(None); // the regular XML parser reports the syntax error
            };
            cursor += 2 + close + 2;
            continue;
        }
        if !tail.starts_with("<!DOCTYPE") {
            return Ok(None);
        }

        let start = cursor;
        cursor += "<!DOCTYPE".len();
        let mut quote = None;
        while let Some(&byte) = bytes.get(cursor) {
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
            } else {
                match byte {
                    b'\'' | b'"' => quote = Some(byte),
                    b'[' => {
                        return Err(String::from("SVG internal DTD subsets are not supported"));
                    }
                    b'>' => return Ok(Some((start, cursor + 1))),
                    _ => {}
                }
            }
            cursor += 1;
        }
        return Err(String::from("unterminated SVG document type declaration"));
    }
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

/// The intrinsic RATIO of an SVG that has an intrinsic ratio but NO intrinsic
/// width or height (referenced with only a `viewBox`) — the CSS 2.2
/// §10.3.2/§10.6.2 "rule 3" case. `None` when the bytes are not SVG, when the
/// root carries a real intrinsic width/height (a genuine intrinsic SIZE — the
/// decoder's natural dimensions handle that), or when there is no ratio.
///
/// Replaced sizing consults this so an auto/auto ratio-only image takes its
/// width from the containing block (the §10.3.2 block-constraint suggestion)
/// instead of the decoder's fabricated "default object size" (`150×150` for a
/// square viewBox), which otherwise renders such icons hugely oversized.
pub(crate) fn svg_bytes_ratio_only(bytes: &[u8]) -> Option<f32> {
    if !looks_like_svg(bytes) {
        return None;
    }
    let data = bounded_svg_data(bytes).ok()?;
    let text = std::str::from_utf8(&data).ok()?;
    let text = svg_without_external_doctype(text).ok()?;
    let doc = resvg::usvg::roxmltree::Document::parse(text.as_ref()).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "svg" {
        return None;
    }
    // A definite root width OR height is an intrinsic dimension → NOT rule 3
    // (the natural size the decoder reports already carries it).
    if root.attribute("width").and_then(svg_length_px).is_some()
        || root.attribute("height").and_then(svg_length_px).is_some()
    {
        return None;
    }
    root.attribute("viewBox").and_then(view_box_ratio)
}

/// `svg_bytes_ratio_only` for a `data:` SVG whose markup is carried in the URL
/// (an inline-rewritten `<svg>` or a page-authored data image). Synchronous —
/// the layout layer reads it without waiting on the decode pipeline. `None` for
/// any non-`data:`-SVG source.
pub(crate) fn svg_url_ratio_only(src: &str) -> Option<f32> {
    let src = src.trim();
    if src.len() < 14 || !src.as_bytes()[..14].eq_ignore_ascii_case(b"data:image/svg") {
        return None;
    }
    svg_bytes_ratio_only(&decode_data_url(src)?)
}

/// Process-global cache of EXTERNAL image URLs whose SVG is ratio-only (a
/// `viewBox` but no intrinsic width/height — §10.3.2 rule 3). An SVG's intrinsic
/// ratio is a property of the resource, not the page, so it is keyed by URL and
/// shared across the session like the connection pool. Refreshed by the image
/// loader on decode (`record_svg_intrinsic_metadata`); read by replaced sizing
/// (`svg_ratio_only_get`) for `<img>` elements whose markup layout can't see
/// (only `data:` SVGs are read inline via `svg_url_ratio_only`).
static SVG_RATIO_ONLY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, f32>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Refresh the external-resource metadata after a successful decode. A URL
/// can be revalidated to different bytes, so a now-dimensioned SVG or raster
/// image must also clear an older ratio-only entry for the same URL.
pub(crate) fn record_svg_intrinsic_metadata(url: &str, bytes: &[u8]) {
    let ratio = svg_bytes_ratio_only(bytes);
    let mut ratios = SVG_RATIO_ONLY.lock().unwrap();
    match ratio {
        Some(ratio) if ratio.is_finite() && ratio > 0.0 => {
            ratios.insert(url.to_string(), ratio);
        }
        _ => {
            ratios.remove(url);
        }
    }
}

/// The recorded ratio-only ratio for a decoded external image URL, if any.
pub(crate) fn svg_ratio_only_get(url: &str) -> Option<f32> {
    SVG_RATIO_ONLY.lock().unwrap().get(url).copied()
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
    options.fontdb = crate::font_system::svg_fontdb();
    options.font_resolver = crate::font_system::svg_font_resolver();
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
            has_alpha: image_has_alpha(&image),
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
    pixelated: bool,
    tint: Option<SvgTint>,
) -> Result<(SlicedProtocol, ImageInfo), String> {
    let (image, info, svg_fitted) = decode_for_box(bytes, picker, size, crop, tint)?;
    encode_sliced(picker, image, size, crop && !svg_fitted, pixelated)
        .map(|protocol| (protocol, info))
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
/// `object-fit`: `false` → contain (scale to the box preserving aspect,
/// UPSCALING included, transparent letterbox); `true` → cover (fill the box
/// preserving aspect, cropping the overflow). Re-encode only when the cell box
/// or crop mode changes. Unlike the standalone VIEWER (which deliberately
/// never upscales past natural size — `encode` above), an inline image's box
/// is its CSS USED size: a page that sizes a small image up (Steam's 41px QR
/// GIF at `width:100%` of a 160px frame, `image-rendering:pixelated`) gets the
/// scaled render a browser gives it, not a natural-size thumbnail lost in a
/// big reserved box.
pub fn encode_sliced(
    picker: &Picker,
    image: DynamicImage,
    size: Size,
    crop: bool,
    pixelated: bool,
) -> Result<SlicedProtocol, String> {
    // CSS Images 3 §5.4 `image-rendering: pixelated`: scale with
    // nearest-neighbor so upscaled blocks stay hard-edged (Steam's 41px QR
    // GIF must stay machine-scannable); default stays the smooth Lanczos.
    let filter = if pixelated {
        FilterType::Nearest
    } else {
        FilterType::Lanczos3
    };
    if crop {
        // object-fit: cover — scale to fill the box preserving aspect and crop
        // the overflow, then slice 1:1 (the image already matches the box, so
        // `Resize::Fit(None)` neither rescales nor pads).
        let f = picker.font_size();
        let (fw, fh) = (
            u32::from(size.width).max(1) * u32::from(f.width).max(1),
            u32::from(size.height).max(1) * u32::from(f.height).max(1),
        );
        let filled = image.resize_to_fill(fw, fh, filter);
        SlicedProtocol::new_with_resize(picker, filled, size, Resize::Fit(None))
            .map_err(|e| e.to_string())
    } else {
        // object-fit: contain — scale to the box preserving aspect (UP or
        // down: `Resize::Scale`, vs `Fit` which never upscales), slack padded
        // transparently.
        SlicedProtocol::new_with_resize(picker, image, size, Resize::Scale(Some(filter)))
            .map_err(|e| e.to_string())
    }
}

/// One layer of an alpha-composite overlap group (LAYOUT_OVERHAUL_PLAN.md P8):
/// its source bytes, used cell box, cell offset within the union, and its
/// `object-fit`/`image-rendering`. `encode_composite` decodes+fits each and
/// alpha-blends them in order.
pub struct CompositeInput<'a> {
    pub bytes: &'a [u8],
    /// The layer's own cell box (used size).
    pub box_cells: Size,
    /// The layer's `(col, row)` offset within the union box, in cells.
    pub off_cells: (u16, u16),
    pub crop: bool,
    pub pixelated: bool,
}

/// Alpha-composite an overlap group into ONE `SlicedProtocol` (P8): allocate a
/// transparent union-sized RGBA canvas (union cells × the terminal font box),
/// decode+fit each layer to its own box exactly as `encode_sliced` would
/// (`object-fit` contain/cover, `image-rendering` filter, SVG silhouette-tinted
/// via `tint`), and `imageops::overlay` (source-over) it at its cell offset in
/// PAINT ORDER (bottom first) — so a lower image shows through an upper image's
/// transparent pixels. The composed canvas is already union-pixel-sized, so the
/// final encode neither rescales nor pads (`Resize::Fit(None)`). This is the one
/// place a terminal honors image-over-image alpha: it must happen before encode,
/// since two already-encoded opaque cell protocols cannot be blended at draw.
pub fn encode_composite(
    picker: &Picker,
    union: Size,
    layers: &[CompositeInput<'_>],
    tint: Option<SvgTint>,
) -> Result<SlicedProtocol, String> {
    let canvas = composite_canvas(picker, union, layers, tint)?;
    // The canvas is already union-pixel-sized, so the encode neither rescales
    // nor pads (`Resize::Fit(None)`); transparent gaps ride into the terminal.
    SlicedProtocol::new_with_resize(
        picker,
        DynamicImage::ImageRgba8(canvas),
        union,
        Resize::Fit(None),
    )
    .map_err(|e| e.to_string())
}

/// Build the composited union canvas (the alpha-blend core of
/// `encode_composite`, split out so the blend/offset math is unit-testable).
fn composite_canvas(
    picker: &Picker,
    union: Size,
    layers: &[CompositeInput<'_>],
    tint: Option<SvgTint>,
) -> Result<image::RgbaImage, String> {
    let font = picker.font_size();
    let (cw, ch) = (u32::from(font.width.max(1)), u32::from(font.height.max(1)));
    let uw = u32::from(union.width.max(1)) * cw;
    let uh = u32::from(union.height.max(1)) * ch;
    let mut canvas = image::RgbaImage::from_pixel(uw, uh, image::Rgba([0, 0, 0, 0]));
    for layer in layers {
        // SVG rasterizes to the box already (`svg_fitted`); a raster decodes at
        // natural size and is fit to its box below.
        let (image, _info, svg_fitted) =
            decode_for_box(layer.bytes, picker, layer.box_cells, layer.crop, tint)?;
        let lw = u32::from(layer.box_cells.width.max(1)) * cw;
        let lh = u32::from(layer.box_cells.height.max(1)) * ch;
        let filter = if layer.pixelated {
            FilterType::Nearest
        } else {
            FilterType::Lanczos3
        };
        let placed: image::RgbaImage = if svg_fitted {
            image.to_rgba8()
        } else if layer.crop {
            // object-fit: cover — fill the box, cropping the overflow.
            image.resize_to_fill(lw, lh, filter).to_rgba8()
        } else {
            // object-fit: contain — scale preserving aspect, centered in a
            // transparent tile so the letterbox slack reveals lower layers.
            let scaled = image.resize(lw, lh, filter).to_rgba8();
            let mut tile = image::RgbaImage::from_pixel(lw, lh, image::Rgba([0, 0, 0, 0]));
            let dx = i64::from(lw.saturating_sub(scaled.width()) / 2);
            let dy = i64::from(lh.saturating_sub(scaled.height()) / 2);
            image::imageops::overlay(&mut tile, &scaled, dx, dy);
            tile
        };
        let ox = i64::from(u32::from(layer.off_cells.0) * cw);
        let oy = i64::from(u32::from(layer.off_cells.1) * ch);
        image::imageops::overlay(&mut canvas, &placed, ox, oy);
    }
    Ok(canvas)
}

/// A `w×h` RGBA PNG filled with one color+alpha (composite-blend test fixture).
#[cfg(test)]
fn rgba_png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba(color));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
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

    fn gif_frames(repeat: Option<image::codecs::gif::Repeat>, frame_count: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut bytes, 30);
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).unwrap();
            }
            for index in 0..frame_count {
                let color = if index % 2 == 0 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(2, 1, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(20 + index as u32 * 10, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    fn push_riff_chunk(bytes: &mut Vec<u8>, name: &[u8; 4], payload: &[u8]) {
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if !payload.len().is_multiple_of(2) {
            bytes.push(0);
        }
    }

    fn push_u24_le(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes()[..3]);
    }

    /// Assemble two independently encoded VP8L frames using the WebP
    /// Container Specification's VP8X/ANIM/ANMF layout. Keeping the fixture
    /// generated makes every asserted timing/loop byte visible in this test.
    fn animated_webp() -> Vec<u8> {
        fn still(color: [u8; 4]) -> Vec<u8> {
            let mut bytes = Vec::new();
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 1, image::Rgba(color)))
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::WebP,
                )
                .unwrap();
            bytes
        }

        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0x02, 0, 0, 0]; // animation flag + reserved bytes
        push_u24_le(&mut vp8x, 1); // canvas width minus one
        push_u24_le(&mut vp8x, 0); // canvas height minus one
        push_riff_chunk(&mut body, b"VP8X", &vp8x);
        let mut anim = vec![0, 0, 0, 0]; // transparent-black BGRA hint
        anim.extend_from_slice(&2u16.to_le_bytes());
        push_riff_chunk(&mut body, b"ANIM", &anim);
        for (color, delay) in [([255, 0, 0, 255], 15), ([0, 0, 255, 255], 25)] {
            let frame = still(color);
            let mut anmf = Vec::new();
            push_u24_le(&mut anmf, 0); // x / 2
            push_u24_le(&mut anmf, 0); // y / 2
            push_u24_le(&mut anmf, 1); // width minus one
            push_u24_le(&mut anmf, 0); // height minus one
            push_u24_le(&mut anmf, delay);
            anmf.push(0); // alpha blend, retain canvas
            anmf.extend_from_slice(&frame[12..]);
            push_riff_chunk(&mut body, b"ANMF", &anmf);
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn png_crc(name: &[u8; 4], payload: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in name.iter().chain(payload) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    fn push_png_chunk(bytes: &mut Vec<u8>, name: &[u8; 4], payload: &[u8]) {
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&png_crc(name, payload).to_be_bytes());
    }

    /// Minimal two-frame RGBA APNG following PNG 3 §11.3.6. The first frame
    /// is the default image and both frames cover the complete 2×1 canvas.
    fn animated_png() -> Vec<u8> {
        use std::io::Write as _;

        fn compressed_scanline(color: [u8; 4]) -> Vec<u8> {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&[0]).unwrap(); // PNG filter type None
            encoder.write_all(&color).unwrap();
            encoder.write_all(&color).unwrap();
            encoder.finish().unwrap()
        }

        fn frame_control(sequence: u32, delay: u16) -> Vec<u8> {
            let mut control = Vec::new();
            control.extend_from_slice(&sequence.to_be_bytes());
            control.extend_from_slice(&2u32.to_be_bytes());
            control.extend_from_slice(&1u32.to_be_bytes());
            control.extend_from_slice(&0u32.to_be_bytes());
            control.extend_from_slice(&0u32.to_be_bytes());
            control.extend_from_slice(&delay.to_be_bytes());
            control.extend_from_slice(&100u16.to_be_bytes());
            control.push(0); // APNG_DISPOSE_OP_NONE
            control.push(0); // APNG_BLEND_OP_SOURCE
            control
        }

        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8
        push_png_chunk(&mut bytes, b"IHDR", &ihdr);
        let mut actl = Vec::new();
        actl.extend_from_slice(&2u32.to_be_bytes());
        actl.extend_from_slice(&2u32.to_be_bytes());
        push_png_chunk(&mut bytes, b"acTL", &actl);
        push_png_chunk(&mut bytes, b"fcTL", &frame_control(0, 2));
        push_png_chunk(&mut bytes, b"IDAT", &compressed_scanline([255, 0, 0, 255]));
        push_png_chunk(&mut bytes, b"fcTL", &frame_control(1, 3));
        let mut fdat = 2u32.to_be_bytes().to_vec();
        fdat.extend_from_slice(&compressed_scanline([0, 0, 255, 255]));
        push_png_chunk(&mut bytes, b"fdAT", &fdat);
        push_png_chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

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

    /// Every emitted sixel sequence (the sliced inline-image path AND the
    /// viewer's plain protocol) is wrapped DECSC … DECRC + CUF1, so after the
    /// terminal processes the anchor cell's "symbol" the cursor sits at
    /// anchor+1 — the position the ratatui backend ASSUMES when it elides the
    /// MoveTo for a horizontally adjacent next cell. Without the wrap, sixel
    /// scrolling mode leaves the cursor below the graphic, and a changed cell
    /// right of a 1-cell-wide icon (the scrollbar thumb beside a vote arrow)
    /// printed in the same run landed at a stray position while its real cell
    /// kept stale pixels — a persistent visual break in the scrollbar.
    #[test]
    fn sixel_sequences_normalize_the_cursor_to_anchor_plus_one() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        let png = red_png();
        let area = Rect::new(0, 0, 12, 6);

        // The sliced path (inline page images; cached sequence).
        let (proto, _) =
            encode_sliced_bytes(&picker, &png, Size::new(2, 1), false, false, None).unwrap();
        let mut buf = Buffer::empty(area);
        ratatui_image::sliced::SlicedImage::new(&proto, (0, 0).into()).render(area, &mut buf);
        let sym = buf.cell((0, 0)).expect("anchor cell").symbol();
        assert!(sym.starts_with("\x1b7"), "sliced: DECSC prefix");
        assert!(sym.ends_with("\x1b8\x1b[1C"), "sliced: DECRC+CUF1 suffix");
        assert!(sym.contains("\x1bP"), "sliced: still carries the DCS");

        // The plain path (the full-panel viewer).
        let (proto, _) = encode_bytes(&picker, &png, Size::new(4, 2), false, None).unwrap();
        let mut buf = Buffer::empty(area);
        ratatui_image::Image::new(&proto).render(area, &mut buf);
        let sym = buf.cell((0, 0)).expect("anchor cell").symbol();
        assert!(sym.starts_with("\x1b7"), "plain: DECSC prefix");
        assert!(sym.ends_with("\x1b8\x1b[1C"), "plain: DECRC+CUF1 suffix");
        assert!(sym.contains("\x1bP"), "plain: still carries the DCS");
    }

    /// Regression for the sliced-sixels parser after the encoder stopped
    /// emitting a trailing Graphics New Line. The DCS String Terminator ends
    /// the payload; it is not necessary for a `-` to precede it. Discarding the
    /// final split element in that form removes six real pixel rows and leaves
    /// the terminal's old bitmap/text damage visible while scrolling.
    #[test]
    fn sliced_sixel_retains_final_band_before_string_terminator() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        use ratatui_image::picker::ProtocolType;
        use ratatui_image::sliced::{SignedPosition, SlicedImage};

        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Sixel);
        let font = picker.font_size();
        let size = Size::new(2, 3);
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            u32::from(size.width) * u32::from(font.width),
            u32::from(size.height) * u32::from(font.height),
            image::Rgb([0x24, 0x91, 0xff]),
        ));
        let protocol = encode_sliced(&picker, image, size, false, false).unwrap();
        let area = Rect::new(0, 0, size.width, size.height);
        let mut buf = Buffer::empty(area);
        SlicedImage::new(&protocol, SignedPosition::from((0, 0))).render(area, &mut buf);

        let sequence = buf[(0, 0)].symbol();
        let dcs_start = sequence.find("\x1bP").expect("sixel DCS");
        let dcs_end = dcs_start
            + sequence[dcs_start..]
                .find("\x1b\\")
                .expect("sixel String Terminator")
            + 2;
        let dcs = &sequence[dcs_start..dcs_end - 2];
        let payload = dcs.split_once('q').expect("sixel data introducer").1;
        let bands = payload.matches('-').count() + usize::from(!payload.is_empty());

        assert_eq!(
            bands * 6,
            usize::from(size.height) * usize::from(font.height),
            "the final sixel band immediately before ST must not be discarded"
        );
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
                mime: SVG_MIME,
                has_alpha: false,
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
    fn svg_text_uses_the_shared_caseless_font_catalog() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="90" height="30">
            <text x="2" y="22" font-family="dEjAvU sAnS" font-size="20" fill="white">TRust</text>
        </svg>"#;
        let (image, mime) = decode(svg).unwrap();
        assert_eq!(mime, SVG_MIME);
        assert!(
            image.to_rgba8().pixels().any(|pixel| pixel[3] != 0),
            "font selection must leave visible SVG text outlines"
        );
    }

    #[test]
    fn external_svg_doctype_is_accepted_without_loading_the_external_subset() {
        // XML 1.0 permits the external identifier, but a non-validating image
        // processor need not fetch it. This is the prologue used by several
        // long-lived SVG 1.1 authoring tools and website logos.
        let svg = br##"<?xml version="1.0" encoding="UTF-8"?>
<!-- a DOCTYPE-looking token in a comment must not confuse the prologue scan -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN"
  "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" width="12px" height="7px"
     viewBox="0 0 12 7"><rect width="12" height="7" fill="#369"/></svg>"##;

        assert_eq!(info(svg).unwrap().width, 12);
        assert_eq!(info(svg).unwrap().height, 7);
        assert_eq!(decode_graphical(svg).unwrap().rgba.len(), 12 * 7 * 4);
    }

    #[test]
    fn svg_internal_dtd_subset_remains_disabled() {
        let svg = br#"<!DOCTYPE svg [<!ENTITY fill "red">]>
<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
  <rect width="4" height="4" fill="&fill;"/>
</svg>"#;
        let error = info(svg).unwrap_err();
        assert!(error.contains("internal DTD subsets"), "{error}");
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
    fn ratio_only_svg_detection() {
        // A `viewBox` with no intrinsic width/height → rule-3 ratio.
        let vb = br#"<svg viewBox="0 0 300 150" xmlns="http://www.w3.org/2000/svg"/>"#;
        assert_eq!(svg_bytes_ratio_only(vb), Some(2.0));
        // A definite width/height IS an intrinsic size → NOT rule 3.
        assert_eq!(svg_bytes_ratio_only(&sample_svg()), None);
        // Only one dimension present is still an intrinsic size → NOT rule 3.
        let one = br#"<svg width="40" viewBox="0 0 300 150" xmlns="http://www.w3.org/2000/svg"/>"#;
        assert_eq!(svg_bytes_ratio_only(one), None);
        // No viewBox and no dimensions → no ratio.
        let none = br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#;
        assert_eq!(svg_bytes_ratio_only(none), None);
        // Non-SVG bytes → None (the loader calls this on every decoded image).
        assert_eq!(svg_bytes_ratio_only(b"\x89PNG\r\n\x1a\n"), None);
        // The `data:` URL wrapper reads the same markup.
        let url = "data:image/svg+xml,%3csvg%20viewBox='0%200%20300%20300'%3e%3c/svg%3e";
        assert_eq!(svg_url_ratio_only(url), Some(1.0));
        assert_eq!(svg_url_ratio_only("data:image/png;base64,iVBOR"), None);
    }

    #[test]
    fn graphical_source_decode_records_and_refreshes_svg_intrinsic_metadata() {
        let source = "https://images.example/ratio-only-regression.svg";
        let ratio_only = br#"<svg viewBox="0 0 300 150" xmlns="http://www.w3.org/2000/svg"/>"#;
        let decoded = decode_graphical_for_source(source, ratio_only).unwrap();
        assert_eq!((decoded.width, decoded.height), (300, 150));
        assert_eq!(svg_ratio_only_get(source), Some(2.0));

        // Revalidation may change the resource at a stable URL. Definite root
        // dimensions are real SVG intrinsic dimensions and must remove the
        // old ratio-only classification.
        decode_graphical_for_source(source, &sample_svg()).unwrap();
        assert_eq!(svg_ratio_only_get(source), None);
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
    fn graphical_gif_animation_streams_composited_frames_and_exact_delays() {
        let bytes = gif_frames(Some(image::codecs::gif::Repeat::Infinite), 2);
        let decoded = decode_graphical_image_for_source("https://e.test/a.gif", bytes.clone())
            .expect("animated GIF");
        assert_eq!((decoded.image.width, decoded.image.height), (2, 1));
        assert!(decoded.image.rgba[0] > 250 && decoded.image.rgba[2] < 5);

        let animation = decoded.animation.expect("two frames are animated");
        assert_eq!(animation.encoded_len(), bytes.len());
        let mut frames = animation.decoder().unwrap();
        assert_eq!(frames.loop_count(), AnimationLoopCount::Infinite);
        let first = frames.next_frame().unwrap().unwrap();
        let second = frames.next_frame().unwrap().unwrap();
        assert_eq!(first.delay, Duration::from_millis(20));
        assert_eq!(second.delay, Duration::from_millis(30));
        assert_eq!(&first.image.rgba[..4], &[255, 0, 0, 255]);
        assert!(second.image.rgba[2] > 250 && second.image.rgba[0] < 5);
        assert!(frames.next_frame().unwrap().is_none());
    }

    #[test]
    fn gif_without_loop_extension_plays_once_and_one_frame_gif_stays_static() {
        let decoded =
            decode_graphical_image_for_source("https://e.test/once.gif", gif_frames(None, 2))
                .unwrap();
        assert_eq!(
            decoded.animation.unwrap().decoder().unwrap().loop_count(),
            AnimationLoopCount::Finite(1),
            "GIF89a's Trailer ends the stream when no application loop extension exists"
        );

        let still = decode_graphical_image_for_source(
            "https://e.test/still.gif",
            gif_frames(Some(image::codecs::gif::Repeat::Infinite), 1),
        )
        .unwrap();
        assert!(still.animation.is_none());
        assert_eq!(&still.image.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn graphical_webp_animation_honors_container_loop_and_millisecond_delays() {
        let decoded =
            decode_graphical_image_for_source("https://e.test/a.webp", animated_webp()).unwrap();
        assert!(decoded.image.rgba[0] > 250 && decoded.image.rgba[2] < 5);
        let mut frames = decoded.animation.unwrap().decoder().unwrap();
        assert_eq!(frames.loop_count(), AnimationLoopCount::Finite(2));
        let first = frames.next_frame().unwrap().unwrap();
        let second = frames.next_frame().unwrap().unwrap();
        assert_eq!(first.delay, Duration::from_millis(15));
        assert_eq!(second.delay, Duration::from_millis(25));
        assert!(second.image.rgba[2] > 250 && second.image.rgba[0] < 5);
    }

    #[test]
    fn graphical_apng_animation_honors_num_plays_and_fractional_delays() {
        let decoded =
            decode_graphical_image_for_source("https://e.test/a.png", animated_png()).unwrap();
        assert_eq!(&decoded.image.rgba[..4], &[255, 0, 0, 255]);
        let mut frames = decoded.animation.unwrap().decoder().unwrap();
        assert_eq!(frames.loop_count(), AnimationLoopCount::Finite(2));
        let first = frames.next_frame().unwrap().unwrap();
        let second = frames.next_frame().unwrap().unwrap();
        assert_eq!(first.delay, Duration::from_millis(20));
        assert_eq!(second.delay, Duration::from_millis(30));
        assert_eq!(&second.image.rgba[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn image_has_alpha_scans_only_real_transparency() {
        // An opaque RGB PNG has no alpha channel → false without a scan.
        assert!(!image_has_alpha(&decode(&red_png()).unwrap().0));
        // An RGBA PNG that happens to be fully opaque → false (scanned).
        let opaque_rgba = decode(&rgba_png(4, 4, [10, 20, 30, 255])).unwrap().0;
        assert!(!image_has_alpha(&opaque_rgba));
        // An RGBA PNG with a transparent pixel → true.
        let transparent = decode(&rgba_png(4, 4, [10, 20, 30, 0])).unwrap().0;
        assert!(image_has_alpha(&transparent));
    }

    #[test]
    fn composite_canvas_alpha_blends_layers_bottom_first() {
        // P8: the bottom layer shows through an upper layer's transparent pixels,
        // and an opaque upper layer covers it — the alpha-composite core.
        let picker = Picker::halfblocks();
        let font = picker.font_size();
        let (cw, ch) = (u32::from(font.width.max(1)), u32::from(font.height.max(1)));
        let cell = Size::new(1, 1);
        fn input(bytes: &[u8], cell: Size) -> CompositeInput<'_> {
            CompositeInput {
                bytes,
                box_cells: cell,
                off_cells: (0, 0),
                crop: false,
                pixelated: false,
            }
        }
        let base = rgba_png(cw, ch, [255, 0, 0, 255]); // opaque red base
        // A fully-transparent overlay: the base shows through.
        let clear = rgba_png(cw, ch, [0, 0, 255, 0]);
        let canvas = composite_canvas(
            &picker,
            cell,
            &[input(&base, cell), input(&clear, cell)],
            None,
        )
        .unwrap();
        assert_eq!(
            canvas.get_pixel(0, 0).0,
            [255, 0, 0, 255],
            "a transparent overlay lets the base show through"
        );
        // A fully-opaque overlay covers the base.
        let solid = rgba_png(cw, ch, [0, 0, 255, 255]);
        let canvas = composite_canvas(
            &picker,
            cell,
            &[input(&base, cell), input(&solid, cell)],
            None,
        )
        .unwrap();
        assert_eq!(
            canvas.get_pixel(0, 0).0,
            [0, 0, 255, 255],
            "an opaque overlay covers the base"
        );
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
        let proto = encode_sliced(&picker, image, Size::new(20, 10), false, false).unwrap();
        let size = proto.size();
        assert!(size.width > 0 && size.width <= 20);
        assert!(size.height > 0 && size.height <= 10);
    }

    /// The per-image sixel sequence cache (vendored ratatui-image): redrawing an
    /// unchanged on-screen image reuses its built byte-string instead of
    /// rebuilding it (`bands().join()` — the dominant at-rest sixel cost). This
    /// exercises the real `SlicedProtocol::Sixel` render path: at rest a redraw is
    /// a cache HIT, a scroll changes the slice key and rebuilds, the cached bytes
    /// are byte-identical to a fresh build, and an encode-thread `prewarm` makes a
    /// newly-appearing image's first draw a hit (the build moved off the render
    /// thread).
    ///
    /// The counters are process-global atomics, but this is the only non-ignored
    /// test that renders a Sixel `SlicedImage`, so keeping every assertion in one
    /// test keeps the deltas deterministic under the parallel test runner.
    #[test]
    fn sixel_sequence_cache_hits_rebuilds_on_scroll_and_prewarms() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        use ratatui_image::picker::ProtocolType;
        use ratatui_image::sliced::{
            SIXEL_SEQ_BUILDS, SIXEL_SEQ_HITS, SIXEL_SEQ_PREWARMS, SignedPosition, SlicedImage,
        };
        use std::sync::atomic::Ordering::Relaxed;

        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Sixel);
        let f = picker.font_size();
        // A box taller than the viewport so a scroll yields a different slice.
        let cols: u16 = 20;
        let rows: u16 = 30;
        let vh: u16 = 10;
        let (iw, ih) = (
            u32::from(cols) * u32::from(f.width),
            u32::from(rows) * u32::from(f.height),
        );
        let (image, _) = decode(&tall_png(iw, ih)).unwrap();
        let sliced = encode_sliced(&picker, image, Size::new(cols, rows), false, false).unwrap();
        let area = Rect::new(0, 0, cols, vh);
        let render = |pos_y: i16, buf: &mut Buffer| {
            SlicedImage::new(&sliced, SignedPosition::from((0, pos_y))).render(area, buf);
        };

        // Cold draw at rest: a miss builds the sequence once.
        SIXEL_SEQ_BUILDS.store(0, Relaxed);
        SIXEL_SEQ_HITS.store(0, Relaxed);
        let mut buf0 = Buffer::empty(area);
        render(0, &mut buf0);
        assert_eq!(SIXEL_SEQ_BUILDS.load(Relaxed), 1, "cold draw builds once");
        assert_eq!(SIXEL_SEQ_HITS.load(Relaxed), 0, "no hit on the cold draw");

        // Redraw, same slice: a hit, no rebuild — and identical output.
        let mut buf1 = Buffer::empty(area);
        render(0, &mut buf1);
        assert_eq!(
            SIXEL_SEQ_BUILDS.load(Relaxed),
            1,
            "at-rest redraw does not rebuild"
        );
        assert_eq!(
            SIXEL_SEQ_HITS.load(Relaxed),
            1,
            "at-rest redraw is a cache hit"
        );
        assert_eq!(buf0, buf1, "the cached sequence renders identical bytes");

        // Scrolling changes skip/drop -> the 1-entry memo misses and rebuilds.
        let mut buf2 = Buffer::empty(area);
        render(-5, &mut buf2);
        assert_eq!(
            SIXEL_SEQ_BUILDS.load(Relaxed),
            2,
            "a scroll rebuilds (new slice key)"
        );
        assert_eq!(
            SIXEL_SEQ_HITS.load(Relaxed),
            1,
            "the scrolled draw is not a hit"
        );

        // Holding at the scrolled position hits again (no rebuild).
        let mut buf3 = Buffer::empty(area);
        render(-5, &mut buf3);
        assert_eq!(
            SIXEL_SEQ_BUILDS.load(Relaxed),
            2,
            "holding the scroll does not rebuild"
        );
        assert_eq!(SIXEL_SEQ_HITS.load(Relaxed), 2, "holding the scroll hits");

        // Prewarm: building the at-rest slice off-thread (on the encode thread)
        // makes a freshly-appearing image's FIRST on-screen draw a hit, not a
        // render-thread build. A fresh protocol, prewarmed, then drawn fully
        // visible (an area that fits the whole image, so skip = drop = 0):
        let (image2, _) = decode(&tall_png(iw, ih)).unwrap();
        let warm = encode_sliced(&picker, image2, Size::new(cols, rows), false, false).unwrap();
        warm.prewarm_sixel_cache();
        let wsize = warm.size();
        let warea = Rect::new(0, 0, wsize.width, wsize.height);
        SIXEL_SEQ_BUILDS.store(0, Relaxed);
        SIXEL_SEQ_HITS.store(0, Relaxed);
        SIXEL_SEQ_PREWARMS.store(0, Relaxed);
        let mut wbuf = Buffer::empty(warea);
        SlicedImage::new(&warm, SignedPosition::from((0, 0))).render(warea, &mut wbuf);
        assert_eq!(
            SIXEL_SEQ_BUILDS.load(Relaxed),
            0,
            "a prewarmed first draw does not build on the render thread"
        );
        assert_eq!(
            SIXEL_SEQ_HITS.load(Relaxed),
            1,
            "a prewarmed first draw is a hit"
        );
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
        let sliced = encode_sliced(&picker, image.clone(), box_size, false, false).unwrap();
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
