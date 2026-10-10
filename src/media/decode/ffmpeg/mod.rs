//! Audio and video decoding through the system FFmpeg, loaded at run time.
//!
//! libavcodec and libavutil are opened with `dlopen` when first asked for
//! (the way Firefox's `FFmpegRuntimeLinker` and Ladybird's `SystemFFmpeg`
//! do) and are never linked, so the program starts and runs without FFmpeg
//! and gains H.264, HEVC and AAC (with VP8, VP9, AV1, AC-3, E-AC-3, MP3,
//! Opus, Vorbis and FLAC as fallbacks) from the user's own installation when
//! there is one. Only decoders are used; nothing is encoded or demuxed.
//!
//! # Supported releases
//!
//! | FFmpeg   | libavcodec | libavutil | `AVFrame` prefix |
//! |----------|-----------:|----------:|------------------|
//! | 5.0, 5.1 | 59         | 57        | `FrameV57`       |
//! | 6.0, 6.1 | 60         | 58        | `FrameV57`       |
//! | 7.0, 7.1 | 61         | 59        | `FrameV59`       |
//! | 8.0, 8.1 | 62         | 60        | `FrameV60`       |
//! | 9.0      | 63         | 61        | `FrameV60`       |
//!
//! The major pairs are `LIBAVCODEC_VERSION_MAJOR` / `LIBAVUTIL_VERSION_MAJOR`
//! in libavcodec/version_major.h and libavutil/version.h of tags `n5.0.3`,
//! `n6.0.1`, `n7.0.3`, `n8.0.3` and `n9.0.2`. `libavcodec.so.63` down to
//! `.59` are tried in that order. A library must have micro version 100 or
//! more (FFmpeg numbers from 100, Libav does not), pair with the libavutil of
//! its release, and export every function listed in `library.rs`; anything
//! else, including FFmpeg 4 and releases newer than 9.0, is refused with a
//! [`LoadError`] that says why for each candidate.
//!
//! `TRUST_FFMPEG=0` disables the library. `TRUST_FFMPEG_LIB=/path/libavcodec.so.N`
//! uses that file and the libavutil beside it instead of searching.
//! `TRUST_FFMPEG_LOG` leaves FFmpeg's logging on; it is silenced otherwise.
//!
//! # ABI strategy
//!
//! FFmpeg's structures change between releases, so as little of them as
//! possible is mirrored, all of it in `abi.rs`:
//!
//! * `AVCodecContext` is never mirrored. It is configured through
//!   `AVOption`s: `threads`, `flags`, `ar`, and `ch_layout` (or `ac` before
//!   FFmpeg 5.1).
//! * Extradata goes through an `AVCodecParameters` of which only the first
//!   five members are written (`codec_type`, `codec_id`, `codec_tag`,
//!   `extradata`, `extradata_size`) before `avcodec_parameters_to_context`.
//!   `AVPacket` is written up to `flags` and `AVCodecDescriptor` read up to
//!   `id`. These prefixes are identical in libavcodec 59 to 63.
//! * `AVFrame`, which decoders fill, is read through one prefix struct per
//!   layout: `FrameV57` (libavutil 57 and 58, ending at the deprecated
//!   `channels`), `FrameV59` and `FrameV60` (ending at `ch_layout`). Each
//!   lists every member up to the last one read, deprecated ones included, so
//!   that the offsets come out right; the tests compare all of them with the
//!   output of `tools/ffmpeg_abi_offsets.c` compiled against the official public
//!   headers (libavutil/frame.h, channel_layout.h, libavcodec/packet.h,
//!   codec_par.h, codec_desc.h) of `n5.0.3`, `n5.1.10`, `n6.0.1`, `n6.1.6`,
//!   `n7.0.3`, `n7.1.5`, `n8.0.3`, `n8.1.3` and `n9.0.2`. Which members exist
//!   depends only on the major version (the `FF_API_*` macros), and every
//!   release of a major gave the same offsets. When a library is opened, a
//!   fresh `av_frame_alloc` frame must also read back the defaults of
//!   libavutil/frame.c (`get_frame_defaults`), which refuses a library whose
//!   layout is not the one chosen for its version.
//! * Enumeration values are not assumed. They move between releases:
//!   `AV_CODEC_ID_HEVC` is 173 up to 8.1 and 172 in 9.0, `AV_CODEC_ID_AV1` is
//!   226, 225 and 222, `AV_PIX_FMT_P010LE` is 159 and then 158. Codec ids,
//!   pixel formats and sample formats are therefore resolved by name when the
//!   library is opened (`avcodec_descriptor_get_by_name`, `av_get_pix_fmt`,
//!   `av_get_sample_fmt`). Constants that did not change in 5.0 to 9.0 are
//!   hard-coded: `AVERROR` tags, `AV_PKT_FLAG_KEY`, `AV_NOPTS_VALUE`, the
//!   input buffer padding (64) and the color range and "unspecified" codes.
//!
//! # Decoding
//!
//! Decoders are found by name (`h264`, `hevc`, `aac`, `libdav1d`, ...), never
//! with `avcodec_find_decoder`, which can hand back a hardware wrapper or
//! the built-in AV1 decoder that needs a hardware device. Video comes back as
//! [`PixelFormat`]s of 8, 10 and 12 bits (4:2:0, 4:2:2, 4:4:4, gray, NV12,
//! P010) and anything else is an [`Error::UnsupportedFormat`]; audio is
//! converted to interleaved `f32`.
//!
//! `send` copies the packet and carries its `pts` to the decoded frame. When
//! FFmpeg answers `EAGAIN` because output must be received first, the frames
//! are received into a bounded queue and the packet is sent again, so the
//! caller needs no `EAGAIN` handling; `receive` returns queued frames first.
//! Whether an undecodable packet shows up as an error from `send` or, with
//! frame threads, later from `receive` or `drain` is up to FFmpeg; decoding
//! carries on either way.
//!
//! ```ignore
//! use crate::media::decode::ffmpeg::{CodecId, Library, VideoConfig, VideoDecoder};
//!
//! let Some(lib) = Library::get().filter(|lib| lib.has_decoder(CodecId::H264)) else {
//!     return Ok(());
//! };
//! let mut decoder = VideoDecoder::new(&lib, CodecId::H264, VideoConfig::default())?;
//! # let (access_unit, pts_us) = (&[0u8; 4][..], 0);
//! decoder.send(access_unit, pts_us, true)?;
//! while let Some(picture) = decoder.receive()? {
//!     println!("{}x{} {:?}", picture.width, picture.height, picture.format);
//! }
//! for picture in decoder.drain().into_iter().flatten() {
//!     println!("{}x{}", picture.width, picture.height);
//! }
//! # Ok::<(), crate::media::decode::ffmpeg::Error>(())
//! ```
//!
//! Decoders and frames are `Send`: a decoder may move between threads and is
//! used by one at a time. Corrupt input produces an `Err` (see
//! [`Error::is_invalid_data`]) or no output, never undefined behavior, and
//! the decoder stays usable.

mod abi;
mod audio;
mod codec;
mod decoder;
mod error;
mod formats;
mod library;
mod video;

#[cfg(test)]
mod tests;

pub use audio::{AudioConfig, AudioDecoder, AudioFrame};
pub use codec::CodecId;
pub use error::{Attempt, Error, LoadError};
pub use library::{Library, Version};
pub use video::{PixelFormat, Plane, PlaneData, VideoConfig, VideoDecoder, VideoFrame};
