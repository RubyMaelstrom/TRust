//! Decoders compiled into TRust: the royalty-free formats.
//!
//! - AV1 through rav1d (`crate::av1`, shared with AVIF images).
//! - MP3, FLAC, Vorbis and PCM through Symphonia.
//!
//! VP9 and Opus are added once their pure-Rust candidates pass TRust's
//! performance and conformance gate; until then this module does not claim
//! them.

use std::sync::Arc;

use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_FLAC, CODEC_ID_MP3, CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F64LE, CODEC_ID_PCM_S16LE,
    CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8, CODEC_ID_VORBIS,
};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};

use super::{AudioDecoder, DecoderModule, Error, VideoDecoder};
use crate::media::codec::{Codec, CodecString};
use crate::media::frame::{self, AudioBlock, Layout, Plane, VideoFrame};
use crate::media::track::{ColorInfo, Packet, TrackInfo};

pub struct Bundled;

impl DecoderModule for Bundled {
    fn name(&self) -> &'static str {
        "bundled"
    }

    fn supports(&self, codec: &CodecString) -> bool {
        match codec.codec {
            // rav1d decodes every AV1 profile at 8, 10 and 12 bits.
            Codec::Av1 => true,
            Codec::Mp3 | Codec::Flac | Codec::Vorbis | Codec::Pcm => true,
            _ => false,
        }
    }

    fn video(&self, track: &TrackInfo) -> Option<Result<Box<dyn VideoDecoder>, Error>> {
        match track.codec {
            Codec::Av1 => Some(Av1::new(track).map(|d| Box::new(d) as Box<dyn VideoDecoder>)),
            _ => None,
        }
    }

    fn audio(&self, track: &TrackInfo) -> Option<Result<Box<dyn AudioDecoder>, Error>> {
        match track.codec {
            Codec::Mp3 | Codec::Flac | Codec::Vorbis | Codec::Pcm => {
                Some(SymphoniaAudio::new(track).map(|d| Box::new(d) as Box<dyn AudioDecoder>))
            }
            _ => None,
        }
    }
}

/// Worker threads for video decoding: enough for 1080p60 on current CPUs
/// without monopolising small ones.
pub(crate) fn video_threads() -> u16 {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4) as u16
}

struct Av1 {
    decoder: crate::av1::Decoder,
    container_color: Option<ColorInfo>,
    /// Display size over coded size, from the container.
    aspect: Option<(u32, u32, u32, u32)>,
}

impl Av1 {
    fn new(track: &TrackInfo) -> Result<Self, Error> {
        let threads = video_threads();
        let config = crate::av1::DecoderConfig {
            threads,
            // dav1d's automatic frame delay for this many threads.
            max_frame_delay: (f64::from(threads).sqrt().ceil() as u16).clamp(1, 8),
            // 8K is the largest picture web video uses.
            frame_size_limit: 8192 * 4352,
            ..Default::default()
        };
        let decoder =
            crate::av1::Decoder::new(&config).map_err(|e| Error::Failed(e.to_string()))?;
        let video = track.video.unwrap_or_default();
        Ok(Self {
            decoder,
            container_color: video.color,
            aspect: (video.display_width != video.width || video.display_height != video.height)
                .then_some((
                    video.width,
                    video.height,
                    video.display_width,
                    video.display_height,
                ))
                .filter(|&(w, h, dw, dh)| w > 0 && h > 0 && dw > 0 && dh > 0),
        })
    }

    fn convert(&self, frame: crate::av1::Frame, duration: i64) -> Option<VideoFrame> {
        let pts = frame.timestamp?;
        let to_plane = |plane: crate::av1::Plane| match plane.samples {
            crate::av1::Samples::Eight(data) => {
                Plane::u8(data, plane.width, plane.width, plane.height)
            }
            crate::av1::Samples::High(data) => {
                Plane::u16(data, plane.width, plane.width, plane.height)
            }
        };
        let layout = match frame.layout {
            crate::av1::PixelLayout::Monochrome => Layout::Monochrome,
            crate::av1::PixelLayout::Yuv420 => Layout::Planar {
                chroma_shift: (1, 1),
            },
            crate::av1::PixelLayout::Yuv422 => Layout::Planar {
                chroma_shift: (1, 0),
            },
            crate::av1::PixelLayout::Yuv444 => Layout::Planar {
                chroma_shift: (0, 0),
            },
        };
        // The sequence header's colour description wins unless it leaves
        // the matrix unspecified (2).
        let stream = ColorInfo {
            primaries: frame.color.color_primaries,
            transfer: frame.color.transfer_characteristics,
            matrix: frame.color.matrix_coefficients,
            full_range: frame.color.full_range,
        };
        let color = if stream.matrix == 2 {
            frame::resolve_color(
                self.container_color.or(Some(stream)),
                frame.width,
                frame.height,
            )
        } else {
            frame::resolve_color(Some(stream), frame.width, frame.height)
        };
        let (display_width, display_height) = display_size(self.aspect, frame.width, frame.height);
        let mut planes = vec![to_plane(frame.y)];
        if let Some([u, v]) = frame.chroma {
            planes.push(to_plane(u));
            planes.push(to_plane(v));
        }
        Some(VideoFrame {
            pts,
            duration,
            width: frame.width,
            height: frame.height,
            display_width,
            display_height,
            bit_depth: frame.bit_depth,
            layout,
            color,
            planes,
        })
    }
}

/// Apply a container's display aspect ratio to a decoded picture size.
pub(crate) fn display_size(
    aspect: Option<(u32, u32, u32, u32)>,
    width: u32,
    height: u32,
) -> (u32, u32) {
    match aspect {
        Some((cw, ch, dw, dh)) => {
            // Keep the decoded height; scale the width by the display
            // aspect ratio relative to the coded one.
            let ratio = (f64::from(dw) / f64::from(dh)) / (f64::from(cw) / f64::from(ch));
            ((f64::from(width) * ratio).round().max(1.0) as u32, height)
        }
        None => (width, height),
    }
}

impl VideoDecoder for Av1 {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
        let frames = self
            .decoder
            .decode_at(&packet.data, Some(packet.pts))
            .map_err(|e| Error::Failed(e.to_string()))?;
        Ok(frames
            .into_iter()
            .filter_map(|frame| self.convert(frame, packet.duration))
            .collect())
    }

    fn drain(&mut self) -> Result<Vec<VideoFrame>, Error> {
        let frames = self
            .decoder
            .drain()
            .map_err(|e| Error::Failed(e.to_string()))?;
        Ok(frames
            .into_iter()
            .filter_map(|frame| self.convert(frame, 0))
            .collect())
    }

    fn flush(&mut self) {
        self.decoder.flush();
    }

    fn name(&self) -> &'static str {
        "rav1d"
    }
}

/// Symphonia's MP3, FLAC, Vorbis and PCM decoders.
struct SymphoniaAudio {
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    track_id: u32,
}

impl SymphoniaAudio {
    fn new(track: &TrackInfo) -> Result<Self, Error> {
        let params = match track
            .native
            .as_ref()
            .and_then(|native| native.0.downcast_ref::<AudioCodecParameters>())
        {
            Some(params) => params.clone(),
            None => params_for(track)?,
        };
        let registry = symphonia::default::get_codecs();
        let Some(registered) = registry.get_audio_decoder(params.codec) else {
            return Err(Error::Unsupported(format!("{} audio", track.codec.name())));
        };
        let options = AudioDecoderOptions::default();
        let decoder = (registered.factory)(&params, &options)
            .map_err(|error| Error::Failed(format!("{}: {error}", track.codec.name())))?;
        Ok(Self {
            decoder,
            track_id: track.id,
        })
    }
}

/// Symphonia codec parameters for a track from TRust's own containers.
fn params_for(track: &TrackInfo) -> Result<AudioCodecParameters, Error> {
    let audio = track.audio.unwrap_or_default();
    let mut params = AudioCodecParameters::new();
    let codec = match track.codec {
        Codec::Mp3 => CODEC_ID_MP3,
        Codec::Vorbis => CODEC_ID_VORBIS,
        Codec::Flac => CODEC_ID_FLAC,
        // Matroska A_PCM/INT/LIT and A_PCM/FLOAT/IEEE.
        Codec::Pcm => match audio.bits_per_sample {
            8 => CODEC_ID_PCM_U8,
            16 => CODEC_ID_PCM_S16LE,
            24 => CODEC_ID_PCM_S24LE,
            32 if track.codec_string.contains("FLOAT") => CODEC_ID_PCM_F32LE,
            32 => CODEC_ID_PCM_S32LE,
            64 => CODEC_ID_PCM_F64LE,
            _ => return Err(Error::Unsupported(String::from("PCM sample format"))),
        },
        _ => return Err(Error::Unsupported(track.codec.name().to_string())),
    };
    params.for_codec(codec);
    if audio.sample_rate > 0 {
        params.with_sample_rate(audio.sample_rate);
    }
    if audio.channels > 0 {
        params.with_channels(Channels::Discrete(audio.channels));
    }
    if track.codec == Codec::Pcm && audio.bits_per_sample > 0 {
        params.with_bits_per_sample(u32::from(audio.bits_per_sample));
        params.with_bits_per_coded_sample(u32::from(audio.bits_per_sample));
    }
    match track.codec {
        Codec::Flac => {
            // Symphonia wants the STREAMINFO block body; containers give the
            // "fLaC" marker and metadata block headers too.
            let config = &track.config[..];
            let blocks = config.strip_prefix(b"fLaC").unwrap_or(config);
            if blocks.len() >= 4 + 34 && blocks[0] & 0x7F == 0 {
                params.with_extra_data(Box::from(&blocks[4..4 + 34]));
            } else if blocks.len() == 34 {
                params.with_extra_data(Box::from(blocks));
            } else {
                return Err(Error::Corrupt(String::from("FLAC STREAMINFO")));
            }
        }
        Codec::Vorbis => {
            if track.config.is_empty() {
                return Err(Error::Corrupt(String::from("Vorbis headers")));
            }
            params.with_extra_data(Box::from(&track.config[..]));
        }
        _ => {}
    }
    Ok(params)
}

impl AudioDecoder for SymphoniaAudio {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<AudioBlock>, Error> {
        let input = symphonia::core::packet::Packet::new(
            self.track_id,
            symphonia::core::units::Timestamp::new(0),
            symphonia::core::units::Duration::new(0),
            Box::<[u8]>::from(&packet.data[..]),
        );
        let decoded = match self.decoder.decode(&input) {
            Ok(decoded) => decoded,
            // A corrupt packet is dropped; decoding continues.
            Err(symphonia::core::errors::Error::DecodeError(what)) => {
                return Err(Error::Corrupt(what.to_string()));
            }
            Err(error) => return Err(Error::Failed(error.to_string())),
        };
        let rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count() as u16;
        if decoded.frames() == 0 || channels == 0 {
            return Ok(Vec::new());
        }
        let mut samples: Vec<f32> = Vec::with_capacity(decoded.frames() * usize::from(channels));
        decoded.copy_to_vec_interleaved(&mut samples);
        Ok(vec![AudioBlock {
            pts: packet.pts,
            rate,
            channels,
            samples: Arc::from(samples),
        }])
    }

    fn flush(&mut self) {
        self.decoder.reset();
    }

    fn name(&self) -> &'static str {
        "symphonia"
    }
}
