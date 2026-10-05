//! WebGL's `PredefinedColorSpace` values (WebGL 1.0
//! #DOM-WebGLRenderingContext-drawingBufferColorSpace and #unpackColorSpace,
//! HTML #predefinedcolorspace). Both spaces use the D65 white point and the sRGB
//! transfer function; they differ only in their primaries. The matrices are
//! CSS Color 4's exact rationals (css-color-4/conversions.js, local snapshot).

use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ColorSpace {
    #[default]
    Srgb,
    DisplayP3,
}

impl ColorSpace {
    /// The binding's numeric encoding of the IDL enumeration value.
    pub(crate) fn from_code(code: f64) -> Option<Self> {
        match code as u32 {
            0 => Some(Self::Srgb),
            1 => Some(Self::DisplayP3),
            _ => None,
        }
    }
}

type Matrix = [[f64; 3]; 3];

const LIN_SRGB_TO_XYZ: Matrix = [
    [506752. / 1228815., 87881. / 245763., 12673. / 70218.],
    [87098. / 409605., 175762. / 245763., 12673. / 175545.],
    [7918. / 409605., 87881. / 737289., 1001167. / 1053270.],
];
const XYZ_TO_LIN_SRGB: Matrix = [
    [12831. / 3959., -329. / 214., -1974. / 3959.],
    [-851781. / 878810., 1648619. / 878810., 36519. / 878810.],
    [705. / 12673., -2585. / 12673., 705. / 667.],
];
const LIN_P3_TO_XYZ: Matrix = [
    [608311. / 1250200., 189793. / 714400., 198249. / 1000160.],
    [35783. / 156275., 247089. / 357200., 198249. / 2500400.],
    [0., 32229. / 714400., 5220557. / 5000800.],
];
const XYZ_TO_LIN_P3: Matrix = [
    [446124. / 178915., -333277. / 357830., -72051. / 178915.],
    [-14852. / 17905., 63121. / 35810., 423. / 17905.],
    [11844. / 330415., -50337. / 660830., 316169. / 330415.],
];

const fn multiply(a: Matrix, b: Matrix) -> Matrix {
    let mut out = [[0.; 3]; 3];
    let mut row = 0;
    while row < 3 {
        let mut column = 0;
        while column < 3 {
            out[row][column] =
                a[row][0] * b[0][column] + a[row][1] * b[1][column] + a[row][2] * b[2][column];
            column += 1;
        }
        row += 1;
    }
    out
}

const SRGB_TO_P3: Matrix = multiply(XYZ_TO_LIN_P3, LIN_SRGB_TO_XYZ);
const P3_TO_SRGB: Matrix = multiply(XYZ_TO_LIN_SRGB, LIN_P3_TO_XYZ);

/// The sRGB EOTF for every 8-bit code value.
static DECODE: LazyLock<[f32; 256]> = LazyLock::new(|| {
    std::array::from_fn(|code| {
        let v = code as f64 / 255.;
        (if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }) as f32
    })
});

const ENCODE_STEPS: usize = 1 << 14;

/// The sRGB inverse EOTF sampled at the center of each linear interval; the
/// interval is finer than the darkest 8-bit step (about 3e-4 in linear light).
static ENCODE: LazyLock<Box<[u8]>> = LazyLock::new(|| {
    (0..ENCODE_STEPS)
        .map(|step| {
            let v = (step as f64 + 0.5) / ENCODE_STEPS as f64;
            let encoded = if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1. / 2.4) - 0.055
            };
            (encoded * 255.).round().clamp(0., 255.) as u8
        })
        .collect()
});

fn encode(linear: f32) -> u8 {
    // Out-of-gamut results are clipped, as on an sRGB output device.
    if linear <= 0. {
        return 0;
    }
    ENCODE[((linear * ENCODE_STEPS as f32) as usize).min(ENCODE_STEPS - 1)]
}

/// Convert `rgba` (RGBA8 pixels) from `from` into `to`. Premultiplied pixels
/// are converted on their unpremultiplied color and premultiplied again.
pub(crate) fn convert_rgba8(
    rgba: &mut [u8],
    from: ColorSpace,
    to: ColorSpace,
    premultiplied: bool,
) {
    let matrix = match (from, to) {
        (ColorSpace::Srgb, ColorSpace::DisplayP3) => SRGB_TO_P3,
        (ColorSpace::DisplayP3, ColorSpace::Srgb) => P3_TO_SRGB,
        _ => return,
    };
    let matrix = matrix.map(|row| row.map(|value| value as f32));
    let decode = &*DECODE;
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = pixel[3];
        if premultiplied && alpha == 0 {
            continue;
        }
        let mut color = [pixel[0], pixel[1], pixel[2]];
        if premultiplied && alpha < 255 {
            for channel in &mut color {
                *channel =
                    ((*channel as u16 * 255 + alpha as u16 / 2) / alpha as u16).min(255) as u8;
            }
        }
        let linear = color.map(|channel| decode[channel as usize]);
        for (row, out) in matrix.iter().zip(&mut color) {
            *out = encode(row[0] * linear[0] + row[1] * linear[1] + row[2] * linear[2]);
        }
        if premultiplied && alpha < 255 {
            for channel in &mut color {
                *channel = ((*channel as u16 * alpha as u16 + 127) / 255) as u8;
            }
        }
        pixel[..3].copy_from_slice(&color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_and_display_p3_convert_through_css_color_4_matrices() {
        // CSS Color 4 #predefined-display-p3: sRGB red is
        // color(display-p3 0.9175 0.2003 0.1386), and P3's pure red lies
        // outside sRGB and clips to (255, 0, 0).
        let mut red = [255, 0, 0, 255];
        convert_rgba8(&mut red, ColorSpace::Srgb, ColorSpace::DisplayP3, false);
        assert_eq!(red, [234, 51, 35, 255]);
        convert_rgba8(&mut red, ColorSpace::DisplayP3, ColorSpace::Srgb, false);
        assert_eq!(red, [255, 0, 0, 255]);
        let mut p3_red = [255, 0, 0, 255];
        convert_rgba8(&mut p3_red, ColorSpace::DisplayP3, ColorSpace::Srgb, false);
        assert_eq!(p3_red, [255, 0, 0, 255]);
        // Neutral colors are unchanged: both spaces share D65 white.
        let mut grays = [0, 0, 0, 255, 128, 128, 128, 255, 255, 255, 255, 255];
        convert_rgba8(&mut grays, ColorSpace::Srgb, ColorSpace::DisplayP3, false);
        assert_eq!(
            grays,
            [0, 0, 0, 255, 128, 128, 128, 255, 255, 255, 255, 255]
        );
        // Premultiplied pixels convert their color, keeping alpha.
        let mut half = [128, 0, 0, 128];
        convert_rgba8(&mut half, ColorSpace::Srgb, ColorSpace::DisplayP3, true);
        assert_eq!(half, [117, 26, 18, 128]);
        let mut clear = [0, 0, 0, 0];
        convert_rgba8(&mut clear, ColorSpace::Srgb, ColorSpace::DisplayP3, true);
        assert_eq!(clear, [0, 0, 0, 0]);
    }
}
