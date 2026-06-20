//! Image decoding and terminal-graphics encoding for the viewer panel.
//!
//! Decoding sniffs the format from the bytes (servers lie about
//! content types) and guards dimensions against decompression bombs.
//! Encoding goes through ratatui-image's `Picker` — sixel where the
//! terminal answered the startup query for it (foot does), unicode
//! half-blocks anywhere else. Both steps are CPU-bound and run on
//! blocking tasks, never the UI thread.

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FilterType, Resize};

/// Hard ceiling on decoded dimensions: a 5 MB download can still claim
/// to be a gigapixel PNG.
const MAX_DIMENSION: u32 = 12_000;

/// Sniff the image format from magic bytes; None when it isn't one we
/// can decode.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    image::guess_format(bytes).ok().map(|f| f.to_mime_type())
}

/// Decode raw bytes into pixels, returning the detected MIME type too.
/// Animated formats decode to their first frame.
pub fn decode(bytes: &[u8]) -> Result<(DynamicImage, &'static str), String> {
    let mime = sniff(bytes).ok_or("unrecognized image format")?;
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
