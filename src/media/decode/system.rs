//! The operating system's decoders on Linux: the user's FFmpeg, loaded at
//! run time by [`super::ffmpeg`] (Firefox's FFmpegRuntimeLinker model).
//!
//! This is where H.264, HEVC and AAC come from on Linux; FFmpeg also backs
//! VP8 (which TRust does not bundle) and serves as a fallback for the other
//! formats when a bundled decoder fails to start. MP4 and Matroska carry
//! H.264/HEVC as length-prefixed NAL units with an avcC/hvcC record and AAC
//! as raw frames with an AudioSpecificConfig; both are handed to FFmpeg as
//! extradata, exactly as its own demuxers would.
//!
//! Video decodes on the GPU's video engine when FFmpeg has a hardware device
//! that decodes the format (`ffmpeg::hardware`), as Firefox's FFmpeg decoder
//! does with VA-API; a stream the device fails before its first picture
//! continues in software from its first packet.

use std::sync::{Arc, Once};

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

/// The formats a hardware device may decode for TRust.
const HARDWARE_FORMATS: [CodecId; 4] = [CodecId::H264, CodecId::Hevc, CodecId::Vp9, CodecId::Av1];

/// Whether a stream with these details is one hardware decoders handle and
/// whose pictures come back in a format TRust presents: 4:2:0 at 8 or 10
/// bits (what the web serves; devices differ on 4:2:2 and 4:4:4, which
/// decode in software).
fn hardware_eligible(details: &crate::media::codec::CodecDetails) -> bool {
    details.bit_depth.is_none_or(|depth| depth <= 10)
        && details.chroma_shift.is_none_or(|shift| shift == (1, 1))
}

/// Learn in the background whether the hardware device decodes each
/// format, once video is first asked about: creating the device takes up
/// to a second, which should not delay the first video's first frame.
fn probe_hardware_in_background() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name(String::from("trust-hwdec-probe"))
            .spawn(|| {
                if let Some(library) = library() {
                    for codec in HARDWARE_FORMATS {
                        library.hardware_decodes(codec);
                    }
                }
            });
    });
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
        if id.is_video() {
            probe_hardware_in_background();
        }
        // FFmpeg's decoders cover every profile of these formats, but
        // TRust's presentation handles 8, 10 and 12 bits.
        codec.details.bit_depth.is_none_or(|depth| depth <= 12)
    }

    fn hardware(&self, codec: &CodecString) -> bool {
        let Some(id) = codec_id(codec.codec).filter(|id| HARDWARE_FORMATS.contains(id)) else {
            return false;
        };
        hardware_eligible(&codec.details)
            && self.supports(codec)
            && library().is_some_and(|library| library.hardware_decodes(id))
    }

    /// Software until the background probe has found what the device
    /// decodes (it starts with the first video query, in `supports`).
    fn hardware_now(&self, codec: &CodecString) -> bool {
        let Some(id) = codec_id(codec.codec).filter(|id| HARDWARE_FORMATS.contains(id)) else {
            return false;
        };
        hardware_eligible(&codec.details)
            && self.supports(codec)
            && library().is_some_and(|library| library.hardware_decodes_now(id) == Some(true))
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
/// Packets kept to replay into a software decoder until the hardware device
/// has decoded a stream's first picture (frame threads delay it by a few).
const MAX_REPLAYED_PACKETS: usize = 64;

struct SystemVideo {
    decoder: ffmpeg::VideoDecoder,
    library: Arc<Library>,
    id: CodecId,
    threads: u32,
    extradata: Option<Vec<u8>>,
    /// While the hardware device has yet to decode a picture: the packets
    /// sent since the stream (re)started at a keyframe, for a software
    /// decoder to take over from (`None` once it decoded one, or in
    /// software).
    replay: Option<Vec<Packet>>,
    /// Consecutive packets whose decoding reported damage without a frame.
    damaged: u32,
    container_color: Option<ColorInfo>,
    aspect: Option<(u32, u32, u32, u32)>,
    name: &'static str,
}

impl SystemVideo {
    fn new(library: &Arc<Library>, id: CodecId, track: &TrackInfo) -> Result<Self, Error> {
        let threads = u32::from(super::bundled::video_threads());
        let hardware = HARDWARE_FORMATS.contains(&id)
            && hardware_eligible(&track.details)
            && library.hardware_decodes(id);
        let config = ffmpeg::VideoConfig {
            threads,
            extradata: extradata(track),
            hardware,
        };
        let decoder = ffmpeg::VideoDecoder::new(library, id, config).map_err(map_error)?;
        let video = track.video.unwrap_or_default();
        Ok(Self {
            replay: decoder.hardware().then(Vec::new),
            decoder,
            library: Arc::clone(library),
            id,
            threads,
            extradata: extradata(track),
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
        let planes = frame_planes(layout, picture.planes);
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

impl SystemVideo {
    /// Decode `packet`; while the hardware device has not yet shown it can
    /// decode this stream, a failure moves the stream to software.
    fn decode_packet(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
        if let Some(replay) = &mut self.replay {
            if packet.keyframe {
                replay.clear();
            }
            if replay.len() < MAX_REPLAYED_PACKETS {
                replay.push(packet.clone());
            } else {
                self.replay = None;
            }
        }
        let result = self.send_and_collect(packet);
        if self.replay.is_none() {
            return result;
        }
        match result {
            Ok(frames) if !frames.is_empty() => {
                self.replay = None;
                Ok(frames)
            }
            Ok(frames) if self.damaged == 0 => Ok(frames),
            // The device failed the stream before its first picture.
            _ => self.continue_in_software(),
        }
    }

    /// Replace the hardware decoder with a software one and replay the
    /// stream from its keyframe.
    fn continue_in_software(&mut self) -> Result<Vec<VideoFrame>, Error> {
        let packets = self.replay.take().unwrap_or_default();
        let config = ffmpeg::VideoConfig {
            threads: self.threads,
            extradata: self.extradata.clone(),
            hardware: false,
        };
        self.decoder =
            ffmpeg::VideoDecoder::new(&self.library, self.id, config).map_err(map_error)?;
        self.damaged = 0;
        let mut frames = Vec::new();
        for packet in &packets {
            frames.extend(self.send_and_collect(packet)?);
        }
        Ok(frames)
    }

    fn send_and_collect(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
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
}

/// FFmpeg's planes as [`Plane`]s: an interleaved chroma plane's width
/// counts its Cb/Cr pairs, not FFmpeg's samples (`frame::Plane`).
fn frame_planes(layout: Layout, planes: Vec<ffmpeg::Plane>) -> Vec<Plane> {
    planes
        .into_iter()
        .enumerate()
        .map(|(index, plane)| Plane {
            data: match plane.data {
                ffmpeg::PlaneData::U8(data) => PlaneData::U8(data),
                ffmpeg::PlaneData::U16(data) => PlaneData::U16(data),
            },
            stride: plane.stride,
            width: if layout == Layout::SemiPlanar && index == 1 {
                plane.width / 2
            } else {
                plane.width
            },
            height: plane.height,
        })
        .collect()
}

impl VideoDecoder for SystemVideo {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
        self.decode_packet(packet)
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
        if let Some(replay) = &mut self.replay {
            replay.clear();
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn hardware(&self) -> bool {
        self.decoder.hardware_frames() > 0
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::track::ColorInfo;

    #[test]
    fn interleaved_chroma_planes_present() {
        // Hardware decoders hand back NV12 or P010: FFmpeg's interleaved
        // plane holds 2 * 3 samples a row for a 6 x 4 picture, which is 3
        // Cb/Cr pairs, and the frame must be one the presentation takes.
        for (wide, layout_depth) in [(false, 8), (true, 16)] {
            let samples = |count: usize, value: u16| {
                if wide {
                    ffmpeg::PlaneData::U16(vec![value << 8; count])
                } else {
                    ffmpeg::PlaneData::U8(vec![value as u8; count])
                }
            };
            let planes = vec![
                ffmpeg::Plane {
                    data: samples(6 * 4, 180),
                    stride: 6,
                    width: 6,
                    height: 4,
                },
                ffmpeg::Plane {
                    data: samples(6 * 2, 128),
                    stride: 6,
                    width: 6,
                    height: 2,
                },
            ];
            let frame = VideoFrame {
                pts: 0,
                duration: 0,
                width: 6,
                height: 4,
                display_width: 6,
                display_height: 4,
                bit_depth: layout_depth,
                layout: Layout::SemiPlanar,
                color: ColorInfo {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: false,
                },
                planes: frame_planes(Layout::SemiPlanar, planes),
            };
            assert_eq!(frame.planes[1].width, 3, "Cb/Cr pairs");
            assert!(frame.valid(), "wide {wide}");
            let (rgba, ..) = frame.to_rgba(1).expect("the frame presents");
            assert_eq!(&rgba[..3], &[truncate(180); 3], "wide {wide}");
        }
    }

    /// Narrow-range grey `luma` (Cb = Cr = 128) as 8-bit R'G'B'.
    fn truncate(luma: u16) -> u8 {
        ((f64::from(luma) - 16.0) * 255.0 / 219.0).round() as u8
    }
}
