//! The FFmpeg ABI surface this crate depends on.
//!
//! Everything version-sensitive is here and nowhere else. `AVCodecContext`
//! is never mirrored (it is configured through `AVOption`s), and neither are
//! `AVCodec` or `AVCodecParameters` beyond their stable first members. Three
//! structures are touched directly, each only up to the last member needed:
//!
//! * `AVPacket` up to `flags`, `AVCodecParameters` up to `extradata_size` and
//!   `AVCodecDescriptor` up to `id`: identical in libavcodec 59 to 63.
//! * `AVFrame`, which decoders fill and the crate must read: one prefix
//!   layout per distinct release family (see [`FrameLayout`]).
//!
//! The offsets in the tests below were produced by compiling
//! `tools/ffmpeg_abi_offsets.c` against the official public headers of
//! `n5.0.3`, `n5.1.10`, `n6.0.1`, `n6.1.6`, `n7.0.3`, `n7.1.5`, `n8.0.3`,
//! `n8.1.3` and `n9.0.2` (libavutil/frame.h, libavutil/channel_layout.h,
//! libavcodec/packet.h, libavcodec/codec_par.h, libavcodec/codec_desc.h).
//! Which `AVFrame` members exist depends only on the library major version
//! (the `FF_API_*` macros), so one table per major covers all its releases.

use std::ffi::{c_int, c_void};
use std::ptr::addr_of;

macro_rules! opaque {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[repr(C)]
            pub(crate) struct $name {
                _private: [u8; 0],
            }
        )*
    };
}

opaque! {
    /// `const AVCodec`.
    AVCodec,
    /// `AVCodecContext`: configured only through `AVOption`s.
    AVCodecContext,
    /// `AVCodecParameters`, written through [`ParametersHead`].
    AVCodecParameters,
    /// `AVPacket`, written through [`PacketHead`].
    AVPacket,
    /// `AVFrame`, read through [`FrameLayout::snapshot`].
    AVFrame,
}

pub(crate) const AV_NUM_DATA_POINTERS: usize = 8;
/// `AV_INPUT_BUFFER_PADDING_SIZE`: 64 in FFmpeg 5 to 9 (32 before FFmpeg 5).
pub(crate) const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
pub(crate) const AV_NOPTS_VALUE: i64 = i64::MIN;
pub(crate) const AV_PKT_FLAG_KEY: c_int = 1;
pub(crate) const AV_LOG_QUIET: c_int = -8;
/// `AVMEDIA_TYPE_VIDEO` and `AVMEDIA_TYPE_AUDIO`.
pub(crate) const AVMEDIA_TYPE_VIDEO: c_int = 0;
pub(crate) const AVMEDIA_TYPE_AUDIO: c_int = 1;
/// `AVCOL_RANGE_UNSPECIFIED`, `AVCOL_RANGE_MPEG` (limited) and
/// `AVCOL_RANGE_JPEG` (full).
pub(crate) const AVCOL_RANGE_UNSPECIFIED: c_int = 0;
pub(crate) const AVCOL_RANGE_JPEG: c_int = 2;
/// `AVCOL_PRI_UNSPECIFIED`, `AVCOL_TRC_UNSPECIFIED` and `AVCOL_SPC_UNSPECIFIED`.
pub(crate) const AVCOL_UNSPECIFIED: c_int = 2;

/// `FFERRTAG('E','O','F',' ')`.
pub(crate) const AVERROR_EOF: c_int = -0x2046_4F45;
/// `FFERRTAG('I','N','D','A')`.
pub(crate) const AVERROR_INVALIDDATA: c_int = -0x4144_4E49;
/// `AVERROR(EAGAIN)`.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) const AVERROR_EAGAIN: c_int = -35;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
pub(crate) const AVERROR_EAGAIN: c_int = -11;

/// The first members of `AVPacket` (libavcodec/packet.h), unchanged in
/// libavcodec 59 to 63. `av_new_packet` allocates `data` and `size`.
#[repr(C)]
pub(crate) struct PacketHead {
    pub buf: *mut c_void,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    pub stream_index: c_int,
    pub flags: c_int,
}

/// The first members of `AVCodecParameters` (libavcodec/codec_par.h),
/// unchanged in libavcodec 59 to 63. `avcodec_parameters_to_context` copies
/// the extradata into the codec context.
#[repr(C)]
pub(crate) struct ParametersHead {
    pub codec_type: c_int,
    pub codec_id: c_int,
    pub codec_tag: u32,
    pub extradata: *mut u8,
    pub extradata_size: c_int,
}

/// The first member of `AVCodecDescriptor` (libavcodec/codec_desc.h).
#[repr(C)]
pub(crate) struct DescriptorHead {
    pub id: c_int,
}

#[repr(C)]
struct Rational {
    num: c_int,
    den: c_int,
}

/// The first two members of `AVChannelLayout` (libavutil/channel_layout.h,
/// libavutil 57.24 and later).
#[repr(C)]
struct ChannelLayoutHead {
    order: c_int,
    nb_channels: c_int,
}

/// The members of an `AVFrame` this crate reads, whatever the release.
pub(crate) struct FrameFields {
    pub data: [*const u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    /// Per-channel plane pointers of planar audio (equal to `data` for up to
    /// eight channels).
    pub extended_data: *const *const u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    /// An `AVPixelFormat` or `AVSampleFormat`, by the frame's media type.
    pub format: c_int,
    pub pts: i64,
    pub sample_aspect_ratio: (c_int, c_int),
    pub sample_rate: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
    pub channels: c_int,
}

/// `AVFrame` of libavutil 57 and 58 (FFmpeg 5.0, 5.1, 6.0, 6.1), up to the
/// deprecated `channels` (libavutil 57.17 has no `ch_layout` yet, and every
/// decoder still fills `channels`: libavcodec/decode.c, `ff_get_buffer`).
///
/// Members the crate does not read stay declared so the ones it does land at
/// the right offsets; the tests pin every one of them.
#[repr(C)]
#[allow(dead_code)]
struct FrameV57 {
    data: [*mut u8; AV_NUM_DATA_POINTERS],
    linesize: [c_int; AV_NUM_DATA_POINTERS],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    key_frame: c_int,
    pict_type: c_int,
    sample_aspect_ratio: Rational,
    pts: i64,
    pkt_dts: i64,
    time_base: Rational,
    coded_picture_number: c_int,
    display_picture_number: c_int,
    quality: c_int,
    opaque: *mut c_void,
    repeat_pict: c_int,
    interlaced_frame: c_int,
    top_field_first: c_int,
    palette_has_changed: c_int,
    reordered_opaque: i64,
    sample_rate: c_int,
    channel_layout: u64,
    buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    extended_buf: *mut *mut c_void,
    nb_extended_buf: c_int,
    side_data: *mut *mut c_void,
    nb_side_data: c_int,
    flags: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    colorspace: c_int,
    chroma_location: c_int,
    best_effort_timestamp: i64,
    pkt_pos: i64,
    pkt_duration: i64,
    metadata: *mut c_void,
    decode_error_flags: c_int,
    channels: c_int,
}

/// `AVFrame` of libavutil 59 (FFmpeg 7.0, 7.1), up to `ch_layout`.
#[repr(C)]
#[allow(dead_code)]
struct FrameV59 {
    data: [*mut u8; AV_NUM_DATA_POINTERS],
    linesize: [c_int; AV_NUM_DATA_POINTERS],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    key_frame: c_int,
    pict_type: c_int,
    sample_aspect_ratio: Rational,
    pts: i64,
    pkt_dts: i64,
    time_base: Rational,
    quality: c_int,
    opaque: *mut c_void,
    repeat_pict: c_int,
    interlaced_frame: c_int,
    top_field_first: c_int,
    palette_has_changed: c_int,
    sample_rate: c_int,
    buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    extended_buf: *mut *mut c_void,
    nb_extended_buf: c_int,
    side_data: *mut *mut c_void,
    nb_side_data: c_int,
    flags: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    colorspace: c_int,
    chroma_location: c_int,
    best_effort_timestamp: i64,
    pkt_pos: i64,
    metadata: *mut c_void,
    decode_error_flags: c_int,
    pkt_size: c_int,
    hw_frames_ctx: *mut c_void,
    opaque_ref: *mut c_void,
    crop_top: usize,
    crop_bottom: usize,
    crop_left: usize,
    crop_right: usize,
    private_ref: *mut c_void,
    ch_layout: ChannelLayoutHead,
}

/// `AVFrame` of libavutil 60 and 61 (FFmpeg 8.0, 8.1, 9.0), up to
/// `ch_layout`; every deprecated member is gone.
#[repr(C)]
#[allow(dead_code)]
struct FrameV60 {
    data: [*mut u8; AV_NUM_DATA_POINTERS],
    linesize: [c_int; AV_NUM_DATA_POINTERS],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    pict_type: c_int,
    sample_aspect_ratio: Rational,
    pts: i64,
    pkt_dts: i64,
    time_base: Rational,
    quality: c_int,
    opaque: *mut c_void,
    repeat_pict: c_int,
    sample_rate: c_int,
    buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    extended_buf: *mut *mut c_void,
    nb_extended_buf: c_int,
    side_data: *mut *mut c_void,
    nb_side_data: c_int,
    flags: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    colorspace: c_int,
    chroma_location: c_int,
    best_effort_timestamp: i64,
    metadata: *mut c_void,
    decode_error_flags: c_int,
    hw_frames_ctx: *mut c_void,
    opaque_ref: *mut c_void,
    crop_top: usize,
    crop_bottom: usize,
    crop_left: usize,
    crop_right: usize,
    private_ref: *mut c_void,
    ch_layout: ChannelLayoutHead,
}

/// Reads one member of the `AVFrame` prefix behind `$p`.
macro_rules! field {
    ($p:expr, $name:ident) => {
        // SAFETY: callers of `snapshot` guarantee `$p` points at a live
        // AVFrame of the matching release, whose prefix is this struct.
        unsafe { addr_of!((*$p).$name).read() }
    };
}

/// The `FrameFields` common to every layout, with the channel count the
/// caller extracted from the layout-specific member.
macro_rules! snapshot {
    ($p:expr, $channels:expr) => {
        FrameFields {
            data: field!($p, data).map(|plane| plane as *const u8),
            linesize: field!($p, linesize),
            extended_data: field!($p, extended_data) as *const *const u8,
            width: field!($p, width),
            height: field!($p, height),
            nb_samples: field!($p, nb_samples),
            format: field!($p, format),
            pts: field!($p, pts),
            sample_aspect_ratio: {
                let ratio = field!($p, sample_aspect_ratio);
                (ratio.num, ratio.den)
            },
            sample_rate: field!($p, sample_rate),
            color_range: field!($p, color_range),
            color_primaries: field!($p, color_primaries),
            color_trc: field!($p, color_trc),
            colorspace: field!($p, colorspace),
            channels: $channels,
        }
    };
}

/// Which `AVFrame` prefix a loaded libavutil uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameLayout {
    /// libavutil 57 and 58.
    V57,
    /// libavutil 59.
    V59,
    /// libavutil 60 and 61.
    V60,
}

impl FrameLayout {
    /// Reads the members this crate needs from a decoded frame.
    ///
    /// # Safety
    /// `frame` must point at a live `AVFrame` allocated by the libavutil
    /// whose layout `self` names.
    pub(crate) unsafe fn snapshot(self, frame: *const AVFrame) -> FrameFields {
        match self {
            FrameLayout::V57 => {
                let p = frame as *const FrameV57;
                snapshot!(p, field!(p, channels))
            }
            FrameLayout::V59 => {
                let p = frame as *const FrameV59;
                snapshot!(p, field!(p, ch_layout).nb_channels)
            }
            FrameLayout::V60 => {
                let p = frame as *const FrameV60;
                snapshot!(p, field!(p, ch_layout).nb_channels)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// Asserts each `offset_of!` against the offset the official headers
    /// give (the output of `tools/ffmpeg_abi_offsets.c`, LP64).
    macro_rules! offsets {
        ($ty:ty { $($field:ident = $offset:expr),* $(,)? }) => {
            $(
                assert_eq!(
                    offset_of!($ty, $field),
                    $offset,
                    concat!(stringify!($ty), ".", stringify!($field))
                );
            )*
        };
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn frame_v57_matches_ffmpeg_5_and_6_headers() {
        // n5.1.10 (avutil 57.28), n6.0.1 (58.2) and n6.1.6 (58.29) give
        // these offsets; n5.0.3 (57.17) the same ones up to `channels`.
        offsets!(FrameV57 {
            data = 0,
            linesize = 64,
            extended_data = 96,
            width = 104,
            height = 108,
            nb_samples = 112,
            format = 116,
            key_frame = 120,
            pict_type = 124,
            sample_aspect_ratio = 128,
            pts = 136,
            pkt_dts = 144,
            time_base = 152,
            coded_picture_number = 160,
            display_picture_number = 164,
            quality = 168,
            opaque = 176,
            repeat_pict = 184,
            interlaced_frame = 188,
            top_field_first = 192,
            palette_has_changed = 196,
            reordered_opaque = 200,
            sample_rate = 208,
            channel_layout = 216,
            buf = 224,
            extended_buf = 288,
            nb_extended_buf = 296,
            side_data = 304,
            nb_side_data = 312,
            flags = 316,
            color_range = 320,
            color_primaries = 324,
            color_trc = 328,
            colorspace = 332,
            chroma_location = 336,
            best_effort_timestamp = 344,
            pkt_pos = 352,
            pkt_duration = 360,
            metadata = 368,
            decode_error_flags = 376,
            channels = 380,
        });
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn frame_v59_matches_ffmpeg_7_headers() {
        // n7.0.3 (avutil 59.8) and n7.1.5 (59.39).
        offsets!(FrameV59 {
            data = 0,
            linesize = 64,
            extended_data = 96,
            width = 104,
            height = 108,
            nb_samples = 112,
            format = 116,
            key_frame = 120,
            pict_type = 124,
            sample_aspect_ratio = 128,
            pts = 136,
            pkt_dts = 144,
            time_base = 152,
            quality = 160,
            opaque = 168,
            repeat_pict = 176,
            interlaced_frame = 180,
            top_field_first = 184,
            palette_has_changed = 188,
            sample_rate = 192,
            buf = 200,
            extended_buf = 264,
            nb_extended_buf = 272,
            side_data = 280,
            nb_side_data = 288,
            flags = 292,
            color_range = 296,
            color_primaries = 300,
            color_trc = 304,
            colorspace = 308,
            chroma_location = 312,
            best_effort_timestamp = 320,
            pkt_pos = 328,
            metadata = 336,
            decode_error_flags = 344,
            pkt_size = 348,
            hw_frames_ctx = 352,
            opaque_ref = 360,
            crop_top = 368,
            crop_bottom = 376,
            crop_left = 384,
            crop_right = 392,
            private_ref = 400,
            ch_layout = 408,
        });
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn frame_v60_matches_ffmpeg_8_and_9_headers() {
        // n8.0.3 (avutil 60.8), n8.1.3 (60.26) and n9.0.2 (61.1).
        offsets!(FrameV60 {
            data = 0,
            linesize = 64,
            extended_data = 96,
            width = 104,
            height = 108,
            nb_samples = 112,
            format = 116,
            pict_type = 120,
            sample_aspect_ratio = 124,
            pts = 136,
            pkt_dts = 144,
            time_base = 152,
            quality = 160,
            opaque = 168,
            repeat_pict = 176,
            sample_rate = 180,
            buf = 184,
            extended_buf = 248,
            nb_extended_buf = 256,
            side_data = 264,
            nb_side_data = 272,
            flags = 276,
            color_range = 280,
            color_primaries = 284,
            color_trc = 288,
            colorspace = 292,
            chroma_location = 296,
            best_effort_timestamp = 304,
            metadata = 312,
            decode_error_flags = 320,
            hw_frames_ctx = 328,
            opaque_ref = 336,
            crop_top = 344,
            crop_bottom = 352,
            crop_left = 360,
            crop_right = 368,
            private_ref = 376,
            ch_layout = 384,
        });
    }

    #[test]
    fn channel_layout_head_matches_headers() {
        // AVChannelLayout { order; nb_channels; union u; void *opaque; }
        assert_eq!(offset_of!(ChannelLayoutHead, order), 0);
        assert_eq!(offset_of!(ChannelLayoutHead, nb_channels), 4);
        assert_eq!(size_of::<ChannelLayoutHead>(), 8);
        assert_eq!(align_of::<Rational>(), 4);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn packet_parameters_and_descriptor_heads_match_every_release() {
        // Identical in n5.0.3 through n9.0.2.
        offsets!(PacketHead {
            buf = 0,
            pts = 8,
            dts = 16,
            data = 24,
            size = 32,
            stream_index = 36,
            flags = 40,
        });
        offsets!(ParametersHead {
            codec_type = 0,
            codec_id = 4,
            codec_tag = 8,
            extradata = 16,
            extradata_size = 24,
        });
        offsets!(DescriptorHead { id = 0 });
    }

    #[test]
    fn constants_match_every_release() {
        // Verified in n5.0.3 through n9.0.2.
        assert_eq!(AV_NUM_DATA_POINTERS, 8);
        assert_eq!(AV_INPUT_BUFFER_PADDING_SIZE, 64);
        assert_eq!(AV_PKT_FLAG_KEY, 1);
        assert_eq!(AV_LOG_QUIET, -8);
        assert_eq!(AVMEDIA_TYPE_VIDEO, 0);
        assert_eq!(AVMEDIA_TYPE_AUDIO, 1);
        assert_eq!(AVCOL_RANGE_UNSPECIFIED, 0);
        assert_eq!(AVCOL_RANGE_JPEG, 2);
        assert_eq!(AVCOL_UNSPECIFIED, 2);
        assert_eq!(AVERROR_EOF, -541_478_725);
        assert_eq!(AVERROR_INVALIDDATA, -1_094_995_529);
        #[cfg(target_os = "linux")]
        assert_eq!(AVERROR_EAGAIN, -11);
    }

    /// Writes `value` at `offset` of a zeroed byte buffer standing in for an
    /// AVFrame of the layout under test.
    fn put<T: Copy>(frame: &mut [u8], offset: usize, value: T) {
        let bytes =
            unsafe { std::slice::from_raw_parts(&value as *const T as *const u8, size_of::<T>()) };
        frame[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Stores the values the snapshot should find and checks it reads them
    /// back through the layout, from the offsets of the official headers.
    #[cfg(target_pointer_width = "64")]
    fn check_snapshot(layout: FrameLayout, o: &SnapshotOffsets) {
        let mut frame = vec![0u8; 512];
        put(&mut frame, 0, 0x1000usize); // data[0]
        put(&mut frame, 8, 0x2000usize); // data[1]
        put(&mut frame, 64, 640i32); // linesize[0]
        put(&mut frame, 68, -320i32); // linesize[1]
        put(&mut frame, 96, 0x3000usize); // extended_data
        put(&mut frame, 104, 1920i32); // width
        put(&mut frame, 108, 1080i32); // height
        put(&mut frame, 112, 1024i32); // nb_samples
        put(&mut frame, 116, 7i32); // format
        put(&mut frame, o.sample_aspect_ratio, [4i32, 3]);
        put(&mut frame, 136, -42i64); // pts
        put(&mut frame, o.sample_rate, 44100i32);
        put(&mut frame, o.color_range, 2i32);
        put(&mut frame, o.color_range + 4, 9i32); // primaries
        put(&mut frame, o.color_range + 8, 16i32); // trc
        put(&mut frame, o.color_range + 12, 9i32); // colorspace
        put(&mut frame, o.channels, 6i32);
        let fields = unsafe { layout.snapshot(frame.as_ptr() as *const AVFrame) };
        assert_eq!(fields.data[0] as usize, 0x1000);
        assert_eq!(fields.data[1] as usize, 0x2000);
        assert!(fields.data[2].is_null());
        assert_eq!(fields.linesize[..3], [640, -320, 0]);
        assert_eq!(fields.extended_data as usize, 0x3000);
        assert_eq!((fields.width, fields.height), (1920, 1080));
        assert_eq!((fields.nb_samples, fields.format), (1024, 7));
        assert_eq!(fields.pts, -42);
        assert_eq!(fields.sample_aspect_ratio, (4, 3));
        assert_eq!(fields.sample_rate, 44100);
        assert_eq!(fields.color_range, 2);
        assert_eq!(
            (fields.color_primaries, fields.color_trc, fields.colorspace),
            (9, 16, 9)
        );
        assert_eq!(fields.channels, 6);
    }

    #[cfg(target_pointer_width = "64")]
    struct SnapshotOffsets {
        sample_aspect_ratio: usize,
        sample_rate: usize,
        color_range: usize,
        channels: usize,
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn snapshots_read_each_layout_at_the_header_offsets() {
        check_snapshot(
            FrameLayout::V57,
            &SnapshotOffsets {
                sample_aspect_ratio: 128,
                sample_rate: 208,
                color_range: 320,
                channels: 380,
            },
        );
        check_snapshot(
            FrameLayout::V59,
            &SnapshotOffsets {
                sample_aspect_ratio: 128,
                sample_rate: 192,
                color_range: 296,
                channels: 408 + 4,
            },
        );
        check_snapshot(
            FrameLayout::V60,
            &SnapshotOffsets {
                sample_aspect_ratio: 124,
                sample_rate: 180,
                color_range: 280,
                channels: 384 + 4,
            },
        );
    }
}
