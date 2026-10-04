//! Y'CbCr to straight-alpha R'G'B'A8 conversion for decoded AV1 pictures.
//!
//! The matrices and quantization follow ITU-T H.273 (code points named in
//! AV1 §6.4.2 "Color config semantics"): studio swing scales luma by 219 and
//! chroma by 224 around offsets 16 and 128 (shifted for higher bit depths);
//! full swing uses the whole code range. MatrixCoefficients 0 (identity, GBR)
//! and 8 (YCgCo) have dedicated equations; the chromaticity-derived matrices
//! (12, 13) compute KR/KB from the colour primaries. Code points without an
//! RGB-equivalent equation here (2 unspecified, reserved values, 11 YDzDx and
//! 14 ICtCp) use BT.601, as libavif does. No transfer-function or gamut
//! conversion happens: output samples are the decoded non-linear values,
//! like TRust's other raster formats.
//!
//! Subsampled chroma is reconstructed with a triangle (bilinear) filter that
//! assumes centre-sited chroma, weighting the nearest chroma sample 3/4 and
//! the next one 1/4 in each subsampled direction (libavif's "bilinear"
//! upsampling, also libjpeg's "fancy" upsampling).

use crate::av1::{Frame, Samples};

/// The colour interpretation chosen for a picture: an AVIF `colr` nclx box
/// when present, otherwise the AV1 sequence header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cicp {
    pub color_primaries: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
}

/// A destination region inside a straight-alpha RGBA8 canvas.
pub(crate) struct Canvas<'a> {
    pub rgba: &'a mut [u8],
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Matrix {
    Identity,
    YCgCo,
    YCbCr {
        cr_to_r: f32,
        cb_to_g: f32,
        cr_to_g: f32,
        cb_to_b: f32,
    },
}

impl Matrix {
    fn new(cicp: Cicp) -> Self {
        let (kr, kb) = match cicp.matrix_coefficients {
            0 => return Self::Identity,
            8 => return Self::YCgCo,
            1 => (0.2126, 0.0722),
            4 => (0.30, 0.11),
            7 => (0.212, 0.087),
            9 | 10 => (0.2627, 0.0593),
            12 | 13 => chromaticity_coefficients(cicp.color_primaries).unwrap_or((0.299, 0.114)),
            // 5, 6, and the fallbacks described in the module comment.
            _ => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        Self::YCbCr {
            cr_to_r: 2.0 * (1.0 - kr),
            cb_to_g: 2.0 * kb * (1.0 - kb) / kg,
            cr_to_g: 2.0 * kr * (1.0 - kr) / kg,
            cb_to_b: 2.0 * (1.0 - kb),
        }
    }
}

/// KR and KB for MatrixCoefficients 12/13, derived from the red, green, blue
/// and white chromaticities of ColourPrimaries (H.273 equations for the
/// chromaticity-derived matrices).
fn chromaticity_coefficients(primaries: u8) -> Option<(f32, f32)> {
    // [xr, yr, xg, yg, xb, yb, xw, yw]
    let p: [f64; 8] = match primaries {
        1 | 2 => [0.64, 0.33, 0.30, 0.60, 0.15, 0.06, 0.3127, 0.3290],
        4 => [0.67, 0.33, 0.21, 0.71, 0.14, 0.08, 0.310, 0.316],
        5 => [0.64, 0.33, 0.29, 0.60, 0.15, 0.06, 0.3127, 0.3290],
        6 | 7 => [0.630, 0.340, 0.310, 0.595, 0.155, 0.070, 0.3127, 0.3290],
        8 => [0.681, 0.319, 0.243, 0.692, 0.145, 0.049, 0.310, 0.316],
        9 => [0.708, 0.292, 0.170, 0.797, 0.131, 0.046, 0.3127, 0.3290],
        10 => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0 / 3.0, 1.0 / 3.0],
        11 => [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.314, 0.351],
        12 => [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.3127, 0.3290],
        22 => [0.630, 0.340, 0.295, 0.605, 0.155, 0.077, 0.3127, 0.3290],
        _ => return None,
    };
    let [xr, yr, xg, yg, xb, yb, xw, yw] = p;
    let (zr, zg, zb, zw) = (1.0 - xr - yr, 1.0 - xg - yg, 1.0 - xb - yb, 1.0 - xw - yw);
    let denominator =
        yw * (xr * (yg * zb - yb * zg) + xg * (yb * zr - yr * zb) + xb * (yr * zg - yg * zr));
    if denominator == 0.0 {
        return None;
    }
    let kr = yr * (xw * (yg * zb - yb * zg) + yw * (xb * zg - xg * zb) + zw * (xg * yb - xb * yg))
        / denominator;
    let kb = yb * (xw * (yr * zg - yg * zr) + yw * (xg * zr - xr * zg) + zw * (xr * yg - xg * yr))
        / denominator;
    (kr > 0.0 && kb > 0.0 && kr + kb < 1.0).then_some((kr as f32, kb as f32))
}

/// Normalized sample values for every code value of one bit depth: luma (and
/// GBR/alpha) to [0, 1], chroma to [-0.5, 0.5].
struct Tables {
    luma: Vec<f32>,
    chroma: Vec<f32>,
}

impl Tables {
    fn new(bit_depth: u8, full_range: bool) -> Self {
        let max = (1u32 << bit_depth) - 1;
        let scale = f32::from(1u16 << (bit_depth - 8));
        let luma = (0..=max)
            .map(|v| {
                let v = v as f32;
                if full_range {
                    v / max as f32
                } else {
                    (v - 16.0 * scale) / (219.0 * scale)
                }
            })
            .collect();
        let chroma = (0..=max)
            .map(|v| {
                let v = v as f32;
                if full_range {
                    (v - (max + 1) as f32 / 2.0) / max as f32
                } else {
                    (v - 128.0 * scale) / (224.0 * scale)
                }
            })
            .collect();
        Self { luma, chroma }
    }
}

trait Sample: Copy {
    fn index(self) -> usize;
}

impl Sample for u8 {
    fn index(self) -> usize {
        usize::from(self)
    }
}

impl Sample for u16 {
    fn index(self) -> usize {
        usize::from(self)
    }
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// The part of a `width`×`height` picture placed at (`x`, `y`) that lies
/// inside the canvas.
fn visible(canvas: &Canvas<'_>, x: usize, y: usize, width: usize, height: usize) -> (usize, usize) {
    (
        width.min(canvas.width.saturating_sub(x)),
        height.min(canvas.height.saturating_sub(y)),
    )
}

/// Convert `frame` and write its colour into the canvas at (`x`, `y`), setting
/// alpha to opaque. Parts outside the canvas are skipped, which implements a
/// grid's crop to its output size.
pub(crate) fn write_rgb(frame: &Frame, cicp: Cicp, canvas: &mut Canvas<'_>, x: usize, y: usize) {
    let tables = Tables::new(frame.bit_depth, cicp.full_range);
    let matrix = Matrix::new(cicp);
    // GBR codes all three components like luma (H.273, MatrixCoefficients 0).
    let chroma_table = if matrix == Matrix::Identity {
        &tables.luma
    } else {
        &tables.chroma
    };
    let planes = Planes {
        luma_table: &tables.luma,
        chroma_table,
        matrix,
    };
    let chroma = frame.chroma.as_ref().map(|[u, v]| (&u.samples, &v.samples));
    match (&frame.y.samples, chroma) {
        (Samples::Eight(luma), None) => planes.write::<u8>(frame, luma, None, canvas, x, y),
        (Samples::Eight(luma), Some((Samples::Eight(u), Samples::Eight(v)))) => {
            planes.write::<u8>(frame, luma, Some((u, v)), canvas, x, y)
        }
        (Samples::High(luma), None) => planes.write::<u16>(frame, luma, None, canvas, x, y),
        (Samples::High(luma), Some((Samples::High(u), Samples::High(v)))) => {
            planes.write::<u16>(frame, luma, Some((u, v)), canvas, x, y)
        }
        // One picture never mixes 8-bit and high-bit-depth planes.
        _ => {}
    }
}

struct Planes<'a> {
    luma_table: &'a [f32],
    chroma_table: &'a [f32],
    matrix: Matrix,
}

impl Planes<'_> {
    fn write<T: Sample>(
        &self,
        frame: &Frame,
        luma: &[T],
        chroma: Option<(&[T], &[T])>,
        canvas: &mut Canvas<'_>,
        x: usize,
        y: usize,
    ) {
        let (luma_table, chroma_table, matrix) = (self.luma_table, self.chroma_table, self.matrix);
        let width = frame.y.width;
        let (columns, rows) = visible(canvas, x, y, width, frame.y.height);
        let (shift_x, shift_y) = frame.layout.subsampling();
        let chroma_width = frame.chroma.as_ref().map_or(0, |[u, _]| u.width);
        let chroma_height = frame.chroma.as_ref().map_or(0, |[u, _]| u.height);
        let mut u_row = vec![0.0f32; chroma_width];
        let mut v_row = vec![0.0f32; chroma_width];
        let lookup = |table: &[f32], sample: T| table.get(sample.index()).copied().unwrap_or(0.0);

        for row in 0..rows {
            let luma_row = &luma[row * width..row * width + columns];
            let out_start = ((y + row) * canvas.width + x) * 4;
            let out = &mut canvas.rgba[out_start..out_start + columns * 4];

            let Some((u, v)) = chroma else {
                for (pixel, &sample) in out.chunks_exact_mut(4).zip(luma_row) {
                    let value = to_u8(lookup(luma_table, sample));
                    pixel.copy_from_slice(&[value, value, value, 255]);
                }
                continue;
            };

            // Vertical reconstruction into normalized chroma rows.
            let (near, far, near_weight) = if shift_y == 1 {
                let near = (row >> 1).min(chroma_height - 1);
                let far = if row & 1 == 0 {
                    near.saturating_sub(1)
                } else {
                    (near + 1).min(chroma_height - 1)
                };
                (near, far, 0.75)
            } else {
                (row, row, 1.0)
            };
            for (i, (u_out, v_out)) in u_row.iter_mut().zip(v_row.iter_mut()).enumerate() {
                let (n, f) = (near * chroma_width + i, far * chroma_width + i);
                *u_out = near_weight * lookup(chroma_table, u[n])
                    + (1.0 - near_weight) * lookup(chroma_table, u[f]);
                *v_out = near_weight * lookup(chroma_table, v[n])
                    + (1.0 - near_weight) * lookup(chroma_table, v[f]);
            }

            for (column, (pixel, &sample)) in out.chunks_exact_mut(4).zip(luma_row).enumerate() {
                let (cb, cr) = if shift_x == 1 {
                    let near = (column >> 1).min(chroma_width - 1);
                    let far = if column & 1 == 0 {
                        near.saturating_sub(1)
                    } else {
                        (near + 1).min(chroma_width - 1)
                    };
                    (
                        0.75 * u_row[near] + 0.25 * u_row[far],
                        0.75 * v_row[near] + 0.25 * v_row[far],
                    )
                } else {
                    (u_row[column], v_row[column])
                };
                let luma = lookup(luma_table, sample);
                let (r, g, b) = match matrix {
                    Matrix::Identity => (cr, luma, cb),
                    Matrix::YCgCo => {
                        // Cb carries Cg and Cr carries Co.
                        let t = luma - cb;
                        (t + cr, luma + cb, t - cr)
                    }
                    Matrix::YCbCr {
                        cr_to_r,
                        cb_to_g,
                        cr_to_g,
                        cb_to_b,
                    } => (
                        luma + cr_to_r * cr,
                        luma - cb_to_g * cb - cr_to_g * cr,
                        luma + cb_to_b * cb,
                    ),
                };
                pixel.copy_from_slice(&[to_u8(r), to_u8(g), to_u8(b), 255]);
            }
        }
    }
}

/// Write an alpha auxiliary image's luma into the canvas alpha channel at
/// (`x`, `y`). AVIF alpha is coded full range; a studio-swing alpha is
/// expanded like libavif does.
pub(crate) fn write_alpha(frame: &Frame, canvas: &mut Canvas<'_>, x: usize, y: usize) {
    let tables = Tables::new(frame.bit_depth, frame.color.full_range);
    match &frame.y.samples {
        Samples::Eight(alpha) => write_alpha_plane(frame, alpha, &tables.luma, canvas, x, y),
        Samples::High(alpha) => write_alpha_plane(frame, alpha, &tables.luma, canvas, x, y),
    }
}

fn write_alpha_plane<T: Sample>(
    frame: &Frame,
    alpha: &[T],
    table: &[f32],
    canvas: &mut Canvas<'_>,
    x: usize,
    y: usize,
) {
    let width = frame.y.width;
    let (columns, rows) = visible(canvas, x, y, width, frame.y.height);
    for row in 0..rows {
        let out_start = ((y + row) * canvas.width + x) * 4;
        let out = &mut canvas.rgba[out_start..out_start + columns * 4];
        for (pixel, &sample) in out
            .chunks_exact_mut(4)
            .zip(&alpha[row * width..row * width + columns])
        {
            pixel[3] = to_u8(table.get(sample.index()).copied().unwrap_or(1.0));
        }
    }
}

/// Convert colour that was premultiplied by alpha (MIAF `prem` reference) to
/// the straight alpha TRust's image resources use.
pub(crate) fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        match alpha {
            255 => {}
            0 => pixel[..3].fill(0),
            _ => {
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
    }
}
