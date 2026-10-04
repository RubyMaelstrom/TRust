//! AV1 decoding, backed by rav1d (the memory-safety-focused Rust port of
//! dav1d).
//!
//! This module is TRust's only contact with rav1d. It drives rav1d's safe
//! Rust API (dav1d's send/get model) and copies each decoded picture into an
//! owned planar YUV [`Frame`], releasing rav1d's buffer at once. Callers never
//! hold decoder memory, and the planes plus their H.273 colour description
//! stay available to any presentation path: AVIF still images in `img::avif`
//! today, animated AVIF and in-page video later.
//!
//! rav1d reports some internal invariant failures by panicking. A panic is
//! contained here: the affected context is discarded and the call returns
//! [`Error::Panicked`], so a malformed bitstream fails like any other
//! undecodable image. (rav1d's dav1d-compatible `extern "C"` functions cannot
//! unwind, which is why TRust does not use them.)
//!
//! Normative references are the AV1 Bitstream & Decoding Process
//! Specification (local AOMediaCodec/av1-spec snapshot 5e04f3f7), 5.5.2
//! "Color config syntax" and 6.4.2 "Color config semantics", and the AV1
//! Codec ISO Media File Format Binding for what a sample contains.
//!
//! The default build compiles rav1d without assembly; the `asm` Cargo feature
//! enables its hand-written SIMD kernels. rav1d has no logging switch in its
//! Rust API: it prints a line to stderr for malformed OBUs.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use rav1d::{PixelLayout as Rav1dLayout, PlanarImageComponent, Rav1dError};

/// Decoder construction parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecoderConfig {
    /// Worker threads. `1` decodes on the calling thread without spawning any,
    /// which is also the only mode where every rav1d panic is contained: a
    /// panicking rav1d worker thread can leave the caller waiting for it.
    pub threads: u16,
    /// Frames decoded in parallel (frame threading); `1` returns every frame
    /// as soon as it is decoded, which is what still images want.
    pub max_frame_delay: u16,
    /// Largest accepted frame, in pixels (upscaled width × frame height). A
    /// sequence header may claim up to 65536×65536; this rejects such frames
    /// before their buffers are allocated. `0` leaves rav1d's own limit.
    pub frame_size_limit: u32,
    /// Synthesize film grain (AV1 §7.18.3) as part of the output process.
    pub apply_grain: bool,
    /// Output every spatial layer instead of only the highest one.
    pub all_layers: bool,
    /// Operating point to decode (AV1 §6.4.1 `operatingPoint`, 0..=31).
    pub operating_point: u8,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            threads: 1,
            max_frame_delay: 1,
            frame_size_limit: 0,
            apply_grain: true,
            all_layers: false,
            operating_point: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// rav1d rejected the settings or could not create its context.
    Open(Rav1dError),
    /// rav1d reported a decoding error.
    Decode(Rav1dError),
    /// rav1d panicked; its context was discarded.
    Panicked,
    /// An earlier failure discarded the decoder's context.
    Closed,
    /// The bitstream ended without producing a frame.
    NoFrame,
    /// The picture has a layout, depth or buffer this module cannot read.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(f, "AV1 decoder setup failed: {error}"),
            Self::Decode(error) => write!(f, "AV1 decode failed: {error}"),
            Self::Panicked => f.write_str("AV1 decoder failed internally"),
            Self::Closed => f.write_str("AV1 decoder was closed by an earlier error"),
            Self::NoFrame => f.write_str("AV1 data produced no frame"),
            Self::Unsupported(what) => write!(f, "unsupported AV1 picture: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// Chroma subsampling of a decoded picture (AV1 §6.4.2 `subsampling_x/y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    Monochrome,
    Yuv420,
    Yuv422,
    Yuv444,
}

impl PixelLayout {
    /// Horizontal and vertical chroma subsampling shifts.
    pub fn subsampling(self) -> (u32, u32) {
        match self {
            Self::Monochrome | Self::Yuv420 => (1, 1),
            Self::Yuv422 => (1, 0),
            Self::Yuv444 => (0, 0),
        }
    }
}

/// The colour description carried by the sequence header (AV1 §6.4.2), as
/// ISO/IEC 23091-4 / ITU-T H.273 code points. rav1d reports ColourPrimaries
/// and MatrixCoefficients 6 (BT.601) as 5 (BT.470 B/G); the two matrices are
/// identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorDescription {
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// `color_range`: true for full swing, false for studio swing.
    pub full_range: bool,
}

/// Sample storage for one plane. Bit depths above 8 use 16-bit samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Samples {
    Eight(Vec<u8>),
    High(Vec<u16>),
}

/// One tightly packed plane (row stride equals `width`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub samples: Samples,
}

/// A decoded picture: luma plus, unless monochrome, both chroma planes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// 8, 10 or 12.
    pub bit_depth: u8,
    pub layout: PixelLayout,
    pub color: ColorDescription,
    pub y: Plane,
    /// U and V; `None` for monochrome pictures.
    pub chroma: Option<[Plane; 2]>,
}

/// A rav1d decoding context. Feed it AV1 temporal units (low-overhead OBU
/// format, as stored in ISOBMFF samples and AVIF items) and collect frames.
pub struct Decoder {
    inner: Option<rav1d::Decoder>,
}

impl Decoder {
    pub fn new(config: &DecoderConfig) -> Result<Self, Error> {
        let mut settings = rav1d::Settings::new();
        // rav1d's setters panic outside these ranges.
        settings.set_n_threads(u32::from(config.threads.clamp(1, 256)));
        settings.set_max_frame_delay(u32::from(config.max_frame_delay.clamp(1, 256)));
        settings.set_frame_size_limit(config.frame_size_limit);
        settings.set_apply_grain(config.apply_grain);
        settings.set_all_layers(config.all_layers);
        settings.set_operating_point(config.operating_point.min(31));
        let inner = catch_unwind(|| rav1d::Decoder::with_settings(&settings))
            .map_err(|_| Error::Panicked)?
            .map_err(Error::Open)?;
        Ok(Self { inner: Some(inner) })
    }

    /// Decode one temporal unit and return every frame it outputs, in output
    /// order. With the default configuration (one frame of delay, highest
    /// layer only) an AVIF image item yields exactly one frame.
    ///
    /// After an error the decoder is closed and every later call fails.
    pub fn decode(&mut self, temporal_unit: &[u8]) -> Result<Vec<Frame>, Error> {
        let inner = self.inner.as_mut().ok_or(Error::Closed)?;
        match catch_unwind(AssertUnwindSafe(|| decode_unit(inner, temporal_unit))) {
            Ok(Ok(frames)) => Ok(frames),
            Ok(Err(error)) => {
                // rav1d may still hold part of the unit; a fresh context is
                // the only clean way to continue, so refuse further input.
                self.close();
                Err(error)
            }
            Err(_) => {
                self.close();
                Err(Error::Panicked)
            }
        }
    }

    /// Decode a still picture: the last frame output for `temporal_unit`.
    pub fn decode_still(&mut self, temporal_unit: &[u8]) -> Result<Frame, Error> {
        self.decode(temporal_unit)?.pop().ok_or(Error::NoFrame)
    }

    /// Discard queued input and pictures, e.g. before seeking.
    pub fn flush(&mut self) {
        if let Some(inner) = self.inner.as_mut()
            && catch_unwind(AssertUnwindSafe(|| inner.flush())).is_err()
        {
            self.close();
        }
    }

    fn close(&mut self) {
        if let Some(inner) = self.inner.take() {
            // Closing touches the same state that just failed; never let a
            // second panic escape from here.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(inner)));
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

fn decode_unit(decoder: &mut rav1d::Decoder, temporal_unit: &[u8]) -> Result<Vec<Frame>, Error> {
    if temporal_unit.is_empty() {
        return Err(Error::NoFrame);
    }
    let pending = |result: Result<(), Rav1dError>| match result {
        Ok(()) => Ok(false),
        Err(Rav1dError::TryAgain) => Ok(true),
        Err(error) => Err(Error::Decode(error)),
    };
    let mut frames = Vec::new();
    // `TryAgain` from sending means "take output first"; from getting, "send
    // more input". Every pass consumes input or outputs a frame, and a frame
    // needs at least one OBU, so a working decoder never reaches this bound.
    let mut input_pending = pending(decoder.send_data(Box::from(temporal_unit), None, None, None))?;
    for _ in 0..temporal_unit.len().saturating_add(16) {
        match decoder.get_picture() {
            Ok(picture) => frames.push(frame_from(&picture)?),
            Err(Rav1dError::TryAgain) if !input_pending => return Ok(frames),
            Err(Rav1dError::TryAgain) => {}
            Err(error) => return Err(Error::Decode(error)),
        }
        if input_pending {
            input_pending = pending(decoder.send_pending_data())?;
        }
    }
    Err(Error::Decode(Rav1dError::TryAgain))
}

fn frame_from(picture: &rav1d::Picture) -> Result<Frame, Error> {
    let (width, height) = (picture.width(), picture.height());
    if width == 0 || height == 0 {
        return Err(Error::Unsupported("empty picture"));
    }
    let bit_depth = match picture.bit_depth() {
        8 => 8,
        10 => 10,
        12 => 12,
        _ => return Err(Error::Unsupported("bit depth")),
    };
    let layout = match picture.pixel_layout() {
        Rav1dLayout::I400 => PixelLayout::Monochrome,
        Rav1dLayout::I420 => PixelLayout::Yuv420,
        Rav1dLayout::I422 => PixelLayout::Yuv422,
        Rav1dLayout::I444 => PixelLayout::Yuv444,
    };
    let color = ColorDescription {
        color_primaries: picture.color_primaries() as u8,
        transfer_characteristics: picture.transfer_characteristic() as u8,
        matrix_coefficients: picture.matrix_coefficients() as u8,
        full_range: matches!(picture.color_range(), rav1d::pixel::YUVRange::Full),
    };
    let (width, height) = (width as usize, height as usize);
    let y = copy_plane(picture, PlanarImageComponent::Y, width, height, bit_depth)?;
    let chroma = if layout == PixelLayout::Monochrome {
        None
    } else {
        let (shift_x, shift_y) = layout.subsampling();
        let chroma_width = (width + (1 << shift_x) - 1) >> shift_x;
        let chroma_height = (height + (1 << shift_y) - 1) >> shift_y;
        Some([
            copy_plane(
                picture,
                PlanarImageComponent::U,
                chroma_width,
                chroma_height,
                bit_depth,
            )?,
            copy_plane(
                picture,
                PlanarImageComponent::V,
                chroma_width,
                chroma_height,
                bit_depth,
            )?,
        ])
    };
    Ok(Frame {
        width: width as u32,
        height: height as u32,
        bit_depth,
        layout,
        color,
        y,
        chroma,
    })
}

/// Copy one plane out of rav1d's padded, strided buffer.
fn copy_plane(
    picture: &rav1d::Picture,
    component: PlanarImageComponent,
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Result<Plane, Error> {
    let data = picture.plane(component);
    let stride = picture.stride(component) as usize;
    let sample_bytes = if bit_depth > 8 { 2 } else { 1 };
    let row_bytes = width * sample_bytes;
    let row = |index: usize| {
        let start = index.checked_mul(stride)?;
        data.get(start..start.checked_add(row_bytes)?)
    };
    if stride < row_bytes || row(height - 1).is_none() {
        return Err(Error::Unsupported("plane buffer"));
    }
    let samples = if sample_bytes == 1 {
        let mut samples = Vec::with_capacity(width * height);
        for index in 0..height {
            samples.extend_from_slice(row(index).unwrap_or_default());
        }
        Samples::Eight(samples)
    } else {
        let mut samples = Vec::with_capacity(width * height);
        for index in 0..height {
            samples.extend(
                row(index)
                    .unwrap_or_default()
                    .chunks_exact(2)
                    .map(|pair| u16::from_ne_bytes([pair[0], pair[1]])),
            );
        }
        Samples::High(samples)
    };
    Ok(Plane {
        width,
        height,
        samples,
    })
}
