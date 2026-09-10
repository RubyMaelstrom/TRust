//! HTML #shadows / #drawing-model (e5071a20, 2026-09-06).
//!
//! Rasterize the infinite source in shadow coordinates BEFORE canvas/clip
//! cropping. Overlapping source tiles include the entire Gaussian kernel halo;
//! only their disjoint inner output tiles are composited. No retained bitmaps.
use crate::canvas::{State, blend, composite_region, needs_full_source};
use resvg::tiny_skia as sk;
use vello_cpu::kurbo::Affine;

const TILE: u32 = 256;
// HTML explicitly permits a hardware/resource limit on Gaussian sigma. Keep
// sigma <= 64 (radius <= 256), so one RGBA source tile is at most 768² pixels.
const MAX_SIGMA: f64 = 64.;

#[derive(Clone)]
pub(crate) struct Shadow {
    pub color: [f32; 4],
    pub serialized: String,
    pub offset: [f64; 2],
    pub blur: f64,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            color: [0.; 4],
            serialized: "rgba(0, 0, 0, 0)".into(),
            offset: [0.; 2],
            blur: 0.,
        }
    }
}

impl Shadow {
    pub fn drawn(&self) -> bool {
        self.color[3] != 0. && (self.blur != 0. || self.offset != [0., 0.])
    }
}

fn kernel(blur: f64) -> Vec<f32> {
    let sigma = (blur * 0.5).min(MAX_SIGMA);
    if sigma <= 0. {
        return vec![1.];
    }
    let radius = (sigma * 4.).ceil() as i32;
    let mut weights: Vec<_> = (-radius..=radius)
        .map(|i| (-0.5 * (f64::from(i) / sigma).powi(2)).exp() as f32)
        .collect();
    let total: f32 = weights.iter().sum();
    for weight in &mut weights {
        *weight /= total;
    }
    weights
}

pub(crate) fn paint(
    bitmap: &mut sk::Pixmap,
    state: &State,
    mut draw_source: impl FnMut(&mut sk::Pixmap, Affine),
) {
    let shadow = &state.shadow;
    if !shadow.drawn() {
        return;
    }
    let mode = blend(&state.composite).unwrap_or_default();
    // Composite both passes even for copy: fractional clip coverage is applied
    // to each result, so eliding the first pass is not generally equivalent.
    let weights = kernel(shadow.blur);
    let radius = (weights.len() / 2) as u32;
    for top in (0..bitmap.height()).step_by(TILE as usize) {
        for left in (0..bitmap.width()).step_by(TILE as usize) {
            let width = (bitmap.width() - left).min(TILE);
            let height = (bitmap.height() - top).min(TILE);
            let sw = width + 2 * radius;
            let sh = height + 2 * radius;
            let Some(mut source) = sk::Pixmap::new(sw, sh) else {
                return;
            };
            draw_source(
                &mut source,
                Affine::translate((
                    shadow.offset[0] + f64::from(radius) - f64::from(left),
                    shadow.offset[1] + f64::from(radius) - f64::from(top),
                )),
            );
            let empty = source.data().iter().skip(3).step_by(4).all(|a| *a == 0);
            if empty && !needs_full_source(mode) {
                continue;
            }
            let Some(mut output) = sk::Pixmap::new(width, height) else {
                return;
            };
            if !empty {
                // Separable Gaussian, retaining floating point alpha between
                // axes. The four-sigma truncation loses less than 0.00013 of
                // 2D mass; normalize once and quantize only the final pixels.
                let mut horizontal = vec![0f32; width as usize * sh as usize];
                for (source_row, row) in source
                    .data()
                    .chunks_exact(sw as usize * 4)
                    .zip(horizontal.chunks_exact_mut(width as usize))
                {
                    if source_row.iter().skip(3).step_by(4).all(|a| *a == 0) {
                        continue;
                    }
                    for (x, value) in row.iter_mut().enumerate() {
                        *value = weights
                            .iter()
                            .enumerate()
                            .map(|(k, weight)| f32::from(source_row[(x + k) * 4 + 3]) * weight)
                            .sum();
                    }
                }
                for (y, row) in output
                    .data_mut()
                    .chunks_exact_mut(width as usize * 4)
                    .enumerate()
                {
                    for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let alpha: f32 = weights
                            .iter()
                            .enumerate()
                            .map(|(k, weight)| horizontal[(y + k) * width as usize + x] * weight)
                            .sum();
                        let alpha = (alpha * shadow.color[3]).round().clamp(0., 255.) as u8;
                        for (channel, color) in pixel[..3].iter_mut().zip(shadow.color) {
                            *channel = (f32::from(alpha) * color).round() as u8;
                        }
                        pixel[3] = alpha;
                    }
                }
            }
            composite_region(bitmap, &output, left, top, mode, state.clip.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_kernel_is_normalized_symmetric_and_resource_bounded() {
        for blur in [0., f64::MIN_POSITIVE, 0.01, 4., 128., f64::MAX] {
            let values = kernel(blur);
            assert!(values.len() <= 513);
            assert!((values.iter().sum::<f32>() - 1.).abs() < 1e-5);
            assert!(values.iter().all(|v| v.is_finite() && *v >= 0.));
            assert!(values.iter().eq(values.iter().rev()));
        }
    }
}
