//! SourceBuffer byte stream parsers: the MSE byte stream formats of the
//! registry that TRust plays, ISO BMFF (local
//! w3c/mse-byte-stream-format-isobmff) and WebM (local
//! w3c/mse-byte-stream-format-webm). Each turns appended bytes into
//! initialization segments and complete coded frames for the segment
//! parser loop (MSE #sourcebuffer-segment-parser-loop), which owns the
//! append state and runs the algorithms on them.

use std::collections::HashMap;
use std::sync::Arc;

use crate::media::Micros;
use crate::media::isobmff::{self, BoxHeader, Movie};
use crate::media::track::{SampleEncryption, TrackInfo, TrackKind};
use crate::media::webm;

/// A coded frame as the byte stream presents it (MSE #coded-frame), with
/// timestamps before any timestampOffset.
#[derive(Clone, Debug)]
pub(super) struct ParsedFrame {
    /// The byte stream's Track ID.
    pub track: u32,
    pub pts: Micros,
    pub dts: Micros,
    pub duration: Micros,
    /// A random access point.
    pub keyframe: bool,
    pub data: Arc<[u8]>,
    pub encryption: Option<Box<SampleEncryption>>,
    pub discard_padding: Micros,
}

/// An initialization segment (MSE #init-segment).
#[derive(Clone, Debug)]
pub(super) struct InitSegment {
    pub tracks: Vec<TrackInfo>,
    /// The duration the segment states, if any.
    pub duration: Option<Micros>,
    /// EME initialization data it carries: ("cenc", its pssh boxes) or a
    /// ("webm", key ID) per encrypted track.
    pub init_data: Vec<(&'static str, Vec<u8>)>,
}

#[derive(Debug)]
pub(super) enum Parsed {
    Init(InitSegment),
    /// The start of a media segment (the append state becomes
    /// PARSING_MEDIA_SEGMENT).
    MediaStart,
    Frames(Vec<ParsedFrame>),
    /// The media segment is complete (back to WAITING_FOR_SEGMENT).
    MediaEnd,
    /// EME initialization data in a media segment (a movie fragment's pssh
    /// boxes).
    InitData(&'static str, Vec<u8>),
}

/// The input violates the byte stream format (the append error algorithm
/// runs).
#[derive(Debug)]
pub(super) struct Violation(pub String);

impl From<isobmff::Error> for Violation {
    fn from(error: isobmff::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<webm::Error> for Violation {
    fn from(error: webm::Error) -> Self {
        Self(error.to_string())
    }
}

/// The byte stream formats a SourceBuffer accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    Mp4,
    WebM,
}

pub(super) enum SegmentParser {
    Mp4(Mp4Parser),
    WebM(Box<WebmParser>),
}

impl SegmentParser {
    pub fn new(format: Format) -> Self {
        match format {
            Format::Mp4 => Self::Mp4(Mp4Parser::default()),
            Format::WebM => Self::WebM(Box::default()),
        }
    }

    /// Add appended bytes to the [[input buffer]].
    pub fn push(&mut self, bytes: &[u8]) {
        match self {
            Self::Mp4(parser) => parser.input.extend_from_slice(bytes),
            Self::WebM(parser) => parser.input.extend_from_slice(bytes),
        }
    }

    /// Parse what the input buffer holds, appending what completed.
    pub fn parse(&mut self, out: &mut Vec<Parsed>) -> Result<(), Violation> {
        match self {
            Self::Mp4(parser) => parser.parse(out),
            Self::WebM(parser) => parser.parse(out),
        }
    }

    /// Whether a media segment is being parsed (PARSING_MEDIA_SEGMENT).
    pub fn parsing_media(&self) -> bool {
        match self {
            Self::Mp4(parser) => parser.pending.is_some(),
            Self::WebM(parser) => parser.parser.in_cluster(),
        }
    }

    /// MSE #sourcebuffer-reset-parser-state: complete coded frames still in
    /// the input buffer are returned for processing, then the input buffer
    /// is emptied; what earlier initialization segments described stays.
    pub fn reset(&mut self, out: &mut Vec<Parsed>) {
        match self {
            Self::Mp4(parser) => parser.reset(out),
            Self::WebM(parser) => parser.reset(out),
        }
    }

    /// Bytes held in the input buffer.
    pub fn buffered_bytes(&self) -> usize {
        match self {
            Self::Mp4(parser) => parser.input.len(),
            Self::WebM(parser) => {
                parser.input.len() + parser.held.values().map(|f| f.data.len()).sum::<usize>()
            }
        }
    }
}

/// A movie fragment waiting for its samples (moof, then mdat boxes).
struct PendingFragment {
    fragment: isobmff::Fragment,
    /// End of the sample data the fragment references, from the moof's
    /// first byte (which is `input[0]` while the fragment is pending).
    needed: u64,
    /// End of the complete boxes read after the moof, from its first byte.
    read: u64,
}

/// ISO BMFF: an initialization segment is `ftyp` + `moov`; a media segment
/// an optional `styp`, a `moof` and one or more `mdat`s. Other top-level
/// boxes between segments are accepted and ignored.
#[derive(Default)]
pub(super) struct Mp4Parser {
    input: Vec<u8>,
    /// Stream offset of `input[0]` (for an explicit base_data_offset).
    offset: u64,
    /// Bytes of an ignored box still to skip.
    skip: u64,
    movie: Option<Movie>,
    pending: Option<PendingFragment>,
}

/// Movie fragments larger than this are refused.
const MAX_BOX: u64 = 256 << 20;

impl Mp4Parser {
    fn consume(&mut self, len: usize) {
        self.input.drain(..len);
        self.offset += len as u64;
    }

    fn parse(&mut self, out: &mut Vec<Parsed>) -> Result<(), Violation> {
        loop {
            if self.skip > 0 {
                let len = (self.input.len() as u64).min(self.skip) as usize;
                self.consume(len);
                self.skip -= len as u64;
                if self.skip > 0 {
                    return Ok(());
                }
            }
            if self.pending.is_some() {
                if !self.continue_fragment(out)? {
                    return Ok(());
                }
                continue;
            }
            let Some(header) = BoxHeader::parse(&self.input)? else {
                return Ok(());
            };
            let Some(size) = header.size else {
                return Err(Violation(format!(
                    "'{}' box extends to the end of the stream",
                    String::from_utf8_lossy(&header.kind)
                )));
            };
            match &header.kind {
                b"moov" | b"moof" => {
                    if size > MAX_BOX {
                        return Err(Violation(String::from("box larger than 256 MiB")));
                    }
                    if (self.input.len() as u64) < size {
                        return Ok(());
                    }
                    let payload = self.input[header.header_len as usize..size as usize].to_vec();
                    if &header.kind == b"moov" {
                        let movie = isobmff::parse_moov(&payload)?;
                        let init = init_segment(&movie)?;
                        self.movie = Some(movie);
                        self.consume(size as usize);
                        out.push(Parsed::Init(init));
                    } else {
                        // A media segment before any initialization
                        // segment is the segment parser loop's error.
                        out.push(Parsed::MediaStart);
                        let Some(movie) = &self.movie else {
                            return Ok(());
                        };
                        let fragment = isobmff::parse_moof(&payload, self.offset, movie)?;
                        if !fragment.pssh.is_empty() {
                            out.push(Parsed::InitData(
                                "cenc",
                                fragment.pssh.iter().flat_map(|p| p.raw.clone()).collect(),
                            ));
                        }
                        if fragment.tracks.iter().any(|t| t.decode_time.is_none()) {
                            return Err(Violation(String::from("track fragment without tfdt")));
                        }
                        let mut needed = size;
                        for track in &fragment.tracks {
                            for sample in &track.samples {
                                if sample.moof_offset < 0 {
                                    return Err(Violation(String::from(
                                        "sample data before the moof",
                                    )));
                                }
                                needed =
                                    needed.max(sample.moof_offset as u64 + u64::from(sample.size));
                            }
                        }
                        self.pending = Some(PendingFragment {
                            fragment,
                            needed,
                            read: size,
                        });
                    }
                }
                // The File Type Box starts an initialization segment;
                // everything else outside a media segment is ignored
                // (mdat included: no movie fragment references it).
                _ => {
                    self.skip = size;
                }
            }
        }
    }

    /// Read the mdat boxes of the pending fragment; once its samples are
    /// all present, emit them. Returns whether parsing can go on.
    fn continue_fragment(&mut self, out: &mut Vec<Parsed>) -> Result<bool, Violation> {
        let pending = self.pending.as_mut().expect("pending fragment");
        while pending.read < pending.needed {
            let at = pending.read as usize;
            let Some(header) = BoxHeader::parse(&self.input[at..])? else {
                return Ok(false);
            };
            if &header.kind != b"mdat" {
                return Err(Violation(String::from(
                    "the media data boxes lack samples of the movie fragment",
                )));
            }
            let Some(size) = header.size else {
                return Err(Violation(String::from(
                    "mdat extends to the end of the stream",
                )));
            };
            if size > MAX_BOX {
                return Err(Violation(String::from("mdat larger than 256 MiB")));
            }
            if (self.input.len() - at) as u64 >= size {
                pending.read += size;
            } else {
                return Ok(false);
            }
        }
        let pending = self.pending.take().expect("pending fragment");
        let movie = self.movie.as_ref().expect("movie");
        let mut frames = Vec::new();
        for track in &pending.fragment.tracks {
            let Some(info) = movie.track(track.track_id) else {
                continue;
            };
            for sample in &track.samples {
                let start = sample.moof_offset as usize;
                let data: Arc<[u8]> = Arc::from(&self.input[start..start + sample.size as usize]);
                let pts = info.presentation_micros(sample.dts, sample.cts_offset);
                let end = info.presentation_micros(
                    sample.dts.saturating_add(i64::from(sample.duration)),
                    sample.cts_offset,
                );
                frames.push(ParsedFrame {
                    track: track.track_id,
                    pts,
                    dts: info.decode_micros(sample.dts),
                    duration: (end - pts).max(0),
                    keyframe: sample.sync,
                    data,
                    encryption: sample.encryption.clone(),
                    discard_padding: 0,
                });
            }
        }
        self.consume(pending.read as usize);
        out.push(Parsed::Frames(frames));
        out.push(Parsed::MediaEnd);
        Ok(true)
    }

    fn reset(&mut self, out: &mut Vec<Parsed>) {
        // A movie fragment's frames are complete only with all its mdat
        // data, which the pending fragment lacks.
        let _ = out;
        self.pending = None;
        self.input.clear();
        self.skip = 0;
    }
}

/// The ISO BMFF initialization segment checks (w3c/mse-byte-stream-format-
/// isobmff "Initialization Segments").
fn init_segment(movie: &Movie) -> Result<InitSegment, Violation> {
    if !movie.fragmented {
        return Err(Violation(String::from(
            "the Movie Box has no Movie Extends Box",
        )));
    }
    if movie.tracks.iter().any(|track| !track.samples.is_empty()) {
        return Err(Violation(String::from(
            "the Movie Box's tracks contain samples",
        )));
    }
    let pssh: Vec<u8> = movie
        .pssh
        .iter()
        .flat_map(|pssh| pssh.raw.clone())
        .collect();
    Ok(InitSegment {
        tracks: movie
            .tracks
            .iter()
            .map(|track| track.info.clone())
            .collect(),
        // A zero duration is the usual "unknown" of fragmented files.
        duration: movie.duration.filter(|&duration| duration > 0),
        init_data: if pssh.is_empty() {
            Vec::new()
        } else {
            vec![("cenc", pssh)]
        },
    })
}

/// WebM: an initialization segment is the EBML header, a Segment header,
/// Info and Tracks; a media segment is one Cluster.
#[derive(Default)]
pub(super) struct WebmParser {
    parser: webm::Parser,
    /// Bytes pushed but not yet given to the parser.
    input: Vec<u8>,
    /// Per track, the frame whose duration is the next frame's timestamp.
    held: HashMap<u32, webm::Frame>,
    /// Per track, the longest frame duration seen (the estimate for a
    /// cluster's last frame, as Chrome's WebMClusterParser does).
    longest: HashMap<u32, Micros>,
    kinds: HashMap<u32, TrackKind>,
}

/// Duration estimates for a track's last frame of a cluster when no frame
/// duration has been seen yet (Chrome's defaults).
const DEFAULT_VIDEO_DURATION: Micros = 63_000;
const DEFAULT_AUDIO_DURATION: Micros = 23_000;

impl WebmParser {
    fn parse(&mut self, out: &mut Vec<Parsed>) -> Result<(), Violation> {
        if self.input.is_empty() {
            return Ok(());
        }
        let input = std::mem::take(&mut self.input);
        let mut events = Vec::new();
        let result = self.parser.push(&input, &mut events);
        let mut frames = Vec::new();
        // Frames go out in stream order with the events around them.
        let flush = |frames: &mut Vec<ParsedFrame>, out: &mut Vec<Parsed>| {
            if !frames.is_empty() {
                out.push(Parsed::Frames(std::mem::take(frames)));
            }
        };
        for event in events {
            match event {
                webm::Event::Frame(frame) => self.frame(frame, &mut frames),
                webm::Event::Header(header) => {
                    flush(&mut frames, out);
                    self.kinds = header.tracks.iter().map(|t| (t.id, t.kind)).collect();
                    out.push(Parsed::Init(InitSegment {
                        tracks: header.tracks.clone(),
                        duration: header.duration,
                        init_data: header
                            .key_ids
                            .iter()
                            .map(|kid| ("webm", kid.clone()))
                            .collect(),
                    }));
                }
                webm::Event::ClusterStart { .. } => {
                    flush(&mut frames, out);
                    out.push(Parsed::MediaStart);
                }
                webm::Event::ClusterEnd => {
                    self.flush_held(&mut frames);
                    flush(&mut frames, out);
                    out.push(Parsed::MediaEnd);
                }
                webm::Event::DocType(_)
                | webm::Event::SegmentStart { .. }
                | webm::Event::Cues(_)
                | webm::Event::SeekHead(_) => {}
            }
        }
        flush(&mut frames, out);
        result.map_err(Violation::from)
    }

    fn frame(&mut self, frame: webm::Frame, frames: &mut Vec<ParsedFrame>) {
        if let Some(held) = self.held.remove(&frame.track) {
            let duration = (frame.pts - held.pts).max(0);
            self.emit(held, duration, frames);
        }
        match frame.duration {
            Some(duration) => self.emit(frame, duration, frames),
            None => {
                self.held.insert(frame.track, frame);
            }
        }
    }

    fn emit(&mut self, frame: webm::Frame, duration: Micros, frames: &mut Vec<ParsedFrame>) {
        let longest = self.longest.entry(frame.track).or_insert(0);
        *longest = (*longest).max(duration);
        frames.push(ParsedFrame {
            track: frame.track,
            pts: frame.pts,
            dts: frame.pts,
            duration,
            keyframe: frame.keyframe,
            data: frame.data,
            encryption: frame.encryption,
            discard_padding: frame.discard_padding,
        });
    }

    /// The end of a cluster: frames still waiting for a duration get the
    /// estimate.
    fn flush_held(&mut self, frames: &mut Vec<ParsedFrame>) {
        let mut held: Vec<webm::Frame> = self.held.drain().map(|(_, frame)| frame).collect();
        held.sort_by_key(|frame| frame.pts);
        for frame in held {
            let estimate = match self.longest.get(&frame.track) {
                Some(&longest) if longest > 0 => longest,
                _ if self.kinds.get(&frame.track) == Some(&TrackKind::Audio) => {
                    DEFAULT_AUDIO_DURATION
                }
                _ => DEFAULT_VIDEO_DURATION,
            };
            self.emit(frame, estimate, frames);
        }
    }

    fn reset(&mut self, out: &mut Vec<Parsed>) {
        // Held frames are complete coded frames: process them.
        let mut frames = Vec::new();
        self.flush_held(&mut frames);
        if !frames.is_empty() {
            out.push(Parsed::Frames(frames));
        }
        self.input.clear();
        self.parser.reset();
    }
}
