//! Pixel and sample formats, resolved by name.
//!
//! `AVPixelFormat` numbers move between releases (`AV_PIX_FMT_P010LE` is 159
//! in FFmpeg 5 and 6 and 158 from 7.0), so the formats this crate converts
//! are looked up with `av_get_pix_fmt` / `av_get_sample_fmt` when the library
//! is opened, and a decoded frame's `format` is matched against that table.
//! (`AVSampleFormat` is the same in FFmpeg 5 to 9 but is resolved alike.)

use std::ffi::{CStr, c_int};

use super::library::Api;
use super::video::PixelFormat;

/// The pixel formats converted, by FFmpeg name. The flag marks the `yuvj*`
/// aliases, which are full range by definition.
const PIXEL_FORMATS: [(&CStr, PixelFormat, bool); 13] = [
    (c"yuv420p", PixelFormat::Yuv420p, false),
    (c"yuvj420p", PixelFormat::Yuv420p, true),
    (c"yuv422p", PixelFormat::Yuv422p, false),
    (c"yuvj422p", PixelFormat::Yuv422p, true),
    (c"yuv444p", PixelFormat::Yuv444p, false),
    (c"yuvj444p", PixelFormat::Yuv444p, true),
    (c"gray", PixelFormat::Gray8, false),
    (c"nv12", PixelFormat::Nv12, false),
    (c"yuv420p10le", PixelFormat::Yuv420p10, false),
    (c"yuv422p10le", PixelFormat::Yuv422p10, false),
    (c"yuv444p10le", PixelFormat::Yuv444p10, false),
    (c"yuv420p12le", PixelFormat::Yuv420p12, false),
    (c"p010le", PixelFormat::P010, false),
];

/// Sample encodings of decoded audio, all in native byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    U8,
    S16,
    S32,
    F32,
    F64,
}

impl SampleFormat {
    pub(crate) fn bytes(self) -> usize {
        match self {
            SampleFormat::U8 => 1,
            SampleFormat::S16 => 2,
            SampleFormat::S32 | SampleFormat::F32 => 4,
            SampleFormat::F64 => 8,
        }
    }
}

/// The sample formats converted, by FFmpeg name; the flag marks planar ones.
const SAMPLE_FORMATS: [(&CStr, SampleFormat, bool); 10] = [
    (c"u8", SampleFormat::U8, false),
    (c"s16", SampleFormat::S16, false),
    (c"s32", SampleFormat::S32, false),
    (c"flt", SampleFormat::F32, false),
    (c"dbl", SampleFormat::F64, false),
    (c"u8p", SampleFormat::U8, true),
    (c"s16p", SampleFormat::S16, true),
    (c"s32p", SampleFormat::S32, true),
    (c"fltp", SampleFormat::F32, true),
    (c"dblp", SampleFormat::F64, true),
];

/// The loaded library's numbers for the formats above.
pub(crate) struct FormatTables {
    pixel: Vec<(c_int, PixelFormat, bool)>,
    sample: Vec<(c_int, SampleFormat, bool)>,
}

impl FormatTables {
    /// A name the library does not know (`AV_PIX_FMT_NONE`, -1) is left out,
    /// so frames in that format are reported as unsupported.
    pub(crate) fn resolve(api: &Api) -> Self {
        let pixel = PIXEL_FORMATS
            .iter()
            // SAFETY: the names are NUL-terminated and the function is a
            // pure lookup.
            .map(|&(name, format, full)| {
                (unsafe { (api.av_get_pix_fmt)(name.as_ptr()) }, format, full)
            })
            .filter(|&(raw, ..)| raw >= 0)
            .collect();
        let sample = SAMPLE_FORMATS
            .iter()
            // SAFETY: as above.
            .map(|&(name, format, planar)| {
                (
                    unsafe { (api.av_get_sample_fmt)(name.as_ptr()) },
                    format,
                    planar,
                )
            })
            .filter(|&(raw, ..)| raw >= 0)
            .collect();
        FormatTables { pixel, sample }
    }

    /// A decoded video frame's `format` as a converted pixel format, and
    /// whether the FFmpeg format (`yuvj*`) implies full range.
    pub(crate) fn pixel(&self, raw: c_int) -> Option<(PixelFormat, bool)> {
        self.pixel
            .iter()
            .find(|&&(value, ..)| value == raw)
            .map(|&(_, format, full)| (format, full))
    }

    /// A decoded audio frame's `format` as a converted sample format, and
    /// whether it is planar.
    pub(crate) fn sample(&self, raw: c_int) -> Option<(SampleFormat, bool)> {
        self.sample
            .iter()
            .find(|&&(value, ..)| value == raw)
            .map(|&(_, format, planar)| (format, planar))
    }
}
