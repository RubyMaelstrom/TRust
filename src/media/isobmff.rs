//! ISO Base Media File Format (ISO/IEC 14496-12) parsing for MP4 playback
//! and the MSE ISO BMFF byte stream format (local
//! w3c/mse-byte-stream-format-isobmff snapshot 40cacca5).
//!
//! This parses boxes from complete in-memory byte ranges: a movie box
//! ([`parse_moov`]) yields tracks and, for non-fragmented files, sample
//! tables; a movie fragment box ([`parse_moof`]) yields each track
//! fragment's samples. Callers own the I/O (random access for progressive
//! files, appended bytes for Media Source Extensions).
//!
//! Codec configuration boxes follow ISO/IEC 14496-15 (avcC §5.3.3.1, hvcC
//! §8.3.3.1), the VP9 ISOBMFF binding (vpcC), the AV1 ISOBMFF binding (av1C),
//! ISO/IEC 14496-1 §7.2.6.5 (esds), the Opus-in-ISOBMFF mapping (dOps) and
//! the FLAC-in-ISOBMFF mapping (dfLa). Common Encryption boxes follow
//! ISO/IEC 23001-7 (sinf/schm/tenc §8.1–8.2, senc §7.2, pssh §8.1).

use std::fmt;
use std::sync::Arc;

use super::codec::{self, Codec, CodecDetails};
use super::track::{
    AudioInfo, ColorInfo, EncryptionScheme, SampleEncryption, Subsample, TrackEncryption,
    TrackInfo, TrackKind, VideoInfo,
};
use super::{Micros, ticks_to_micros};

/// Upper bound on samples expanded from one movie's sample tables. Tables
/// can claim billions of samples in a few bytes (an `stts` run, a constant
/// `stsz`); expansion beyond this is refused rather than allocated.
pub const MAX_SAMPLES: usize = 20_000_000;

/// Upper bound on samples in one track fragment run.
const MAX_FRAGMENT_SAMPLES: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes end inside a structure.
    Truncated(&'static str),
    /// A structure violates the format.
    Malformed(&'static str),
    /// A valid structure TRust does not play.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(what) => write!(f, "truncated MP4 {what}"),
            Self::Malformed(what) => write!(f, "malformed MP4 {what}"),
            Self::Unsupported(what) => write!(f, "unsupported MP4 {what}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// A four-character box type.
pub type FourCc = [u8; 4];

/// A box header (ISO/IEC 14496-12 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: FourCc,
    /// Header bytes, including a 64-bit size and a `uuid` user type.
    pub header_len: u64,
    /// Total size including the header; `None` extends to the end of the
    /// enclosing container (size 0, only legal for the last box).
    pub size: Option<u64>,
    pub user_type: Option<[u8; 16]>,
}

impl BoxHeader {
    /// Parse a header at the start of `bytes`. `Ok(None)` when more bytes
    /// are needed to read it.
    pub fn parse(bytes: &[u8]) -> Result<Option<Self>> {
        if bytes.len() < 8 {
            return Ok(None);
        }
        let size32 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let kind = [bytes[4], bytes[5], bytes[6], bytes[7]];
        let (mut header_len, size) = match size32 {
            0 => (8, None),
            1 => {
                if bytes.len() < 16 {
                    return Ok(None);
                }
                let large = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes"));
                (16, Some(large))
            }
            n => (8, Some(u64::from(n))),
        };
        let mut user_type = None;
        if &kind == b"uuid" {
            let start = header_len as usize;
            if bytes.len() < start + 16 {
                return Ok(None);
            }
            user_type = Some(bytes[start..start + 16].try_into().expect("16 bytes"));
            header_len += 16;
        }
        if let Some(size) = size
            && size < header_len
        {
            return Err(Error::Malformed("box size"));
        }
        Ok(Some(Self {
            kind,
            header_len,
            size,
            user_type,
        }))
    }
}

/// A big-endian reader over one box's payload.
#[derive(Clone)]
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8]> {
        if self.remaining() < len {
            return Err(Error::Truncated(what));
        }
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn skip(&mut self, len: usize, what: &'static str) -> Result<()> {
        self.take(len, what).map(|_| ())
    }

    fn u8(&mut self, what: &'static str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2, what)?.try_into().expect("2"),
        ))
    }

    fn u24(&mut self, what: &'static str) -> Result<u32> {
        let b = self.take(3, what)?;
        Ok(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4, what)?.try_into().expect("4"),
        ))
    }

    fn i32(&mut self, what: &'static str) -> Result<i32> {
        Ok(i32::from_be_bytes(
            self.take(4, what)?.try_into().expect("4"),
        ))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8, what)?.try_into().expect("8"),
        ))
    }

    fn i64(&mut self, what: &'static str) -> Result<i64> {
        Ok(i64::from_be_bytes(
            self.take(8, what)?.try_into().expect("8"),
        ))
    }

    /// FullBox version and flags (§4.2).
    fn full_box(&mut self, what: &'static str) -> Result<(u8, u32)> {
        let version = self.u8(what)?;
        let flags = self.u24(what)?;
        Ok((version, flags))
    }

    fn rest(&mut self) -> &'a [u8] {
        let slice = &self.bytes[self.pos..];
        self.pos = self.bytes.len();
        slice
    }
}

/// Iterate the child boxes of a container payload. A box whose size runs
/// past the payload is reported as truncated.
struct Children<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Children<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Result<(FourCc, &'a [u8], Option<[u8; 16]>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.bytes[self.pos..];
        if rest.is_empty() {
            return None;
        }
        // Some writers pad containers with a 4-byte zero terminator
        // (QuickTime's udta convention); ignore trailing bytes too short
        // to hold a box.
        if rest.len() < 8 {
            self.pos = self.bytes.len();
            return None;
        }
        let header = match BoxHeader::parse(rest) {
            Ok(Some(header)) => header,
            Ok(None) => {
                self.pos = self.bytes.len();
                return Some(Err(Error::Truncated("box header")));
            }
            Err(error) => {
                self.pos = self.bytes.len();
                return Some(Err(error));
            }
        };
        let size = header.size.unwrap_or(rest.len() as u64);
        if size > rest.len() as u64 {
            self.pos = self.bytes.len();
            return Some(Err(Error::Truncated("child box")));
        }
        let payload = &rest[header.header_len as usize..size as usize];
        self.pos += size as usize;
        Some(Ok((header.kind, payload, header.user_type)))
    }
}

fn find_child<'a>(bytes: &'a [u8], kind: &FourCc) -> Result<Option<&'a [u8]>> {
    for child in Children::new(bytes) {
        let (child_kind, payload, _) = child?;
        if &child_kind == kind {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

/// A `pssh` box (ISO/IEC 23001-7 §8.1), kept whole as EME "cenc" init data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pssh {
    pub system_id: [u8; 16],
    pub key_ids: Vec<[u8; 16]>,
    /// The entire box, header included.
    pub raw: Vec<u8>,
}

fn parse_pssh(payload: &[u8]) -> Result<Pssh> {
    let mut r = Reader::new(payload);
    let (version, _) = r.full_box("pssh")?;
    let system_id: [u8; 16] = r.take(16, "pssh")?.try_into().expect("16");
    let mut key_ids = Vec::new();
    if version > 0 {
        let count = r.u32("pssh")? as usize;
        if count > r.remaining() / 16 {
            return Err(Error::Truncated("pssh key ids"));
        }
        for _ in 0..count {
            key_ids.push(r.take(16, "pssh")?.try_into().expect("16"));
        }
    }
    let data_len = r.u32("pssh")? as usize;
    r.skip(data_len, "pssh data")?;
    let mut raw = Vec::with_capacity(payload.len() + 8);
    raw.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    raw.extend_from_slice(b"pssh");
    raw.extend_from_slice(payload);
    Ok(Pssh {
        system_id,
        key_ids,
        raw,
    })
}

/// Track extends defaults (`trex`, §8.8.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackExtends {
    pub track_id: u32,
    pub sample_description_index: u32,
    pub sample_duration: u32,
    pub sample_size: u32,
    pub sample_flags: u32,
}

/// One sample of a non-fragmented track's sample tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Absolute file offset.
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track's media timescale.
    pub dts: i64,
    /// Composition offset (`ctts`).
    pub cts_offset: i32,
    pub duration: u32,
    pub sync: bool,
}

/// A track of the movie box.
#[derive(Clone, Debug)]
pub struct MovieTrack {
    pub info: TrackInfo,
    /// Media timescale (`mdhd`).
    pub timescale: u32,
    /// Ticks (media timescale) subtracted from composition times: the edit
    /// list's media_time, minus any leading empty edit.
    pub composition_shift: i64,
    /// Samples of a non-fragmented track, in decode order.
    pub samples: Vec<Sample>,
    /// The track header enables the track (`tkhd` flag 0x1).
    pub enabled: bool,
}

impl MovieTrack {
    /// Presentation time of a sample with decode time `dts` and composition
    /// offset `cts_offset` (both in media timescale ticks).
    pub fn presentation_micros(&self, dts: i64, cts_offset: i64) -> Micros {
        ticks_to_micros(
            dts.saturating_add(cts_offset)
                .saturating_sub(self.composition_shift),
            u64::from(self.timescale),
        )
    }

    pub fn decode_micros(&self, dts: i64) -> Micros {
        ticks_to_micros(
            dts.saturating_sub(self.composition_shift),
            u64::from(self.timescale),
        )
    }
}

/// The parsed movie box (`moov`).
#[derive(Clone, Debug, Default)]
pub struct Movie {
    pub timescale: u32,
    /// Movie duration (mvhd, else mehd for fragmented files).
    pub duration: Option<Micros>,
    pub tracks: Vec<MovieTrack>,
    /// `mvex` is present: samples arrive in movie fragments.
    pub fragmented: bool,
    pub track_extends: Vec<TrackExtends>,
    pub pssh: Vec<Pssh>,
}

impl Movie {
    pub fn track(&self, id: u32) -> Option<&MovieTrack> {
        self.tracks.iter().find(|track| track.info.id == id)
    }

    fn trex(&self, id: u32) -> TrackExtends {
        self.track_extends
            .iter()
            .copied()
            .find(|trex| trex.track_id == id)
            .unwrap_or(TrackExtends {
                track_id: id,
                sample_description_index: 1,
                ..Default::default()
            })
    }
}

/// Parse a movie box payload (the bytes after its header). Tracks whose
/// handler or sample entry TRust does not recognise are skipped; a movie
/// without any recognised track is an error.
pub fn parse_moov(payload: &[u8]) -> Result<Movie> {
    let mut movie = Movie::default();
    let mut mvhd_duration = None;
    let mut traks = Vec::new();
    for child in Children::new(payload) {
        let (kind, body, _) = child?;
        match &kind {
            b"mvhd" => {
                let mut r = Reader::new(body);
                let (version, _) = r.full_box("mvhd")?;
                let (timescale, duration) = if version == 1 {
                    r.skip(16, "mvhd")?;
                    (r.u32("mvhd")?, r.u64("mvhd")?)
                } else {
                    r.skip(8, "mvhd")?;
                    (r.u32("mvhd")?, u64::from(r.u32("mvhd")?))
                };
                movie.timescale = timescale;
                // All ones means unknown (§8.2.2.3).
                let unknown =
                    duration == u64::MAX || (version == 0 && duration == u64::from(u32::MAX));
                if !unknown && duration > 0 {
                    mvhd_duration = Some(duration);
                }
            }
            b"trak" => traks.push(body),
            b"mvex" => {
                movie.fragmented = true;
                for child in Children::new(body) {
                    let (kind, body, _) = child?;
                    match &kind {
                        b"trex" => {
                            let mut r = Reader::new(body);
                            r.full_box("trex")?;
                            movie.track_extends.push(TrackExtends {
                                track_id: r.u32("trex")?,
                                sample_description_index: r.u32("trex")?,
                                sample_duration: r.u32("trex")?,
                                sample_size: r.u32("trex")?,
                                sample_flags: r.u32("trex")?,
                            });
                        }
                        b"mehd" => {
                            let mut r = Reader::new(body);
                            let (version, _) = r.full_box("mehd")?;
                            let duration = if version == 1 {
                                r.u64("mehd")?
                            } else {
                                u64::from(r.u32("mehd")?)
                            };
                            if mvhd_duration.is_none() && duration > 0 {
                                mvhd_duration = Some(duration);
                            }
                        }
                        _ => {}
                    }
                }
            }
            b"pssh" => movie.pssh.push(parse_pssh(body)?),
            _ => {}
        }
    }
    if movie.timescale == 0 {
        return Err(Error::Malformed("movie header"));
    }
    movie.duration = mvhd_duration.map(|duration| {
        ticks_to_micros(
            duration.min(i64::MAX as u64) as i64,
            u64::from(movie.timescale),
        )
    });
    for trak in traks {
        if let Some(track) = parse_trak(trak, movie.timescale, movie.fragmented)? {
            if movie.tracks.iter().any(|t| t.info.id == track.info.id) {
                return Err(Error::Malformed("duplicate track id"));
            }
            movie.tracks.push(track);
        }
    }
    if movie.tracks.is_empty() {
        return Err(Error::Unsupported("movie without playable tracks"));
    }
    Ok(movie)
}

fn parse_trak(
    payload: &[u8],
    movie_timescale: u32,
    fragmented: bool,
) -> Result<Option<MovieTrack>> {
    let mut track_id = None;
    let mut enabled = true;
    let mut tkhd_size = (0u32, 0u32);
    let mut edits: Vec<(u64, i64)> = Vec::new();
    let mut mdia = None;
    for child in Children::new(payload) {
        let (kind, body, _) = child?;
        match &kind {
            b"tkhd" => {
                let mut r = Reader::new(body);
                let (version, flags) = r.full_box("tkhd")?;
                enabled = flags & 1 != 0;
                if version == 1 {
                    r.skip(16, "tkhd")?;
                } else {
                    r.skip(8, "tkhd")?;
                }
                track_id = Some(r.u32("tkhd")?);
                r.skip(4, "tkhd")?;
                r.skip(if version == 1 { 8 } else { 4 }, "tkhd")?;
                // reserved(8) layer(2) alternate_group(2) volume(2)
                // reserved(2) matrix(36)
                r.skip(8 + 2 + 2 + 2 + 2 + 36, "tkhd")?;
                let width = r.u32("tkhd")? >> 16;
                let height = r.u32("tkhd")? >> 16;
                tkhd_size = (width, height);
            }
            b"edts" => {
                if let Some(elst) = find_child(body, b"elst")? {
                    let mut r = Reader::new(elst);
                    let (version, _) = r.full_box("elst")?;
                    let count = r.u32("elst")? as usize;
                    let entry = if version == 1 { 20 } else { 12 };
                    if count > r.remaining() / entry {
                        return Err(Error::Truncated("edit list"));
                    }
                    for _ in 0..count.min(64) {
                        let (duration, media_time) = if version == 1 {
                            (r.u64("elst")?, r.i64("elst")?)
                        } else {
                            (u64::from(r.u32("elst")?), i64::from(r.i32("elst")?))
                        };
                        r.skip(4, "elst")?;
                        edits.push((duration, media_time));
                    }
                }
            }
            b"mdia" => mdia = Some(body),
            _ => {}
        }
    }
    let (Some(track_id), Some(mdia)) = (track_id, mdia) else {
        return Err(Error::Malformed("track without tkhd or mdia"));
    };
    let mut timescale = 0;
    let mut media_duration = None;
    let mut language = String::new();
    let mut handler = [0u8; 4];
    let mut label = String::new();
    let mut stbl = None;
    for child in Children::new(mdia) {
        let (kind, body, _) = child?;
        match &kind {
            b"mdhd" => {
                let mut r = Reader::new(body);
                let (version, _) = r.full_box("mdhd")?;
                let duration;
                if version == 1 {
                    r.skip(16, "mdhd")?;
                    timescale = r.u32("mdhd")?;
                    duration = r.u64("mdhd")?;
                } else {
                    r.skip(8, "mdhd")?;
                    timescale = r.u32("mdhd")?;
                    duration = u64::from(r.u32("mdhd")?);
                }
                let unknown =
                    duration == u64::MAX || (version == 0 && duration == u64::from(u32::MAX));
                if !unknown && duration > 0 {
                    media_duration = Some(duration);
                }
                let packed = r.u16("mdhd")?;
                language = unpack_language(packed);
            }
            b"hdlr" => {
                let mut r = Reader::new(body);
                r.full_box("hdlr")?;
                r.skip(4, "hdlr")?;
                handler = r.take(4, "hdlr")?.try_into().expect("4");
                r.skip(12, "hdlr")?;
                let name = r.rest();
                let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                label = String::from_utf8_lossy(&name[..end]).trim().to_string();
            }
            b"minf" => stbl = find_child(body, b"stbl")?,
            _ => {}
        }
    }
    if timescale == 0 {
        return Err(Error::Malformed("media header"));
    }
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        _ => return Ok(None),
    };
    let Some(stbl) = stbl else {
        return Err(Error::Malformed("track without sample table"));
    };
    let Some(stsd) = find_child(stbl, b"stsd")? else {
        return Err(Error::Malformed("track without sample descriptions"));
    };
    let Some(mut info) = parse_stsd(stsd, track_id, kind)? else {
        return Ok(None);
    };
    info.language = language;
    // Handler names are often tool names ("VideoHandler", "SoundHandler");
    // they are not author-facing labels.
    if !matches!(
        label.as_str(),
        "" | "VideoHandler" | "SoundHandler" | "Core Media Video" | "Core Media Audio"
    ) && !label.starts_with("Mainconcept")
        && !label.starts_with("L-SMASH")
        && !label.starts_with("GPAC")
    {
        info.label = label;
    }
    info.default = enabled;
    if let Some(video) = info.video.as_mut()
        && tkhd_size.0 > 0
        && tkhd_size.1 > 0
        && video.display_width == video.width
        && video.display_height == video.height
    {
        // The track header's presentation size carries the display aspect
        // ratio when no `pasp` box does.
        video.display_width = tkhd_size.0;
        video.display_height = tkhd_size.1;
    }
    // ISO/IEC 14496-12 §8.6.6: an edit list maps media time to presentation
    // time. Like Chrome and Firefox, honour a leading empty edit (a delay)
    // followed by one normal edit (a start offset); ignore anything richer.
    let mut composition_shift = 0i64;
    let mut edits = edits.as_slice();
    let mut delay = 0u64;
    if let Some(&(duration, -1)) = edits.first() {
        delay = duration;
        edits = &edits[1..];
    }
    if let Some(&(_, media_time)) = edits.first()
        && media_time >= 0
    {
        composition_shift = media_time;
    }
    if delay > 0 && movie_timescale > 0 {
        let delay_media = (i128::from(delay) * i128::from(timescale) / i128::from(movie_timescale))
            .clamp(0, i128::from(i64::MAX)) as i64;
        composition_shift = composition_shift.saturating_sub(delay_media);
    }
    if info.kind == TrackKind::Audio && composition_shift > 0 && info.codec == Codec::Aac {
        // An AAC edit list's start offset is encoder priming.
        info.codec_delay = ticks_to_micros(composition_shift, u64::from(timescale));
    }
    let samples = if fragmented {
        Vec::new()
    } else {
        parse_sample_table(stbl)?
    };
    info.duration = media_duration.map(|duration| {
        ticks_to_micros(duration.min(i64::MAX as u64) as i64, u64::from(timescale))
    });
    Ok(Some(MovieTrack {
        info,
        timescale,
        composition_shift,
        samples,
        enabled,
    }))
}

/// ISO 639-2/T packed as three 5-bit letters (§8.4.2.3).
fn unpack_language(packed: u16) -> String {
    let letters: String = [10u16, 5, 0]
        .iter()
        .map(|shift| (((packed >> shift) & 0x1F) as u8 + 0x60) as char)
        .collect();
    if letters == "und" || !letters.chars().all(|c| c.is_ascii_lowercase()) {
        String::new()
    } else {
        letters
    }
}

fn parse_sample_table(stbl: &[u8]) -> Result<Vec<Sample>> {
    let mut stts: &[u8] = &[];
    let mut ctts: Option<&[u8]> = None;
    let mut stsc: &[u8] = &[];
    let mut stsz: Option<(bool, &[u8])> = None;
    let mut chunk_offsets: Option<(bool, &[u8])> = None;
    let mut stss: Option<&[u8]> = None;
    for child in Children::new(stbl) {
        let (kind, body, _) = child?;
        match &kind {
            b"stts" => stts = body,
            b"ctts" => ctts = Some(body),
            b"stsc" => stsc = body,
            b"stsz" => stsz = Some((false, body)),
            b"stz2" => stsz = Some((true, body)),
            b"stco" => chunk_offsets = Some((false, body)),
            b"co64" => chunk_offsets = Some((true, body)),
            b"stss" => stss = Some(body),
            _ => {}
        }
    }
    let (Some(stsz), Some(chunk_offsets)) = (stsz, chunk_offsets) else {
        return Err(Error::Malformed("sample table without sizes or offsets"));
    };
    // Sizes.
    let sizes: Vec<u32> = {
        let (compact, body) = stsz;
        let mut r = Reader::new(body);
        r.full_box("stsz")?;
        if compact {
            r.skip(3, "stz2")?;
            let field = r.u8("stz2")?;
            let count = r.u32("stz2")? as usize;
            if count > MAX_SAMPLES {
                return Err(Error::Unsupported("sample count"));
            }
            let mut sizes = Vec::with_capacity(count.min(r.remaining() * 2 + 1));
            match field {
                4 => {
                    for index in 0..count {
                        if index % 2 == 0 {
                            let byte = r.u8("stz2")?;
                            sizes.push(u32::from(byte >> 4));
                            if index + 1 < count {
                                sizes.push(u32::from(byte & 0xF));
                            }
                        }
                    }
                }
                8 => {
                    for _ in 0..count {
                        sizes.push(u32::from(r.u8("stz2")?));
                    }
                }
                16 => {
                    for _ in 0..count {
                        sizes.push(u32::from(r.u16("stz2")?));
                    }
                }
                _ => return Err(Error::Malformed("stz2 field size")),
            }
            sizes.truncate(count);
            sizes
        } else {
            let constant = r.u32("stsz")?;
            let count = r.u32("stsz")? as usize;
            if count > MAX_SAMPLES {
                return Err(Error::Unsupported("sample count"));
            }
            if constant != 0 {
                vec![constant; count]
            } else {
                if count > r.remaining() / 4 {
                    return Err(Error::Truncated("stsz"));
                }
                (0..count).map(|_| r.u32("stsz")).collect::<Result<_>>()?
            }
        }
    };
    let count = sizes.len();
    if count == 0 {
        return Ok(Vec::new());
    }
    // Chunk offsets.
    let offsets: Vec<u64> = {
        let (large, body) = chunk_offsets;
        let mut r = Reader::new(body);
        r.full_box("chunk offsets")?;
        let chunks = r.u32("chunk offsets")? as usize;
        let entry = if large { 8 } else { 4 };
        if chunks > r.remaining() / entry {
            return Err(Error::Truncated("chunk offsets"));
        }
        (0..chunks)
            .map(|_| {
                if large {
                    r.u64("co64")
                } else {
                    r.u32("stco").map(u64::from)
                }
            })
            .collect::<Result<_>>()?
    };
    // Sample-to-chunk runs: (first_chunk (1-based), samples_per_chunk).
    let mut runs = Vec::new();
    {
        let mut r = Reader::new(stsc);
        r.full_box("stsc")?;
        let entries = r.u32("stsc")? as usize;
        if entries > r.remaining() / 12 {
            return Err(Error::Truncated("stsc"));
        }
        for _ in 0..entries {
            let first_chunk = r.u32("stsc")?;
            let per_chunk = r.u32("stsc")?;
            r.skip(4, "stsc")?;
            if first_chunk == 0 || runs.last().is_some_and(|&(prev, _)| first_chunk <= prev) {
                return Err(Error::Malformed("stsc chunk order"));
            }
            runs.push((first_chunk, per_chunk));
        }
    }
    let mut samples = Vec::with_capacity(count);
    let mut sample = 0usize;
    'chunks: for (run_index, &(first_chunk, per_chunk)) in runs.iter().enumerate() {
        let last_chunk = runs
            .get(run_index + 1)
            .map_or(offsets.len() as u64, |&(next, _)| u64::from(next) - 1);
        for chunk in u64::from(first_chunk)..=last_chunk {
            let Some(&chunk_offset) = offsets.get(chunk as usize - 1) else {
                break 'chunks;
            };
            let mut offset = chunk_offset;
            for _ in 0..per_chunk {
                if sample >= count {
                    break 'chunks;
                }
                let size = sizes[sample];
                samples.push(Sample {
                    offset,
                    size,
                    dts: 0,
                    cts_offset: 0,
                    duration: 0,
                    sync: stss.is_none(),
                });
                offset = offset.saturating_add(u64::from(size));
                sample += 1;
            }
        }
    }
    if samples.len() != count {
        return Err(Error::Malformed(
            "sample table covers fewer samples than stsz",
        ));
    }
    // Decode times.
    {
        let mut r = Reader::new(stts);
        r.full_box("stts")?;
        let entries = r.u32("stts")? as usize;
        if entries > r.remaining() / 8 {
            return Err(Error::Truncated("stts"));
        }
        let mut dts = 0i64;
        let mut index = 0usize;
        for _ in 0..entries {
            let run = r.u32("stts")? as usize;
            let delta = r.u32("stts")?;
            for _ in 0..run {
                if index >= count {
                    break;
                }
                samples[index].dts = dts;
                samples[index].duration = delta;
                dts = dts.saturating_add(i64::from(delta));
                index += 1;
            }
        }
        // Samples beyond the table repeat the last duration.
        let last = samples[..index].last().map_or(0, |s| s.duration);
        for sample in &mut samples[index..] {
            sample.dts = dts;
            sample.duration = last;
            dts = dts.saturating_add(i64::from(last));
        }
    }
    if let Some(ctts) = ctts {
        let mut r = Reader::new(ctts);
        let (version, _) = r.full_box("ctts")?;
        let entries = r.u32("ctts")? as usize;
        if entries > r.remaining() / 8 {
            return Err(Error::Truncated("ctts"));
        }
        let mut index = 0usize;
        for _ in 0..entries {
            let run = r.u32("ctts")? as usize;
            let raw = r.u32("ctts")?;
            // Version 0 offsets are unsigned, but writers commonly store
            // negative values there; reading them as signed is what every
            // browser does.
            let _ = version;
            let offset = raw as i32;
            for _ in 0..run {
                if index >= count {
                    break;
                }
                samples[index].cts_offset = offset;
                index += 1;
            }
        }
    }
    if let Some(stss) = stss {
        let mut r = Reader::new(stss);
        r.full_box("stss")?;
        let entries = r.u32("stss")? as usize;
        if entries > r.remaining() / 4 {
            return Err(Error::Truncated("stss"));
        }
        for _ in 0..entries {
            let number = r.u32("stss")? as usize;
            if number >= 1 && number <= count {
                samples[number - 1].sync = true;
            }
        }
    }
    Ok(samples)
}

/// Parse the sample description box; the first entry describes the track
/// (TRust does not switch sample descriptions within a track).
fn parse_stsd(payload: &[u8], track_id: u32, kind: TrackKind) -> Result<Option<TrackInfo>> {
    let mut r = Reader::new(payload);
    r.full_box("stsd")?;
    let entries = r.u32("stsd")?;
    if entries == 0 {
        return Ok(None);
    }
    let Some((entry_kind, entry, _)) = Children::new(r.rest()).next().transpose()? else {
        return Err(Error::Truncated("sample entry"));
    };
    match kind {
        TrackKind::Video => parse_visual_entry(entry_kind, entry, track_id),
        TrackKind::Audio => parse_audio_entry(entry_kind, entry, track_id),
        TrackKind::Text => Ok(None),
    }
}

/// Protection scheme information (`sinf`, ISO/IEC 23001-7 §8.1–8.2): the
/// original format and the track encryption defaults.
fn parse_sinf(children: &[u8]) -> Result<Option<(FourCc, TrackEncryption)>> {
    let Some(sinf) = find_child(children, b"sinf")? else {
        return Ok(None);
    };
    let mut original = None;
    let mut scheme = None;
    let mut tenc = None;
    for child in Children::new(sinf) {
        let (kind, body, _) = child?;
        match &kind {
            b"frma" => original = Some(Reader::new(body).take(4, "frma")?.try_into().expect("4")),
            b"schm" => {
                let mut r = Reader::new(body);
                r.full_box("schm")?;
                let kind: FourCc = r.take(4, "schm")?.try_into().expect("4");
                scheme = Some(match &kind {
                    b"cenc" => EncryptionScheme::Cenc,
                    b"cbcs" => EncryptionScheme::Cbcs,
                    _ => return Err(Error::Unsupported("protection scheme")),
                });
            }
            b"schi" => tenc = find_child(body, b"tenc")?,
            _ => {}
        }
    }
    let (Some(original), Some(tenc)) = (original, tenc) else {
        return Err(Error::Malformed("protection scheme information"));
    };
    let mut r = Reader::new(tenc);
    let (version, _) = r.full_box("tenc")?;
    r.skip(1, "tenc")?;
    let pattern = r.u8("tenc")?;
    let (crypt_byte_block, skip_byte_block) = if version > 0 {
        (pattern >> 4, pattern & 0xF)
    } else {
        (0, 0)
    };
    let default_is_protected = r.u8("tenc")? != 0;
    let per_sample_iv_size = r.u8("tenc")?;
    let default_kid: [u8; 16] = r.take(16, "tenc")?.try_into().expect("16");
    let mut constant_iv = None;
    if default_is_protected && per_sample_iv_size == 0 {
        let size = r.u8("tenc")? as usize;
        constant_iv = Some(r.take(size, "tenc constant IV")?.to_vec());
    }
    if !matches!(per_sample_iv_size, 0 | 8 | 16) {
        return Err(Error::Malformed("tenc IV size"));
    }
    Ok(Some((
        original,
        TrackEncryption {
            scheme: scheme.unwrap_or(EncryptionScheme::Cenc),
            default_kid,
            default_is_protected,
            per_sample_iv_size,
            constant_iv,
            crypt_byte_block,
            skip_byte_block,
        },
    )))
}

fn parse_visual_entry(mut kind: FourCc, entry: &[u8], track_id: u32) -> Result<Option<TrackInfo>> {
    let mut r = Reader::new(entry);
    // SampleEntry: reserved(6) data_reference_index(2); VisualSampleEntry:
    // pre_defined/reserved(16) width(2) height(2) resolutions(8)
    // reserved(4) frame_count(2) compressorname(32) depth(2) pre_defined(2).
    r.skip(6 + 2 + 16, "visual sample entry")?;
    let width = u32::from(r.u16("visual sample entry")?);
    let height = u32::from(r.u16("visual sample entry")?);
    r.skip(8 + 4 + 2 + 32 + 2 + 2, "visual sample entry")?;
    let children = r.rest();
    let mut encryption = None;
    if &kind == b"encv" {
        let Some((original, tenc)) = parse_sinf(children)? else {
            return Err(Error::Malformed("encrypted sample entry"));
        };
        kind = original;
        encryption = Some(tenc);
    }
    let mut video = VideoInfo {
        width,
        height,
        display_width: width,
        display_height: height,
        color: None,
    };
    let mut config: &[u8] = &[];
    let mut details = CodecDetails::default();
    let codec = match &kind {
        b"avc1" | b"avc3" => Codec::H264,
        b"hvc1" | b"hev1" => Codec::Hevc,
        b"vp09" => Codec::Vp9,
        b"vp08" => Codec::Vp8,
        b"av01" => Codec::Av1,
        _ => return Ok(None),
    };
    for child in Children::new(children) {
        let (child_kind, body, _) = child?;
        match &child_kind {
            b"avcC" if codec == Codec::H264 => {
                config = body;
                if body.len() < 4 {
                    return Err(Error::Truncated("avcC"));
                }
                details.profile = Some(body[1]);
                details.level = Some(body[3]);
                details.bit_depth = Some(8);
                details.chroma_shift = Some((1, 1));
                // §5.3.3.1.2: High profiles append chroma format and depth
                // after the parameter sets.
                if let Some((chroma, depth)) = avcc_extension(body) {
                    details.chroma_shift = Some(match chroma {
                        0 | 1 => (1, 1),
                        2 => (1, 0),
                        _ => (0, 0),
                    });
                    details.bit_depth = Some(depth);
                }
            }
            b"hvcC" if codec == Codec::Hevc => {
                config = body;
                if body.len() < 23 {
                    return Err(Error::Truncated("hvcC"));
                }
                details.profile = Some(body[1] & 0x1F);
                details.level = Some(body[12]);
                details.chroma_shift = Some(match body[16] & 3 {
                    0 | 1 => (1, 1),
                    2 => (1, 0),
                    _ => (0, 0),
                });
                details.bit_depth = Some((body[17] & 7) + 8);
            }
            b"vpcC" if codec == Codec::Vp9 || codec == Codec::Vp8 => {
                config = body;
                let mut r = Reader::new(body);
                let (version, _) = r.full_box("vpcC")?;
                if version != 1 {
                    return Err(Error::Unsupported("vpcC version"));
                }
                details.profile = Some(r.u8("vpcC")?);
                details.level = Some(r.u8("vpcC")?);
                let packed = r.u8("vpcC")?;
                details.bit_depth = Some(packed >> 4);
                details.chroma_shift = Some(match (packed >> 1) & 7 {
                    0 | 1 => (1, 1),
                    2 => (1, 0),
                    _ => (0, 0),
                });
                let primaries = r.u8("vpcC")?;
                let transfer = r.u8("vpcC")?;
                let matrix = r.u8("vpcC")?;
                video.color = Some(ColorInfo {
                    primaries,
                    transfer,
                    matrix,
                    full_range: packed & 1 != 0,
                });
            }
            // An init segment written before the first frame (FFmpeg's
            // empty_moov) can carry an empty av1C; the sequence header in
            // the bitstream then describes the stream.
            b"av1C" if codec == Codec::Av1 && body.len() >= 4 => {
                config = body;
                details.profile = Some(body[1] >> 5);
                details.level = Some(body[1] & 0x1F);
                let high_bitdepth = body[2] & 0x40 != 0;
                let twelve_bit = body[2] & 0x20 != 0;
                details.bit_depth = Some(match (high_bitdepth, twelve_bit) {
                    (true, true) => 12,
                    (true, false) => 10,
                    _ => 8,
                });
                details.chroma_shift = Some(((body[2] >> 3) & 1, (body[2] >> 2) & 1));
            }
            b"pasp" => {
                let mut r = Reader::new(body);
                let h = r.u32("pasp")?;
                let v = r.u32("pasp")?;
                if h > 0 && v > 0 && h != v {
                    let scaled = u64::from(width) * u64::from(h) / u64::from(v);
                    video.display_width = scaled.clamp(1, u64::from(u32::MAX)) as u32;
                }
            }
            b"colr" => {
                let mut r = Reader::new(body);
                let colour_type: FourCc = r.take(4, "colr")?.try_into().expect("4");
                if matches!(&colour_type, b"nclx" | b"nclc") {
                    let primaries = r.u16("colr")?.min(255) as u8;
                    let transfer = r.u16("colr")?.min(255) as u8;
                    let matrix = r.u16("colr")?.min(255) as u8;
                    let full_range =
                        &colour_type == b"nclx" && r.u8("colr").is_ok_and(|b| b & 0x80 != 0);
                    if video.color.is_none() || &colour_type == b"nclx" {
                        video.color = Some(ColorInfo {
                            primaries,
                            transfer,
                            matrix,
                            full_range,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    let mut info = TrackInfo::new(track_id, TrackKind::Video, codec);
    info.codec_string = match codec {
        Codec::H264 if config.len() >= 4 => format!(
            "{}.{:02X}{:02X}{:02X}",
            String::from_utf8_lossy(&kind),
            config[1],
            config[2],
            config[3]
        ),
        Codec::Hevc => hevc_codec_string(&kind, config),
        _ => codec::describe(codec, &details),
    };
    info.details = details;
    info.config = Arc::from(config);
    info.video = Some(video);
    info.encryption = encryption;
    Ok(Some(info))
}

/// avcC trailing fields for profiles 100, 110, 122 and 144 (and 244):
/// `chroma_format` and `bit_depth_luma_minus8`.
fn avcc_extension(avcc: &[u8]) -> Option<(u8, u8)> {
    let profile = *avcc.get(1)?;
    if !matches!(profile, 100 | 110 | 122 | 144 | 244) {
        return None;
    }
    let mut pos = 5;
    let sps_count = (*avcc.get(pos)? & 0x1F) as usize;
    pos += 1;
    for _ in 0..sps_count {
        let len = u16::from_be_bytes([*avcc.get(pos)?, *avcc.get(pos + 1)?]) as usize;
        pos += 2 + len;
    }
    let pps_count = *avcc.get(pos)? as usize;
    pos += 1;
    for _ in 0..pps_count {
        let len = u16::from_be_bytes([*avcc.get(pos)?, *avcc.get(pos + 1)?]) as usize;
        pos += 2 + len;
    }
    let chroma = *avcc.get(pos)? & 3;
    let depth = (*avcc.get(pos + 1)? & 7) + 8;
    Some((chroma, depth))
}

/// ISO/IEC 14496-15 Annex E.3 codec string from an hvcC record.
fn hevc_codec_string(kind: &FourCc, hvcc: &[u8]) -> String {
    let prefix = String::from_utf8_lossy(kind);
    if hvcc.len() < 13 {
        return format!("{prefix}.1.6.L93.B0");
    }
    let space = match hvcc[1] >> 6 {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let tier = if hvcc[1] & 0x20 != 0 { 'H' } else { 'L' };
    let profile = hvcc[1] & 0x1F;
    let flags = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]).reverse_bits();
    let mut string = format!("{prefix}.{space}{profile}.{flags:X}.{tier}{}", hvcc[12]);
    // Constraint bytes, omitting trailing zero bytes.
    let constraints = &hvcc[6..12];
    let used = constraints
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1);
    for byte in &constraints[..used.max(1)] {
        string.push_str(&format!(".{byte:X}"));
    }
    string
}

fn parse_audio_entry(mut kind: FourCc, entry: &[u8], track_id: u32) -> Result<Option<TrackInfo>> {
    let mut r = Reader::new(entry);
    r.skip(6 + 2, "audio sample entry")?;
    // QuickTime sound sample descriptions reuse the first reserved field
    // as a version (QTFF "Sound Sample Description"); ISO's is zero.
    let version = r.u16("audio sample entry")?;
    r.skip(6, "audio sample entry")?;
    let mut channels = r.u16("audio sample entry")?;
    let bits = r.u16("audio sample entry")?;
    r.skip(4, "audio sample entry")?;
    let mut sample_rate = r.u32("audio sample entry")? >> 16;
    match version {
        0 => {}
        1 => r.skip(16, "QuickTime sound description v1")?,
        2 => {
            // Version 2: sizeOfStructOnly(4) audioSampleRate(f64)
            // numAudioChannels(4) always7F000000(4) constBitsPerChannel(4)
            // formatSpecificFlags(4) constBytesPerAudioPacket(4)
            // constLPCMFramesPerAudioPacket(4).
            r.skip(4, "QuickTime sound description v2")?;
            let rate = f64::from_bits(r.u64("QuickTime sound description v2")?);
            if rate.is_finite() && rate > 0.0 && rate < 1e7 {
                sample_rate = rate as u32;
            }
            channels = r
                .u32("QuickTime sound description v2")?
                .min(u32::from(u16::MAX)) as u16;
            r.skip(20, "QuickTime sound description v2")?;
        }
        _ => return Err(Error::Unsupported("sound sample description version")),
    }
    let children = r.rest();
    let mut encryption = None;
    if &kind == b"enca" {
        let Some((original, tenc)) = parse_sinf(children)? else {
            return Err(Error::Malformed("encrypted sample entry"));
        };
        kind = original;
        encryption = Some(tenc);
    }
    let mut details = CodecDetails::default();
    let mut config: Vec<u8> = Vec::new();
    let mut codec_delay = 0;
    let codec = match &kind {
        b"mp4a" => {
            let Some(esds) = find_child(children, b"esds")? else {
                return Err(Error::Malformed("mp4a without esds"));
            };
            let (oti, specific) = parse_esds(esds)?;
            match oti {
                0x40 | 0x66 | 0x67 | 0x68 => {
                    let Some(specific) = specific else {
                        return Err(Error::Malformed("AAC without AudioSpecificConfig"));
                    };
                    let asc = parse_audio_specific_config(&specific)?;
                    details.audio_object_type = Some(asc.object_type);
                    if asc.sample_rate > 0 {
                        sample_rate = asc.sample_rate;
                    }
                    if asc.channels > 0 {
                        channels = asc.channels;
                    }
                    config = specific;
                    Codec::Aac
                }
                0x69 | 0x6B => Codec::Mp3,
                0xA5 => Codec::Ac3,
                0xA6 => Codec::Eac3,
                0xAD => Codec::Opus,
                _ => return Ok(None),
            }
        }
        b".mp3" => Codec::Mp3,
        b"Opus" => {
            let Some(dops) = find_child(children, b"dOps")? else {
                return Err(Error::Malformed("Opus without dOps"));
            };
            let (head, pre_skip) = opus_head_from_dops(dops)?;
            codec_delay = ticks_to_micros(i64::from(pre_skip), 48_000);
            channels = u16::from(head[9]);
            config = head;
            sample_rate = 48_000;
            Codec::Opus
        }
        b"fLaC" => {
            let Some(dfla) = find_child(children, b"dfLa")? else {
                return Err(Error::Malformed("fLaC without dfLa"));
            };
            let mut r = Reader::new(dfla);
            r.full_box("dfLa")?;
            // Metadata blocks as in a FLAC stream; keep them with the
            // stream marker so a FLAC decoder can read STREAMINFO.
            config = b"fLaC".to_vec();
            config.extend_from_slice(r.rest());
            Codec::Flac
        }
        b"ac-3" => Codec::Ac3,
        b"ec-3" => Codec::Eac3,
        b"alac" => {
            if let Some(alac) = find_child(children, b"alac")? {
                config = alac.to_vec();
            }
            Codec::Alac
        }
        b"lpcm" | b"sowt" | b"twos" | b"in24" | b"in32" | b"fl32" | b"fl64" | b"ipcm" | b"fpcm" => {
            Codec::Pcm
        }
        _ => return Ok(None),
    };
    let mut info = TrackInfo::new(track_id, TrackKind::Audio, codec);
    info.codec_string = codec::describe(codec, &details);
    info.details = details;
    info.config = Arc::from(config);
    info.audio = Some(AudioInfo {
        sample_rate,
        channels,
        bits_per_sample: bits,
    });
    info.codec_delay = codec_delay;
    if codec == Codec::Opus {
        info.seek_preroll = 80_000;
    }
    info.encryption = encryption;
    Ok(Some(info))
}

/// ES_Descriptor (ISO/IEC 14496-1 §7.2.6.5): the DecoderConfigDescriptor's
/// objectTypeIndication and its DecoderSpecificInfo.
fn parse_esds(esds: &[u8]) -> Result<(u8, Option<Vec<u8>>)> {
    let mut r = Reader::new(esds);
    r.full_box("esds")?;
    let mut bytes = r.rest();
    let mut oti = None;
    let mut specific = None;
    // Walk descriptors depth-first: ES_Descr(3) ⊃ DecoderConfigDescr(4) ⊃
    // DecSpecificInfo(5).
    while !bytes.is_empty() {
        let tag = bytes[0];
        let mut len = 0usize;
        let mut pos = 1;
        for _ in 0..4 {
            let Some(&byte) = bytes.get(pos) else {
                return Err(Error::Truncated("esds descriptor"));
            };
            pos += 1;
            len = (len << 7) | usize::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                break;
            }
        }
        let Some(body) = bytes.get(pos..pos + len) else {
            return Err(Error::Truncated("esds descriptor"));
        };
        match tag {
            3 => {
                let mut r = Reader::new(body);
                r.skip(2, "ES_Descriptor")?;
                let flags = r.u8("ES_Descriptor")?;
                if flags & 0x80 != 0 {
                    r.skip(2, "ES_Descriptor")?;
                }
                if flags & 0x40 != 0 {
                    let url_len = r.u8("ES_Descriptor")? as usize;
                    r.skip(url_len, "ES_Descriptor")?;
                }
                if flags & 0x20 != 0 {
                    r.skip(2, "ES_Descriptor")?;
                }
                bytes = r.rest();
                continue;
            }
            4 => {
                let mut r = Reader::new(body);
                oti = Some(r.u8("DecoderConfigDescriptor")?);
                r.skip(12, "DecoderConfigDescriptor")?;
                bytes = r.rest();
                continue;
            }
            5 => {
                specific = Some(body.to_vec());
                break;
            }
            _ => {}
        }
        bytes = &bytes[pos + len..];
    }
    let Some(oti) = oti else {
        return Err(Error::Malformed("esds without DecoderConfigDescriptor"));
    };
    Ok((oti, specific))
}

/// The parts of an AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1) needed to
/// describe the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    pub object_type: u8,
    pub sample_rate: u32,
    pub channels: u16,
}

const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

pub fn parse_audio_specific_config(bytes: &[u8]) -> Result<AudioSpecificConfig> {
    let mut bits = BitReader::new(bytes);
    let read_object_type = |bits: &mut BitReader| -> Result<u8> {
        let mut object_type = bits.read(5)? as u8;
        if object_type == 31 {
            object_type = 32 + bits.read(6)? as u8;
        }
        Ok(object_type)
    };
    let object_type = read_object_type(&mut bits)?;
    let rate_index = bits.read(4)?;
    let sample_rate = if rate_index == 15 {
        bits.read(24)?
    } else {
        AAC_SAMPLE_RATES
            .get(rate_index as usize)
            .copied()
            .unwrap_or(0)
    };
    let channel_config = bits.read(4)? as u16;
    let channels = match channel_config {
        1..=6 => channel_config,
        7 => 8,
        _ => 0,
    };
    Ok(AudioSpecificConfig {
        object_type,
        sample_rate,
        channels,
    })
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read(&mut self, count: u32) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = *self
                .bytes
                .get(self.bit / 8)
                .ok_or(Error::Truncated("AudioSpecificConfig"))?;
            let bit = (byte >> (7 - self.bit % 8)) & 1;
            value = (value << 1) | u32::from(bit);
            self.bit += 1;
        }
        Ok(value)
    }
}

/// Convert a `dOps` box (Opus in ISOBMFF §4.3.2, big-endian) to the
/// RFC 7845 §5.1 identification header (little-endian) that Opus decoders
/// take, and return its pre-skip.
fn opus_head_from_dops(dops: &[u8]) -> Result<(Vec<u8>, u16)> {
    let mut r = Reader::new(dops);
    let version = r.u8("dOps")?;
    if version != 0 {
        return Err(Error::Unsupported("dOps version"));
    }
    let channels = r.u8("dOps")?;
    let pre_skip = r.u16("dOps")?;
    let rate = r.u32("dOps")?;
    let gain = r.u16("dOps")?;
    let family = r.u8("dOps")?;
    let mut head = Vec::with_capacity(21 + usize::from(channels));
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&rate.to_le_bytes());
    head.extend_from_slice(&gain.to_le_bytes());
    head.push(family);
    if family != 0 {
        head.push(r.u8("dOps")?);
        head.push(r.u8("dOps")?);
        head.extend_from_slice(r.take(usize::from(channels), "dOps channel mapping")?);
    }
    if channels == 0 {
        return Err(Error::Malformed("dOps channel count"));
    }
    Ok((head, pre_skip))
}

/// A sample of a track fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentSample {
    /// Offset of the sample data from the start of the `moof` box.
    pub moof_offset: i64,
    pub size: u32,
    /// Decode time (media timescale).
    pub dts: i64,
    pub cts_offset: i64,
    pub duration: u32,
    pub sync: bool,
    pub encryption: Option<Box<SampleEncryption>>,
}

/// One track fragment (`traf`).
#[derive(Clone, Debug, Default)]
pub struct TrackFragment {
    pub track_id: u32,
    /// The fragment's `tfdt` baseMediaDecodeTime; without one, decode times
    /// continue from the track's previous fragment and `samples` start at 0.
    pub decode_time: Option<i64>,
    pub samples: Vec<FragmentSample>,
}

/// A parsed movie fragment (`moof`).
#[derive(Clone, Debug, Default)]
pub struct Fragment {
    pub sequence_number: u32,
    pub tracks: Vec<TrackFragment>,
    pub pssh: Vec<Pssh>,
}

/// PIFF 1.1's sample encryption box, a `uuid` box with this user type.
const PIFF_SAMPLE_ENCRYPTION: [u8; 16] = [
    0xa2, 0x39, 0x4f, 0x52, 0x5a, 0x9b, 0x4f, 0x14, 0xa2, 0x44, 0x6c, 0x42, 0x7c, 0x64, 0x8d, 0xf4,
];

/// Parse a movie fragment box payload. Data offsets are relative to the
/// first byte of the `moof` box itself; `base_data_offset` (§8.8.7.1) is
/// accepted only when it equals the moof's own position (`moof_position`),
/// which progressive files sometimes state explicitly.
pub fn parse_moof(payload: &[u8], moof_position: u64, movie: &Movie) -> Result<Fragment> {
    let mut fragment = Fragment::default();
    // The implicit base for the second and later track fragments is the end
    // of the preceding fragment's data (§8.8.7.1).
    let mut implicit_base: i64 = 0;
    for child in Children::new(payload) {
        let (kind, body, _) = child?;
        match &kind {
            b"mfhd" => {
                let mut r = Reader::new(body);
                r.full_box("mfhd")?;
                fragment.sequence_number = r.u32("mfhd")?;
            }
            b"traf" => {
                let traf = parse_traf(body, moof_position, movie, &mut implicit_base)?;
                if let Some(traf) = traf {
                    fragment.tracks.push(traf);
                }
            }
            b"pssh" => fragment.pssh.push(parse_pssh(body)?),
            _ => {}
        }
    }
    Ok(fragment)
}

fn parse_traf(
    payload: &[u8],
    moof_position: u64,
    movie: &Movie,
    implicit_base: &mut i64,
) -> Result<Option<TrackFragment>> {
    let Some(tfhd) = find_child(payload, b"tfhd")? else {
        return Err(Error::Malformed("traf without tfhd"));
    };
    let mut r = Reader::new(tfhd);
    let (_, flags) = r.full_box("tfhd")?;
    let track_id = r.u32("tfhd")?;
    let Some(track) = movie.track(track_id) else {
        // A track TRust skipped (text, unknown codec).
        return Ok(None);
    };
    let trex = movie.trex(track_id);
    let mut base: i64 = if flags & 0x1 != 0 {
        let absolute = r.u64("tfhd")?;
        absolute as i64 - moof_position as i64
    } else if flags & 0x20000 != 0 {
        0
    } else {
        *implicit_base
    };
    if flags & 0x2 != 0 {
        r.u32("tfhd")?;
    }
    let default_duration = if flags & 0x8 != 0 {
        r.u32("tfhd")?
    } else {
        trex.sample_duration
    };
    let default_size = if flags & 0x10 != 0 {
        r.u32("tfhd")?
    } else {
        trex.sample_size
    };
    let default_flags = if flags & 0x20 != 0 {
        r.u32("tfhd")?
    } else {
        trex.sample_flags
    };
    let mut decode_time = None;
    let mut samples = Vec::new();
    let mut sample_encryption: Option<&[u8]> = None;
    let mut piff = false;
    for child in Children::new(payload) {
        let (kind, body, user_type) = child?;
        match &kind {
            b"tfdt" => {
                let mut r = Reader::new(body);
                let (version, _) = r.full_box("tfdt")?;
                decode_time = Some(if version == 1 {
                    r.u64("tfdt")? as i64
                } else {
                    i64::from(r.u32("tfdt")?)
                });
            }
            b"trun" => {
                let mut r = Reader::new(body);
                let (version, run_flags) = r.full_box("trun")?;
                let count = r.u32("trun")? as usize;
                if count > MAX_FRAGMENT_SAMPLES {
                    return Err(Error::Unsupported("track run length"));
                }
                if run_flags & 0x1 != 0 {
                    base = i64::from(r.i32("trun")?);
                }
                let first_flags = if run_flags & 0x4 != 0 {
                    Some(r.u32("trun")?)
                } else {
                    None
                };
                let per_sample = [0x100, 0x200, 0x400, 0x800]
                    .iter()
                    .filter(|&&bit| run_flags & bit != 0)
                    .count()
                    * 4;
                if per_sample > 0 && count > r.remaining() / per_sample {
                    return Err(Error::Truncated("trun"));
                }
                let mut offset = base;
                for index in 0..count {
                    let duration = if run_flags & 0x100 != 0 {
                        r.u32("trun")?
                    } else {
                        default_duration
                    };
                    let size = if run_flags & 0x200 != 0 {
                        r.u32("trun")?
                    } else {
                        default_size
                    };
                    let sample_flags = if run_flags & 0x400 != 0 {
                        r.u32("trun")?
                    } else if index == 0 {
                        first_flags.unwrap_or(default_flags)
                    } else {
                        default_flags
                    };
                    let cts_offset = if run_flags & 0x800 != 0 {
                        let raw = r.u32("trun")?;
                        if version == 0 {
                            // Unsigned in version 0, but negative values
                            // appear in the wild; read as signed like
                            // browsers do.
                            i64::from(raw as i32)
                        } else {
                            i64::from(raw as i32)
                        }
                    } else {
                        0
                    };
                    // §8.8.3.1: sample_is_non_sync_sample is bit 16;
                    // sample_depends_on (bits 24–25) == 2 marks an I-frame.
                    let sync = (sample_flags >> 16) & 1 == 0;
                    samples.push(FragmentSample {
                        moof_offset: offset,
                        size,
                        dts: 0,
                        cts_offset,
                        duration,
                        sync,
                        encryption: None,
                    });
                    offset = offset.saturating_add(i64::from(size));
                }
                base = offset;
            }
            b"senc" => sample_encryption = Some(body),
            b"uuid" if user_type == Some(PIFF_SAMPLE_ENCRYPTION) => {
                sample_encryption = Some(body);
                piff = true;
            }
            _ => {}
        }
    }
    *implicit_base = base;
    // Decode times: tfdt, else continue from the track's previous fragment
    // (the caller rebases those; start at zero here).
    let mut dts = decode_time.unwrap_or(0);
    for sample in &mut samples {
        sample.dts = dts;
        dts = dts.saturating_add(i64::from(sample.duration));
    }
    if let Some(encryption) = &track.info.encryption
        && let Some(senc) = sample_encryption
    {
        apply_sample_encryption(senc, piff, encryption, &mut samples)?;
    } else if let Some(encryption) = &track.info.encryption
        && encryption.default_is_protected
        && encryption.per_sample_iv_size == 0
    {
        // Constant IV, no per-sample information: every sample is
        // protected in full with the default key.
        for sample in &mut samples {
            sample.encryption = Some(Box::new(SampleEncryption {
                scheme: encryption.scheme,
                kid: encryption.default_kid,
                iv: encryption.constant_iv.clone().unwrap_or_default(),
                subsamples: Vec::new(),
                crypt_byte_block: encryption.crypt_byte_block,
                skip_byte_block: encryption.skip_byte_block,
            }));
        }
    }
    Ok(Some(TrackFragment {
        track_id,
        decode_time,
        samples,
    }))
}

/// Sample Encryption box (ISO/IEC 23001-7 §7.2).
fn apply_sample_encryption(
    senc: &[u8],
    piff: bool,
    encryption: &TrackEncryption,
    samples: &mut [FragmentSample],
) -> Result<()> {
    let mut r = Reader::new(senc);
    let (_, flags) = r.full_box("senc")?;
    let mut iv_size = usize::from(encryption.per_sample_iv_size);
    if piff && flags & 0x1 != 0 {
        // PIFF override: AlgorithmID(3) IV_size(1) KID(16).
        r.skip(3, "PIFF senc")?;
        iv_size = usize::from(r.u8("PIFF senc")?);
        r.skip(16, "PIFF senc")?;
    }
    let count = r.u32("senc")? as usize;
    if count != samples.len() {
        return Err(Error::Malformed("senc sample count"));
    }
    if !encryption.default_is_protected {
        return Ok(());
    }
    for sample in samples.iter_mut() {
        let iv = if iv_size == 0 {
            encryption.constant_iv.clone().unwrap_or_default()
        } else {
            r.take(iv_size, "senc IV")?.to_vec()
        };
        let mut subsamples = Vec::new();
        if flags & 0x2 != 0 {
            let entries = r.u16("senc")? as usize;
            if entries > r.remaining() / 6 {
                return Err(Error::Truncated("senc subsamples"));
            }
            for _ in 0..entries {
                subsamples.push(Subsample {
                    clear: u32::from(r.u16("senc")?),
                    protected: r.u32("senc")?,
                });
            }
            let total: u64 = subsamples
                .iter()
                .map(|s| u64::from(s.clear) + u64::from(s.protected))
                .sum();
            if total != u64::from(sample.size) {
                return Err(Error::Malformed("subsample sizes"));
            }
        }
        sample.encryption = Some(Box::new(SampleEncryption {
            scheme: encryption.scheme,
            kid: encryption.default_kid,
            iv,
            subsamples,
            crypt_byte_block: encryption.crypt_byte_block,
            skip_byte_block: encryption.skip_byte_block,
        }));
    }
    Ok(())
}

/// One reference of a segment index (`sidx`, §8.16.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentReference {
    /// File offset of the referenced subsegment.
    pub offset: u64,
    pub size: u32,
    /// Earliest presentation time, in the index's timescale.
    pub time: u64,
    pub duration: u32,
    /// The reference points to another `sidx`.
    pub index: bool,
}

/// Parse a segment index whose box ends at `end_offset` in the file.
pub fn parse_sidx(payload: &[u8], end_offset: u64) -> Result<(u32, Vec<SegmentReference>)> {
    let mut r = Reader::new(payload);
    let (version, _) = r.full_box("sidx")?;
    r.u32("sidx")?;
    let timescale = r.u32("sidx")?;
    let (earliest, first_offset) = if version == 0 {
        (u64::from(r.u32("sidx")?), u64::from(r.u32("sidx")?))
    } else {
        (r.u64("sidx")?, r.u64("sidx")?)
    };
    r.skip(2, "sidx")?;
    let count = r.u16("sidx")? as usize;
    let mut offset = end_offset.saturating_add(first_offset);
    let mut time = earliest;
    let mut references = Vec::with_capacity(count);
    for _ in 0..count {
        let word = r.u32("sidx")?;
        let duration = r.u32("sidx")?;
        r.u32("sidx")?;
        let size = word & 0x7FFF_FFFF;
        references.push(SegmentReference {
            offset,
            size,
            time,
            duration,
            index: word & 0x8000_0000 != 0,
        });
        offset = offset.saturating_add(u64::from(size));
        time = time.saturating_add(u64::from(duration));
    }
    Ok((timescale, references))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![
            version,
            (flags >> 16) as u8,
            (flags >> 8) as u8,
            flags as u8,
        ];
        body.extend_from_slice(payload);
        boxed(kind, &body)
    }

    #[test]
    fn box_headers() {
        assert_eq!(BoxHeader::parse(&[0, 0, 0]).unwrap(), None);
        let header = BoxHeader::parse(b"\0\0\0\x10moov").unwrap().unwrap();
        assert_eq!(&header.kind, b"moov");
        assert_eq!(header.size, Some(16));
        let large = BoxHeader::parse(b"\0\0\0\x01mdat\0\0\0\0\0\0\0\x20")
            .unwrap()
            .unwrap();
        assert_eq!(large.header_len, 16);
        assert_eq!(large.size, Some(32));
        assert!(BoxHeader::parse(b"\0\0\0\x04free").is_err());
        let open = BoxHeader::parse(b"\0\0\0\0mdat").unwrap().unwrap();
        assert_eq!(open.size, None);
    }

    #[test]
    fn audio_specific_config() {
        // AAC LC, 44.1 kHz, stereo.
        let asc = parse_audio_specific_config(&[0x12, 0x10]).unwrap();
        assert_eq!(asc.object_type, 2);
        assert_eq!(asc.sample_rate, 44100);
        assert_eq!(asc.channels, 2);
        // HE-AAC v1 explicit signalling (AOT 5), 24 kHz core.
        let he = parse_audio_specific_config(&[0x2B, 0x10, 0x88]).unwrap();
        assert_eq!(he.object_type, 5);
    }

    #[test]
    fn language_unpacking() {
        // "eng" = e(5) n(14) g(7).
        let packed = (5u16 << 10) | (14 << 5) | 7;
        assert_eq!(unpack_language(packed), "eng");
        let und = (21u16 << 10) | (14 << 5) | 4;
        assert_eq!(unpack_language(und), "");
    }

    #[test]
    fn opus_head_conversion() {
        let mut dops = vec![0, 2];
        dops.extend_from_slice(&312u16.to_be_bytes());
        dops.extend_from_slice(&48000u32.to_be_bytes());
        dops.extend_from_slice(&0u16.to_be_bytes());
        dops.push(0);
        let (head, skip) = opus_head_from_dops(&dops).unwrap();
        assert_eq!(skip, 312);
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], 2);
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), 312);
        assert_eq!(head.len(), 19);
    }

    fn sample_movie() -> Vec<u8> {
        let mut mvhd = vec![0u8; 8];
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&2000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 80]);
        let mvhd = full(b"mvhd", 0, 0, &mvhd);
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&1u32.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 4 + 4 + 52]);
        tkhd.extend_from_slice(&(64u32 << 16).to_be_bytes());
        tkhd.extend_from_slice(&(48u32 << 16).to_be_bytes());
        let tkhd = full(b"tkhd", 0, 1, &tkhd);
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&90_000u32.to_be_bytes());
        mdhd.extend_from_slice(&180_000u32.to_be_bytes());
        mdhd.extend_from_slice(&((5u16 << 10) | (14 << 5) | 7).to_be_bytes());
        mdhd.extend_from_slice(&[0, 0]);
        let mdhd = full(b"mdhd", 0, 0, &mdhd);
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"VideoHandler\0");
        let hdlr = full(b"hdlr", 0, 0, &hdlr);
        // av01 sample entry with av1C (profile 0, level 4, 8-bit 4:2:0).
        let mut entry = vec![0u8; 6];
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0u8; 16]);
        entry.extend_from_slice(&64u16.to_be_bytes());
        entry.extend_from_slice(&48u16.to_be_bytes());
        entry.extend_from_slice(&[0u8; 8 + 4 + 2 + 32 + 2 + 2]);
        entry.extend(boxed(b"av1C", &[0x81, 0x04, 0x0C, 0x00]));
        let av01 = boxed(b"av01", &entry);
        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend(av01);
        let stsd = full(b"stsd", 0, 0, &stsd);
        // Three samples: sizes 10, 20, 30 in two chunks (2 + 1).
        let mut stts = 1u32.to_be_bytes().to_vec();
        stts.extend_from_slice(&3u32.to_be_bytes());
        stts.extend_from_slice(&3000u32.to_be_bytes());
        let stts = full(b"stts", 0, 0, &stts);
        let mut stsc = 2u32.to_be_bytes().to_vec();
        for (first, per) in [(1u32, 2u32), (2, 1)] {
            stsc.extend_from_slice(&first.to_be_bytes());
            stsc.extend_from_slice(&per.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
        }
        let stsc = full(b"stsc", 0, 0, &stsc);
        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend_from_slice(&3u32.to_be_bytes());
        for size in [10u32, 20, 30] {
            stsz.extend_from_slice(&size.to_be_bytes());
        }
        let stsz = full(b"stsz", 0, 0, &stsz);
        let mut stco = 2u32.to_be_bytes().to_vec();
        stco.extend_from_slice(&1000u32.to_be_bytes());
        stco.extend_from_slice(&5000u32.to_be_bytes());
        let stco = full(b"stco", 0, 0, &stco);
        let mut stss = 1u32.to_be_bytes().to_vec();
        stss.extend_from_slice(&1u32.to_be_bytes());
        let stss = full(b"stss", 0, 0, &stss);
        let stbl = boxed(b"stbl", &[stsd, stts, stsc, stsz, stco, stss].concat());
        let minf = boxed(b"minf", &stbl);
        let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
        // Edit list: start at media time 3000 (one frame).
        let mut elst = 1u32.to_be_bytes().to_vec();
        elst.extend_from_slice(&2000u32.to_be_bytes());
        elst.extend_from_slice(&3000i32.to_be_bytes());
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let edts = boxed(b"edts", &full(b"elst", 0, 0, &elst));
        let trak = boxed(b"trak", &[tkhd, edts, mdia].concat());
        [mvhd, trak].concat()
    }

    #[test]
    fn movie_with_sample_tables() {
        let movie = parse_moov(&sample_movie()).unwrap();
        assert_eq!(movie.duration, Some(2_000_000));
        assert!(!movie.fragmented);
        let track = &movie.tracks[0];
        assert_eq!(track.info.codec, Codec::Av1);
        assert_eq!(track.info.codec_string, "av01.0.04M.08");
        assert_eq!(track.info.language, "eng");
        assert_eq!(track.info.label, "");
        assert_eq!(track.timescale, 90_000);
        assert_eq!(track.info.duration, Some(2_000_000));
        assert_eq!(track.samples.len(), 3);
        assert_eq!(track.samples[0].offset, 1000);
        assert_eq!(track.samples[1].offset, 1010);
        assert_eq!(track.samples[2].offset, 5000);
        assert!(track.samples[0].sync && !track.samples[1].sync);
        assert_eq!(track.samples[2].dts, 6000);
        // The edit list's media_time shifts presentation back one frame.
        assert_eq!(track.presentation_micros(3000, 0), 0);
        assert_eq!(track.presentation_micros(6000, 0), 33_333);
    }

    #[test]
    fn fragment_runs_and_defaults() {
        let mut movie = parse_moov(&sample_movie()).unwrap();
        movie.fragmented = true;
        movie.track_extends.push(TrackExtends {
            track_id: 1,
            sample_description_index: 1,
            sample_duration: 1500,
            sample_size: 0,
            sample_flags: 0x0001_0000,
        });
        let mut tfhd = 1u32.to_be_bytes().to_vec();
        tfhd.extend_from_slice(&[]);
        let tfhd = full(b"tfhd", 0, 0x20000, &tfhd);
        let tfdt = full(b"tfdt", 1, 0, &90_000u64.to_be_bytes());
        // trun: data offset, first-sample-flags, per-sample sizes.
        let mut trun = 2u32.to_be_bytes().to_vec();
        trun.extend_from_slice(&100i32.to_be_bytes());
        trun.extend_from_slice(&0x0200_0000u32.to_be_bytes());
        trun.extend_from_slice(&7u32.to_be_bytes());
        trun.extend_from_slice(&9u32.to_be_bytes());
        let trun = full(b"trun", 0, 0x1 | 0x4 | 0x200, &trun);
        let traf = boxed(b"traf", &[tfhd, tfdt, trun].concat());
        let mfhd = full(b"mfhd", 0, 0, &5u32.to_be_bytes());
        let fragment = parse_moof(&[mfhd, traf].concat(), 0, &movie).unwrap();
        assert_eq!(fragment.sequence_number, 5);
        let samples = &fragment.tracks[0].samples;
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].moof_offset, 100);
        assert_eq!(samples[1].moof_offset, 107);
        assert_eq!(samples[0].dts, 90_000);
        assert_eq!(samples[1].dts, 91_500);
        assert!(samples[0].sync);
        assert!(!samples[1].sync);
    }

    #[test]
    fn hevc_codec_strings() {
        // Main profile, compatibility flags 0x60000000 (bits 1 and 2),
        // level 93, constraint byte 0xB0.
        let mut hvcc = vec![1, 0x01, 0x60, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 93];
        hvcc.resize(23, 0);
        assert_eq!(hevc_codec_string(b"hvc1", &hvcc), "hvc1.1.6.L93.B0");
    }
}
