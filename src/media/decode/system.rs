//! The operating system's decoders on Linux: the user's FFmpeg, loaded at
//! run time by [`super::ffmpeg`] (Firefox's FFmpegRuntimeLinker model).
//!
//! This is where H.264, HEVC and AAC come from on Linux; FFmpeg also backs
//! VP8 (which TRust does not bundle) and serves as a fallback for the other
//! formats when a bundled decoder fails to start. MP4 and Matroska carry
//! H.264/HEVC as length-prefixed NAL units with an avcC/hvcC record and AAC
//! as raw frames with an AudioSpecificConfig; both are handed to FFmpeg as
//! extradata, exactly as its own demuxers would.

use std::sync::Arc;

use super::ffmpeg::{self, CodecId, Library};
use super::{AudioDecoder, DecoderModule, Error, VideoDecoder};
use crate::media::codec::{Codec, CodecString};
use crate::media::frame::{self, AudioBlock, Layout, Plane, PlaneData, VideoFrame};
use crate::media::track::{ColorInfo, Packet, TrackInfo};

pub struct SystemFfmpeg;

fn codec_id(codec: Codec) -> Option<CodecId> {
    Some(match codec {
        Codec::H264 => CodecId::H264,
        Codec::Hevc => CodecId::Hevc,
        Codec::Aac => CodecId::Aac,
        Codec::Ac3 => CodecId::Ac3,
        Codec::Eac3 => CodecId::Eac3,
        Codec::Vp8 => CodecId::Vp8,
        Codec::Vp9 => CodecId::Vp9,
        Codec::Av1 => CodecId::Av1,
        Codec::Mp3 => CodecId::Mp3,
        Codec::Opus => CodecId::Opus,
        Codec::Vorbis => CodecId::Vorbis,
        Codec::Flac => CodecId::Flac,
        _ => return None,
    })
}

fn library() -> Option<Arc<Library>> {
    Library::get()
}

impl DecoderModule for SystemFfmpeg {
    fn name(&self) -> &'static str {
        "system-ffmpeg"
    }

    fn supports(&self, codec: &CodecString) -> bool {
        let Some(id) = codec_id(codec.codec) else {
            return false;
        };
        let Some(library) = library() else {
            return false;
        };
        if !library.has_decoder(id) {
            return false;
        }
        // FFmpeg's decoders cover every profile of these formats, but
        // TRust's presentation handles 8, 10 and 12 bits.
        codec.details.bit_depth.is_none_or(|depth| depth <= 12)
    }

    fn video(&self, track: &TrackInfo) -> Option<Result<Box<dyn VideoDecoder>, Error>> {
        let id = codec_id(track.codec).filter(|id| id.is_video())?;
        let library = library()?;
        Some(SystemVideo::new(&library, id, track).map(|d| Box::new(d) as Box<dyn VideoDecoder>))
    }

    fn audio(&self, track: &TrackInfo) -> Option<Result<Box<dyn AudioDecoder>, Error>> {
        let id = codec_id(track.codec).filter(|id| !id.is_video())?;
        let library = library()?;
        Some(SystemAudio::new(&library, id, track).map(|d| Box::new(d) as Box<dyn AudioDecoder>))
    }
}

fn map_error(error: ffmpeg::Error) -> Error {
    if error.is_invalid_data() {
        Error::Corrupt(error.to_string())
    } else {
        Error::Failed(error.to_string())
    }
}

/// Extradata for a track: the codec configuration record or header, when
/// the format has one.
fn extradata(track: &TrackInfo) -> Option<Vec<u8>> {
    (!track.config.is_empty()).then(|| track.config.to_vec())
}

/// Packets in a row that may fail to decode, producing no frame, before the
/// stream counts as undecodable.
const MAX_DAMAGED_PACKETS: u32 = 48;

struct SystemVideo {
    decoder: ffmpeg::VideoDecoder,
    /// Consecutive packets whose decoding reported damage without a frame.
    damaged: u32,
    container_color: Option<ColorInfo>,
    aspect: Option<(u32, u32, u32, u32)>,
    name: &'static str,
}

impl SystemVideo {
    fn new(library: &Arc<Library>, id: CodecId, track: &TrackInfo) -> Result<Self, Error> {
        let config = ffmpeg::VideoConfig {
            threads: u32::from(super::bundled::video_threads()),
            extradata: extradata(track),
        };
        let decoder = ffmpeg::VideoDecoder::new(library, id, config).map_err(map_error)?;
        let video = track.video.unwrap_or_default();
        Ok(Self {
            decoder,
            damaged: 0,
            container_color: video.color,
            aspect: (video.display_width != video.width || video.display_height != video.height)
                .then_some((
                    video.width,
                    video.height,
                    video.display_width,
                    video.display_height,
                ))
                .filter(|&(w, h, dw, dh)| w > 0 && h > 0 && dw > 0 && dh > 0),
            name: match id {
                CodecId::H264 => "ffmpeg-h264",
                CodecId::Hevc => "ffmpeg-hevc",
                CodecId::Vp8 => "ffmpeg-vp8",
                CodecId::Vp9 => "ffmpeg-vp9",
                _ => "ffmpeg-video",
            },
        })
    }

    fn convert(&self, picture: ffmpeg::VideoFrame, duration: i64) -> Option<VideoFrame> {
        use ffmpeg::PixelFormat::*;
        let (layout, bit_depth) = match picture.format {
            Yuv420p => (
                Layout::Planar {
                    chroma_shift: (1, 1),
                },
                8,
            ),
            Yuv422p => (
                Layout::Planar {
                    chroma_shift: (1, 0),
                },
                8,
            ),
            Yuv444p => (
                Layout::Planar {
                    chroma_shift: (0, 0),
                },
                8,
            ),
            Gray8 => (Layout::Monochrome, 8),
            Nv12 => (Layout::SemiPlanar, 8),
            Yuv420p10 => (
                Layout::Planar {
                    chroma_shift: (1, 1),
                },
                10,
            ),
            Yuv422p10 => (
                Layout::Planar {
                    chroma_shift: (1, 0),
                },
                10,
            ),
            Yuv444p10 => (
                Layout::Planar {
                    chroma_shift: (0, 0),
                },
                10,
            ),
            Yuv420p12 => (
                Layout::Planar {
                    chroma_shift: (1, 1),
                },
                12,
            ),
            P010 => (Layout::SemiPlanar, 16),
        };
        if picture.pts == i64::MIN {
            return None;
        }
        let planes = picture
            .planes
            .into_iter()
            .map(|plane| Plane {
                data: match plane.data {
                    ffmpeg::PlaneData::U8(data) => PlaneData::U8(data),
                    ffmpeg::PlaneData::U16(data) => PlaneData::U16(data),
                },
                stride: plane.stride,
                width: plane.width,
                height: plane.height,
            })
            .collect();
        let stream = ColorInfo {
            primaries: picture.primaries,
            transfer: picture.transfer,
            matrix: picture.matrix,
            full_range: picture.color_range_full,
        };
        let color = if stream.matrix == 2 {
            let mut color = frame::resolve_color(
                self.container_color.or(Some(stream)),
                picture.width,
                picture.height,
            );
            color.full_range |= picture.color_range_full;
            color
        } else {
            frame::resolve_color(Some(stream), picture.width, picture.height)
        };
        // The stream's sample aspect ratio, else the container's.
        let (display_width, display_height) = if picture.sample_aspect.0 != picture.sample_aspect.1
            && picture.sample_aspect.0 > 0
            && picture.sample_aspect.1 > 0
        {
            let width = u64::from(picture.width) * u64::from(picture.sample_aspect.0)
                / u64::from(picture.sample_aspect.1);
            (width.clamp(1, u64::from(u32::MAX)) as u32, picture.height)
        } else {
            super::bundled::display_size(self.aspect, picture.width, picture.height)
        };
        Some(VideoFrame {
            pts: picture.pts,
            duration,
            width: picture.width,
            height: picture.height,
            display_width,
            display_height,
            bit_depth,
            layout,
            color,
            planes,
        })
    }

    fn collect(&mut self, duration: i64) -> Result<Vec<VideoFrame>, Error> {
        let mut frames = Vec::new();
        loop {
            match self.decoder.receive() {
                Ok(Some(picture)) => frames.extend(self.convert(picture, duration)),
                Ok(None) => return Ok(frames),
                // A late error for a damaged packet: keep what decoded.
                Err(error) if error.is_decode_error() => {
                    self.damaged += 1;
                    if self.damaged > MAX_DAMAGED_PACKETS {
                        return Err(map_error(error));
                    }
                }
                Err(error) => return Err(map_error(error)),
            }
        }
    }
}

impl VideoDecoder for SystemVideo {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
        match self.decoder.send(&packet.data, packet.pts, packet.keyframe) {
            Ok(()) => {}
            // FFmpeg took the packet and conceals the damage (as its own
            // command line tool continues); only a run of failures without
            // any frame ends the stream.
            Err(error) if error.is_decode_error() => {
                self.damaged += 1;
                if self.damaged > MAX_DAMAGED_PACKETS {
                    return Err(map_error(error));
                }
            }
            Err(error) => return Err(map_error(error)),
        }
        let frames = self.collect(packet.duration)?;
        if !frames.is_empty() {
            self.damaged = 0;
        }
        Ok(frames)
    }

    fn drain(&mut self) -> Result<Vec<VideoFrame>, Error> {
        let pictures = self.decoder.drain();
        Ok(pictures
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|picture| self.convert(picture, 0))
            .collect())
    }

    fn flush(&mut self) {
        self.decoder.flush();
        self.damaged = 0;
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct SystemAudio {
    decoder: ffmpeg::AudioDecoder,
}

impl SystemAudio {
    fn new(library: &Arc<Library>, id: CodecId, track: &TrackInfo) -> Result<Self, Error> {
        let audio = track.audio.unwrap_or_default();
        let mut extradata = extradata(track);
        if id == CodecId::Flac
            && let Some(data) = extradata.as_mut()
        {
            // FFmpeg's FLAC decoder takes STREAMINFO alone (or with the
            // "fLaC" marker and block header, which it also accepts).
            if data.starts_with(b"fLaC") && data.len() >= 4 + 4 + 34 {
                *data = data[8..8 + 34].to_vec();
            }
        }
        let config = ffmpeg::AudioConfig {
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            extradata,
        };
        let decoder = ffmpeg::AudioDecoder::new(library, id, config).map_err(map_error)?;
        Ok(Self { decoder })
    }
}

impl AudioDecoder for SystemAudio {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<AudioBlock>, Error> {
        if let Err(error) = self.decoder.send(&packet.data, packet.pts, true) {
            if !error.is_invalid_data() {
                return Err(map_error(error));
            }
            return Err(Error::Corrupt(error.to_string()));
        }
        let mut blocks = Vec::new();
        loop {
            match self.decoder.receive() {
                Ok(Some(frame)) => {
                    if frame.channels == 0 || frame.samples.is_empty() {
                        continue;
                    }
                    blocks.push(AudioBlock {
                        pts: if frame.pts == i64::MIN {
                            packet.pts
                        } else {
                            frame.pts
                        },
                        rate: frame.sample_rate,
                        channels: frame.channels,
                        samples: Arc::from(frame.samples),
                    });
                }
                Ok(None) => return Ok(blocks),
                Err(error) if error.is_invalid_data() => continue,
                Err(error) => return Err(map_error(error)),
            }
        }
    }

    fn drain(&mut self) -> Result<Vec<AudioBlock>, Error> {
        Ok(self
            .decoder
            .drain()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|frame| frame.channels > 0 && !frame.samples.is_empty())
            .map(|frame| AudioBlock {
                pts: frame.pts,
                rate: frame.sample_rate,
                channels: frame.channels,
                samples: Arc::from(frame.samples),
            })
            .collect())
    }

    fn flush(&mut self) {
        self.decoder.flush();
    }

    fn name(&self) -> &'static str {
        "ffmpeg-audio"
    }
}
