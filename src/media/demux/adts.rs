//! Raw AAC in ADTS framing (ISO/IEC 13818-7 §6.2, ISO/IEC 14496-3
//! §1.A.2.2): `audio/aac` resources. Packets carry raw AAC (header
//! removed) and the track carries the equivalent AudioSpecificConfig, the
//! same shape MP4 and WebM produce, so the decoders see one format.

use std::sync::Arc;

use super::{ByteSource, Demuxer, Error};
use crate::media::codec::{self, Codec, CodecDetails};
use crate::media::track::{AudioInfo, Packet, TrackInfo, TrackKind};
use crate::media::{MICROS_PER_SECOND, Micros};

const RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// A parsed ADTS frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub object_type: u8,
    pub rate_index: u8,
    pub channel_config: u8,
    pub header_len: usize,
    pub frame_len: usize,
    pub blocks: u8,
}

pub(super) fn parse_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < 7 || bytes[0] != 0xFF || bytes[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = bytes[1] & 1 == 1;
    let object_type = (bytes[2] >> 6) + 1;
    let rate_index = (bytes[2] >> 2) & 0xF;
    let channel_config = ((bytes[2] & 1) << 2) | (bytes[3] >> 6);
    let frame_len = (usize::from(bytes[3] & 3) << 11)
        | (usize::from(bytes[4]) << 3)
        | usize::from(bytes[5] >> 5);
    let blocks = (bytes[6] & 3) + 1;
    let header_len = if protection_absent { 7 } else { 9 };
    if rate_index as usize >= RATES.len() || frame_len < header_len {
        return None;
    }
    Some(Header {
        object_type,
        rate_index,
        channel_config,
        header_len,
        frame_len,
        blocks,
    })
}

/// A frame header at the start of `bytes`, confirmed by a second one where
/// enough bytes are available (sniffing).
pub(super) fn sync(bytes: &[u8]) -> Option<Header> {
    let header = parse_header(bytes)?;
    if bytes.len() >= header.frame_len + 7 {
        parse_header(&bytes[header.frame_len..])?;
    }
    Some(header)
}

/// Bytes of an ID3v2 tag at the start of a stream (ID3v2.4 §3.1).
fn id3_len(bytes: &[u8]) -> usize {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return 0;
    }
    let size = bytes[6..10]
        .iter()
        .fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7F));
    let footer = if bytes[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

pub(super) struct AdtsDemuxer {
    source: Box<dyn ByteSource>,
    track: [TrackInfo; 1],
    first: u64,
    position: u64,
    /// Samples decoded so far, for timestamps.
    samples: u64,
    rate: u32,
    average_frame: f64,
}

impl AdtsDemuxer {
    pub(super) fn open(mut source: Box<dyn ByteSource>) -> Result<Self, Error> {
        let head = source.read_up_to(0, 64 << 10)?;
        let first = id3_len(&head);
        let window = head.get(first..).unwrap_or(&[]);
        let Some(offset) = (0..window.len().min(8192)).find(|&i| sync(&window[i..]).is_some())
        else {
            return Err(Error::Malformed(String::from("no ADTS frame")));
        };
        let header = sync(&window[offset..]).expect("found above");
        let first = (first + offset) as u64;
        let rate = RATES[usize::from(header.rate_index)];
        // AudioSpecificConfig: objectType(5) rateIndex(4) channels(4).
        let asc = [
            (header.object_type << 3) | (header.rate_index >> 1),
            ((header.rate_index & 1) << 7) | (header.channel_config << 3),
        ];
        let mut info = TrackInfo::new(1, TrackKind::Audio, Codec::Aac);
        info.details = CodecDetails {
            audio_object_type: Some(header.object_type),
            ..Default::default()
        };
        info.codec_string = codec::describe(Codec::Aac, &info.details);
        info.config = Arc::from(&asc[..]);
        info.audio = Some(AudioInfo {
            sample_rate: rate,
            channels: u16::from(header.channel_config.max(1)),
            bits_per_sample: 0,
        });
        // Average frame size of the first frames, for duration and seeking.
        let mut pos = offset;
        let mut frames = 0usize;
        let mut bytes = 0usize;
        while let Some(h) = parse_header(&window[pos..]) {
            frames += 1;
            bytes += h.frame_len;
            pos += h.frame_len;
            if frames >= 64 || pos >= window.len() {
                break;
            }
        }
        let average_frame = if frames > 0 {
            bytes as f64 / frames as f64
        } else {
            0.0
        };
        if let (Some(len), true) = (source.len(), average_frame > 0.0) {
            let total_frames = (len.saturating_sub(first)) as f64 / average_frame;
            info.duration = Some(
                (total_frames * 1024.0 * MICROS_PER_SECOND as f64 / f64::from(rate)) as Micros,
            );
        }
        Ok(Self {
            source,
            track: [info],
            first,
            position: first,
            samples: 0,
            rate,
            average_frame,
        })
    }

    fn resync(&mut self) -> Result<bool, Error> {
        let window = self.source.read_up_to(self.position, 16 << 10)?;
        match (0..window.len()).find(|&i| sync(&window[i..]).is_some()) {
            Some(skip) => {
                self.position += skip as u64;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl Demuxer for AdtsDemuxer {
    fn tracks(&self) -> &[TrackInfo] {
        &self.track
    }

    fn duration(&self) -> Option<Micros> {
        self.track[0].duration
    }

    fn read(&mut self, track: u32) -> Result<Option<Packet>, Error> {
        if track != 1 {
            return Ok(None);
        }
        for _ in 0..2 {
            let head = self.source.read_up_to(self.position, 9)?;
            if head.len() < 7 {
                return Ok(None);
            }
            let Some(header) = parse_header(&head) else {
                if !self.resync()? {
                    return Ok(None);
                }
                continue;
            };
            let frame = self.source.read_up_to(self.position, header.frame_len)?;
            if frame.len() < header.frame_len {
                return Ok(None);
            }
            self.position += header.frame_len as u64;
            let samples = 1024 * u64::from(header.blocks);
            let pts = (self.samples as i64 * MICROS_PER_SECOND) / i64::from(self.rate);
            self.samples += samples;
            let duration = (samples as i64 * MICROS_PER_SECOND) / i64::from(self.rate);
            return Ok(Some(Packet {
                track: 1,
                pts,
                dts: pts,
                duration,
                keyframe: true,
                data: Arc::from(&frame[header.header_len..]),
                encryption: None,
                discard_padding: 0,
                config: None,
            }));
        }
        Ok(None)
    }

    fn seek(&mut self, time: Micros) -> Result<Micros, Error> {
        // Constant-bitrate estimate, then resynchronise on a frame header.
        let frame =
            (time.max(0) as f64 * f64::from(self.rate) / MICROS_PER_SECOND as f64 / 1024.0).floor();
        self.position = self.first + (frame * self.average_frame) as u64;
        self.samples = frame as u64 * 1024;
        if !self.resync()? {
            self.position = self.first;
            self.samples = 0;
        }
        Ok((self.samples as i64 * MICROS_PER_SECOND) / i64::from(self.rate))
    }

    fn seekable(&self) -> bool {
        self.source.seekable() && self.average_frame > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::demux::MemorySource;

    fn frame(len: usize) -> Vec<u8> {
        // AAC LC (profile 1), 44.1 kHz (index 4), stereo, no CRC.
        let mut out = vec![
            0xFF,
            0xF1,
            (1 << 6) | (4 << 2),
            (2 << 6) | ((len >> 11) as u8 & 3),
            (len >> 3) as u8,
            ((len & 7) << 5) as u8 | 0x1F,
            0xFC,
        ];
        out.resize(len, 0x11);
        out
    }

    #[test]
    fn reads_frames_and_config() {
        let mut bytes = Vec::new();
        for _ in 0..10 {
            bytes.extend(frame(100));
        }
        let mut demuxer = AdtsDemuxer::open(Box::new(MemorySource::new(bytes))).unwrap();
        assert_eq!(demuxer.tracks()[0].config.as_ref(), &[0x12, 0x10]);
        assert_eq!(demuxer.tracks()[0].audio.unwrap().sample_rate, 44100);
        let first = demuxer.read(1).unwrap().unwrap();
        assert_eq!(first.data.len(), 93);
        assert_eq!(first.pts, 0);
        let second = demuxer.read(1).unwrap().unwrap();
        assert_eq!(second.pts, 23_219);
        let mut count = 2;
        while demuxer.read(1).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 10);
        assert_eq!(demuxer.seek(100_000).unwrap(), 92_879);
    }
}
