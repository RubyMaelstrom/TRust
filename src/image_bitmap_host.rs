//! HTML #dom-createImageBitmap / #cropped-to-the-source-rectangle-with-formatting.
//! Local WHATWG snapshot e5071a20 (2026-09-06). Bitmap bytes stay in privately
//! rooted JS typed arrays, so closing or collecting a bitmap releases its storage.
use super::{Ctx, HostState, Value, host_arg_string};
use image::{ImageDecoder, RgbaImage, imageops::FilterType};

const MAX_BYTES: usize = 256 * 1024 * 1024;

fn byte_len(width: u32, height: u32) -> Option<usize> {
    let size = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (width != 0 && height != 0 && size <= MAX_BYTES).then_some(size)
}

fn decode(bytes: &[u8], from_image: bool) -> Option<RgbaImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BYTES as u64);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    byte_len(width, height)?;
    let orientation = if from_image {
        decoder.orientation().ok()?
    } else {
        image::metadata::Orientation::NoTransforms
    };
    let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image.into_rgba8())
}

/// `[sx, sy, sw, sh, resizeWidth, resizeHeight, flipY, alpha, quality]`.
/// A missing rectangle/dimension is represented only by the private binding's NaN.
fn format(mut input: RgbaImage, input_premultiplied: bool, n: &[f64]) -> Option<(RgbaImage, bool)> {
    if n.len() != 9 {
        return None;
    }
    let (x, y, width, height) = if n[2].is_nan() {
        (0, 0, input.width(), input.height())
    } else {
        let (x, y, w, h) = (n[0] as i64, n[1] as i64, n[2] as i64, n[3] as i64);
        (
            x + w.min(0),
            y + h.min(0),
            u32::try_from(w.unsigned_abs()).ok()?,
            u32::try_from(h.unsigned_abs()).ok()?,
        )
    };
    byte_len(width, height)?;
    let out_width = if !n[4].is_nan() {
        n[4] as u32
    } else if !n[5].is_nan() {
        u32::try_from((width as u64 * n[5] as u64).div_ceil(height as u64)).ok()?
    } else {
        width
    };
    let out_height = if !n[5].is_nan() {
        n[5] as u32
    } else if !n[4].is_nan() {
        u32::try_from((height as u64 * n[4] as u64).div_ceil(width as u64)).ok()?
    } else {
        height
    };
    byte_len(out_width, out_height)?;
    if x != 0 || y != 0 || width != input.width() || height != input.height() {
        let mut cropped = RgbaImage::new(width, height);
        // Intersect before iterating: a distant crop is transparent, without a
        // loop over an attacker-selected off-image coordinate range.
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + width as i64).min(input.width() as i64);
        let bottom = (y + height as i64).min(input.height() as i64);
        for row in top..bottom {
            for col in left..right {
                cropped.put_pixel(
                    (col - x) as u32,
                    (row - y) as u32,
                    *input.get_pixel(col as u32, row as u32),
                );
            }
        }
        input = cropped;
    }
    if input.width() != out_width || input.height() != out_height {
        let quality = match n[8] as u8 {
            0 => FilterType::Nearest,
            2 => FilterType::CatmullRom,
            3 => FilterType::Lanczos3,
            _ => FilterType::Triangle,
        };
        input = image::imageops::resize(&input, out_width, out_height, quality);
    }
    if n[6] != 0. {
        image::imageops::flip_vertical_in_place(&mut input);
    }
    // Preserve the source representation for default. Explicit alpha options
    // are applied once, including when the source is another ImageBitmap.
    let premultiplied = if n[7] < 0. {
        input_premultiplied
    } else {
        n[7] != 0.
    };
    if premultiplied != input_premultiplied {
        for pixel in input.pixels_mut() {
            let alpha = pixel[3] as u32;
            for channel in 0..3 {
                pixel[channel] = if premultiplied {
                    ((pixel[channel] as u32 * alpha + 127) / 255) as u8
                } else {
                    (pixel[channel] as u32 * 255 + alpha / 2)
                        .checked_div(alpha)
                        .unwrap_or(0)
                        .min(255) as u8
                };
            }
        }
    }
    Some((input, premultiplied))
}

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    if op == "slots" {
        let candidate = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Ok(ctx
            .host_mut::<HostState>()
            .expect("bitmap host")
            .image_bitmap_slots
            .get_or_insert(candidate)
            .clone());
    }
    let Some(numbers) = args.get(1) else {
        return Ok(Value::Null);
    };
    let length = ctx
        .member_get(numbers, "length")?
        .as_num_opt()
        .unwrap_or(0.) as usize;
    if length > 16 {
        return Ok(Value::Null);
    }
    let mut n = Vec::with_capacity(length);
    for index in 0..length {
        n.push(
            ctx.member_get(numbers, &index.to_string())?
                .as_num_opt()
                .unwrap_or(f64::NAN),
        );
    }
    let Some(bytes) = args.get(2).and_then(|v| ctx.typed_array_bytes(v)) else {
        return Ok(Value::Null);
    };
    let result = if op == "decode" && n.len() == 9 {
        decode(&bytes, n[6] == 0.).and_then(|input| format(input, false, &n))
    } else if op == "raw" && n.len() == 14 {
        let (width, height) = (n[0] as u32, n[1] as u32);
        let half = n[2] != 0.;
        let space = n[3] as u8;
        let expected =
            byte_len(width, height).and_then(|len| len.checked_mul(if half { 2 } else { 1 }));
        if expected != Some(bytes.len()) {
            None
        } else {
            let rgba = if !half && space == 0 {
                bytes
            } else {
                let mut rgba = Vec::with_capacity(bytes.len() / if half { 2 } else { 1 });
                for p in bytes.chunks_exact(if half { 8 } else { 4 }) {
                    let v: [f32; 4] = std::array::from_fn(|i| {
                        if half {
                            half::f16::from_ne_bytes([p[i * 2], p[i * 2 + 1]]).to_f32()
                        } else {
                            p[i] as f32 / 255.
                        }
                    });
                    let c = crate::canvas::convert([v[0], v[1], v[2]], space, 0);
                    rgba.extend(
                        [c[0], c[1], c[2], v[3]].map(|v| (v.clamp(0., 1.) * 255.).round() as u8),
                    );
                }
                rgba
            };
            RgbaImage::from_raw(width, height, rgba)
                .and_then(|input| format(input, n[4] != 0., &n[5..]))
        }
    } else {
        None
    };
    let Some((image, premultiplied)) = result else {
        return Ok(Value::Null);
    };
    let pixels = ctx.make_uint8array(image.as_raw())?;
    // Do not let inherited setters/thenables expose opaque source pixels.
    let record = ctx.new_object_with_proto(&Value::Null);
    for (i, value) in [
        Value::Num(image.width() as f64),
        Value::Num(image.height() as f64),
        pixels,
        Value::Bool(true),
        Value::Num(1.),
        Value::Bool(premultiplied),
    ]
    .into_iter()
    .enumerate()
    {
        ctx.member_set(&record, &i.to_string(), value)?;
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> [f64; 9] {
        [0., 0., f64::NAN, f64::NAN, f64::NAN, f64::NAN, 0., -1., 0.]
    }
    #[test]
    fn crop_padding_negative_extents_flip_and_aspect_resize() {
        let image = RgbaImage::from_raw(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        )
        .unwrap();
        let mut n = options();
        n[..4].copy_from_slice(&[1., 2., -2., -2.]);
        n[6] = 1.;
        let (cropped, _) = format(image.clone(), false, &n).unwrap();
        assert_eq!(
            cropped.into_raw(),
            [0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0, 255, 0, 0, 255]
        );
        let mut n = options();
        n[..4].copy_from_slice(&[0., 0., 2., 1.]);
        n[5] = 3.;
        assert_eq!(format(image, false, &n).unwrap().0.dimensions(), (6, 3));
    }
    #[test]
    fn alpha_is_converted_once_and_allocations_are_bounded() {
        let image = RgbaImage::from_raw(1, 1, vec![200, 100, 50, 128]).unwrap();
        let mut n = options();
        n[7] = 1.;
        let (image, premultiplied) = format(image, false, &n).unwrap();
        assert!(premultiplied);
        assert_eq!(image.as_raw(), &[100, 50, 25, 128]);
        let (again, _) = format(image.clone(), true, &n).unwrap();
        assert_eq!(again, image);
        n[4] = u32::MAX as f64;
        assert!(format(image, true, &n).is_none());
    }
}
