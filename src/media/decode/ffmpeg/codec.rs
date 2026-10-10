//! The codecs this crate knows how to ask FFmpeg for.

use std::ffi::CStr;

/// A codec, independent of the FFmpeg release.
///
/// FFmpeg's `AVCodecID` numbers are not stable between releases (`HEVC` is
/// 173 up to FFmpeg 8.1 and 172 in 9.0; `AV1` is 226, 225 and 222), so the
/// loader resolves each codec's id by name when the library is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecId {
    H264,
    Hevc,
    Aac,
    Ac3,
    Eac3,
    Vp8,
    Vp9,
    Av1,
    Mp3,
    Opus,
    Vorbis,
    Flac,
}

impl CodecId {
    /// Every codec, in declaration order; `index()` is the position here.
    pub const ALL: [CodecId; 12] = [
        CodecId::H264,
        CodecId::Hevc,
        CodecId::Aac,
        CodecId::Ac3,
        CodecId::Eac3,
        CodecId::Vp8,
        CodecId::Vp9,
        CodecId::Av1,
        CodecId::Mp3,
        CodecId::Opus,
        CodecId::Vorbis,
        CodecId::Flac,
    ];

    /// Whether this is a video codec (otherwise it is an audio codec).
    pub fn is_video(self) -> bool {
        matches!(
            self,
            CodecId::H264 | CodecId::Hevc | CodecId::Vp8 | CodecId::Vp9 | CodecId::Av1
        )
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    /// FFmpeg's descriptor name (`avcodec_descriptor_get_by_name`), which is
    /// the same in every release.
    pub(crate) fn descriptor_name(self) -> &'static CStr {
        match self {
            CodecId::H264 => c"h264",
            CodecId::Hevc => c"hevc",
            CodecId::Aac => c"aac",
            CodecId::Ac3 => c"ac3",
            CodecId::Eac3 => c"eac3",
            CodecId::Vp8 => c"vp8",
            CodecId::Vp9 => c"vp9",
            CodecId::Av1 => c"av1",
            CodecId::Mp3 => c"mp3",
            CodecId::Opus => c"opus",
            CodecId::Vorbis => c"vorbis",
            CodecId::Flac => c"flac",
        }
    }

    /// The software decoders to use, best first, by FFmpeg name.
    ///
    /// Looked up by name rather than with `avcodec_find_decoder`, which can
    /// return a hardware wrapper (`h264_cuvid`, `hevc_v4l2m2m`, ...) that
    /// needs a device and gives frames this crate does not copy, or, for AV1,
    /// the built-in `av1` decoder that only drives hardware. dav1d and libaom
    /// are the AV1 software decoders; `libopenh264` is the H.264 one of
    /// builds without FFmpeg's own.
    pub(crate) fn decoder_names(self) -> &'static [&'static CStr] {
        match self {
            CodecId::H264 => &[c"h264", c"libopenh264"],
            CodecId::Hevc => &[c"hevc"],
            CodecId::Aac => &[c"aac"],
            CodecId::Ac3 => &[c"ac3"],
            CodecId::Eac3 => &[c"eac3"],
            CodecId::Vp8 => &[c"vp8", c"libvpx"],
            CodecId::Vp9 => &[c"vp9", c"libvpx-vp9"],
            CodecId::Av1 => &[c"libdav1d", c"libaom-av1"],
            CodecId::Mp3 => &[c"mp3float", c"mp3"],
            CodecId::Opus => &[c"opus", c"libopus"],
            CodecId::Vorbis => &[c"vorbis", c"libvorbis"],
            CodecId::Flac => &[c"flac"],
        }
    }
}
