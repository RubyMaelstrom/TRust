//! Reading packets from a media resource (HTML #media-resource) for
//! progressive playback: sniffing the container, then per-track packet
//! readers over a random-access [`ByteSource`].
//!
//! Media Source Extensions do not use this layer: a SourceBuffer parses its
//! appended bytes itself (`isobmff`, `webm`) into track buffers.

use std::fmt;
use std::io;
use std::sync::Arc;

use super::Micros;
use super::container::Container;
use super::track::{Packet, TrackInfo};

mod adts;
mod audio_file;
mod mp4;
mod webm;

#[derive(Debug)]
pub enum Error {
    /// The byte source failed or was aborted.
    Io(io::Error),
    /// The resource is not a valid media resource of its format.
    Malformed(String),
    /// The resource is valid but uses a format TRust does not play.
    Unsupported(String),
    /// No packet is available yet (Media Source Extensions: the
    /// application has not appended it). The demuxer calls its waker when
    /// that may have changed.
    Pending,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "media read failed: {error}"),
            Self::Malformed(what) => write!(f, "malformed media resource: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported media resource: {what}"),
            Self::Pending => f.write_str("media data is not available yet"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<super::isobmff::Error> for Error {
    fn from(error: super::isobmff::Error) -> Self {
        match error {
            super::isobmff::Error::Unsupported(_) => Self::Unsupported(error.to_string()),
            _ => Self::Malformed(error.to_string()),
        }
    }
}

impl From<super::webm::Error> for Error {
    fn from(error: super::webm::Error) -> Self {
        match error {
            super::webm::Error::Unsupported(_) => Self::Unsupported(error.to_string()),
            super::webm::Error::Malformed(_) => Self::Malformed(error.to_string()),
        }
    }
}

/// Random access to a media resource's bytes. Reads may block while the
/// bytes are fetched; an aborted read fails with
/// [`io::ErrorKind::Interrupted`].
// `len` may be unknown (`None`), so emptiness is no separate question.
#[allow(clippy::len_without_is_empty)]
pub trait ByteSource: Send {
    /// Read up to `buf.len()` bytes at `offset`. Zero means the end of the
    /// resource.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// The resource's length, when known.
    fn len(&self) -> Option<u64>;
    /// Whether reads may jump backwards and forwards (HTTP ranges or a
    /// fully held resource).
    fn seekable(&self) -> bool;

    /// Read exactly `len` bytes at `offset`, failing with
    /// `UnexpectedEof` at the end of the resource.
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let read = self.read_at(offset + filled as u64, &mut out[filled..])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "media resource ended",
                ));
            }
            filled += read;
        }
        Ok(out)
    }

    /// Read up to `len` bytes at `offset`, fewer only at the end.
    fn read_up_to(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let read = self.read_at(offset + filled as u64, &mut out[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        out.truncate(filled);
        Ok(out)
    }
}

/// A resource held entirely in memory (`data:` and `blob:` URLs, tests).
pub struct MemorySource {
    bytes: Arc<[u8]>,
}

impl MemorySource {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
}

impl ByteSource for MemorySource {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= self.bytes.len() {
            return Ok(0);
        }
        let len = buf.len().min(self.bytes.len() - start);
        buf[..len].copy_from_slice(&self.bytes[start..start + len]);
        Ok(len)
    }

    fn len(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }

    fn seekable(&self) -> bool {
        true
    }
}

/// A per-track packet reader for one media resource.
pub trait Demuxer: Send {
    fn tracks(&self) -> &[TrackInfo];
    /// The resource's duration, when known.
    fn duration(&self) -> Option<Micros>;
    /// The next packet of `track` in decode order; `None` at its end.
    fn read(&mut self, track: u32) -> Result<Option<Packet>, Error>;
    /// Position every track at its last random access point at or before
    /// `time`; returns the earliest such point.
    fn seek(&mut self, time: Micros) -> Result<Micros, Error>;
    fn seekable(&self) -> bool;
    /// The start of the media timeline (HTML's "earliest possible
    /// position" is zero; containers may start later).
    fn start_time(&self) -> Micros {
        0
    }
    /// Block until [`tracks`](Self::tracks) is known (Media Source
    /// Extensions: until every SourceBuffer has received an initialization
    /// segment). Fails when the media goes away first or `stop` is set.
    fn wait_for_tracks(&mut self, _stop: &std::sync::atomic::AtomicBool) -> Result<(), Error> {
        Ok(())
    }
    /// Install the function a demuxer whose reads return
    /// [`Error::Pending`] calls when packets may have become available.
    fn set_waker(&mut self, _waker: Waker) {}
}

/// Wakes a player that is waiting for packets.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// MIME Sniffing #rules-for-sniffing-audio-and-video-specifically (local
/// whatwg/mimesniff snapshot), with the signatures of the formats TRust
/// plays. Returns the container a resource header matches.
pub fn sniff(header: &[u8]) -> Option<Container> {
    let starts = |pattern: &[u8]| header.starts_with(pattern);
    if starts(b"ID3") {
        return Some(Container::MpegAudio);
    }
    if starts(b"OggS\0") {
        return Some(Container::Ogg);
    }
    if header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WAVE" {
        return Some(Container::Wav);
    }
    if mp4_signature(header) {
        return Some(Container::Mp4);
    }
    if let Some(container) = ebml_signature(header) {
        return Some(container);
    }
    if starts(b"fLaC") {
        return Some(Container::Flac);
    }
    if mp3_signature(header) {
        return Some(Container::MpegAudio);
    }
    if adts::sync(header).is_some() {
        return Some(Container::Adts);
    }
    None
}

/// MIME Sniffing "signature for MP4", extended like Firefox to any `ftyp`
/// box: QuickTime and M4A files often name only other brands.
fn mp4_signature(header: &[u8]) -> bool {
    if header.len() < 12 {
        return false;
    }
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    &header[4..8] == b"ftyp" && size >= 8 && size.is_multiple_of(4)
}

/// MIME Sniffing "signature for WebM", also recognising Matroska.
fn ebml_signature(header: &[u8]) -> Option<Container> {
    if !header.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }
    let limit = header.len().min(64);
    let mut iter = 4;
    while iter + 1 < limit {
        if header[iter] == 0x42 && header[iter + 1] == 0x82 {
            // DocType: a one-byte size vint, then the string.
            let size_byte = *header.get(iter + 2)?;
            if size_byte & 0x80 == 0 {
                return None;
            }
            let size = usize::from(size_byte & 0x7F);
            let doc_type = header.get(iter + 3..iter + 3 + size)?;
            let doc_type = doc_type.split(|&b| b == 0).next().unwrap_or(doc_type);
            return match doc_type {
                b"webm" => Some(Container::WebM),
                b"matroska" => Some(Container::Matroska),
                _ => None,
            };
        }
        iter += 1;
    }
    None
}

/// MPEG-1/2 Layer III frame header (MIME Sniffing "signature for MP3
/// without ID3"): a valid header whose computed frame length leads to a
/// second valid header.
fn mp3_signature(header: &[u8]) -> bool {
    let Some(length) = mp3_frame_length(header) else {
        return false;
    };
    header.len() >= length + 4 && mp3_frame_length(&header[length..]).is_some()
}

fn mp3_frame_length(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (bytes[1] >> 3) & 3;
    let layer = (bytes[1] >> 1) & 3;
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_index = usize::from(bytes[2] >> 4);
    let rate_index = usize::from((bytes[2] >> 2) & 3);
    if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let padding = usize::from((bytes[2] >> 1) & 1);
    const V1: [usize; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2: [usize; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    const RATES: [usize; 3] = [44100, 48000, 32000];
    let mpeg1 = version == 3;
    let bitrate = if mpeg1 {
        V1[bitrate_index]
    } else {
        V2[bitrate_index]
    } * 1000;
    let mut rate = RATES[rate_index];
    if !mpeg1 {
        rate /= if version == 2 { 2 } else { 4 };
    }
    let scale = if mpeg1 { 144 } else { 72 };
    Some(scale * bitrate / rate + padding)
}

/// Open a demuxer for `source`. `hint` is the container named by the
/// response's `Content-Type` or the `<source type>`, used when sniffing
/// finds no signature (MIME Sniffing: the supplied MIME type).
pub fn open(
    mut source: Box<dyn ByteSource>,
    hint: Option<Container>,
) -> Result<Box<dyn Demuxer>, Error> {
    let header = source.read_up_to(0, 1445)?;
    let container = sniff(&header)
        .or(hint)
        .ok_or_else(|| Error::Unsupported(String::from("unrecognised media container")))?;
    match container {
        Container::Mp4 => Ok(Box::new(mp4::Mp4Demuxer::open(source)?)),
        Container::WebM | Container::Matroska => Ok(Box::new(webm::WebmDemuxer::open(source)?)),
        Container::Adts => Ok(Box::new(adts::AdtsDemuxer::open(source)?)),
        Container::Ogg | Container::MpegAudio | Container::Flac | Container::Wav => Ok(Box::new(
            audio_file::AudioFileDemuxer::open(source, container)?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing_signatures() {
        assert_eq!(sniff(b"ID3\x04\0\0\0\0\0\0"), Some(Container::MpegAudio));
        assert_eq!(sniff(b"OggS\0\x02rest"), Some(Container::Ogg));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), Some(Container::Wav));
        assert_eq!(
            sniff(b"\0\0\0\x18ftypisom\0\0\0\0isomiso2"),
            Some(Container::Mp4)
        );
        assert_eq!(
            sniff(b"\0\0\0\x14ftypM4A \0\0\0\0M4A "),
            Some(Container::Mp4)
        );
        let webm = [
            0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81, 0x01, 0x42, 0x82, 0x84, b'w', b'e',
            b'b', b'm',
        ];
        assert_eq!(sniff(&webm), Some(Container::WebM));
        let mut mkv = webm.to_vec();
        mkv[11] = 0x88;
        mkv.truncate(12);
        mkv.extend_from_slice(b"matroska");
        assert_eq!(sniff(&mkv), Some(Container::Matroska));
        assert_eq!(sniff(b"fLaC\0\0\0\x22"), Some(Container::Flac));
        // Two MPEG-1 Layer III frames at 128 kb/s, 44.1 kHz: 417 bytes.
        let mut mp3 = vec![0u8; 417 + 4];
        mp3[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        mp3[417..].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(sniff(&mp3), Some(Container::MpegAudio));
        assert_eq!(sniff(b"<html>"), None);
    }

    #[test]
    fn memory_source_reads() {
        let mut source = MemorySource::new(vec![1u8, 2, 3, 4, 5]);
        assert_eq!(source.read_exact_at(1, 3).unwrap(), vec![2, 3, 4]);
        assert_eq!(source.read_up_to(3, 10).unwrap(), vec![4, 5]);
        assert!(source.read_exact_at(4, 2).is_err());
        assert_eq!(source.len(), Some(5));
    }
}
