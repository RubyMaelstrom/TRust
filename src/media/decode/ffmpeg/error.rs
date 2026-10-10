//! Error types of the FFmpeg bindings.

use std::fmt;

use super::codec::CodecId;

/// Why the system FFmpeg libraries could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// `TRUST_FFMPEG=0` switched FFmpeg off.
    Disabled,
    /// No candidate library was usable; one entry per candidate tried, in the
    /// order tried.
    Unavailable(Vec<Attempt>),
}

/// One rejected candidate `libavcodec`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The name or path handed to `dlopen`.
    pub path: String,
    /// The loader's message, or the reason the library was not acceptable.
    pub reason: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Disabled => f.write_str("FFmpeg is disabled by TRUST_FFMPEG=0"),
            LoadError::Unavailable(attempts) => {
                f.write_str("no usable FFmpeg library")?;
                for (i, attempt) in attempts.iter().enumerate() {
                    f.write_str(if i == 0 { ": " } else { "; " })?;
                    write!(f, "{} ({})", attempt.path, attempt.reason)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// A decoder could not be created, or refused data.
#[derive(Debug)]
pub enum Error {
    /// The loaded FFmpeg has no usable decoder for the codec (for example a
    /// build without H.264 or AAC).
    NoDecoder(CodecId),
    /// A video decoder was requested for an audio codec or the reverse.
    WrongMediaType(CodecId),
    /// The caller's data cannot be handed to FFmpeg (empty or oversized).
    InvalidInput(&'static str),
    /// An FFmpeg call failed. `code` is the negative `AVERROR` value;
    /// `AVERROR_INVALIDDATA` (see [`Error::is_invalid_data`]) is the usual
    /// answer to corrupt input and leaves the decoder usable.
    Ffmpeg {
        call: &'static str,
        code: i32,
        message: String,
    },
    /// The decoder produced a frame in a pixel or sample format that is not
    /// converted. `raw` is the release-specific enum value.
    UnsupportedFormat { raw: i32, name: String },
    /// A decoded frame failed validation (implausible size, missing plane,
    /// short line); the frame is dropped.
    InvalidFrame(&'static str),
    /// Too many decoded frames are waiting; call `receive()` before sending
    /// more input. The rejected packet was not consumed.
    QueueFull,
    /// An allocation failed.
    OutOfMemory,
}

impl Error {
    /// The `AVERROR` code, for errors that came from FFmpeg.
    pub fn code(&self) -> Option<i32> {
        match self {
            Error::Ffmpeg { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Whether FFmpeg rejected the data as undecodable (`AVERROR_INVALIDDATA`).
    /// The decoder stays usable: later packets, typically from the next
    /// keyframe, decode normally.
    pub fn is_invalid_data(&self) -> bool {
        self.code() == Some(super::abi::AVERROR_INVALIDDATA)
    }

    /// Whether FFmpeg reported damaged data while decoding: any failure
    /// but the decoder's state (`EAGAIN`, `AVERROR_EOF`), memory (`ENOMEM`)
    /// or misuse (`EINVAL`). Decoders signal damage with
    /// `AVERROR_INVALIDDATA` or older generic codes (H.264's frame threads
    /// report a damaged macroblock of an earlier frame as -1); the packet
    /// was taken and decoding continues with error concealment.
    pub fn is_decode_error(&self) -> bool {
        const ENOMEM: i32 = -12;
        const EINVAL: i32 = -22;
        self.code().is_some_and(|code| {
            code < 0
                && !matches!(
                    code,
                    super::abi::AVERROR_EAGAIN | super::abi::AVERROR_EOF | ENOMEM | EINVAL
                )
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDecoder(codec) => write!(f, "FFmpeg has no {codec:?} decoder"),
            Error::WrongMediaType(codec) => {
                write!(f, "{codec:?} is not of the media type of this decoder")
            }
            Error::InvalidInput(why) => write!(f, "invalid input: {why}"),
            Error::Ffmpeg {
                call,
                code,
                message,
            } => write!(f, "{call} failed: {message} ({code})"),
            Error::UnsupportedFormat { raw, name } => {
                write!(f, "unsupported frame format {name} ({raw})")
            }
            Error::InvalidFrame(why) => write!(f, "invalid decoded frame: {why}"),
            Error::QueueFull => f.write_str("decoded frames are waiting to be received"),
            Error::OutOfMemory => f.write_str("out of memory"),
        }
    }
}

impl std::error::Error for Error {}
