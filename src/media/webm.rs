//! Matroska/WebM parsing (IETF CELLAR Matroska, RFC 9559; EBML, RFC 8794),
//! for progressive playback and the MSE WebM byte stream format (local
//! w3c/mse-byte-stream-format-webm snapshot 83fa1f5d).
//!
//! [`Parser`] is incremental: bytes are pushed in arbitrary pieces and it
//! emits events as soon as each element is complete. Master elements that
//! carry media (Segment, Cluster) are entered without buffering their
//! contents, including the unknown-size elements live streams use (RFC 9559
//! §6.2: an unknown-size element ends where an element that cannot be its
//! child begins). Metadata (Info, Tracks, Cues) is buffered and parsed whole;
//! large elements TRust does not use (Tags, Attachments) are skipped without
//! buffering.
//!
//! Encrypted tracks follow the WebM Encryption specification (signal byte,
//! 8-byte IV, optional partitions), mapped to Common Encryption `cenc`.

use std::fmt;
use std::sync::Arc;

use super::Micros;
use super::codec::{self, Codec, CodecDetails};
use super::track::{
    AudioInfo, ColorInfo, EncryptionScheme, SampleEncryption, Subsample, TrackEncryption,
    TrackInfo, TrackKind, VideoInfo,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Malformed(&'static str),
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed WebM {what}"),
            Self::Unsupported(what) => write!(f, "unsupported WebM {what}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

// Element IDs (RFC 9559 §5, RFC 8794 §11.2), with their length markers.
const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const FLAG_ENABLED: u32 = 0xB9;
const FLAG_DEFAULT: u32 = 0x88;
const DEFAULT_DURATION: u32 = 0x23_E383;
const NAME: u32 = 0x536E;
const LANGUAGE: u32 = 0x22_B59C;
const LANGUAGE_BCP47: u32 = 0x22_B59D;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CODEC_DELAY: u32 = 0x56AA;
const SEEK_PRE_ROLL: u32 = 0x56BB;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const DISPLAY_WIDTH: u32 = 0x54B0;
const DISPLAY_HEIGHT: u32 = 0x54BA;
const DISPLAY_UNIT: u32 = 0x54B2;
const COLOUR: u32 = 0x55B0;
const MATRIX_COEFFICIENTS: u32 = 0x55B1;
const BITS_PER_CHANNEL: u32 = 0x55B2;
const RANGE: u32 = 0x55B9;
const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const PRIMARIES: u32 = 0x55BB;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CONTENT_ENCODING: u32 = 0x6240;
const CONTENT_ENCODING_TYPE: u32 = 0x5033;
const CONTENT_ENCRYPTION: u32 = 0x5035;
const CONTENT_ENC_ALGO: u32 = 0x47E1;
const CONTENT_ENC_KEY_ID: u32 = 0x47E2;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const REFERENCE_BLOCK: u32 = 0xFB;
const DISCARD_PADDING: u32 = 0x75A2;

/// Elements that may appear as direct children of a Segment: they end an
/// unknown-size Cluster.
fn is_segment_child(id: u32) -> bool {
    matches!(
        id,
        SEEK_HEAD | INFO | TRACKS | CUES | CLUSTER | 0x1254_C367 | 0x1043_A770 | 0x1941_A469
    )
}

/// The largest metadata element buffered whole (Info, Tracks, Cues,
/// BlockGroup); larger ones are refused.
const MAX_BUFFERED_ELEMENT: u64 = 64 << 20;

/// Read an EBML variable-size integer (RFC 8794 §4). Returns the value
/// (with the length marker removed unless `keep_marker`), its length, and
/// whether all value bits were one (the reserved "unknown" size).
fn read_vint(bytes: &[u8], keep_marker: bool) -> Result<Option<(u64, usize, bool)>> {
    let Some(&first) = bytes.first() else {
        return Ok(None);
    };
    if first == 0 {
        return Err(Error::Malformed("variable-size integer"));
    }
    let len = first.leading_zeros() as usize + 1;
    if bytes.len() < len {
        return Ok(None);
    }
    // The value bits of the first byte (none for an 8-byte integer).
    let mask = (0xFFu16 >> len) as u8;
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first & mask)
    };
    let mut all_ones = (first & mask) == mask;
    for &byte in &bytes[1..len] {
        value = (value << 8) | u64::from(byte);
        all_ones &= byte == 0xFF;
    }
    Ok(Some((value, len, all_ones)))
}

/// Parse an element header: (id, data size or None for unknown, header
/// length). `Ok(None)` when more bytes are needed.
fn read_header(bytes: &[u8]) -> Result<Option<(u32, Option<u64>, usize)>> {
    let Some((id, id_len, _)) = read_vint(bytes, true)? else {
        return Ok(None);
    };
    if id_len > 4 {
        return Err(Error::Malformed("element ID"));
    }
    let Some((size, size_len, unknown)) = read_vint(&bytes[id_len..], false)? else {
        return Ok(None);
    };
    Ok(Some((
        id as u32,
        (!unknown).then_some(size),
        id_len + size_len,
    )))
}

fn read_uint(data: &[u8]) -> Result<u64> {
    if data.len() > 8 {
        return Err(Error::Malformed("unsigned integer"));
    }
    Ok(data.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
}

fn read_float(data: &[u8]) -> Result<f64> {
    match data.len() {
        0 => Ok(0.0),
        4 => Ok(f64::from(f32::from_be_bytes(data.try_into().expect("4")))),
        8 => Ok(f64::from_be_bytes(data.try_into().expect("8"))),
        _ => Err(Error::Malformed("float")),
    }
}

fn read_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Iterate the complete child elements of a buffered master element.
fn children(mut data: &[u8]) -> impl Iterator<Item = Result<(u32, &[u8])>> {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let header = match read_header(data) {
            Ok(Some(header)) => header,
            Ok(None) => {
                data = &[];
                return Some(Err(Error::Malformed("truncated child element")));
            }
            Err(error) => {
                data = &[];
                return Some(Err(error));
            }
        };
        let (id, size, header_len) = header;
        let Some(size) = size else {
            data = &[];
            return Some(Err(Error::Malformed("unknown-size child element")));
        };
        let end = header_len as u64 + size;
        if end > data.len() as u64 {
            data = &[];
            return Some(Err(Error::Malformed("child element overruns its parent")));
        }
        let body = &data[header_len..end as usize];
        data = &data[end as usize..];
        Some(Ok((id, body)))
    })
}

/// Segment information (Info, Tracks).
#[derive(Clone, Debug)]
pub struct Header {
    /// Nanoseconds per timestamp tick (TimestampScale).
    pub timestamp_scale: u64,
    pub duration: Option<Micros>,
    pub tracks: Vec<TrackInfo>,
    /// DefaultDuration per track number, in microseconds.
    pub default_durations: Vec<(u32, Micros)>,
    /// EME "webm" init data: each encrypted track's ContentEncKeyID.
    pub key_ids: Vec<Vec<u8>>,
}

/// A cue: the cluster holding a keyframe of `track` at `time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CuePoint {
    pub time: Micros,
    pub track: u32,
    /// Absolute byte offset of the Cluster element.
    pub cluster_offset: u64,
}

/// One coded frame of a Block (after lacing).
#[derive(Clone, Debug)]
pub struct Frame {
    pub track: u32,
    pub pts: Micros,
    /// From BlockDuration, else the track's DefaultDuration; `None` when the
    /// stream leaves it implicit (the next frame's timestamp).
    pub duration: Option<Micros>,
    pub keyframe: bool,
    pub data: Arc<[u8]>,
    pub discard_padding: Micros,
    pub encryption: Option<Box<SampleEncryption>>,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// The EBML header declared this DocType ("webm" or "matroska").
    DocType(String),
    /// A Segment starts; `data_offset` is the absolute offset of its data,
    /// the origin for SeekHead and Cue positions.
    SegmentStart {
        data_offset: u64,
    },
    /// Info and Tracks are complete: the initialization segment.
    Header(Box<Header>),
    /// Cue points; `cluster_offset`s are relative to the Segment's data.
    Cues(Vec<CuePoint>),
    /// SeekHead entries: (element ID, position relative to the Segment's
    /// data), e.g. where Cues stored at the end of a file begin.
    SeekHead(Vec<(u32, u64)>),
    /// A Cluster starts at absolute `offset` with timestamp `time`.
    ClusterStart {
        offset: u64,
        time: Micros,
    },
    Frame(Frame),
    /// A Cluster ended (its size was reached, or another element began).
    ClusterEnd,
}

#[derive(Clone, Copy, Debug)]
struct Open {
    id: u32,
    /// Absolute end offset; `None` for unknown size.
    end: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct TrackState {
    number: u32,
    kind: Option<TrackKind>,
    default_duration: Option<Micros>,
    encryption_key: Option<Vec<u8>>,
}

/// Incremental Matroska/WebM parser.
pub struct Parser {
    buffer: Vec<u8>,
    /// Absolute stream offset of `buffer[0]`.
    base: u64,
    open: Vec<Open>,
    /// Bytes of an element being skipped without buffering.
    skipping: u64,
    timestamp_scale: u64,
    cluster_time: Option<i64>,
    tracks: Vec<TrackState>,
    info: Option<(u64, Option<f64>)>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            base: 0,
            open: Vec::new(),
            skipping: 0,
            timestamp_scale: 1_000_000,
            cluster_time: None,
            tracks: Vec::new(),
            info: None,
        }
    }

    /// Absolute offset of the next byte the parser expects.
    pub fn position(&self) -> u64 {
        self.base + self.buffer.len() as u64 + self.skipping
    }

    /// Resume parsing at a Cluster boundary at absolute `offset`, inside a
    /// Segment whose data ends at `segment_end` (for progressive seeking).
    /// Track and timestamp-scale state is kept.
    pub fn resume_at(&mut self, offset: u64, segment_end: Option<u64>) {
        self.buffer.clear();
        self.base = offset;
        self.skipping = 0;
        self.cluster_time = None;
        self.open.clear();
        self.open.push(Open {
            id: SEGMENT,
            end: segment_end,
        });
    }

    /// Forget all parse state except the parsed tracks and timestamp scale
    /// (MSE "reset parser state": the next bytes start a new segment or
    /// cluster).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.base = 0;
        self.skipping = 0;
        self.cluster_time = None;
        self.open.clear();
    }

    /// Whether a Cluster is open (MSE: the parser is inside a media segment).
    pub fn in_cluster(&self) -> bool {
        self.open.iter().any(|open| open.id == CLUSTER)
    }

    /// Push bytes and collect the events they complete.
    pub fn push(&mut self, mut bytes: &[u8], events: &mut Vec<Event>) -> Result<()> {
        if self.skipping > 0 {
            let skip = (bytes.len() as u64).min(self.skipping) as usize;
            self.skipping -= skip as u64;
            self.base += skip as u64;
            bytes = &bytes[skip..];
            if self.skipping == 0 {
                self.close_finished(events);
            }
        }
        self.buffer.extend_from_slice(bytes);
        self.parse(events)
    }

    fn close_finished(&mut self, events: &mut Vec<Event>) {
        let position = self.base;
        while let Some(open) = self.open.last() {
            if open.end.is_some_and(|end| position >= end) {
                if open.id == CLUSTER {
                    events.push(Event::ClusterEnd);
                    self.cluster_time = None;
                }
                self.open.pop();
            } else {
                break;
            }
        }
    }

    fn parse(&mut self, events: &mut Vec<Event>) -> Result<()> {
        let mut consumed = 0usize;
        let result = loop {
            if self.skipping > 0 {
                break Ok(());
            }
            let position = self.base + consumed as u64;
            // Close sized masters that ended here.
            while let Some(open) = self.open.last() {
                if open.end.is_some_and(|end| position >= end) {
                    if open.id == CLUSTER {
                        events.push(Event::ClusterEnd);
                        self.cluster_time = None;
                    }
                    self.open.pop();
                } else {
                    break;
                }
            }
            let rest = &self.buffer[consumed..];
            let header = match read_header(rest) {
                Ok(Some(header)) => header,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
            let (id, size, header_len) = header;
            // An unknown-size Cluster ends where a Segment-level element
            // begins (RFC 9559 §6.2).
            if is_segment_child(id)
                && let Some(open) = self.open.last()
                && open.id == CLUSTER
                && open.end.is_none()
            {
                events.push(Event::ClusterEnd);
                self.cluster_time = None;
                self.open.pop();
            }
            match id {
                EBML => {
                    let Some(size) = size else {
                        break Err(Error::Malformed("EBML header size"));
                    };
                    let total = header_len as u64 + size;
                    if (rest.len() as u64) < total {
                        break Ok(());
                    }
                    let body = &rest[header_len..total as usize];
                    for child in children(body) {
                        match child {
                            Ok((DOC_TYPE, data)) => events.push(Event::DocType(read_string(data))),
                            Ok(_) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    // A new EBML header starts a new stream: forget an old
                    // segment (MSE appends after a reset may do this).
                    self.open.clear();
                    consumed += total as usize;
                }
                SEGMENT => {
                    let data_offset = position + header_len as u64;
                    self.open.clear();
                    self.open.push(Open {
                        id: SEGMENT,
                        end: size.map(|size| data_offset.saturating_add(size)),
                    });
                    events.push(Event::SegmentStart { data_offset });
                    consumed += header_len;
                }
                CLUSTER => {
                    if !self.open.iter().any(|open| open.id == SEGMENT) {
                        // MSE media segments may arrive without the Segment
                        // header once initialised; treat as inside one.
                        self.open.push(Open {
                            id: SEGMENT,
                            end: None,
                        });
                    }
                    let data_offset = position + header_len as u64;
                    self.open.push(Open {
                        id: CLUSTER,
                        end: size.map(|size| data_offset.saturating_add(size)),
                    });
                    self.cluster_time = None;
                    consumed += header_len;
                    // ClusterStart waits for the Timestamp element.
                    events.push(Event::ClusterStart {
                        offset: position,
                        time: Micros::MIN,
                    });
                }
                TIMESTAMP if self.in_cluster() => {
                    let Some(size) = size else {
                        break Err(Error::Malformed("cluster timestamp"));
                    };
                    let total = header_len as u64 + size;
                    if (rest.len() as u64) < total {
                        break Ok(());
                    }
                    let value = match read_uint(&rest[header_len..total as usize]) {
                        Ok(value) => value,
                        Err(error) => break Err(error),
                    };
                    let value = value.min(i64::MAX as u64) as i64;
                    self.cluster_time = Some(value);
                    let time = self.ticks(value);
                    if let Some(Event::ClusterStart { time: pending, .. }) = events
                        .iter_mut()
                        .rev()
                        .find(|event| matches!(event, Event::ClusterStart { .. }))
                        && *pending == Micros::MIN
                    {
                        *pending = time;
                    }
                    consumed += total as usize;
                }
                SIMPLE_BLOCK | BLOCK_GROUP if self.in_cluster() => {
                    let Some(size) = size else {
                        break Err(Error::Malformed("block size"));
                    };
                    if size > MAX_BUFFERED_ELEMENT {
                        break Err(Error::Unsupported("block larger than 64 MiB"));
                    }
                    let total = header_len as u64 + size;
                    if (rest.len() as u64) < total {
                        break Ok(());
                    }
                    let body = rest[header_len..total as usize].to_vec();
                    consumed += total as usize;
                    let result = if id == SIMPLE_BLOCK {
                        self.block(&body, true, None, None, 0, events)
                    } else {
                        self.block_group(&body, events)
                    };
                    if let Err(error) = result {
                        break Err(error);
                    }
                }
                SEEK_HEAD | INFO | TRACKS | CUES => {
                    let Some(size) = size else {
                        break Err(Error::Malformed("metadata element size"));
                    };
                    if size > MAX_BUFFERED_ELEMENT {
                        break Err(Error::Unsupported("metadata element larger than 64 MiB"));
                    }
                    let total = header_len as u64 + size;
                    if (rest.len() as u64) < total {
                        break Ok(());
                    }
                    let body = rest[header_len..total as usize].to_vec();
                    consumed += total as usize;
                    let result = match id {
                        SEEK_HEAD => parse_seek_head(&body)
                            .map(|entries| events.push(Event::SeekHead(entries))),
                        INFO => self.parse_info(&body),
                        TRACKS => self.parse_tracks(&body, events),
                        _ => parse_cues(&body, self.timestamp_scale)
                            .map(|cues| events.push(Event::Cues(cues))),
                    };
                    if let Err(error) = result {
                        break Err(error);
                    }
                }
                _ => {
                    // Anything else is skipped: Tags, Chapters, Attachments,
                    // Void, CRC-32, cluster Position/PrevSize, and unknown
                    // elements.
                    let Some(size) = size else {
                        break Err(Error::Malformed("unknown-size element"));
                    };
                    let total = header_len as u64 + size;
                    let available = rest.len() as u64;
                    if available >= total {
                        consumed += total as usize;
                    } else {
                        consumed += rest.len();
                        self.skipping = total - available;
                    }
                }
            }
        };
        self.buffer.drain(..consumed);
        self.base += consumed as u64;
        result
    }

    fn ticks(&self, ticks: i64) -> Micros {
        scaled(ticks, self.timestamp_scale)
    }

    pub fn timestamp_scale(&self) -> u64 {
        self.timestamp_scale
    }

    /// The absolute end of the open Segment, when its size is known.
    pub fn segment_end(&self) -> Option<u64> {
        self.open
            .iter()
            .find(|open| open.id == SEGMENT)
            .and_then(|open| open.end)
    }

    fn parse_info(&mut self, body: &[u8]) -> Result<()> {
        let mut scale = 1_000_000u64;
        let mut duration = None;
        for child in children(body) {
            let (id, data) = child?;
            match id {
                TIMESTAMP_SCALE => scale = read_uint(data)?,
                DURATION => duration = Some(read_float(data)?),
                _ => {}
            }
        }
        if scale == 0 {
            return Err(Error::Malformed("TimestampScale"));
        }
        self.timestamp_scale = scale;
        self.info = Some((scale, duration));
        Ok(())
    }

    fn parse_tracks(&mut self, body: &[u8], events: &mut Vec<Event>) -> Result<()> {
        let mut tracks = Vec::new();
        let mut states = Vec::new();
        let mut default_durations = Vec::new();
        let mut key_ids = Vec::new();
        for child in children(body) {
            let (id, data) = child?;
            if id != TRACK_ENTRY {
                continue;
            }
            let (state, info) = parse_track_entry(data)?;
            if let Some(duration) = state.default_duration {
                default_durations.push((state.number, duration));
            }
            if let Some(key) = &state.encryption_key {
                key_ids.push(key.clone());
            }
            if states.iter().any(|s: &TrackState| s.number == state.number) {
                return Err(Error::Malformed("duplicate TrackNumber"));
            }
            states.push(state);
            if let Some(info) = info {
                tracks.push(info);
            }
        }
        self.tracks = states;
        let (scale, duration) = self.info.unwrap_or((self.timestamp_scale, None));
        let duration = duration
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * scale as f64 / 1000.0).round() as Micros);
        events.push(Event::Header(Box::new(Header {
            timestamp_scale: scale,
            duration,
            tracks,
            default_durations,
            key_ids,
        })));
        Ok(())
    }

    fn block_group(&mut self, body: &[u8], events: &mut Vec<Event>) -> Result<()> {
        let mut block = None;
        let mut duration = None;
        let mut reference = false;
        let mut discard = 0i64;
        for child in children(body) {
            let (id, data) = child?;
            match id {
                BLOCK => block = Some(data),
                BLOCK_DURATION => duration = Some(read_uint(data)?),
                REFERENCE_BLOCK => reference = true,
                DISCARD_PADDING => {
                    // Signed nanoseconds.
                    let raw = read_uint(data)?;
                    let bits = data.len() * 8;
                    discard = if bits > 0 && bits < 64 && raw >> (bits - 1) & 1 == 1 {
                        (raw as i64) - (1i64 << bits)
                    } else {
                        raw as i64
                    };
                }
                _ => {}
            }
        }
        let Some(block) = block else {
            return Err(Error::Malformed("BlockGroup without Block"));
        };
        let duration = duration.map(|d| d.min(i64::MAX as u64) as i64);
        self.block(
            block,
            false,
            Some(!reference),
            duration,
            discard / 1000,
            events,
        )
    }

    /// Parse a (Simple)Block (RFC 9559 §10) and emit its laced frames.
    fn block(
        &mut self,
        body: &[u8],
        simple: bool,
        group_keyframe: Option<bool>,
        group_duration: Option<i64>,
        discard_padding: Micros,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        let Some((number, len, _)) = read_vint(body, false)? else {
            return Err(Error::Malformed("block track number"));
        };
        if body.len() < len + 3 {
            return Err(Error::Malformed("block header"));
        }
        let number = number as u32;
        let relative = i16::from_be_bytes([body[len], body[len + 1]]);
        let flags = body[len + 2];
        let payload = &body[len + 3..];
        let Some(cluster_time) = self.cluster_time else {
            return Err(Error::Malformed("block before cluster timestamp"));
        };
        let Some(track) = self.tracks.iter().find(|t| t.number == number).cloned() else {
            // Blocks of tracks the header did not declare are ignored.
            return Ok(());
        };
        if track.kind.is_none() {
            return Ok(());
        }
        let keyframe = if simple {
            flags & 0x80 != 0
        } else {
            group_keyframe.unwrap_or(false)
        };
        let frames = unlace(payload, (flags >> 1) & 3)?;
        let pts = self.ticks(cluster_time.saturating_add(i64::from(relative)));
        let duration = group_duration
            .map(|d| self.ticks(d))
            .or(track.default_duration.filter(|_| frames.len() == 1));
        let laced = frames.len() > 1;
        for (index, data) in frames.into_iter().enumerate() {
            // Laced frames share the block timestamp; spread them by the
            // track's DefaultDuration when known.
            let offset = if laced {
                track.default_duration.unwrap_or(0) * index as i64
            } else {
                0
            };
            let (data, encryption) = match &track.encryption_key {
                Some(key) => decrypt_framing(data, key)?,
                None => (Arc::from(data), None),
            };
            events.push(Event::Frame(Frame {
                track: number,
                pts: pts.saturating_add(offset),
                duration: if laced {
                    track.default_duration
                } else {
                    duration
                },
                keyframe,
                data,
                discard_padding,
                encryption,
            }));
        }
        Ok(())
    }
}

/// Convert timestamp ticks to microseconds under a TimestampScale.
fn scaled(ticks: i64, timestamp_scale: u64) -> Micros {
    let nanos = i128::from(ticks) * i128::from(timestamp_scale);
    nanos
        .div_euclid(1000)
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// SeekHead (RFC 9559 §5.1.1): Seek entries of (SeekID, SeekPosition).
fn parse_seek_head(body: &[u8]) -> Result<Vec<(u32, u64)>> {
    const SEEK: u32 = 0x4DBB;
    const SEEK_ID: u32 = 0x53AB;
    const SEEK_POSITION: u32 = 0x53AC;
    let mut entries = Vec::new();
    for child in children(body) {
        let (id, data) = child?;
        if id != SEEK {
            continue;
        }
        let mut seek_id = None;
        let mut position = None;
        for child in children(data) {
            let (id, data) = child?;
            match id {
                SEEK_ID => seek_id = Some(read_uint(data)? as u32),
                SEEK_POSITION => position = Some(read_uint(data)?),
                _ => {}
            }
        }
        if let (Some(id), Some(position)) = (seek_id, position) {
            entries.push((id, position));
        }
    }
    Ok(entries)
}

/// The Cues element's ID, for locating it through a SeekHead.
pub const CUES_ID: u32 = CUES;

/// Cues (RFC 9559 §5.1.5) under `timestamp_scale`.
pub fn parse_cues(body: &[u8], timestamp_scale: u64) -> Result<Vec<CuePoint>> {
    let mut cues = Vec::new();
    for child in children(body) {
        let (id, data) = child?;
        if id != CUE_POINT {
            continue;
        }
        let mut time = None;
        let mut positions = Vec::new();
        for child in children(data) {
            let (id, data) = child?;
            match id {
                CUE_TIME => time = Some(read_uint(data)?),
                CUE_TRACK_POSITIONS => {
                    let mut track = None;
                    let mut cluster = None;
                    for child in children(data) {
                        let (id, data) = child?;
                        match id {
                            CUE_TRACK => track = Some(read_uint(data)? as u32),
                            CUE_CLUSTER_POSITION => cluster = Some(read_uint(data)?),
                            _ => {}
                        }
                    }
                    if let (Some(track), Some(cluster)) = (track, cluster) {
                        positions.push((track, cluster));
                    }
                }
                _ => {}
            }
        }
        if let Some(time) = time {
            let time = scaled(time.min(i64::MAX as u64) as i64, timestamp_scale);
            for (track, cluster) in positions {
                cues.push(CuePoint {
                    time,
                    track,
                    cluster_offset: cluster,
                });
            }
        }
    }
    Ok(cues)
}

/// Parse a complete Cues element (header included) read from a file.
pub fn parse_cues_element(bytes: &[u8], timestamp_scale: u64) -> Result<Vec<CuePoint>> {
    let Some((id, Some(size), header_len)) = read_header(bytes)? else {
        return Err(Error::Malformed("Cues element header"));
    };
    if id != CUES {
        return Err(Error::Malformed("SeekHead does not point at Cues"));
    }
    let end = header_len as u64 + size;
    if end > bytes.len() as u64 {
        return Err(Error::Malformed("truncated Cues"));
    }
    parse_cues(&bytes[header_len..end as usize], timestamp_scale)
}

/// The size of the element starting at `bytes`, header included, when its
/// header is complete and its size known.
pub fn element_size(bytes: &[u8]) -> Option<u64> {
    let (_, size, header_len) = read_header(bytes).ok()??;
    size.map(|size| header_len as u64 + size)
}

/// Split a block payload by its lacing mode (RFC 9559 §10.3).
fn unlace(payload: &[u8], lacing: u8) -> Result<Vec<&[u8]>> {
    if lacing == 0 {
        return Ok(vec![payload]);
    }
    let Some(&count_minus_one) = payload.first() else {
        return Err(Error::Malformed("lace count"));
    };
    let count = usize::from(count_minus_one) + 1;
    let mut pos = 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        // Xiph lacing.
        1 => {
            for _ in 0..count - 1 {
                let mut size = 0usize;
                loop {
                    let Some(&byte) = payload.get(pos) else {
                        return Err(Error::Malformed("Xiph lace size"));
                    };
                    pos += 1;
                    size += usize::from(byte);
                    if byte != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        // Fixed-size lacing.
        2 => {
            let rest = payload.len() - pos;
            if !rest.is_multiple_of(count) {
                return Err(Error::Malformed("fixed lace size"));
            }
            sizes.extend(std::iter::repeat_n(rest / count, count - 1));
        }
        // EBML lacing: the first size as a vint, then signed differences.
        3 => {
            let Some((first, len, _)) = read_vint(&payload[pos..], false)? else {
                return Err(Error::Malformed("EBML lace size"));
            };
            pos += len;
            let mut size = first as i64;
            sizes.push(first as usize);
            for _ in 1..count - 1 {
                let Some((raw, len, _)) = read_vint(&payload[pos..], false)? else {
                    return Err(Error::Malformed("EBML lace size"));
                };
                pos += len;
                let bias = (1i64 << (7 * len - 1)) - 1;
                size += raw as i64 - bias;
                if size < 0 {
                    return Err(Error::Malformed("EBML lace size"));
                }
                sizes.push(size as usize);
            }
        }
        _ => unreachable!("two-bit lacing field"),
    }
    let used: usize = sizes.iter().sum();
    if pos + used > payload.len() {
        return Err(Error::Malformed("lace sizes overrun the block"));
    }
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        frames.push(&payload[pos..pos + size]);
        pos += size;
    }
    frames.push(&payload[pos..]);
    Ok(frames)
}

/// A frame's data and its encryption metadata, if encrypted.
type Framed = (Arc<[u8]>, Option<Box<SampleEncryption>>);

/// WebM Encryption: split the signal byte, IV and partitions from a frame.
fn decrypt_framing(data: &[u8], key: &[u8]) -> Result<Framed> {
    let Some(&signal) = data.first() else {
        return Err(Error::Malformed("encrypted frame signal byte"));
    };
    if signal & 0x01 == 0 {
        return Ok((Arc::from(&data[1..]), None));
    }
    if data.len() < 9 {
        return Err(Error::Malformed("encrypted frame IV"));
    }
    let mut iv = data[1..9].to_vec();
    iv.resize(16, 0);
    let mut pos = 9;
    let mut subsamples = Vec::new();
    let body_len;
    if signal & 0x02 != 0 {
        let Some(&count) = data.get(pos) else {
            return Err(Error::Malformed("partition count"));
        };
        pos += 1;
        let mut offsets = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let Some(bytes) = data.get(pos..pos + 4) else {
                return Err(Error::Malformed("partition offset"));
            };
            offsets.push(u32::from_be_bytes(bytes.try_into().expect("4")));
            pos += 4;
        }
        body_len = (data.len() - pos) as u32;
        let mut previous = 0u32;
        let mut boundaries = offsets.clone();
        boundaries.push(body_len);
        let mut clear = true;
        let mut pending_clear = 0u32;
        for boundary in boundaries {
            if boundary < previous || boundary > body_len {
                return Err(Error::Malformed("partition offsets"));
            }
            let span = boundary - previous;
            if clear {
                pending_clear = span;
            } else {
                subsamples.push(Subsample {
                    clear: pending_clear,
                    protected: span,
                });
                pending_clear = 0;
            }
            clear = !clear;
            previous = boundary;
        }
        if pending_clear > 0 || subsamples.is_empty() {
            subsamples.push(Subsample {
                clear: pending_clear,
                protected: 0,
            });
        }
    } else {
        body_len = (data.len() - pos) as u32;
        let _ = body_len;
    }
    let mut kid = [0u8; 16];
    let len = key.len().min(16);
    kid[..len].copy_from_slice(&key[..len]);
    Ok((
        Arc::from(&data[pos..]),
        Some(Box::new(SampleEncryption {
            scheme: EncryptionScheme::Cenc,
            kid,
            iv,
            subsamples,
            crypt_byte_block: 0,
            skip_byte_block: 0,
        })),
    ))
}

fn parse_track_entry(data: &[u8]) -> Result<(TrackState, Option<TrackInfo>)> {
    let mut number = None;
    let mut track_type = 0u64;
    let mut enabled = true;
    let mut default = true;
    let mut default_duration = None;
    let mut name = String::new();
    let mut language = String::new();
    let mut codec_id = String::new();
    let mut codec_private: &[u8] = &[];
    let mut codec_delay = 0u64;
    let mut seek_preroll = 0u64;
    let mut video = None;
    let mut audio = None;
    let mut encryption_key = None;
    for child in children(data) {
        let (id, data) = child?;
        match id {
            TRACK_NUMBER => number = Some(read_uint(data)?),
            TRACK_TYPE => track_type = read_uint(data)?,
            FLAG_ENABLED => enabled = read_uint(data)? != 0,
            FLAG_DEFAULT => default = read_uint(data)? != 0,
            DEFAULT_DURATION => default_duration = Some(read_uint(data)?),
            NAME => name = read_string(data),
            LANGUAGE if language.is_empty() => language = read_string(data),
            LANGUAGE_BCP47 => language = read_string(data),
            CODEC_ID => codec_id = read_string(data),
            CODEC_PRIVATE => codec_private = data,
            CODEC_DELAY => codec_delay = read_uint(data)?,
            SEEK_PRE_ROLL => seek_preroll = read_uint(data)?,
            VIDEO => video = Some(parse_video(data)?),
            AUDIO => audio = Some(parse_audio(data)?),
            CONTENT_ENCODINGS => encryption_key = parse_content_encodings(data)?,
            _ => {}
        }
    }
    let Some(number) = number.filter(|&n| n > 0 && n <= u64::from(u32::MAX)) else {
        return Err(Error::Malformed("TrackNumber"));
    };
    let number = number as u32;
    let default_duration = default_duration.map(|ns| (ns / 1000).min(i64::MAX as u64) as i64);
    let kind = match track_type {
        1 => Some(TrackKind::Video),
        2 => Some(TrackKind::Audio),
        _ => None,
    };
    let codec = match codec_id.as_str() {
        "V_VP8" => Some(Codec::Vp8),
        "V_VP9" => Some(Codec::Vp9),
        "V_AV1" => Some(Codec::Av1),
        "V_MPEG4/ISO/AVC" => Some(Codec::H264),
        "V_MPEGH/ISO/HEVC" => Some(Codec::Hevc),
        "V_THEORA" => Some(Codec::Theora),
        "A_OPUS" => Some(Codec::Opus),
        "A_VORBIS" => Some(Codec::Vorbis),
        "A_FLAC" => Some(Codec::Flac),
        "A_MPEG/L3" => Some(Codec::Mp3),
        "A_AC3" => Some(Codec::Ac3),
        "A_EAC3" => Some(Codec::Eac3),
        "A_PCM/INT/LIT" | "A_PCM/FLOAT/IEEE" => Some(Codec::Pcm),
        id if id.starts_with("A_AAC") => Some(Codec::Aac),
        _ => None,
    };
    let state = TrackState {
        number,
        kind: kind.filter(|_| codec.is_some()),
        default_duration,
        encryption_key: encryption_key.clone(),
    };
    let (Some(kind), Some(codec)) = (kind, codec) else {
        return Ok((state, None));
    };
    if codec.is_video() != (kind == TrackKind::Video) {
        return Ok((
            TrackState {
                kind: None,
                ..state
            },
            None,
        ));
    }
    let mut info = TrackInfo::new(number, kind, codec);
    let mut details = CodecDetails::default();
    let mut config: Vec<u8> = codec_private.to_vec();
    let mut audio_info = audio.map(|(rate, channels, bits)| AudioInfo {
        sample_rate: rate,
        channels,
        bits_per_sample: bits,
    });
    match codec {
        Codec::Vp9 => {
            details.profile = Some(0);
            details.bit_depth = Some(8);
            // CodecPrivate may carry VP9 codec feature metadata (ID 1
            // profile, 2 level, 3 bit depth, 4 chroma subsampling).
            let mut pos = 0;
            while pos + 2 <= codec_private.len() {
                let (feature, len) = (codec_private[pos], usize::from(codec_private[pos + 1]));
                let Some(value) = codec_private.get(pos + 2..pos + 2 + len) else {
                    break;
                };
                if len == 1 {
                    match feature {
                        1 => details.profile = Some(value[0]),
                        2 => details.level = Some(value[0]),
                        3 => details.bit_depth = Some(value[0]),
                        _ => {}
                    }
                }
                pos += 2 + len;
            }
        }
        Codec::Av1 if codec_private.len() >= 4 => {
            details.profile = Some(codec_private[1] >> 5);
            details.level = Some(codec_private[1] & 0x1F);
            let high = codec_private[2] & 0x40 != 0;
            let twelve = codec_private[2] & 0x20 != 0;
            details.bit_depth = Some(match (high, twelve) {
                (true, true) => 12,
                (true, false) => 10,
                _ => 8,
            });
        }
        Codec::H264 if codec_private.len() >= 4 => {
            details.profile = Some(codec_private[1]);
            details.level = Some(codec_private[3]);
        }
        Codec::Opus => {
            if codec_private.len() >= 19 && &codec_private[..8] == b"OpusHead" {
                let pre_skip = u16::from_le_bytes([codec_private[10], codec_private[11]]);
                if codec_delay == 0 {
                    codec_delay = u64::from(pre_skip) * 1_000_000_000 / 48_000;
                }
                if let Some(audio) = audio_info.as_mut() {
                    audio.channels = u16::from(codec_private[9]);
                    audio.sample_rate = 48_000;
                }
            } else {
                return Err(Error::Malformed("Opus CodecPrivate"));
            }
            if seek_preroll == 0 {
                seek_preroll = 80_000_000;
            }
        }
        Codec::Aac => {
            if let Ok(asc) = super::isobmff::parse_audio_specific_config(codec_private) {
                details.audio_object_type = Some(asc.object_type);
            } else {
                details.audio_object_type = Some(2);
                config.clear();
            }
        }
        _ => {}
    }
    info.codec_string = codec::describe(codec, &details);
    info.details = details;
    info.config = Arc::from(config);
    info.video = video;
    info.audio = audio_info;
    info.codec_delay = (codec_delay / 1000).min(i64::MAX as u64) as i64;
    info.seek_preroll = (seek_preroll / 1000).min(i64::MAX as u64) as i64;
    info.language = if language == "und" {
        String::new()
    } else {
        language
    };
    info.label = name;
    info.default = default && enabled;
    if let Some(key) = &encryption_key {
        let mut kid = [0u8; 16];
        let len = key.len().min(16);
        kid[..len].copy_from_slice(&key[..len]);
        info.encryption = Some(TrackEncryption {
            scheme: EncryptionScheme::Cenc,
            default_kid: kid,
            default_is_protected: true,
            per_sample_iv_size: 8,
            constant_iv: None,
            crypt_byte_block: 0,
            skip_byte_block: 0,
        });
    }
    Ok((state, Some(info)))
}

fn parse_video(data: &[u8]) -> Result<VideoInfo> {
    let mut video = VideoInfo::default();
    let mut display = (None, None);
    let mut display_unit = 0;
    for child in children(data) {
        let (id, data) = child?;
        match id {
            PIXEL_WIDTH => video.width = read_uint(data)?.min(u64::from(u32::MAX)) as u32,
            PIXEL_HEIGHT => video.height = read_uint(data)?.min(u64::from(u32::MAX)) as u32,
            DISPLAY_WIDTH => display.0 = Some(read_uint(data)?.min(u64::from(u32::MAX)) as u32),
            DISPLAY_HEIGHT => display.1 = Some(read_uint(data)?.min(u64::from(u32::MAX)) as u32),
            DISPLAY_UNIT => display_unit = read_uint(data)?,
            COLOUR => {
                let mut primaries = 2u8;
                let mut transfer = 2u8;
                let mut matrix = 2u8;
                let mut full_range = false;
                for child in children(data) {
                    let (id, data) = child?;
                    let value = read_uint(data)?.min(255) as u8;
                    match id {
                        MATRIX_COEFFICIENTS => matrix = value,
                        PRIMARIES => primaries = value,
                        TRANSFER_CHARACTERISTICS => transfer = value,
                        // Range: 1 broadcast, 2 full.
                        RANGE => full_range = value == 2,
                        BITS_PER_CHANNEL => {}
                        _ => {}
                    }
                }
                video.color = Some(ColorInfo {
                    primaries,
                    transfer,
                    matrix,
                    full_range,
                });
            }
            _ => {}
        }
    }
    video.display_width = video.width;
    video.display_height = video.height;
    // DisplayUnit 0 is pixels; other units (cm, inches, aspect ratio) give
    // only a ratio.
    if let (Some(w), Some(h)) = display
        && w > 0
        && h > 0
        && video.height > 0
    {
        if display_unit == 0 {
            video.display_width = w;
            video.display_height = h;
        } else {
            video.display_width = (u64::from(video.height) * u64::from(w) / u64::from(h)) as u32;
        }
    }
    Ok(video)
}

fn parse_audio(data: &[u8]) -> Result<(u32, u16, u16)> {
    let mut rate = 8000.0;
    let mut output_rate = None;
    let mut channels = 1;
    let mut bits = 0;
    for child in children(data) {
        let (id, data) = child?;
        match id {
            SAMPLING_FREQUENCY => rate = read_float(data)?,
            OUTPUT_SAMPLING_FREQUENCY => output_rate = Some(read_float(data)?),
            CHANNELS => channels = read_uint(data)?.min(255) as u16,
            BIT_DEPTH => bits = read_uint(data)?.min(64) as u16,
            _ => {}
        }
    }
    let rate = output_rate.unwrap_or(rate);
    let rate = if rate.is_finite() && rate > 0.0 && rate < 1e7 {
        rate as u32
    } else {
        0
    };
    Ok((rate, channels, bits))
}

/// ContentEncodings: TRust supports exactly one content encoding, WebM
/// encryption (type 1, AES); compression (type 0) is refused.
fn parse_content_encodings(data: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut key = None;
    for child in children(data) {
        let (id, data) = child?;
        if id != CONTENT_ENCODING {
            continue;
        }
        let mut kind = 0;
        let mut encryption = None;
        for child in children(data) {
            let (id, data) = child?;
            match id {
                CONTENT_ENCODING_TYPE => kind = read_uint(data)?,
                CONTENT_ENCRYPTION => encryption = Some(data),
                _ => {}
            }
        }
        if kind != 1 {
            return Err(Error::Unsupported("content compression"));
        }
        let Some(encryption) = encryption else {
            return Err(Error::Malformed("ContentEncryption"));
        };
        let mut algorithm = 0;
        for child in children(encryption) {
            let (id, data) = child?;
            match id {
                CONTENT_ENC_ALGO => algorithm = read_uint(data)?,
                CONTENT_ENC_KEY_ID => key = Some(data.to_vec()),
                _ => {}
            }
        }
        if algorithm != 5 {
            return Err(Error::Unsupported("content encryption algorithm"));
        }
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let id_bytes = id.to_be_bytes();
        let skip = id_bytes.iter().position(|&b| b != 0).unwrap_or(3);
        out.extend_from_slice(&id_bytes[skip..]);
        // 8-byte size vint.
        let size = body.len() as u64 | (1 << 56);
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn uint(id: u32, value: u64) -> Vec<u8> {
        element(id, &value.to_be_bytes())
    }

    fn unknown_size(id: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let id_bytes = id.to_be_bytes();
        let skip = id_bytes.iter().position(|&b| b != 0).unwrap_or(3);
        out.extend_from_slice(&id_bytes[skip..]);
        out.push(0xFF);
        out
    }

    fn simple_block(track: u8, relative: i16, keyframe: bool, data: &[u8]) -> Vec<u8> {
        let mut body = vec![0x80 | track];
        body.extend_from_slice(&relative.to_be_bytes());
        body.push(if keyframe { 0x80 } else { 0 });
        body.extend_from_slice(data);
        element(SIMPLE_BLOCK, &body)
    }

    fn stream() -> Vec<u8> {
        let mut ebml = element(EBML, &element(DOC_TYPE, b"webm"));
        let info = element(
            INFO,
            &[
                uint(TIMESTAMP_SCALE, 1_000_000),
                element(DURATION, &2000.0f64.to_be_bytes()),
            ]
            .concat(),
        );
        let video = element(
            VIDEO,
            &[uint(PIXEL_WIDTH, 64), uint(PIXEL_HEIGHT, 48)].concat(),
        );
        let track1 = element(
            TRACK_ENTRY,
            &[
                uint(TRACK_NUMBER, 1),
                uint(TRACK_TYPE, 1),
                element(CODEC_ID, b"V_VP9"),
                uint(DEFAULT_DURATION, 33_333_333),
                video,
            ]
            .concat(),
        );
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        let audio = element(
            AUDIO,
            &[
                element(SAMPLING_FREQUENCY, &48000.0f64.to_be_bytes()),
                uint(CHANNELS, 2),
            ]
            .concat(),
        );
        let track2 = element(
            TRACK_ENTRY,
            &[
                uint(TRACK_NUMBER, 2),
                uint(TRACK_TYPE, 2),
                element(CODEC_ID, b"A_OPUS"),
                element(CODEC_PRIVATE, &head),
                audio,
            ]
            .concat(),
        );
        let tracks = element(TRACKS, &[track1, track2].concat());
        let cluster1 = element(
            CLUSTER,
            &[
                uint(TIMESTAMP, 0),
                simple_block(1, 0, true, b"key"),
                simple_block(2, 0, true, b"aa"),
                simple_block(1, 33, false, b"delta"),
            ]
            .concat(),
        );
        // A live-style unknown-size cluster, ended by the next cluster.
        let mut cluster2 = unknown_size(CLUSTER);
        cluster2.extend(uint(TIMESTAMP, 1000));
        cluster2.extend(simple_block(1, 0, true, b"k2"));
        let cluster3 = element(
            CLUSTER,
            &[uint(TIMESTAMP, 2000), simple_block(2, -5, true, b"a3")].concat(),
        );
        ebml.extend(unknown_size(SEGMENT));
        ebml.extend(info);
        ebml.extend(tracks);
        ebml.extend(cluster1);
        ebml.extend(cluster2);
        ebml.extend(cluster3);
        ebml
    }

    fn collect(chunks: &[&[u8]]) -> Vec<Event> {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            parser.push(chunk, &mut events).unwrap();
        }
        events
    }

    fn frames(events: &[Event]) -> Vec<(u32, Micros, bool, Vec<u8>)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Frame(frame) => {
                    Some((frame.track, frame.pts, frame.keyframe, frame.data.to_vec()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_header_and_clusters() {
        let bytes = stream();
        let events = collect(&[&bytes]);
        let header = events
            .iter()
            .find_map(|event| match event {
                Event::Header(header) => Some(header),
                _ => None,
            })
            .unwrap();
        assert_eq!(header.duration, Some(2_000_000));
        assert_eq!(header.tracks.len(), 2);
        assert_eq!(header.tracks[0].codec, Codec::Vp9);
        assert_eq!(header.tracks[0].video.unwrap().width, 64);
        assert_eq!(header.tracks[1].codec, Codec::Opus);
        assert_eq!(header.tracks[1].codec_delay, 6500);
        assert_eq!(header.tracks[1].audio.unwrap().channels, 2);
        let frames = frames(&events);
        assert_eq!(
            frames,
            vec![
                (1, 0, true, b"key".to_vec()),
                (2, 0, true, b"aa".to_vec()),
                (1, 33_000, false, b"delta".to_vec()),
                (1, 1_000_000, true, b"k2".to_vec()),
                (2, 1_995_000, true, b"a3".to_vec()),
            ]
        );
        let ends = events
            .iter()
            .filter(|e| matches!(e, Event::ClusterEnd))
            .count();
        assert_eq!(ends, 3);
    }

    #[test]
    fn byte_at_a_time_matches_whole() {
        let bytes = stream();
        let pieces: Vec<&[u8]> = bytes.chunks(1).collect();
        let whole = frames(&collect(&[&bytes]));
        assert_eq!(frames(&collect(&pieces)), whole);
        let pieces: Vec<&[u8]> = bytes.chunks(7).collect();
        assert_eq!(frames(&collect(&pieces)), whole);
    }

    #[test]
    fn lacing_modes() {
        // Xiph: 3 frames of sizes 2, 300, rest.
        let mut xiph = vec![2u8, 2, 255, 45];
        xiph.extend(vec![1u8; 2]);
        xiph.extend(vec![2u8; 300]);
        xiph.extend(vec![3u8; 5]);
        let frames = unlace(&xiph, 1).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.len()).collect::<Vec<_>>(),
            [2, 300, 5]
        );
        // Fixed: 2 frames of 3 bytes.
        let fixed = [1u8, 9, 9, 9, 8, 8, 8];
        assert_eq!(
            unlace(&fixed, 2).unwrap(),
            vec![&[9u8, 9, 9][..], &[8, 8, 8][..]]
        );
        // EBML: sizes 4, 6 (+2 via signed difference), rest 1.
        let mut ebml = vec![2u8, 0x84, 0xBF + 2];
        ebml.extend([0u8; 4]);
        ebml.extend([1u8; 6]);
        ebml.push(7);
        let frames = unlace(&ebml, 3).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.len()).collect::<Vec<_>>(),
            [4, 6, 1]
        );
        assert!(unlace(&[5u8, 1, 1], 1).is_err());
    }

    #[test]
    fn encrypted_partitions() {
        // Signal: encrypted + partitioned; IV; 2 partitions at 3 and 7 of 10.
        let mut frame = vec![0x03];
        frame.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        frame.push(2);
        frame.extend_from_slice(&3u32.to_be_bytes());
        frame.extend_from_slice(&7u32.to_be_bytes());
        frame.extend_from_slice(&[0u8; 10]);
        let (data, encryption) = decrypt_framing(&frame, &[0xAB; 16]).unwrap();
        assert_eq!(data.len(), 10);
        let encryption = encryption.unwrap();
        assert_eq!(encryption.iv[..8], [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(encryption.iv[8..], [0; 8]);
        assert_eq!(
            encryption.subsamples,
            vec![
                Subsample {
                    clear: 3,
                    protected: 4
                },
                Subsample {
                    clear: 3,
                    protected: 0
                },
            ]
        );
        // Clear frame of an encrypted track.
        let (data, encryption) = decrypt_framing(&[0x00, 9, 9], &[1]).unwrap();
        assert_eq!(&data[..], &[9, 9]);
        assert!(encryption.is_none());
    }

    #[test]
    fn vints() {
        assert_eq!(read_vint(&[0x81], false).unwrap(), Some((1, 1, false)));
        assert_eq!(
            read_vint(&[0x40, 0x02], false).unwrap(),
            Some((2, 2, false))
        );
        assert_eq!(read_vint(&[0xFF], false).unwrap(), Some((127, 1, true)));
        assert_eq!(
            read_vint(&[0x1A, 0x45, 0xDF, 0xA3], true).unwrap(),
            Some((0x1A45DFA3, 4, false))
        );
        assert_eq!(read_vint(&[0x40], false).unwrap(), None);
        assert!(read_vint(&[0x00], false).is_err());
    }
}
