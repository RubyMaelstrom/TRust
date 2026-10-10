//! Ogg, MP3, FLAC and WAV resources through Symphonia's container readers.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_FLAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_PCM_ALAW,
    CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F64LE, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16LE,
    CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8, CODEC_ID_VORBIS,
};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::units::{Time, TimeBase, Timestamp};

use super::{ByteSource, Demuxer, Error};
use crate::media::codec::{self, Codec};
use crate::media::container::Container;
use crate::media::track::{AudioInfo, NativeParams, Packet, TrackInfo, TrackKind};
use crate::media::{MICROS_PER_SECOND, Micros};

/// A [`ByteSource`] as the `Read + Seek` stream Symphonia reads.
struct Stream {
    source: Mutex<Box<dyn ByteSource>>,
    position: u64,
    len: Option<u64>,
    seekable: bool,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("media source poisoned"))?;
        let read = source.read_at(self.position, buf)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for Stream {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self
                .len
                .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "length unknown"))?
                .checked_add_signed(delta),
        };
        let target = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        self.position = target;
        Ok(target)
    }
}

impl MediaSource for Stream {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

pub(super) struct AudioFileDemuxer {
    reader: Box<dyn FormatReader + 'static>,
    tracks: Vec<TrackInfo>,
    time_bases: Vec<Option<TimeBase>>,
    duration: Option<Micros>,
    seekable: bool,
}

fn to_micros(ts: Timestamp, base: Option<TimeBase>) -> Micros {
    match base {
        Some(base) => {
            let value =
                i128::from(ts.get()) * i128::from(base.numer.get()) * i128::from(MICROS_PER_SECOND);
            (value / i128::from(base.denom.get())).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                as i64
        }
        None => ts.get(),
    }
}

impl AudioFileDemuxer {
    pub(super) fn open(source: Box<dyn ByteSource>, container: Container) -> Result<Self, Error> {
        let len = source.len();
        let seekable = source.seekable();
        let stream = Stream {
            source: Mutex::new(source),
            position: 0,
            len,
            seekable,
        };
        let mss = MediaSourceStream::new(Box::new(stream), MediaSourceStreamOptions::default());
        let options = FormatOptions::default();
        let reader: Box<dyn FormatReader> = match container {
            Container::Ogg => Box::new(
                symphonia::default::formats::OggReader::try_new(mss, options)
                    .map_err(|e| Error::Malformed(e.to_string()))?,
            ),
            Container::MpegAudio => Box::new(
                symphonia::default::formats::MpaReader::try_new(mss, options)
                    .map_err(|e| Error::Malformed(e.to_string()))?,
            ),
            Container::Flac => Box::new(
                symphonia::default::formats::FlacReader::try_new(mss, options)
                    .map_err(|e| Error::Malformed(e.to_string()))?,
            ),
            Container::Wav => Box::new(
                symphonia::default::formats::WavReader::try_new(mss, options)
                    .map_err(|e| Error::Malformed(e.to_string()))?,
            ),
            _ => return Err(Error::Unsupported(String::from("container"))),
        };
        let mut tracks = Vec::new();
        let mut time_bases = Vec::new();
        let mut duration = None;
        for track in reader.tracks() {
            let Some(CodecParameters::Audio(params)) = &track.codec_params else {
                continue;
            };
            let codec = match params.codec {
                CODEC_ID_OPUS => Codec::Opus,
                CODEC_ID_VORBIS => Codec::Vorbis,
                CODEC_ID_FLAC => Codec::Flac,
                CODEC_ID_MP3 | CODEC_ID_MP2 | CODEC_ID_MP1 => Codec::Mp3,
                CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S32LE | CODEC_ID_PCM_U8
                | CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F64LE | CODEC_ID_PCM_ALAW
                | CODEC_ID_PCM_MULAW => Codec::Pcm,
                _ => continue,
            };
            let mut info = TrackInfo::new(track.id, TrackKind::Audio, codec);
            info.codec_string = codec::describe(codec, &info.details);
            info.audio = Some(AudioInfo {
                sample_rate: params.sample_rate.unwrap_or(0),
                channels: params.channels.as_ref().map_or(0, |c| c.count() as u16),
                bits_per_sample: params.bits_per_sample.unwrap_or(0) as u16,
            });
            if let Some(extra) = &params.extra_data {
                info.config = Arc::from(&extra[..]);
            }
            if codec == Codec::Opus {
                // RFC 7845 §4.2: pre-skip from the identification header.
                if info.config.len() >= 12 && &info.config[..8] == b"OpusHead" {
                    let pre_skip = u16::from_le_bytes([info.config[10], info.config[11]]);
                    info.codec_delay = i64::from(pre_skip) * MICROS_PER_SECOND / 48_000;
                }
                info.seek_preroll = 80_000;
            }
            info.native = Some(NativeParams(Arc::new(params.clone())));
            info.language = track.language.clone().unwrap_or_default();
            let base = track.time_base;
            if let (Some(frames), Some(base)) = (track.num_frames, base) {
                let micros = to_micros(
                    Timestamp::new(frames.min(i64::MAX as u64) as i64),
                    Some(base),
                );
                info.duration = Some(micros);
                duration = duration.max(Some(micros));
            }
            tracks.push(info);
            time_bases.push(base);
        }
        if tracks.is_empty() {
            return Err(Error::Unsupported(String::from("no playable audio track")));
        }
        Ok(Self {
            reader,
            tracks,
            time_bases,
            duration,
            seekable,
        })
    }
}

impl Demuxer for AudioFileDemuxer {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn duration(&self) -> Option<Micros> {
        self.duration
    }

    fn read(&mut self, track: u32) -> Result<Option<Packet>, Error> {
        let Some(index) = self.tracks.iter().position(|t| t.id == track) else {
            return Ok(None);
        };
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(symphonia::core::errors::Error::IoError(error)) => {
                    if error.kind() == io::ErrorKind::UnexpectedEof {
                        return Ok(None);
                    }
                    return Err(Error::Io(error));
                }
                Err(error) => return Err(Error::Malformed(error.to_string())),
            };
            if packet.track_id != track {
                continue;
            }
            let base = self.time_bases[index];
            let pts = to_micros(packet.pts, base);
            let duration = to_micros(
                Timestamp::new(packet.dur.get().min(i64::MAX as u64) as i64),
                base,
            );
            let trim_end = to_micros(
                Timestamp::new(packet.trim_end.get().min(i64::MAX as u64) as i64),
                base,
            );
            return Ok(Some(Packet {
                track,
                pts,
                dts: pts,
                duration,
                keyframe: true,
                data: Arc::from(packet.data),
                encryption: None,
                discard_padding: trim_end,
                config: None,
            }));
        }
    }

    fn seek(&mut self, time: Micros) -> Result<Micros, Error> {
        let to = SeekTo::Time {
            time: Time::from_micros(time.max(0)),
            track_id: self.tracks.first().map(|t| t.id),
        };
        let seeked = self
            .reader
            .seek(SeekMode::Coarse, to)
            .map_err(|e| Error::Malformed(e.to_string()))?;
        let index = self
            .tracks
            .iter()
            .position(|t| t.id == seeked.track_id)
            .unwrap_or(0);
        Ok(to_micros(seeked.actual_ts, self.time_bases[index]))
    }

    fn seekable(&self) -> bool {
        self.seekable
    }
}
