//! Media Source Extensions (local w3c/media-source@8e0611a): the
//! MediaSource and SourceBuffer state and algorithms behind the prelude's
//! IDL, and the demuxer a media element's [`Player`] reads the attached
//! MediaSource's active track buffers through.
//!
//! Script runs the IDL, the events and the media element's side of each
//! algorithm; this module runs the segment parser loop
//! (#sourcebuffer-segment-parser-loop), the initialization segment received
//! algorithm, coded frame processing, removal and eviction, the duration
//! change and end of stream algorithms' buffer side, and computes the
//! `buffered` ranges. Each operation returns an [`Outcome`] naming what
//! script must observe (tracks to create, the duration changing, the media
//! element reaching HAVE_METADATA, an append error).
//!
//! Timestamps are integer microseconds, as in Chrome: the spec's doubles
//! "SHOULD behave in a similar manner", and coded frames' own timestamps
//! convert from their timescales without accumulating rounding.
//!
//! [`Player`]: crate::media::player::Player

mod buffer;
mod parser;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use buffer::{CodedFrame, Read, Reader, TrackBuffer};
use parser::{Format, InitSegment, Parsed, ParsedFrame, SegmentParser};

use super::codec::CodecString;
use super::container::{self, Capabilities, Container};
use super::decode::Registry;
use super::demux::{self, Demuxer, Waker};
use super::track::{Packet, TrackInfo, TrackKind};
use super::{Micros, micros_to_seconds, seconds_to_micros};

/// #dom-readystate
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyState {
    Closed,
    Open,
    Ended,
}

impl ReadyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::Ended => "ended",
        }
    }
}

/// #dom-appendmode
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMode {
    Segments,
    Sequence,
}

impl AppendMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Sequence => "sequence",
        }
    }
}

/// An exception an operation throws, by Web IDL / DOMException name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    Type,
    InvalidState,
    NotSupported,
    QuotaExceeded,
    NotFound,
}

impl Exception {
    pub fn name(self) -> &'static str {
        match self {
            Self::Type => "TypeError",
            Self::InvalidState => "InvalidStateError",
            Self::NotSupported => "NotSupportedError",
            Self::QuotaExceeded => "QuotaExceededError",
            Self::NotFound => "NotFoundError",
        }
    }
}

/// An AudioTrack or VideoTrack the initialization segment received
/// algorithm creates (for the SourceBuffer and the media element).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTrack {
    pub kind: TrackKind,
    /// The byte stream's Track ID (in-band tracks: track_ID, TrackNumber).
    pub id: String,
    /// The AudioTrack/VideoTrack `kind` attribute.
    pub kind_name: &'static str,
    pub label: String,
    pub language: String,
    /// `enabled` (audio) or `selected` (video).
    pub enabled: bool,
}

/// What an operation did that script observes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The append error algorithm ran (the parser was reset and the end of
    /// stream algorithm ran with a "decode" error: the MediaSource is
    /// "ended"), for this reason.
    pub append_error: Option<String>,
    pub tracks: Vec<NewTrack>,
    /// The SourceBuffer joined activeSourceBuffers.
    pub activated: bool,
    /// The duration change algorithm changed the duration.
    pub duration_changed: bool,
    /// Every SourceBuffer has received an initialization segment: the media
    /// element goes from HAVE_NOTHING to HAVE_METADATA.
    pub metadata: bool,
    /// EME initialization data encountered (#initdata-encountered: the
    /// media element fires `encrypted` for each): type and data.
    pub init_data: Vec<(String, Vec<u8>)>,
}

/// Memory a SourceBuffer may hold before eviction (#sourcebuffer-coded-
/// frame-eviction leaves the capacity to the user agent; Firefox uses
/// about 100 MB for video and Chrome 12 MB for audio).
const VIDEO_CAPACITY: usize = 100 << 20;
const AUDIO_CAPACITY: usize = 12 << 20;
/// SourceBuffers per MediaSource.
const MAX_SOURCE_BUFFERS: usize = 8;
/// Eviction first keeps this much media before the current position.
const KEEP_BEHIND: Micros = 30_000_000;

struct SourceBuffer {
    id: u32,
    format: Format,
    parser: SegmentParser,
    mode: AppendMode,
    /// [[generate timestamps flag]]: false for every format TRust accepts
    /// (ISO BMFF and WebM carry their timestamps).
    generate_timestamps: bool,
    timestamp_offset: Micros,
    append_window_start: Micros,
    /// `None` is positive Infinity.
    append_window_end: Option<Micros>,
    group_start: Option<Micros>,
    group_end: Micros,
    first_init: bool,
    pending_init_change: bool,
    audio_added: bool,
    video_added: bool,
    tracks: Vec<TrackBuffer>,
    buffer_full: bool,
    /// In activeSourceBuffers.
    active: bool,
}

impl SourceBuffer {
    fn new(id: u32, format: Format) -> Self {
        Self {
            id,
            format,
            parser: SegmentParser::new(format),
            mode: AppendMode::Segments,
            generate_timestamps: false,
            timestamp_offset: 0,
            append_window_start: 0,
            append_window_end: None,
            group_start: None,
            group_end: 0,
            first_init: false,
            pending_init_change: false,
            audio_added: false,
            video_added: false,
            tracks: Vec::new(),
            buffer_full: false,
            active: false,
        }
    }

    fn bytes(&self) -> usize {
        self.tracks.iter().map(TrackBuffer::bytes).sum::<usize>() + self.parser.buffered_bytes()
    }

    fn capacity(&self) -> usize {
        if self
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Video)
        {
            VIDEO_CAPACITY
        } else {
            AUDIO_CAPACITY
        }
    }

    /// #dom-sourcebuffer-buffered (the intersection of the audio and video
    /// track buffer ranges).
    fn buffered(&mut self, ended: bool) -> Vec<(Micros, Micros)> {
        let mut tracks: Vec<&mut TrackBuffer> = self
            .tracks
            .iter_mut()
            .filter(|track| matches!(track.kind, TrackKind::Audio | TrackKind::Video))
            .collect();
        let Some(highest) = tracks
            .iter_mut()
            .filter_map(|t| t.ranges().last().map(|r| r.1))
            .max()
        else {
            return Vec::new();
        };
        let mut intersection = vec![(0, highest)];
        for track in tracks {
            let mut ranges = track.ranges().to_vec();
            if ended && let Some(last) = ranges.last_mut() {
                last.1 = highest;
            }
            intersection = intersect(&intersection, &ranges);
        }
        intersection
    }

    /// Unset the coded frame group state of every track buffer.
    fn reset_groups(&mut self) {
        for track in &mut self.tracks {
            track.reset_group();
        }
    }
}

/// The intersection of two normalized range lists.
fn intersect(a: &[(Micros, Micros)], b: &[(Micros, Micros)]) -> Vec<(Micros, Micros)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if start < end {
            out.push((start, end));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn seconds(ranges: &[(Micros, Micros)]) -> Vec<(f64, f64)> {
    ranges
        .iter()
        .map(|&(start, end)| (micros_to_seconds(start), micros_to_seconds(end)))
        .collect()
}

struct State {
    ready: ReadyState,
    /// #dom-mediasource-duration (NaN until known).
    duration: f64,
    buffers: Vec<SourceBuffer>,
    next_buffer: u32,
    next_track: u32,
    /// [[live seekable range]]
    live_seekable: Option<(f64, f64)>,
    metadata: bool,
    waker: Option<Waker>,
    /// A capacity replacing the per-kind ones (tests).
    capacity: Option<usize>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A MediaSource object's state (owned by the page; dropping it detaches).
pub struct MediaSource {
    shared: Arc<Shared>,
}

impl Default for MediaSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        self.detach();
    }
}

impl MediaSource {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    ready: ReadyState::Closed,
                    duration: f64::NAN,
                    buffers: Vec::new(),
                    next_buffer: 1,
                    next_track: 1,
                    live_seekable: None,
                    metadata: false,
                    waker: None,
                    capacity: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    /// Wake waiting readers after a change (outside the state lock).
    fn notify(&self) {
        self.shared.changed.notify_all();
        let waker = self.lock().waker.clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    pub fn ready_state(&self) -> ReadyState {
        self.lock().ready
    }

    /// #dom-mediasource-duration's getter.
    pub fn duration(&self) -> f64 {
        let state = self.lock();
        if state.ready == ReadyState::Closed {
            f64::NAN
        } else {
            state.duration
        }
    }

    /// #mediasource-attach for a closed MediaSource: "open". Returns false
    /// when it is not closed (the media element's failure steps run).
    pub fn attach(&self) -> bool {
        let mut state = self.lock();
        if state.ready != ReadyState::Closed {
            return false;
        }
        state.ready = ReadyState::Open;
        true
    }

    /// #mediasource-detach: closed, NaN duration, no SourceBuffers.
    pub fn detach(&self) {
        {
            let mut state = self.lock();
            state.ready = ReadyState::Closed;
            state.duration = f64::NAN;
            state.buffers.clear();
            state.live_seekable = None;
            state.metadata = false;
        }
        self.notify();
    }

    /// An "ended" MediaSource opens again (appendBuffer, remove and the
    /// attribute setters); returns whether it did (script fires
    /// `sourceopen`).
    pub fn reopen(&self) -> bool {
        let mut state = self.lock();
        if state.ready == ReadyState::Ended {
            state.ready = ReadyState::Open;
            true
        } else {
            false
        }
    }

    /// #dom-mediasource-duration's setter after its TypeError and
    /// `updating` checks.
    pub fn set_duration(&self, value: f64) -> Result<Outcome, Exception> {
        let mut outcome = Outcome::default();
        {
            let mut state = self.lock();
            if state.ready != ReadyState::Open {
                return Err(Exception::InvalidState);
            }
            outcome.duration_changed = state.duration_change(value)?;
        }
        self.notify();
        Ok(outcome)
    }

    /// #dom-mediasource-addsourcebuffer after the empty-type check;
    /// `element_has_metadata`: the media element reached HAVE_METADATA (no
    /// tracks can be added during playback). Returns the SourceBuffer id.
    pub fn add_source_buffer(
        &self,
        mime: &str,
        element_has_metadata: bool,
    ) -> Result<u32, Exception> {
        let format = mse_format(mime).ok_or(Exception::NotSupported)?;
        let mut state = self.lock();
        if element_has_metadata || state.buffers.len() >= MAX_SOURCE_BUFFERS {
            return Err(Exception::QuotaExceeded);
        }
        if state.ready != ReadyState::Open {
            return Err(Exception::InvalidState);
        }
        let id = state.next_buffer;
        state.next_buffer += 1;
        state.buffers.push(SourceBuffer::new(id, format));
        Ok(id)
    }

    /// #dom-mediasource-removesourcebuffer's buffer side.
    pub fn remove_source_buffer(&self, id: u32) -> Result<(), Exception> {
        {
            let mut state = self.lock();
            let index = state.index(id).ok_or(Exception::NotFound)?;
            state.buffers.remove(index);
        }
        self.notify();
        Ok(())
    }

    /// #dom-mediasource-endofstream after its `updating` check: runs the
    /// end of stream algorithm.
    pub fn end_of_stream(&self, error: bool) -> Result<Outcome, Exception> {
        let outcome = {
            let mut state = self.lock();
            if state.ready != ReadyState::Open {
                return Err(Exception::InvalidState);
            }
            state.end_of_stream(error)
        };
        self.notify();
        Ok(outcome)
    }

    pub fn set_live_seekable_range(&self, range: Option<(f64, f64)>) -> Result<(), Exception> {
        let mut state = self.lock();
        if state.ready != ReadyState::Open {
            return Err(Exception::InvalidState);
        }
        state.live_seekable = range;
        Ok(())
    }

    pub fn live_seekable_range(&self) -> Option<(f64, f64)> {
        self.lock().live_seekable
    }

    /// #sourcebuffer-prepare-append's eviction and buffer full steps, for
    /// `len` new bytes; `position` is the current playback position.
    pub fn prepare_append(&self, id: u32, len: usize, position: Micros) -> Result<(), Exception> {
        let mut state = self.lock();
        let index = state.index(id).ok_or(Exception::InvalidState)?;
        state.evict(index, len, position);
        if state.buffers[index].buffer_full {
            return Err(Exception::QuotaExceeded);
        }
        Ok(())
    }

    /// appendBuffer() step 2: add bytes to the [[input buffer]].
    pub fn append(&self, id: u32, bytes: &[u8]) -> Result<(), Exception> {
        let mut state = self.lock();
        let index = state.index(id).ok_or(Exception::InvalidState)?;
        state.buffers[index].parser.push(bytes);
        Ok(())
    }

    /// #sourcebuffer-buffer-append: run the segment parser loop over the
    /// input buffer.
    pub fn process(&self, id: u32) -> Outcome {
        let outcome = {
            let mut state = self.lock();
            let Some(index) = state.index(id) else {
                return Outcome::default();
            };
            state.segment_parser_loop(index)
        };
        self.notify();
        outcome
    }

    /// #dom-sourcebuffer-abort's buffer side: reset the parser state and
    /// the append window.
    pub fn abort(&self, id: u32) -> Outcome {
        let mut outcome = Outcome::default();
        {
            let mut state = self.lock();
            if let Some(index) = state.index(id) {
                state.reset_parser_state(index, &mut outcome);
                let buffer = &mut state.buffers[index];
                buffer.append_window_start = 0;
                buffer.append_window_end = None;
            }
        }
        self.notify();
        outcome
    }

    /// #sourcebuffer-range-removal's coded frame removal, in seconds.
    pub fn remove(&self, id: u32, start: f64, end: f64) {
        {
            let mut state = self.lock();
            if let Some(index) = state.index(id) {
                let end = if end.is_finite() {
                    seconds_to_micros(end)
                } else {
                    Micros::MAX
                };
                state.coded_frame_removal(index, seconds_to_micros(start), end);
            }
        }
        self.notify();
    }

    /// #dom-sourcebuffer-buffered, in seconds.
    pub fn buffered(&self, id: u32) -> Vec<(f64, f64)> {
        let mut state = self.lock();
        let ended = state.ready == ReadyState::Ended;
        match state.index(id) {
            Some(index) => seconds(&state.buffers[index].buffered(ended)),
            None => Vec::new(),
        }
    }

    /// #htmlmediaelement-extensions-buffered: the intersection over the
    /// active SourceBuffers.
    pub fn element_buffered(&self) -> Vec<(f64, f64)> {
        let mut state = self.lock();
        let ended = state.ready == ReadyState::Ended;
        let active: Vec<Vec<(Micros, Micros)>> = state
            .buffers
            .iter_mut()
            .filter(|buffer| buffer.active)
            .map(|buffer| buffer.buffered(ended))
            .collect();
        let Some(highest) = active
            .iter()
            .filter_map(|ranges| ranges.last().map(|r| r.1))
            .max()
        else {
            return Vec::new();
        };
        let mut intersection = vec![(0, highest)];
        for mut ranges in active {
            if ended && let Some(last) = ranges.last_mut() {
                last.1 = highest;
            }
            intersection = intersect(&intersection, &ranges);
        }
        seconds(&intersection)
    }

    pub fn mode(&self, id: u32) -> AppendMode {
        let state = self.lock();
        state
            .index(id)
            .map_or(AppendMode::Segments, |index| state.buffers[index].mode)
    }

    /// #dom-sourcebuffer-mode's setter after its removed/updating checks.
    /// Returns whether the MediaSource reopened, and the result.
    pub fn set_mode(&self, id: u32, mode: AppendMode) -> (bool, Result<(), Exception>) {
        let mut state = self.lock();
        let Some(index) = state.index(id) else {
            return (false, Err(Exception::InvalidState));
        };
        if state.buffers[index].generate_timestamps && mode == AppendMode::Segments {
            return (false, Err(Exception::Type));
        }
        let reopened = state.ready == ReadyState::Ended;
        if reopened {
            state.ready = ReadyState::Open;
        }
        let buffer = &mut state.buffers[index];
        if buffer.parser.parsing_media() {
            return (reopened, Err(Exception::InvalidState));
        }
        if mode == AppendMode::Sequence {
            buffer.group_start = Some(buffer.group_end);
        }
        buffer.mode = mode;
        (reopened, Ok(()))
    }

    pub fn timestamp_offset(&self, id: u32) -> f64 {
        let state = self.lock();
        state.index(id).map_or(0.0, |index| {
            micros_to_seconds(state.buffers[index].timestamp_offset)
        })
    }

    /// #dom-sourcebuffer-timestampoffset's setter after its removed and
    /// updating checks.
    pub fn set_timestamp_offset(&self, id: u32, offset: f64) -> (bool, Result<(), Exception>) {
        let mut state = self.lock();
        let Some(index) = state.index(id) else {
            return (false, Err(Exception::InvalidState));
        };
        let reopened = state.ready == ReadyState::Ended;
        if reopened {
            state.ready = ReadyState::Open;
        }
        let buffer = &mut state.buffers[index];
        if buffer.parser.parsing_media() {
            return (reopened, Err(Exception::InvalidState));
        }
        let offset = seconds_to_micros(offset);
        if buffer.mode == AppendMode::Sequence {
            buffer.group_start = Some(offset);
        }
        buffer.timestamp_offset = offset;
        (reopened, Ok(()))
    }

    /// appendWindowStart and appendWindowEnd (Infinity for no end).
    pub fn append_window(&self, id: u32) -> (f64, f64) {
        let state = self.lock();
        state.index(id).map_or((0.0, f64::INFINITY), |index| {
            let buffer = &state.buffers[index];
            (
                micros_to_seconds(buffer.append_window_start),
                buffer
                    .append_window_end
                    .map_or(f64::INFINITY, micros_to_seconds),
            )
        })
    }

    /// The append window setters, after their checks.
    pub fn set_append_window(&self, id: u32, start: f64, end: f64) {
        let mut state = self.lock();
        if let Some(index) = state.index(id) {
            let buffer = &mut state.buffers[index];
            buffer.append_window_start = seconds_to_micros(start);
            buffer.append_window_end = end.is_finite().then(|| seconds_to_micros(end));
        }
    }

    /// #dom-sourcebuffer-changetype after its empty-type, removed and
    /// updating checks. Returns whether the MediaSource reopened.
    pub fn change_type(&self, id: u32, mime: &str) -> Result<(bool, Outcome), Exception> {
        let format = mse_format(mime).ok_or(Exception::NotSupported)?;
        let mut outcome = Outcome::default();
        let reopened = {
            let mut state = self.lock();
            let index = state.index(id).ok_or(Exception::InvalidState)?;
            let reopened = state.ready == ReadyState::Ended;
            if reopened {
                state.ready = ReadyState::Open;
            }
            state.reset_parser_state(index, &mut outcome);
            let buffer = &mut state.buffers[index];
            if buffer.format != format {
                buffer.format = format;
                buffer.parser = SegmentParser::new(format);
            }
            buffer.generate_timestamps = false;
            buffer.pending_init_change = true;
            reopened
        };
        self.notify();
        Ok((reopened, outcome))
    }

    /// Whether the buffer is parsing a media segment (the attribute
    /// setters' InvalidStateError).
    pub fn parsing_media(&self, id: u32) -> bool {
        let state = self.lock();
        state
            .index(id)
            .is_some_and(|index| state.buffers[index].parser.parsing_media())
    }

    /// Track selection put the SourceBuffer in or out of
    /// activeSourceBuffers.
    pub fn set_active(&self, id: u32, active: bool) {
        let mut state = self.lock();
        if let Some(index) = state.index(id) {
            state.buffers[index].active = active;
        }
    }

    #[cfg(test)]
    fn set_capacity(&self, capacity: usize) {
        self.lock().capacity = Some(capacity);
    }

    /// Bytes the SourceBuffers hold.
    pub fn memory_bytes(&self) -> usize {
        self.lock().buffers.iter().map(SourceBuffer::bytes).sum()
    }

    /// A demuxer over the active track buffers, for the media element's
    /// player.
    pub fn demuxer(&self) -> MseDemuxer {
        MseDemuxer {
            shared: self.shared.clone(),
            tracks: Vec::new(),
            readers: Vec::new(),
        }
    }
}

/// The byte stream format of a supported MSE type.
fn mse_format(mime: &str) -> Option<Format> {
    if !container::is_type_supported_mse(mime, Registry::get()) {
        return None;
    }
    match container::classify(mime)?.0 {
        Container::Mp4 => Some(Format::Mp4),
        Container::WebM => Some(Format::WebM),
        _ => None,
    }
}

impl State {
    fn index(&self, id: u32) -> Option<usize> {
        self.buffers.iter().position(|buffer| buffer.id == id)
    }

    /// #duration-change-algorithm. Returns whether the duration changed.
    fn duration_change(&mut self, mut new: f64) -> Result<bool, Exception> {
        if self.duration == new || (self.duration.is_nan() && new.is_nan()) {
            return Ok(false);
        }
        let highest_pts = self
            .buffers
            .iter()
            .flat_map(|buffer| buffer.tracks.iter())
            .filter_map(TrackBuffer::highest_pts)
            .max();
        if highest_pts.is_some_and(|pts| new < micros_to_seconds(pts)) {
            return Err(Exception::InvalidState);
        }
        let highest_end = self.highest_end_time();
        if let Some(end) = highest_end
            && new < micros_to_seconds(end)
        {
            new = micros_to_seconds(end);
        }
        self.duration = new;
        Ok(true)
    }

    /// The largest track buffer ranges end time of all track buffers.
    fn highest_end_time(&mut self) -> Option<Micros> {
        self.buffers
            .iter_mut()
            .flat_map(|buffer| buffer.tracks.iter_mut())
            .filter_map(|track| track.ranges().last().map(|range| range.1))
            .max()
    }

    /// #end-of-stream-algorithm's MediaSource side.
    fn end_of_stream(&mut self, error: bool) -> Outcome {
        let mut outcome = Outcome::default();
        self.ready = ReadyState::Ended;
        if !error {
            let end = self.highest_end_time().map_or(0.0, micros_to_seconds);
            outcome.duration_changed = self.duration_change(end).unwrap_or(false);
        }
        outcome
    }

    fn segment_parser_loop(&mut self, index: usize) -> Outcome {
        let mut outcome = Outcome::default();
        let mut parsed = Vec::new();
        let result = self.buffers[index].parser.parse(&mut parsed);
        let mut failure = None;
        for item in parsed {
            if let Err(reason) = self.handle(index, item, &mut outcome) {
                failure = Some(reason);
                break;
            }
        }
        if let Some(reason) = failure.or_else(|| result.err().map(|violation| violation.0)) {
            self.append_error(index, reason, &mut outcome);
        }
        outcome
    }

    /// One result of the parser, as the segment parser loop runs it.
    fn handle(&mut self, index: usize, item: Parsed, outcome: &mut Outcome) -> Result<(), String> {
        const NO_INIT: &str = "a media segment came before its initialization segment";
        match item {
            Parsed::Init(init) => {
                let init_data = init.init_data.clone();
                self.initialization_segment_received(index, init, outcome)?;
                outcome.init_data.extend(
                    init_data
                        .into_iter()
                        .map(|(kind, data)| (kind.to_string(), data)),
                );
                Ok(())
            }
            Parsed::InitData(kind, data) => {
                outcome.init_data.push((kind.to_string(), data));
                Ok(())
            }
            Parsed::MediaStart => {
                let buffer = &self.buffers[index];
                if !buffer.first_init || buffer.pending_init_change {
                    return Err(String::from(NO_INIT));
                }
                Ok(())
            }
            Parsed::Frames(frames) => {
                let buffer = &self.buffers[index];
                if !buffer.first_init || buffer.pending_init_change {
                    return Err(String::from(NO_INIT));
                }
                self.coded_frame_processing(index, frames, outcome);
                Ok(())
            }
            Parsed::MediaEnd => Ok(()),
        }
    }

    /// #sourcebuffer-append-error: reset the parser, then the end of
    /// stream algorithm with a "decode" error (script fires the events).
    fn append_error(&mut self, index: usize, reason: String, outcome: &mut Outcome) {
        self.reset_parser_state_discarding(index);
        outcome.append_error = Some(reason);
        self.end_of_stream(true);
    }

    /// #sourcebuffer-reset-parser-state
    fn reset_parser_state(&mut self, index: usize, outcome: &mut Outcome) {
        let mut parsed = Vec::new();
        self.buffers[index].parser.reset(&mut parsed);
        for item in parsed {
            if let Parsed::Frames(frames) = item
                && self.buffers[index].first_init
            {
                self.coded_frame_processing(index, frames, outcome);
            }
        }
        self.finish_reset(index);
    }

    /// The reset after an append error: frames left in the input buffer
    /// are not processed.
    fn reset_parser_state_discarding(&mut self, index: usize) {
        let mut parsed = Vec::new();
        self.buffers[index].parser.reset(&mut parsed);
        self.finish_reset(index);
    }

    fn finish_reset(&mut self, index: usize) {
        let buffer = &mut self.buffers[index];
        buffer.reset_groups();
        if buffer.mode == AppendMode::Sequence {
            buffer.group_start = Some(buffer.group_end);
        }
    }

    /// #sourcebuffer-init-segment-received
    fn initialization_segment_received(
        &mut self,
        index: usize,
        init: InitSegment,
        outcome: &mut Outcome,
    ) -> Result<(), String> {
        // 1. The duration, if still unknown.
        if self.duration.is_nan() {
            let duration = init.duration.map_or(f64::INFINITY, micros_to_seconds);
            outcome.duration_changed |= self.duration_change(duration).unwrap_or(false);
        }
        // 2. Text tracks are not played: only audio and video count.
        let tracks: Vec<TrackInfo> = init
            .tracks
            .into_iter()
            .filter(|track| matches!(track.kind, TrackKind::Audio | TrackKind::Video))
            .collect();
        if tracks.is_empty() {
            return Err(String::from(
                "the initialization segment has no audio or video track",
            ));
        }
        let registry = Registry::get();
        let supported = |track: &TrackInfo| {
            registry.decodes(&CodecString {
                codec: track.codec,
                details: track.details,
                ambiguous: false,
            })
        };
        if let Some(track) = tracks.iter().find(|track| !supported(track)) {
            return Err(format!("unsupported codec {}", track.codec_string));
        }
        let buffer = &mut self.buffers[index];
        if buffer.first_init {
            // 3. Same tracks as the first initialization segment.
            for kind in [TrackKind::Audio, TrackKind::Video] {
                let new = tracks.iter().filter(|t| t.kind == kind).count();
                let old = buffer.tracks.iter().filter(|t| t.kind == kind).count();
                if new != old {
                    return Err(String::from(
                        "the initialization segment's tracks differ from the first one's",
                    ));
                }
            }
            for track in tracks {
                let single = buffer
                    .tracks
                    .iter()
                    .filter(|t| t.kind == track.kind)
                    .count()
                    == 1;
                let Some(existing) = buffer
                    .tracks
                    .iter_mut()
                    .find(|t| t.kind == track.kind && (single || t.stream_id == track.id))
                else {
                    return Err(String::from(
                        "the initialization segment's track IDs differ from the first one's",
                    ));
                };
                existing.stream_id = track.id;
                existing.describe(track);
            }
            for track in &mut buffer.tracks {
                track.need_random_access_point = true;
            }
        } else {
            // 5. Create the tracks and their track buffers.
            let mut active = false;
            for track in tracks {
                let first = match track.kind {
                    TrackKind::Audio => !std::mem::replace(&mut buffer.audio_added, true),
                    _ => !std::mem::replace(&mut buffer.video_added, true),
                };
                active |= first;
                outcome.tracks.push(NewTrack {
                    kind: track.kind,
                    id: track.id.to_string(),
                    kind_name: if first { "main" } else { "" },
                    label: track.label.clone(),
                    language: track.language.clone(),
                    enabled: first,
                });
                let id = self.next_track;
                self.next_track += 1;
                buffer.tracks.push(TrackBuffer::new(id, track));
            }
            if active {
                buffer.active = true;
                outcome.activated = true;
            }
        }
        buffer.first_init = true;
        buffer.pending_init_change = false;
        if !self.metadata && self.buffers.iter().all(|buffer| buffer.first_init) {
            self.metadata = true;
            outcome.metadata = true;
        }
        Ok(())
    }

    /// #sourcebuffer-coded-frame-processing for the frames of a media
    /// segment.
    fn coded_frame_processing(
        &mut self,
        index: usize,
        frames: Vec<ParsedFrame>,
        outcome: &mut Outcome,
    ) {
        for frame in frames {
            self.process_frame(index, frame);
        }
        // Data beyond the duration extends it.
        let group_end = micros_to_seconds(self.buffers[index].group_end);
        if group_end > self.duration {
            outcome.duration_changed |= self.duration_change(group_end).unwrap_or(false);
        }
    }

    fn process_frame(&mut self, index: usize, frame: ParsedFrame) {
        let buffer = &mut self.buffers[index];
        let Some(track) = buffer
            .tracks
            .iter()
            .position(|t| t.stream_id == frame.track)
        else {
            // A track that is not played (text, or not in the
            // initialization segment).
            return;
        };
        let duration = frame.duration;
        // Loop Top.
        let (pts, dts) = loop {
            let (mut pts, mut dts) = if buffer.generate_timestamps {
                (0, 0)
            } else {
                (frame.pts, frame.dts)
            };
            if buffer.mode == AppendMode::Sequence
                && let Some(start) = buffer.group_start.take()
            {
                buffer.timestamp_offset = start - pts;
                buffer.group_end = start;
                for track in &mut buffer.tracks {
                    track.need_random_access_point = true;
                }
            }
            if buffer.timestamp_offset != 0 {
                pts = pts.saturating_add(buffer.timestamp_offset);
                dts = dts.saturating_add(buffer.timestamp_offset);
            }
            let track_buffer = &buffer.tracks[track];
            let discontinuity = track_buffer.last_decode.is_some_and(|last| {
                dts < last || dts - last > 2 * track_buffer.last_duration.unwrap_or(0)
            });
            if discontinuity {
                match buffer.mode {
                    AppendMode::Segments => buffer.group_end = pts,
                    AppendMode::Sequence => buffer.group_start = Some(buffer.group_end),
                }
                buffer.reset_groups();
                continue;
            }
            break (pts, dts);
        };
        let frame_end = pts.saturating_add(duration);
        // The append window.
        if pts < buffer.append_window_start
            || buffer.append_window_end.is_some_and(|end| frame_end > end)
        {
            buffer.tracks[track].need_random_access_point = true;
            return;
        }
        let track_buffer = &mut buffer.tracks[track];
        if track_buffer.need_random_access_point {
            if !frame.keyframe {
                return;
            }
            track_buffer.need_random_access_point = false;
        }
        // A frame overlapping the start of the new one.
        if track_buffer.last_decode.is_none()
            && let Some(overlapped) = track_buffer.overlapping(pts)
        {
            let (start, overlapped_dts) = (
                track_buffer.frames[overlapped].pts,
                track_buffer.frames[overlapped].dts,
            );
            match track_buffer.kind {
                // #sourcebuffer-audio-splice-frame-algorithm without
                // crossfading: the overlapped frame plays up to the new
                // one, its decoded audio past that point discarded (where
                // the spec's silence frame would be, its own audio is).
                TrackKind::Audio if start < pts => track_buffer.trim_to(overlapped, pts),
                TrackKind::Audio => {
                    track_buffer.remove_where(true, |f| f.dts == overlapped_dts && f.pts == start);
                }
                TrackKind::Video if pts < start + 1 => {
                    track_buffer.remove_where(true, |f| f.dts == overlapped_dts && f.pts == start);
                }
                _ => {}
            }
        }
        // Remove existing coded frames the new one replaces.
        match track_buffer.highest_end {
            None => {
                track_buffer.remove_where(true, |f| f.pts >= pts && f.pts < frame_end);
            }
            Some(highest) if highest <= pts => {
                track_buffer.remove_where(true, |f| f.pts >= highest && f.pts < frame_end);
            }
            Some(_) => {}
        }
        let config = track_buffer.info.clone();
        let group = track_buffer.group;
        track_buffer.insert(CodedFrame::new(frame, pts, dts, config, group));
        track_buffer.last_decode = Some(dts);
        track_buffer.last_duration = Some(duration);
        if track_buffer
            .highest_end
            .is_none_or(|highest| frame_end > highest)
        {
            track_buffer.highest_end = Some(frame_end);
        }
        if frame_end > buffer.group_end {
            buffer.group_end = frame_end;
        }
        if buffer.generate_timestamps {
            buffer.timestamp_offset = frame_end;
        }
    }

    /// #sourcebuffer-coded-frame-removal
    fn coded_frame_removal(&mut self, index: usize, start: Micros, end: Micros) {
        let duration = if self.duration.is_finite() {
            seconds_to_micros(self.duration)
        } else {
            Micros::MAX
        };
        let buffer = &mut self.buffers[index];
        let mut discontinuity = None;
        for track in &mut buffer.tracks {
            let remove_end = track.next_random_access_point(end).unwrap_or(duration);
            let last_decode = track.last_decode;
            let removed = track.remove_where(false, |f| f.pts >= start && f.pts < remove_end);
            if let Some(frame) = removed.iter().find(|f| Some(f.dts) == last_decode) {
                discontinuity = Some(frame.pts);
            }
        }
        if let Some(pts) = discontinuity {
            match buffer.mode {
                AppendMode::Segments => buffer.group_end = pts,
                AppendMode::Sequence => buffer.group_start = Some(buffer.group_end),
            }
            buffer.reset_groups();
        }
        let capacity = self.capacity.unwrap_or_else(|| buffer.capacity());
        if buffer.buffer_full && buffer.bytes() < capacity {
            buffer.buffer_full = false;
        }
    }

    /// #sourcebuffer-coded-frame-eviction. The buffer full flag is set
    /// ahead of time when the new data would exceed the capacity (the
    /// spec's note allows it); media before the current position is freed,
    /// a whole group of pictures at a time.
    fn evict(&mut self, index: usize, len: usize, position: Micros) {
        let capacity = self.capacity;
        let fits = |buffer: &SourceBuffer| {
            buffer.bytes().saturating_add(len) <= capacity.unwrap_or_else(|| buffer.capacity())
        };
        if fits(&self.buffers[index]) {
            self.buffers[index].buffer_full = false;
            return;
        }
        self.buffers[index].buffer_full = true;
        for keep in [KEEP_BEHIND, 0] {
            // The random access point every track can restart from.
            let cut = self.buffers[index]
                .tracks
                .iter()
                .filter_map(|track| {
                    track.previous_random_access_point(position.saturating_sub(keep))
                })
                .min();
            if let Some(cut) = cut {
                self.coded_frame_removal(index, Micros::MIN, cut);
            }
            // Removal clears the flag by the buffer alone; what counts here
            // is room for the new data.
            let room = fits(&self.buffers[index]);
            self.buffers[index].buffer_full = !room;
            if room {
                return;
            }
        }
    }
}

/// Reads an attached MediaSource's track buffers for a [`Player`]
/// (MSE #mediasource-seeking and SourceBuffer monitoring: reads wait for
/// appended data; the end of the stream comes from endOfStream()).
///
/// [`Player`]: crate::media::player::Player
pub struct MseDemuxer {
    shared: Arc<Shared>,
    tracks: Vec<TrackInfo>,
    readers: Vec<(u32, Reader)>,
}

impl MseDemuxer {
    fn find(state: &mut State, track: u32) -> Option<&mut TrackBuffer> {
        state
            .buffers
            .iter_mut()
            .flat_map(|buffer| buffer.tracks.iter_mut())
            .find(|buffer| buffer.id == track)
    }
}

impl Demuxer for MseDemuxer {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    /// The media element takes its duration from the MediaSource.
    fn duration(&self) -> Option<Micros> {
        None
    }

    fn read(&mut self, track: u32) -> Result<Option<Packet>, demux::Error> {
        let mut state = self.shared.lock();
        if state.ready == ReadyState::Closed {
            return Err(demux::Error::Pending);
        }
        let ended = state.ready == ReadyState::Ended;
        let Some(reader) = self
            .readers
            .iter_mut()
            .find(|(id, _)| *id == track)
            .map(|(_, reader)| reader)
        else {
            return Ok(None);
        };
        // A removed SourceBuffer's tracks end.
        let Some(buffer) = Self::find(&mut state, track) else {
            return Ok(None);
        };
        match reader.read(buffer, ended) {
            Read::Packet(packet) => Ok(Some(packet)),
            Read::Pending => Err(demux::Error::Pending),
            Read::End => Ok(None),
        }
    }

    fn seek(&mut self, time: Micros) -> Result<Micros, demux::Error> {
        let mut state = self.shared.lock();
        let mut landed = time;
        for (id, reader) in &mut self.readers {
            reader.seek(time);
            if let Some(buffer) = Self::find(&mut state, *id)
                && buffer.kind == TrackKind::Video
                && let Some(start) = Reader::landing(buffer, time)
            {
                landed = landed.min(start);
            }
        }
        Ok(landed)
    }

    fn seekable(&self) -> bool {
        true
    }

    fn wait_for_tracks(&mut self, stop: &AtomicBool) -> Result<(), demux::Error> {
        let mut state = self.shared.lock();
        loop {
            if stop.load(Ordering::Acquire) || state.ready == ReadyState::Closed {
                return Err(demux::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "the MediaSource was detached",
                )));
            }
            if state.metadata {
                break;
            }
            // Detaching notifies; the timeout bounds a missed stop.
            state = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_secs(60))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        for buffer in state.buffers.iter().flat_map(|buffer| buffer.tracks.iter()) {
            self.tracks.push((*buffer.info).clone());
            self.readers.push((buffer.id, Reader::new(0)));
        }
        Ok(())
    }

    fn set_waker(&mut self, waker: Waker) {
        self.shared.lock().waker = Some(waker);
    }
}
