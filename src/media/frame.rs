//! Decoded media: video frames in their native planar YUV layout and audio
//! in interleaved 32-bit float.
//!
//! [`VideoFrame::to_rgba`] is the CPU presentation path: an 8-bit fixed-point
//! Y'CbCr→R'G'B' conversion (ITU-T H.273 matrices and quantization, as
//! `img::yuv` uses at full precision for still images) with optional
//! integer box downscaling, cheap enough to run per frame. Colour is not
//! converted between primaries or transfer functions here: SDR video is
//! shown as sRGB, as browsers do without colour management.

use std::sync::Arc;

use super::Micros;
use super::track::ColorInfo;

/// Samples of one plane: 8-bit, or 16-bit containers for 9–16-bit depths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaneData {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// One image plane. `stride` is in samples, not bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub data: PlaneData,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl Plane {
    pub fn u8(data: Vec<u8>, stride: usize, width: usize, height: usize) -> Self {
        Self {
            data: PlaneData::U8(data),
            stride,
            width,
            height,
        }
    }

    pub fn u16(data: Vec<u16>, stride: usize, width: usize, height: usize) -> Self {
        Self {
            data: PlaneData::U16(data),
            stride,
            width,
            height,
        }
    }

    fn valid(&self, samples_per_pixel: usize) -> bool {
        let len = match &self.data {
            PlaneData::U8(data) => data.len(),
            PlaneData::U16(data) => data.len(),
        };
        self.stride >= self.width * samples_per_pixel
            && (self.height == 0
                || len >= self.stride * (self.height - 1) + self.width * samples_per_pixel)
    }
}

/// How the planes of a frame are arranged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Y, U and V planes with the given chroma subsampling shifts.
    Planar { chroma_shift: (u8, u8) },
    /// Y plane plus one interleaved UV plane, 4:2:0 (NV12, P010).
    SemiPlanar,
    /// Luma only.
    Monochrome,
}

/// A decoded picture ready for presentation at `pts`.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub pts: Micros,
    pub duration: Micros,
    /// Visible picture size in samples.
    pub width: u32,
    pub height: u32,
    /// Presentation size (pixel aspect ratio applied).
    pub display_width: u32,
    pub display_height: u32,
    pub bit_depth: u8,
    pub layout: Layout,
    pub color: ColorInfo,
    pub planes: Vec<Plane>,
}

impl VideoFrame {
    /// Whether the planes are consistent with the declared layout and size.
    pub fn valid(&self) -> bool {
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 || !(8..=16).contains(&self.bit_depth) {
            return false;
        }
        let Some(y) = self.planes.first() else {
            return false;
        };
        if y.width < w || y.height < h || !y.valid(1) {
            return false;
        }
        match self.layout {
            Layout::Monochrome => true,
            Layout::Planar {
                chroma_shift: (sx, sy),
            } => {
                let cw = (w + (1 << sx) - 1) >> sx;
                let ch = (h + (1 << sy) - 1) >> sy;
                self.planes.len() >= 3
                    && self.planes[1..3]
                        .iter()
                        .all(|p| p.width >= cw && p.height >= ch && p.valid(1))
            }
            Layout::SemiPlanar => {
                let cw = w.div_ceil(2);
                let ch = h.div_ceil(2);
                self.planes.len() >= 2
                    && self.planes[1].width >= cw
                    && self.planes[1].height >= ch
                    && self.planes[1].valid(2)
            }
        }
    }

    /// The integer factor by which [`Self::to_rgba`] would reduce this frame
    /// for a presentation box of `target` pixels: the largest box filter
    /// that keeps the result at least as large as the target.
    pub fn reduction_for(&self, target: (u32, u32)) -> u32 {
        if target.0 == 0 || target.1 == 0 {
            return 1;
        }
        let fx = self.width / target.0.max(1);
        let fy = self.height / target.1.max(1);
        fx.min(fy).clamp(1, 8)
    }

    /// Convert to straight-alpha (opaque) RGBA8, reduced by an integer box
    /// filter of `factor` (1 = full size). Returns the pixels and their size.
    pub fn to_rgba(&self, factor: u32) -> Option<(Vec<u8>, u32, u32)> {
        if !self.valid() {
            return None;
        }
        let factor = factor.clamp(1, 8) as usize;
        let out_w = (self.width as usize / factor).max(1);
        let out_h = (self.height as usize / factor).max(1);
        let mut rgba = vec![0u8; out_w * out_h * 4];
        let coefficients = Coefficients::new(self.color, self.bit_depth);
        // Split rows across threads for large pictures.
        let rows_per_job = 64;
        let jobs = out_h.div_ceil(rows_per_job);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(4);
        if jobs > 1 && threads > 1 && out_w * out_h >= 640 * 360 {
            let chunk_rows = out_h.div_ceil(threads);
            std::thread::scope(|scope| {
                for (index, chunk) in rgba.chunks_mut(chunk_rows * out_w * 4).enumerate() {
                    let first = index * chunk_rows;
                    let coefficients = &coefficients;
                    scope.spawn(move || {
                        self.convert_rows(coefficients, factor, out_w, first, chunk);
                    });
                }
            });
        } else {
            self.convert_rows(&coefficients, factor, out_w, 0, &mut rgba);
        }
        Some((rgba, out_w as u32, out_h as u32))
    }

    fn convert_rows(
        &self,
        k: &Coefficients,
        factor: usize,
        out_w: usize,
        first_row: usize,
        out: &mut [u8],
    ) {
        if factor == 1 && self.convert_rows_8bit(k, out_w, first_row, out) {
            return;
        }
        self.convert_rows_by_sample(k, factor, out_w, first_row, out);
    }

    /// [`Self::convert_rows`] at full size for 8-bit samples, the common
    /// case and one run for every presented frame: each row's luma and
    /// chroma are read as slices rather than one clamped, bounds-checked
    /// sample at a time, and a chroma pair's terms are computed once for the
    /// pixels sharing it. The pixels are those of the general path. False
    /// (writing nothing) for other depths, subsamplings or the identity
    /// matrix.
    fn convert_rows_8bit(
        &self,
        k: &Coefficients,
        out_w: usize,
        first_row: usize,
        out: &mut [u8],
    ) -> bool {
        let plane = |index: usize| match self.planes.get(index) {
            Some(Plane {
                data: PlaneData::U8(data),
                stride,
                ..
            }) if self.bit_depth == 8 => Some((data.as_slice(), *stride)),
            _ => None,
        };
        let Some((luma, luma_stride)) = plane(0) else {
            return false;
        };
        if k.identity {
            return false;
        }
        let chroma = match self.layout {
            Layout::Monochrome => None,
            Layout::Planar {
                chroma_shift: (sx @ (0 | 1), sy),
            } => match (plane(1), plane(2)) {
                (Some(u), Some(v)) => Some((u, Some(v), sx, sy)),
                _ => return false,
            },
            Layout::Planar { .. } => return false,
            // Interleaved Cb/Cr pairs, 4:2:0.
            Layout::SemiPlanar => match plane(1) {
                Some(uv) => Some((uv, None, 1, 1)),
                None => return false,
            },
        };
        for (row, line) in out.chunks_exact_mut(out_w * 4).enumerate() {
            let y = first_row + row;
            let lumas = &luma[y * luma_stride..][..out_w];
            let pixels = line.as_chunks_mut::<4>().0;
            match chroma {
                None => {
                    let terms = k.terms(128, 128);
                    for (pixel, &luma) in pixels.iter_mut().zip(lumas) {
                        *pixel = k.pixel(i32::from(luma), terms);
                    }
                }
                Some(((u, u_stride), Some((v, v_stride)), sx, sy)) => {
                    let cy = y >> sy;
                    let pairs = u[cy * u_stride..]
                        .iter()
                        .copied()
                        .zip(v[cy * v_stride..].iter().copied());
                    if sx == 0 {
                        for ((pixel, &luma), (cb, cr)) in pixels.iter_mut().zip(lumas).zip(pairs) {
                            *pixel = k.pixel(i32::from(luma), k.terms(cb.into(), cr.into()));
                        }
                    } else {
                        convert_pixel_pairs(k, pixels, lumas, pairs);
                    }
                }
                Some(((uv, uv_stride), None, _, sy)) => {
                    let pairs = uv[(y >> sy) * uv_stride..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&[u, v]| (u, v));
                    convert_pixel_pairs(k, pixels, lumas, pairs);
                }
            }
        }
        true
    }

    fn convert_rows_by_sample(
        &self,
        k: &Coefficients,
        factor: usize,
        out_w: usize,
        first_row: usize,
        out: &mut [u8],
    ) {
        let rows = out.len() / (out_w * 4);
        let shift = self.bit_depth.saturating_sub(8);
        let area = (factor * factor) as u32;
        for row in 0..rows {
            let oy = first_row + row;
            let line = &mut out[row * out_w * 4..(row + 1) * out_w * 4];
            for ox in 0..out_w {
                let (y, u, v) = if factor == 1 {
                    self.sample(ox, oy)
                } else {
                    // Box filter: average luma over the block, chroma at its
                    // centre (chroma is already lower resolution).
                    let mut sum = 0u32;
                    for dy in 0..factor {
                        for dx in 0..factor {
                            sum += self.luma(ox * factor + dx, oy * factor + dy);
                        }
                    }
                    let (_, u, v) = self.sample(ox * factor + factor / 2, oy * factor + factor / 2);
                    ((sum + area / 2) / area, u, v)
                };
                let (r, g, b) = k.apply(y >> shift, u >> shift, v >> shift);
                let pixel = &mut line[ox * 4..ox * 4 + 4];
                pixel[0] = r;
                pixel[1] = g;
                pixel[2] = b;
                pixel[3] = 255;
            }
        }
    }

    #[inline]
    fn luma(&self, x: usize, y: usize) -> u32 {
        let plane = &self.planes[0];
        let x = x.min(self.width as usize - 1);
        let y = y.min(self.height as usize - 1);
        read(plane, y * plane.stride + x)
    }

    #[inline]
    fn sample(&self, x: usize, y: usize) -> (u32, u32, u32) {
        let x = x.min(self.width as usize - 1);
        let y = y.min(self.height as usize - 1);
        let luma = read(&self.planes[0], y * self.planes[0].stride + x);
        let mid = 128u32 << self.bit_depth.saturating_sub(8);
        match self.layout {
            Layout::Monochrome => (luma, mid, mid),
            Layout::Planar {
                chroma_shift: (sx, sy),
            } => {
                let cx = x >> sx;
                let cy = y >> sy;
                let u = &self.planes[1];
                let v = &self.planes[2];
                (
                    luma,
                    read(u, cy * u.stride + cx),
                    read(v, cy * v.stride + cx),
                )
            }
            Layout::SemiPlanar => {
                let uv = &self.planes[1];
                let index = (y >> 1) * uv.stride + (x >> 1) * 2;
                (luma, read(uv, index), read(uv, index + 1))
            }
        }
    }
}

/// One row of 8-bit samples with horizontally halved chroma: each (Cb, Cr)
/// of `chroma` serves two pixels, and the next one a last odd pixel. Fixed
/// size pairs let the compiler vectorize the row (several times faster than
/// pixel-at-a-time chunks).
#[inline]
fn convert_pixel_pairs(
    k: &Coefficients,
    pixels: &mut [[u8; 4]],
    lumas: &[u8],
    mut chroma: impl Iterator<Item = (u8, u8)>,
) {
    let (pixel_pairs, pixel_tail) = pixels.as_chunks_mut::<2>();
    let (luma_pairs, luma_tail) = lumas.as_chunks::<2>();
    for ((pixels, &[left, right]), (cb, cr)) in
        pixel_pairs.iter_mut().zip(luma_pairs).zip(&mut chroma)
    {
        let terms = k.terms(cb.into(), cr.into());
        *pixels = [
            k.pixel(i32::from(left), terms),
            k.pixel(i32::from(right), terms),
        ];
    }
    if let ([pixel], [luma], Some((cb, cr))) = (pixel_tail, luma_tail, chroma.next()) {
        *pixel = k.pixel(i32::from(*luma), k.terms(cb.into(), cr.into()));
    }
}

#[inline]
fn read(plane: &Plane, index: usize) -> u32 {
    match &plane.data {
        PlaneData::U8(data) => u32::from(data[index]),
        PlaneData::U16(data) => u32::from(data[index]),
    }
}

/// Fixed-point (14-bit) conversion constants for 8-bit inputs.
struct Coefficients {
    y_offset: i32,
    y_scale: i32,
    cr_r: i32,
    cb_g: i32,
    cr_g: i32,
    cb_b: i32,
    identity: bool,
}

impl Coefficients {
    fn new(color: ColorInfo, _bit_depth: u8) -> Self {
        // H.273 Table 4 KR/KB. Unspecified (2) and reserved code points use
        // BT.601 for SD sizes; callers pass BT.709 for HD when unspecified.
        let (kr, kb) = match color.matrix {
            1 => (0.2126, 0.0722),
            4 => (0.30, 0.11),
            5 | 6 => (0.299, 0.114),
            7 => (0.212, 0.087),
            9 | 10 => (0.2627, 0.0593),
            _ => (0.299, 0.114),
        };
        let identity = color.matrix == 0;
        let (y_offset, y_range, c_range) = if color.full_range {
            (0.0, 255.0, 255.0)
        } else {
            (16.0, 219.0, 224.0)
        };
        let kg = 1.0 - kr - kb;
        let scale = |value: f64| (value * f64::from(1 << 14)).round() as i32;
        let y_scale = 255.0 / y_range;
        let c_scale = 255.0 / c_range;
        Self {
            y_offset: y_offset as i32,
            y_scale: scale(y_scale),
            cr_r: scale(2.0 * (1.0 - kr) * c_scale),
            cb_g: scale(2.0 * (1.0 - kb) * kb / kg * c_scale),
            cr_g: scale(2.0 * (1.0 - kr) * kr / kg * c_scale),
            cb_b: scale(2.0 * (1.0 - kb) * c_scale),
            identity,
        }
    }

    #[inline]
    fn apply(&self, y: u32, u: u32, v: u32) -> (u8, u8, u8) {
        if self.identity {
            // GBR: Y'=G, Cb=B, Cr=R.
            return (v.min(255) as u8, y.min(255) as u8, u.min(255) as u8);
        }
        let [r, g, b, _] = self.pixel(y as i32, self.terms(u as i32, v as i32));
        (r, g, b)
    }

    /// The chroma contributions (with rounding) to R', G' and B' of the
    /// pixels sharing one (Cb, Cr) sample.
    #[inline]
    fn terms(&self, cb: i32, cr: i32) -> [i32; 3] {
        let (cb, cr) = (cb - 128, cr - 128);
        let round = 1 << 13;
        [
            self.cr_r * cr + round,
            round - self.cb_g * cb - self.cr_g * cr,
            self.cb_b * cb + round,
        ]
    }

    /// One opaque pixel from its luma and its chroma `terms`.
    #[inline]
    fn pixel(&self, y: i32, [r, g, b]: [i32; 3]) -> [u8; 4] {
        let y = (y - self.y_offset) * self.y_scale;
        let channel = |term: i32| ((y + term) >> 14).clamp(0, 255) as u8;
        [channel(r), channel(g), channel(b), 255]
    }
}

/// The matrix to assume when a stream leaves it unspecified: BT.709 for HD
/// sizes, BT.601 otherwise (as browsers and FFmpeg do).
pub fn default_color(width: u32, height: u32) -> ColorInfo {
    let hd = width > 1024 || height > 576;
    ColorInfo {
        primaries: if hd { 1 } else { 6 },
        transfer: if hd { 1 } else { 6 },
        matrix: if hd { 1 } else { 6 },
        full_range: false,
    }
}

/// Fill in unspecified (2) colour code points.
pub fn resolve_color(color: Option<ColorInfo>, width: u32, height: u32) -> ColorInfo {
    let fallback = default_color(width, height);
    match color {
        None => fallback,
        Some(mut color) => {
            if color.matrix == 2 || color.matrix == 3 {
                color.matrix = fallback.matrix;
            }
            if color.primaries == 2 {
                color.primaries = fallback.primaries;
            }
            if color.transfer == 2 {
                color.transfer = fallback.transfer;
            }
            color
        }
    }
}

/// Decoded audio: interleaved 32-bit float samples in [-1, 1].
#[derive(Clone, Debug)]
pub struct AudioBlock {
    pub pts: Micros,
    pub rate: u32,
    pub channels: u16,
    pub samples: Arc<[f32]>,
}

impl AudioBlock {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / usize::from(self.channels)
        }
    }

    pub fn duration(&self) -> Micros {
        if self.rate == 0 {
            return 0;
        }
        (self.frames() as i64 * super::MICROS_PER_SECOND) / i64::from(self.rate)
    }

    pub fn end(&self) -> Micros {
        self.pts + self.duration()
    }

    /// Drop `micros` of audio from the start (codec delay, seek pre-roll).
    pub fn trim_start(&mut self, micros: Micros) {
        if micros <= 0 || self.channels == 0 {
            return;
        }
        let frames = ((micros as i128 * i128::from(self.rate)) / 1_000_000)
            .min(self.frames() as i128) as usize;
        let channels = usize::from(self.channels);
        self.samples = Arc::from(&self.samples[frames * channels..]);
        self.pts += (frames as i64 * super::MICROS_PER_SECOND) / i64::from(self.rate.max(1));
    }

    /// Drop `micros` of audio from the end (DiscardPadding, stream end).
    pub fn trim_end(&mut self, micros: Micros) {
        if micros <= 0 || self.channels == 0 {
            return;
        }
        let drop = ((micros as i128 * i128::from(self.rate)) / 1_000_000).min(self.frames() as i128)
            as usize;
        let keep = self.frames() - drop;
        let channels = usize::from(self.channels);
        self.samples = Arc::from(&self.samples[..keep * channels]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(y: u8, u: u8, v: u8, full_range: bool, matrix: u8) -> VideoFrame {
        VideoFrame {
            pts: 0,
            duration: 0,
            width: 4,
            height: 2,
            display_width: 4,
            display_height: 2,
            bit_depth: 8,
            layout: Layout::Planar {
                chroma_shift: (1, 1),
            },
            color: ColorInfo {
                primaries: 1,
                transfer: 1,
                matrix,
                full_range,
            },
            planes: vec![
                Plane::u8(vec![y; 8], 4, 4, 2),
                Plane::u8(vec![u; 2], 2, 2, 1),
                Plane::u8(vec![v; 2], 2, 2, 1),
            ],
        }
    }

    #[test]
    fn limited_range_extremes() {
        let (white, w, h) = frame(235, 128, 128, false, 1).to_rgba(1).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&white[..4], &[255, 255, 255, 255]);
        let (black, ..) = frame(16, 128, 128, false, 1).to_rgba(1).unwrap();
        assert_eq!(&black[..4], &[0, 0, 0, 255]);
        let (full_white, ..) = frame(255, 128, 128, true, 1).to_rgba(1).unwrap();
        assert_eq!(&full_white[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn bt709_red() {
        // BT.709 limited-range pure red: Y'=63, Cb=102, Cr=240.
        let (red, ..) = frame(63, 102, 240, false, 1).to_rgba(1).unwrap();
        assert!(
            red[0] >= 253 && red[1] <= 2 && red[2] <= 2,
            "{:?}",
            &red[..4]
        );
        // BT.601 limited-range pure red: Y'=81, Cb=90, Cr=240.
        let (red601, ..) = frame(81, 90, 240, false, 6).to_rgba(1).unwrap();
        assert!(
            red601[0] >= 253 && red601[1] <= 2 && red601[2] <= 2,
            "{:?}",
            &red601[..4]
        );
    }

    #[test]
    fn box_reduction_and_high_depth() {
        let mut big = frame(235, 128, 128, false, 1);
        big.width = 4;
        big.height = 2;
        let (pixels, w, h) = big.to_rgba(2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(&pixels[..4], &[255, 255, 255, 255]);
        let high = VideoFrame {
            bit_depth: 10,
            planes: vec![
                Plane::u16(vec![940; 8], 4, 4, 2),
                Plane::u16(vec![512; 2], 2, 2, 1),
                Plane::u16(vec![512; 2], 2, 2, 1),
            ],
            ..frame(0, 0, 0, false, 1)
        };
        let (pixels, ..) = high.to_rgba(1).unwrap();
        assert_eq!(&pixels[..4], &[255, 255, 255, 255]);
    }

    /// The 8-bit full-size row path converts exactly as the per-sample path
    /// does, for every layout and odd sizes (chroma rounded up, the last
    /// pixel of a pair-wise row on its own).
    #[test]
    fn row_conversion_matches_sample_conversion() {
        let (w, h) = (7usize, 5usize);
        let ramp = |len: usize, seed: usize| -> Vec<u8> {
            (0..len)
                .map(|i| ((i * 37 + seed * 101) % 256) as u8)
                .collect()
        };
        let layouts = [
            Layout::Planar {
                chroma_shift: (1, 1),
            },
            Layout::Planar {
                chroma_shift: (1, 0),
            },
            Layout::Planar {
                chroma_shift: (0, 0),
            },
            Layout::SemiPlanar,
            Layout::Monochrome,
        ];
        for (index, layout) in layouts.into_iter().enumerate() {
            let (cw, ch, planes) = match layout {
                Layout::Planar {
                    chroma_shift: (sx, sy),
                } => {
                    let cw = (w + (1 << sx) - 1) >> sx;
                    let ch = (h + (1 << sy) - 1) >> sy;
                    (cw, ch, 2)
                }
                Layout::SemiPlanar => (w.div_ceil(2), h.div_ceil(2), 1),
                Layout::Monochrome => (0, 0, 0),
            };
            let mut planes_out = vec![Plane::u8(ramp(9 * h, index), 9, w, h)];
            for plane in 0..planes {
                let width = if matches!(layout, Layout::SemiPlanar) {
                    cw * 2
                } else {
                    cw
                };
                planes_out.push(Plane::u8(
                    ramp((width + 3) * ch, plane + 7),
                    width + 3,
                    cw,
                    ch,
                ));
            }
            for (matrix, full_range) in [(1, false), (6, true), (0, true)] {
                let frame = VideoFrame {
                    width: w as u32,
                    height: h as u32,
                    layout,
                    planes: planes_out.clone(),
                    ..frame(0, 0, 0, full_range, matrix)
                };
                assert!(frame.valid(), "{layout:?}");
                let k = Coefficients::new(frame.color, 8);
                let mut rows = vec![0u8; w * h * 4];
                // The identity (GBR) matrix keeps to the general path.
                let converted = frame.convert_rows_8bit(&k, w, 0, &mut rows);
                assert_eq!(converted, matrix != 0, "{layout:?}");
                if !converted {
                    continue;
                }
                let mut samples = vec![0u8; w * h * 4];
                frame.convert_rows_by_sample(&k, 1, w, 0, &mut samples);
                assert_eq!(rows, samples, "{layout:?} matrix {matrix}");
                // A band of rows starting part way down, as the threads run.
                let mut band = vec![0u8; w * 2 * 4];
                assert!(frame.convert_rows_8bit(&k, w, 3, &mut band));
                assert_eq!(band, samples[w * 3 * 4..w * 5 * 4]);
            }
        }
    }

    #[test]
    fn invalid_planes_are_refused() {
        let mut broken = frame(0, 0, 0, false, 1);
        broken.planes[0] = Plane::u8(vec![0; 3], 4, 4, 2);
        assert!(broken.to_rgba(1).is_none());
        broken.planes.truncate(1);
        assert!(!broken.valid());
    }

    #[test]
    fn audio_trimming() {
        let mut block = AudioBlock {
            pts: 0,
            rate: 1000,
            channels: 2,
            samples: Arc::from(vec![0.0f32; 2000]),
        };
        assert_eq!(block.duration(), 1_000_000);
        block.trim_start(250_000);
        assert_eq!(block.frames(), 750);
        assert_eq!(block.pts, 250_000);
        block.trim_end(500_000);
        assert_eq!(block.frames(), 250);
        assert_eq!(block.end(), 500_000);
    }
}
