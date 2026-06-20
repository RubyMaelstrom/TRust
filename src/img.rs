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
