//! Audio decoding.

use std::ffi::CString;
use std::sync::Arc;

use super::abi::FrameFields;
use super::codec::CodecId;
use super::decoder::{Core, MediaType, Pipeline};
use super::error::Error;
use super::formats::SampleFormat;
use super::library::Library;

/// Decoded frames waiting for `receive()` before `send()` reports
/// [`Error::QueueFull`]. Audio frames are small.
const MAX_QUEUED_FRAMES: usize = 256;
/// Plausibility limits for a decoded frame (FFmpeg's own channel layouts
/// stop at 64 channels; FLAC blocks hold at most 65535 samples per channel).
const MAX_CHANNELS: usize = 64;
const MAX_SAMPLES_PER_CHANNEL: usize = 1 << 17;

/// Settings for [`AudioDecoder::new`].
#[derive(Clone, Debug, Default)]
pub struct AudioConfig {
    /// Sample rate in Hz, or 0 when the stream states it (ADTS, AC-3, MP3).
    pub sample_rate: u32,
    /// Channel count, or 0 when the stream states it.
    pub channels: u16,
    /// Codec setup data: the AudioSpecificConfig of raw AAC, `OpusHead`, the
    /// Vorbis headers in Xiph lacing, FLAC's STREAMINFO. `None` for framed
    /// streams that carry it in-band (ADTS AAC, AC-3, E-AC-3, MP3).
    pub extradata: Option<Vec<u8>>,
}

/// A block of decoded audio.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    /// The `pts` given to `send()` with the packet this audio came from
    /// (`i64::MIN` if FFmpeg could not tell).
    pub pts: i64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples, `channels` per sample time, in -1.0..=1.0 for
    /// integer sources. Channels are in FFmpeg's native order (front left,
    /// front right, front centre, LFE, back left, back right, ...).
    pub samples: Vec<f32>,
}

/// An audio decoder over the system FFmpeg; used like
/// [`VideoDecoder`](super::VideoDecoder).
pub struct AudioDecoder {
    pipeline: Pipeline<AudioFrame>,
}

impl AudioDecoder {
    /// Opens a decoder for `codec` (AAC, AC-3, E-AC-3, MP3, Opus, Vorbis or
    /// FLAC).
    pub fn new(lib: &Arc<Library>, codec: CodecId, config: AudioConfig) -> Result<Self, Error> {
        if codec.is_video() {
            return Err(Error::WrongMediaType(codec));
        }
        if usize::from(config.channels) > MAX_CHANNELS {
            return Err(Error::InvalidInput("too many channels"));
        }
        let mut core = Core::new(lib, codec, MediaType::Audio, config.extradata.as_deref())?;
        // After the extradata: applying it resets these.
        if config.sample_rate > 0 {
            core.set_int(c"ar", i64::from(config.sample_rate))?;
        }
        if config.channels > 0 {
            set_channels(&mut core, config.channels)?;
        }
        core.open()?;
        Ok(AudioDecoder {
            pipeline: Pipeline::new(core, MAX_QUEUED_FRAMES, convert),
        })
    }

    /// Sends one packet: an ADTS, AC-3, E-AC-3 or MP3 frame, or a codec
    /// packet. See [`VideoDecoder::send`](super::VideoDecoder::send).
    pub fn send(&mut self, data: &[u8], pts: i64, keyframe: bool) -> Result<(), Error> {
        self.pipeline.send(data, pts, keyframe)
    }

    /// The next block of audio, or `None` when the decoder needs more input.
    /// An `Err` stands for a block that could not be decoded.
    pub fn receive(&mut self) -> Result<Option<AudioFrame>, Error> {
        self.pipeline.receive()
    }

    /// Ends the stream and returns everything still buffered, in order, each
    /// failure in the place of its audio (see
    /// [`VideoDecoder::drain`](super::VideoDecoder::drain)). The decoder is
    /// then reset for a new stream.
    pub fn drain(&mut self) -> Vec<Result<AudioFrame, Error>> {
        self.pipeline.drain()
    }

    /// Discards buffered audio and decoder state, for seeking.
    pub fn flush(&mut self) {
        self.pipeline.flush();
    }
}

/// Tells the decoder how many channels to expect: through `ch_layout`
/// (libavcodec 59.37, FFmpeg 5.1, and later), else the older `ac` option
/// (FFmpeg 5.0), which later releases no longer have.
fn set_channels(core: &mut Core, channels: u16) -> Result<(), Error> {
    let count =
        CString::new(format!("{channels}c")).map_err(|_| Error::InvalidInput("channels"))?;
    core.set_str(c"ch_layout", &count)
        .or_else(|_| core.set_int(c"ac", i64::from(channels)))
}

/// Copies a decoded `AVFrame` into interleaved `f32` samples.
pub(super) fn convert(lib: &Library, frame: &FrameFields) -> Result<AudioFrame, Error> {
    let (format, planar) =
        lib.formats
            .sample(frame.format)
            .ok_or_else(|| Error::UnsupportedFormat {
                raw: frame.format,
                name: lib.sample_format_name(frame.format),
            })?;
    let channels = match usize::try_from(frame.channels) {
        Ok(channels @ 1..=MAX_CHANNELS) => channels,
        _ => return Err(Error::InvalidFrame("implausible channel count")),
    };
    let per_channel = match usize::try_from(frame.nb_samples) {
        Ok(samples @ 1..=MAX_SAMPLES_PER_CHANNEL) => samples,
        _ => return Err(Error::InvalidFrame("implausible sample count")),
    };
    let sample_rate = match u32::try_from(frame.sample_rate) {
        Ok(rate @ 1..) => rate,
        _ => return Err(Error::InvalidFrame("missing sample rate")),
    };

    let mut samples: Vec<f32> = Vec::new();
    samples
        .try_reserve_exact(channels * per_channel)
        .map_err(|_| Error::OutOfMemory)?;
    samples.resize(channels * per_channel, 0.0);
    let to_f32 = sample_reader(format);
    if planar {
        if frame.extended_data.is_null() {
            return Err(Error::InvalidFrame("missing audio planes"));
        }
        for channel in 0..channels {
            // SAFETY: for planar audio `extended_data` holds one pointer per
            // channel, each addressing `nb_samples` samples of the format.
            let bytes = unsafe {
                let plane = *frame.extended_data.add(channel);
                if plane.is_null() {
                    return Err(Error::InvalidFrame("missing audio plane"));
                }
                std::slice::from_raw_parts(plane, per_channel * format.bytes())
            };
            for (index, sample) in bytes.chunks_exact(format.bytes()).enumerate() {
                samples[index * channels + channel] = to_f32(sample);
            }
        }
    } else {
        let plane = frame.data[0];
        if plane.is_null() {
            return Err(Error::InvalidFrame("missing audio plane"));
        }
        // SAFETY: packed audio keeps all channels interleaved in `data[0]`.
        let bytes =
            unsafe { std::slice::from_raw_parts(plane, channels * per_channel * format.bytes()) };
        for (slot, sample) in samples.iter_mut().zip(bytes.chunks_exact(format.bytes())) {
            *slot = to_f32(sample);
        }
    }

    Ok(AudioFrame {
        pts: frame.pts,
        sample_rate,
        channels: channels as u16,
        samples,
    })
}

/// Converts one sample's bytes (native byte order) to `f32`.
fn sample_reader(format: SampleFormat) -> fn(&[u8]) -> f32 {
    match format {
        SampleFormat::U8 => |s| (f32::from(s[0]) - 128.0) / 128.0,
        SampleFormat::S16 => |s| f32::from(i16::from_ne_bytes([s[0], s[1]])) / 32768.0,
        SampleFormat::S32 => {
            |s| (i32::from_ne_bytes([s[0], s[1], s[2], s[3]]) as f64 / 2147483648.0) as f32
        }
        SampleFormat::F32 => |s| f32::from_ne_bytes([s[0], s[1], s[2], s[3]]),
        SampleFormat::F64 => {
            |s| f64::from_ne_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32
        }
    }
}
