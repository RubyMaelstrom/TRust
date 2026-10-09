//! Colour-managed decoding of CMYK and YCCK JPEGs.
//!
//! CSS Color 4 #tagged-images (local csswg-drafts snapshot 81c27f686901): an
//! image whose format assigns it a valid colour profile is in that colour
//! space. A JPEG whose embedded ICC profile (APP2 `ICC_PROFILE`) has a CMYK
//! data colour space is CMYK, converted through that profile to sRGB when it
//! is decoded, as RGB and gray profiles are in `super::color`.
//!
//! JPEG itself (ITU-T T.81) leaves the meaning of components to the file
//! format; JFIF covers only gray and YCbCr. A four-component frame is CMYK,
//! or YCCK when an Adobe APP14 segment's transform flag is 2 (Adobe
//! Technical Note #5116): Y, Cb and Cr carry the first three channels through
//! the JFIF YCbCr transform and K is stored as is. Adobe applications write
//! the samples of the files they mark with APP14 inverted, 0 being full
//! coverage (the libjpeg documentation's "CAUTION" on Photoshop CMYK files),
//! while ICC device values make 0 no colorant, so the samples of a file with
//! an Adobe segment are inverted before conversion. Without one they are
//! taken as stored, libjpeg's "straight CMYK". None of these texts is in the
//! local standards library; the libjpeg-turbo documentation is the source.
//!
//! Work is bounded like the other raster decoders: the frame dimensions and
//! decode allocation are capped before decoding, and the profile by
//! `super::color`'s size and parsing limits. Any file this path cannot
//! handle (no CMYK profile, a malformed or unsupported profile, an
//! undecodable frame) returns `None` and takes the general decoder, whose
//! CMYK conversion is not colour managed.

use image::RgbImage;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::color;

/// Decode a CMYK or YCCK JPEG with an embedded CMYK ICC profile to sRGB.
/// `None` for any other JPEG, or when colour management is impossible.
pub(crate) fn decode(bytes: &[u8], max_dimension: u32, max_alloc: u64) -> Option<RgbImage> {
    let max = max_dimension as usize;
    let options = DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(max)
        .set_max_height(max);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().ok()?;
    let input = decoder.input_colorspace()?;
    if !matches!(input, ColorSpace::CMYK | ColorSpace::YCCK) {
        return None;
    }
    let profile = decoder.icc_profile()?;
    if !color::is_cmyk_profile(&profile) {
        return None;
    }
    let conversion = color::cmyk_conversion(&color::icc_tag(Some(profile))?)?;
    let (width, height) = decoder.dimensions()?;
    let (width32, height32) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    if width == 0
        || height == 0
        || width32 > max_dimension
        || height32 > max_dimension
        || u64::from(width32) * u64::from(height32) * 4 > max_alloc
    {
        return None;
    }
    // Ask for the stored components: zune-jpeg copies them out unchanged
    // (upsampled) when the output colour space is the input's.
    decoder.set_options(decoder.options().jpeg_set_out_colorspace(input));
    let mut samples = decoder.decode().ok()?;
    if samples.len() != width * height * 4 {
        return None;
    }
    let inverted = adobe_transform(bytes).is_some();
    to_rgb(
        &mut samples,
        width,
        input == ColorSpace::YCCK,
        inverted,
        |cmyk, rgb| conversion.convert(cmyk, rgb),
    )?;
    RgbImage::from_raw(width32, height32, samples)
}

/// Convert decoded four-component samples in place, row by row, to RGB:
/// first to ICC CMYK device values, then through `convert`. The buffer
/// shrinks to three bytes per pixel; each RGB row ends before the next CMYK
/// row starts, so a row's input is read before anything overwrites it.
fn to_rgb(
    samples: &mut Vec<u8>,
    width: usize,
    ycck: bool,
    inverted: bool,
    convert: impl Fn(&[u8], &mut [u8]) -> bool,
) -> Option<()> {
    let rows = samples.len() / (width * 4);
    let mut cmyk = vec![0u8; width * 4];
    let mut rgb = vec![0u8; width * 3];
    for row in 0..rows {
        cmyk.copy_from_slice(&samples[row * width * 4..(row + 1) * width * 4]);
        for pixel in cmyk.as_chunks_mut::<4>().0 {
            *pixel = device_cmyk(*pixel, ycck, inverted);
        }
        if !convert(&cmyk, &mut rgb) {
            return None;
        }
        samples[row * width * 3..(row + 1) * width * 3].copy_from_slice(&rgb);
    }
    samples.truncate(rows * width * 3);
    Some(())
}

/// One pixel's stored samples as ICC CMYK device values (0 = no colorant).
///
/// YCCK holds the first three channels as YCbCr, which libjpeg converts back
/// with the JFIF transform and then complements (`ycck_cmyk_convert`):
/// C = 255 − R(Y, Cb, Cr) and likewise M and Y, giving the channels as the
/// file's CMYK would store them. Adobe's inverted storage then makes the
/// coverage the complement of those values.
fn device_cmyk([c, m, y, k]: [u8; 4], ycck: bool, inverted: bool) -> [u8; 4] {
    let stored = if ycck {
        let [r, g, b] = ycc_to_rgb(c, m, y);
        [255 - r, 255 - g, 255 - b, k]
    } else {
        [c, m, y, k]
    };
    if inverted {
        stored.map(|value| 255 - value)
    } else {
        stored
    }
}

/// The JFIF YCbCr to RGB transform (ITU-T T.871), full range, rounded and
/// clamped as libjpeg does.
fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0);
    let channel = |value: f32| value.round().clamp(0.0, 255.0) as u8;
    [
        channel(y + 1.402 * cr),
        channel(y - 0.344_136 * cb - 0.714_136 * cr),
        channel(y + 1.772 * cb),
    ]
}

/// The colour transform flag of an Adobe APP14 segment before the first scan,
/// if the file has one. Markers are walked by their lengths (T.81 B.1.1.4) up
/// to the first SOS.
fn adobe_transform(bytes: &[u8]) -> Option<u8> {
    if bytes.get(..2) != Some(&[0xFF, 0xD8][..]) {
        return None;
    }
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        match marker {
            // Fill bytes before a marker, and markers without a segment.
            0xFF => at += 1,
            0x01 | 0xD0..=0xD8 => at += 2,
            // Start of scan or end of image: no APP14 governs the frame.
            0xDA | 0xD9 => return None,
            _ => {
                let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
                if length < 2 {
                    return None;
                }
                let segment = bytes.get(at + 4..at + 2 + length)?;
                // "Adobe", version (2), flags0 (2), flags1 (2), transform (1).
                if marker == 0xEE && segment.len() >= 12 && segment.starts_with(b"Adobe") {
                    return Some(segment[11]);
                }
                at += 2 + length;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/src/img/fixtures/cmyk/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn pixel(bytes: &[u8], x: u32, y: u32) -> [u8; 3] {
        let (image, mime) = super::super::decode_raster(bytes).unwrap();
        assert_eq!(mime, "image/jpeg");
        let rgb = image.to_rgb8();
        assert_eq!(rgb.dimensions(), (32, 16));
        rgb.get_pixel(x, y).0
    }

    fn assert_near(actual: [u8; 3], expected: [u8; 3], name: &str) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 4),
            "{name}: {actual:?} is not near {expected:?}"
        );
    }

    /// The fixtures are 32×16: no ink on the left half, 100% cyan on the
    /// right, tagged with the CC0 "CGATS001Compat-v2-micro" CMYK profile
    /// (fixtures/cmyk/README). LittleCMS (ImageMagick, perceptual intent)
    /// converts them to sRGB (254, 255, 254) and (0, 173, 239); the naive
    /// conversion would give white and (0, 255, 255).
    #[test]
    fn cmyk_profiles_convert_adobe_ycck_and_straight_samples() {
        for name in ["cmyk-adobe.jpg", "ycck-adobe.jpg", "cmyk-straight.jpg"] {
            let bytes = fixture(name);
            assert_near(pixel(&bytes, 4, 4), [254, 255, 254], name);
            assert_near(pixel(&bytes, 20, 4), [0, 173, 239], name);
        }
    }

    #[test]
    fn malformed_cmyk_profiles_fall_back_to_the_naive_conversion() {
        let mut bytes = fixture("cmyk-adobe.jpg");
        // Rename the profile's only colour tag ('A2B0'): the profile parses
        // but has no device-to-PCS transform.
        let at = bytes
            .windows(4)
            .position(|window| window == b"A2B0")
            .unwrap();
        bytes[at..at + 4].copy_from_slice(b"zzzz");
        assert!(decode(&bytes, 12_000, u64::MAX).is_none());
        assert_near(pixel(&bytes, 20, 4), [0, 255, 255], "renamed A2B0");
        // A profile whose header names another colour space is not taken.
        let mut bytes = fixture("cmyk-adobe.jpg");
        let at = bytes
            .windows(4)
            .position(|window| window == b"CMYK")
            .unwrap();
        bytes[at..at + 4].copy_from_slice(b"RGB ");
        assert!(decode(&bytes, 12_000, u64::MAX).is_none());
        assert!(super::super::decode_raster(&bytes).is_ok());
    }

    #[test]
    fn limits_apply_before_decoding() {
        let bytes = fixture("cmyk-adobe.jpg");
        assert!(decode(&bytes, 12_000, u64::MAX).is_some());
        assert!(decode(&bytes, 31, u64::MAX).is_none());
        assert!(decode(&bytes, 12_000, 32 * 16 * 4 - 1).is_none());
    }

    #[test]
    fn device_values_follow_adobe_inversion_and_ycck() {
        // Straight CMYK is used as stored; Adobe-marked CMYK is inverted.
        assert_eq!(
            device_cmyk([0, 64, 128, 255], false, false),
            [0, 64, 128, 255]
        );
        assert_eq!(
            device_cmyk([0, 64, 128, 255], false, true),
            [255, 191, 127, 0]
        );
        // YCCK: Y 255 with neutral chroma is R = G = B = 255, so the stored
        // CMY are 0, which Adobe inversion turns into full coverage; K is
        // inverted alone.
        assert_eq!(
            device_cmyk([255, 128, 128, 200], true, true),
            [255, 255, 255, 55]
        );
        // Y 0 is no coverage of C, M and Y.
        assert_eq!(device_cmyk([0, 128, 128, 255], true, true), [0, 0, 0, 0]);
        // Cr above neutral raises R, i.e. lowers the stored C.
        let [c, m, _, _] = device_cmyk([128, 128, 200, 255], true, false);
        assert!(c < 128 && m > 128, "{c} {m}");
    }

    #[test]
    fn adobe_segment_is_found_before_the_first_scan() {
        let app14 = [
            0xFF, 0xEE, 0x00, 0x0E, b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, 2,
        ];
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
        jpeg.extend_from_slice(&app14);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        assert_eq!(adobe_transform(&jpeg), Some(2));
        // After SOS the segment no longer counts.
        let mut late = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
        late.extend_from_slice(&app14);
        assert_eq!(adobe_transform(&late), None);
        // Truncated or malformed segments end the walk.
        assert_eq!(adobe_transform(&jpeg[..12]), None);
        assert_eq!(adobe_transform(&[0xFF, 0xD8, 0xFF, 0xEE, 0x00, 0x01]), None);
        assert_eq!(adobe_transform(b"not a jpeg"), None);
    }

    #[test]
    fn rows_shrink_in_place_to_rgb() {
        // Two rows of two pixels; the stand-in conversion writes C, M, Y.
        let mut samples = (0u8..16).collect::<Vec<_>>();
        to_rgb(&mut samples, 2, false, false, |cmyk, rgb| {
            for (out, pixel) in rgb
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(cmyk.as_chunks::<4>().0)
            {
                out.copy_from_slice(&pixel[..3]);
            }
            true
        })
        .unwrap();
        assert_eq!(samples, [0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14]);
        // A failing conversion fails the decode.
        let mut samples = vec![0u8; 8];
        assert!(to_rgb(&mut samples, 2, false, false, |_, _| false).is_none());
    }
}
