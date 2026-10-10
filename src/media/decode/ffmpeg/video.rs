//! Video decoding.

use std::sync::Arc;

use super::abi::{AVCOL_RANGE_JPEG, FrameFields};
use super::codec::CodecId;
use super::decoder::{Core, MediaType, Pipeline};
use super::error::Error;
use super::library::Library;

/// Decoded frames waiting for `receive()` before `send()` reports
/// [`Error::QueueFull`]. Frame threading and reordering hold a handful.
const MAX_QUEUED_FRAMES: usize = 32;
/// Plausibility limits for a decoded picture: larger sizes mean the frame
/// was corrupted or misread, and are rejected rather than copied.
const MAX_DIMENSION: usize = 16384;
const MAX_PIXELS: usize = 1 << 27;
const MAX_THREADS: u32 = 64;

/// Settings for [`VideoDecoder::new`].
#[derive(Clone, Debug, Default)]
pub struct VideoConfig {
    /// Decoding threads; 0 lets FFmpeg choose (up to 16). Frame threading
    /// delays output by about `threads - 1` frames until `drain()`.
    pub threads: u32,
    /// Codec setup data, or `None` for streams that carry their parameter
    /// sets in-band (Annex B H.264 and HEVC, VP8, VP9, AV1 with sequence
    /// headers). For length-prefixed H.264 or HEVC (MP4 and Matroska) it is
    /// the `avcC` / `hvcC` record, and the packets are length-prefixed NAL
    /// units; Annex B parameter sets are accepted too.
    pub extradata: Option<Vec<u8>>,
}

/// The pixel layouts decoded frames are returned in.
///
/// FFmpeg's `yuvj*` aliases are reported as the plain format with
/// `color_range_full` set. Samples of the 10 and 12 bit formats are
/// right-aligned (`yuv420p10le`: 0..=1023); [`PixelFormat::P010`] samples
/// are left-aligned in 16 bits (`value << 6`) as in P010 itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// Planar Y, U, V; chroma halved in both directions. 8 bits.
    Yuv420p,
    /// Planar Y, U, V; chroma halved horizontally. 8 bits.
    Yuv422p,
    /// Planar Y, U, V at full resolution. 8 bits.
    Yuv444p,
    /// Luma only. 8 bits.
    Gray8,
    /// Planar Y and one interleaved U, V plane (chroma halved both ways).
    Nv12,
    Yuv420p10,
    Yuv422p10,
    Yuv444p10,
    Yuv420p12,
    /// NV12 layout with 16-bit samples holding 10-bit values.
    P010,
}

impl PixelFormat {
    /// Bits of precision per sample.
    pub fn bit_depth(self) -> u8 {
        match self {
            PixelFormat::Yuv420p
            | PixelFormat::Yuv422p
            | PixelFormat::Yuv444p
            | PixelFormat::Gray8
            | PixelFormat::Nv12 => 8,
            PixelFormat::Yuv420p10
            | PixelFormat::Yuv422p10
            | PixelFormat::Yuv444p10
            | PixelFormat::P010 => 10,
            PixelFormat::Yuv420p12 => 12,
        }
    }

    /// Whether samples are 16-bit ([`PlaneData::U16`]) rather than bytes.
    pub fn is_high_bit_depth(self) -> bool {
        self.bit_depth() > 8
    }

    /// Width and height in samples of each plane of a `width` x `height`
    /// picture, and how many planes there are.
    fn plane_sizes(self, width: usize, height: usize) -> ([(usize, usize); 3], usize) {
        let half_width = width.div_ceil(2);
        let half_height = height.div_ceil(2);
        match self {
            PixelFormat::Yuv420p | PixelFormat::Yuv420p10 | PixelFormat::Yuv420p12 => (
                [
                    (width, height),
                    (half_width, half_height),
                    (half_width, half_height),
                ],
                3,
            ),
            PixelFormat::Yuv422p | PixelFormat::Yuv422p10 => (
                [(width, height), (half_width, height), (half_width, height)],
                3,
            ),
            PixelFormat::Yuv444p | PixelFormat::Yuv444p10 => {
                ([(width, height), (width, height), (width, height)], 3)
            }
            PixelFormat::Gray8 => ([(width, height), (0, 0), (0, 0)], 1),
            PixelFormat::Nv12 | PixelFormat::P010 => {
                ([(width, height), (2 * half_width, half_height), (0, 0)], 2)
            }
        }
    }
}

/// One plane's samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaneData {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

impl PlaneData {
    pub fn as_u8(&self) -> Option<&[u8]> {
        match self {
            PlaneData::U8(samples) => Some(samples),
            PlaneData::U16(_) => None,
        }
    }

    pub fn as_u16(&self) -> Option<&[u16]> {
        match self {
            PlaneData::U16(samples) => Some(samples),
            PlaneData::U8(_) => None,
        }
    }
}

/// One plane of a decoded picture, copied out of FFmpeg's buffers with
/// tightly packed rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub data: PlaneData,
    /// Samples from the start of one row to the next (equal to `width`).
    pub stride: usize,
    /// Samples per row (for NV12's interleaved plane: U and V samples).
    pub width: usize,
    pub height: usize,
}

/// A decoded picture.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoFrame {
    /// The `pts` given to `send()` with the packet this picture came from
    /// (`i64::MIN` if FFmpeg could not tell).
    pub pts: i64,
    /// Displayed size after cropping, in pixels.
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Y (or luma only), then U and V (or the interleaved UV plane).
    pub planes: Vec<Plane>,
    /// Full-range samples (0..=255 rather than 16..=235 luma).
    pub color_range_full: bool,
    /// Matrix coefficients, H.273 `MatrixCoefficients` (2 if unspecified).
    pub matrix: u8,
    /// Colour primaries, H.273 `ColourPrimaries` (2 if unspecified).
    pub primaries: u8,
    /// Transfer characteristics, H.273 `TransferCharacteristics` (2 if
    /// unspecified).
    pub transfer: u8,
    /// Sample aspect ratio (1, 1) when the stream does not say.
    pub sample_aspect: (u32, u32),
}

/// A video decoder over the system FFmpeg. Feed it packets with
/// [`send`](Self::send) and collect pictures, in display order, with
/// [`receive`](Self::receive). It may move between threads but is used by
/// one at a time.
pub struct VideoDecoder {
    pipeline: Pipeline<VideoFrame>,
}

impl VideoDecoder {
    /// Opens a decoder for `codec` (H.264, HEVC, VP8, VP9 or AV1).
    pub fn new(lib: &Arc<Library>, codec: CodecId, config: VideoConfig) -> Result<Self, Error> {
        if !codec.is_video() {
            return Err(Error::WrongMediaType(codec));
        }
        let mut core = Core::new(lib, codec, MediaType::Video, config.extradata.as_deref())?;
        core.set_int(c"threads", i64::from(config.threads.min(MAX_THREADS)))?;
        // Crop exactly even when the left or top edge is unaligned; the
        // planes are copied byte-wise, so alignment does not matter.
        core.set_str(c"flags", c"+unaligned")?;
        core.open()?;
        Ok(VideoDecoder {
            pipeline: Pipeline::new(core, MAX_QUEUED_FRAMES, convert),
        })
    }

    /// Sends one packet: a whole access unit (Annex B with in-band
    /// parameter sets for H.264 and HEVC) or frame. `pts` is carried to the
    /// decoded picture unchanged; `keyframe` marks a random access point.
    ///
    /// Pictures FFmpeg wants received before it takes more input are held
    /// for `receive()`, so this never needs the output drained first. An
    /// `Err` means this packet was not accepted (see
    /// [`Error::is_invalid_data`]); the decoder remains usable. Failures to
    /// decode earlier packets come out of `receive()`.
    pub fn send(&mut self, data: &[u8], pts: i64, keyframe: bool) -> Result<(), Error> {
        self.pipeline.send(data, pts, keyframe)
    }

    /// The next picture, or `None` when the decoder needs more input. An
    /// `Err` stands for a picture that could not be decoded; call again for
    /// the ones after it.
    pub fn receive(&mut self) -> Result<Option<VideoFrame>, Error> {
        self.pipeline.receive()
    }

    /// Ends the stream and returns every picture still buffered, in order,
    /// each failure in the place of its picture (`.into_iter().flatten()`
    /// keeps only the pictures). The decoder is then reset and accepts a new
    /// stream from a keyframe.
    pub fn drain(&mut self) -> Vec<Result<VideoFrame, Error>> {
        self.pipeline.drain()
    }

    /// Discards buffered pictures and decoder state, for seeking: the next
    /// packet should be a keyframe.
    pub fn flush(&mut self) {
        self.pipeline.flush();
    }
}

/// Copies a decoded `AVFrame` into a [`VideoFrame`].
pub(super) fn convert(lib: &Library, frame: &FrameFields) -> Result<VideoFrame, Error> {
    let (format, full_range_format) =
        lib.formats
            .pixel(frame.format)
            .ok_or_else(|| Error::UnsupportedFormat {
                raw: frame.format,
                name: lib.pixel_format_name(frame.format),
            })?;
    let (width, height) = match (usize::try_from(frame.width), usize::try_from(frame.height)) {
        (Ok(width @ 1..=MAX_DIMENSION), Ok(height @ 1..=MAX_DIMENSION))
            if width * height <= MAX_PIXELS =>
        {
            (width, height)
        }
        _ => return Err(Error::InvalidFrame("implausible picture size")),
    };

    let (sizes, count) = format.plane_sizes(width, height);
    let mut planes = Vec::with_capacity(count);
    for (index, &(plane_width, plane_height)) in sizes[..count].iter().enumerate() {
        // SAFETY: `frame` describes an AVFrame the decoder just filled: for
        // a picture of this size and format its plane pointers address
        // `linesize` bytes per row for every row (rows below the cropped
        // area included), which `copy_plane` bounds-checks against the
        // bytes it reads per row.
        let data = unsafe {
            copy_plane(
                frame.data[index],
                frame.linesize[index],
                plane_width,
                plane_height,
                format.is_high_bit_depth(),
            )?
        };
        planes.push(Plane {
            data,
            stride: plane_width,
            width: plane_width,
            height: plane_height,
        });
    }

    let (ratio_num, ratio_den) = frame.sample_aspect_ratio;
    Ok(VideoFrame {
        pts: frame.pts,
        width: width as u32,
        height: height as u32,
        format,
        planes,
        color_range_full: full_range_format || frame.color_range == AVCOL_RANGE_JPEG,
        matrix: narrow(frame.colorspace),
        primaries: narrow(frame.color_primaries),
        transfer: narrow(frame.color_trc),
        sample_aspect: match (u32::try_from(ratio_num), u32::try_from(ratio_den)) {
            (Ok(num @ 1..), Ok(den @ 1..)) => (num, den),
            _ => (1, 1),
        },
    })
}

/// An H.273 code point as a byte; values FFmpeg defines beyond that range
/// (none today) read as unspecified.
fn narrow(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(2)
}

/// Copies `rows` rows of `samples` samples each from a plane whose rows are
/// `linesize` bytes apart (negative for bottom-up planes, where `plane`
/// addresses the first picture row). 16-bit samples are little-endian.
///
/// # Safety
/// For each of the `rows` rows, `plane + row * linesize` must be readable
/// for `samples` samples, if `plane` is not null.
unsafe fn copy_plane(
    plane: *const u8,
    linesize: i32,
    samples: usize,
    rows: usize,
    wide: bool,
) -> Result<PlaneData, Error> {
    if plane.is_null() {
        return Err(Error::InvalidFrame("missing plane"));
    }
    let row_bytes = samples * if wide { 2 } else { 1 };
    if (linesize.unsigned_abs() as usize) < row_bytes {
        return Err(Error::InvalidFrame(
            "line is shorter than the picture width",
        ));
    }
    let total = samples * rows;
    let row = |index: usize| {
        // SAFETY: by this function's contract.
        unsafe {
            std::slice::from_raw_parts(plane.offset(index as isize * linesize as isize), row_bytes)
        }
    };
    if wide {
        let mut out: Vec<u16> = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| Error::OutOfMemory)?;
        for index in 0..rows {
            out.extend(
                row(index)
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair)),
            );
        }
        Ok(PlaneData::U16(out))
    } else {
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| Error::OutOfMemory)?;
        for index in 0..rows {
            out.extend_from_slice(row(index));
        }
        Ok(PlaneData::U8(out))
    }
}
