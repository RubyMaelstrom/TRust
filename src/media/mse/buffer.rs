//! Track buffers (MSE #track-buffers): the coded frames of one track in
//! decode order, with the coded frame group state of the coded frame
//! processing algorithm, the track buffer ranges, and a reader position for
//! playback.

use std::sync::Arc;

use super::parser::ParsedFrame;
use crate::media::Micros;
use crate::media::track::{Packet, TrackInfo, TrackKind};

/// A buffered coded frame, timestamps after timestampOffset.
#[derive(Clone, Debug)]
pub(super) struct CodedFrame {
    pub pts: Micros,
    pub dts: Micros,
    pub duration: Micros,
    pub keyframe: bool,
    pub data: Arc<[u8]>,
    pub encryption: Option<Box<crate::media::track::SampleEncryption>>,
    pub discard_padding: Micros,
    /// The track description in force when the frame was appended.
    pub config: Arc<TrackInfo>,
    /// The coded frame group the frame was appended in.
    pub group: u64,
}

impl CodedFrame {
    pub fn new(
        frame: ParsedFrame,
        pts: Micros,
        dts: Micros,
        config: Arc<TrackInfo>,
        group: u64,
    ) -> Self {
        Self {
            pts,
            dts,
            duration: frame.duration,
            keyframe: frame.keyframe,
            data: frame.data,
            encryption: frame.encryption,
            discard_padding: frame.discard_padding,
            config,
            group,
        }
    }

    pub fn end(&self) -> Micros {
        self.pts.saturating_add(self.duration)
    }
}

/// Ranges further apart than this never coalesce, whatever the frame
/// durations (a long gap is a gap).
const MAX_COALESCE: Micros = 1_000_000;

pub(super) struct TrackBuffer {
    /// The MediaSource-wide track id packets carry.
    pub id: u32,
    /// The Track ID of the byte stream.
    pub stream_id: u32,
    pub kind: TrackKind,
    /// The track description from the latest initialization segment.
    pub info: Arc<TrackInfo>,
    /// Coded frames in decode order (then presentation order).
    pub frames: Vec<CodedFrame>,
    pub last_decode: Option<Micros>,
    pub last_duration: Option<Micros>,
    pub highest_end: Option<Micros>,
    pub need_random_access_point: bool,
    /// The current coded frame group (counted from the track buffer's
    /// creation).
    pub group: u64,
    /// The longest frame duration buffered so far, for coalescing ranges.
    longest: Micros,
    /// The track buffer ranges, recomputed after changes.
    ranges: Option<Vec<(Micros, Micros)>>,
    bytes: usize,
}

impl TrackBuffer {
    /// A track buffer for the track `info` describes; packets and the
    /// description carry `id` instead of the byte stream's Track ID.
    pub fn new(id: u32, info: TrackInfo) -> Self {
        let stream_id = info.id;
        let kind = info.kind;
        let mut buffer = Self {
            id,
            stream_id,
            kind,
            info: Arc::new(info),
            frames: Vec::new(),
            last_decode: None,
            last_duration: None,
            highest_end: None,
            need_random_access_point: true,
            group: 0,
            longest: 0,
            ranges: Some(Vec::new()),
            bytes: 0,
        };
        buffer.describe(Arc::unwrap_or_clone(buffer.info.clone()));
        buffer
    }

    /// Take the track description of a new initialization segment.
    pub fn describe(&mut self, mut info: TrackInfo) {
        info.id = self.id;
        self.info = Arc::new(info);
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Unset the coded frame group state (a discontinuity).
    pub fn reset_group(&mut self) {
        self.last_decode = None;
        self.last_duration = None;
        self.highest_end = None;
        self.need_random_access_point = true;
        self.group += 1;
    }

    /// The gap below which two ranges are one: MSE #track-buffers allows
    /// coalescing ranges closer than twice the longest frame duration.
    pub fn tolerance(&self) -> Micros {
        (2 * self.longest).clamp(1, MAX_COALESCE)
    }

    /// The highest presentation timestamp of any buffered frame.
    pub fn highest_pts(&self) -> Option<Micros> {
        self.frames.iter().map(|frame| frame.pts).max()
    }

    /// The highest frame end time.
    pub fn highest_frame_end(&self) -> Option<Micros> {
        self.frames.iter().map(CodedFrame::end).max()
    }

    /// The frame whose presentation interval contains `pts`.
    pub fn overlapping(&self, pts: Micros) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| frame.pts <= pts && pts < frame.end())
    }

    /// Remove the frames `remove` selects, then every frame after each in
    /// decode order up to the next random access point (their possible
    /// decoding dependencies). Coded frame processing passes
    /// `keep_group`: frames the current coded frame group appended depend
    /// on none of the old ones. Returns the removed frames.
    pub fn remove_where(
        &mut self,
        keep_group: bool,
        remove: impl Fn(&CodedFrame) -> bool,
    ) -> Vec<CodedFrame> {
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.frames.len());
        let mut dependent = false;
        let group = keep_group.then_some(self.group);
        for frame in self.frames.drain(..) {
            if remove(&frame) {
                dependent = true;
                removed.push(frame);
            } else if dependent && !frame.keyframe && group != Some(frame.group) {
                removed.push(frame);
            } else {
                dependent = false;
                kept.push(frame);
            }
        }
        self.frames = kept;
        if !removed.is_empty() {
            self.bytes -= removed.iter().map(|frame| frame.data.len()).sum::<usize>();
            self.ranges = None;
        }
        removed
    }

    /// Add a frame at its place in decode order.
    pub fn insert(&mut self, frame: CodedFrame) {
        let key = (frame.dts, frame.pts);
        let at = self.frames.partition_point(|f| (f.dts, f.pts) <= key);
        self.longest = self.longest.max(frame.duration);
        self.bytes += frame.data.len();
        self.ranges = None;
        self.frames.insert(at, frame);
    }

    /// Shorten the frame at `index` to end at `end`, dropping the decoded
    /// audio past it (an audio splice without crossfading: the new frame
    /// replaces the overlapped part).
    pub fn trim_to(&mut self, index: usize, end: Micros) {
        let frame = &mut self.frames[index];
        let cut = frame.end() - end;
        if cut > 0 {
            frame.duration -= cut;
            frame.discard_padding += cut;
            self.ranges = None;
        }
    }

    /// The track buffer ranges, in presentation time.
    pub fn ranges(&mut self) -> &[(Micros, Micros)] {
        if self.ranges.is_none() {
            let mut spans: Vec<(Micros, Micros)> =
                self.frames.iter().map(|f| (f.pts, f.end())).collect();
            spans.sort_unstable();
            let tolerance = self.tolerance();
            let mut ranges: Vec<(Micros, Micros)> = Vec::new();
            for (start, end) in spans {
                match ranges.last_mut() {
                    Some(last) if start <= last.1 + tolerance => last.1 = last.1.max(end),
                    _ => ranges.push((start, end)),
                }
            }
            self.ranges = Some(ranges);
        }
        self.ranges.as_deref().unwrap_or(&[])
    }

    /// The first keyframe at or after `time`.
    pub fn next_random_access_point(&self, time: Micros) -> Option<Micros> {
        self.frames
            .iter()
            .filter(|frame| frame.keyframe && frame.pts >= time)
            .map(|frame| frame.pts)
            .min()
    }

    /// The last keyframe at or before `time`.
    pub fn previous_random_access_point(&self, time: Micros) -> Option<Micros> {
        self.frames
            .iter()
            .filter(|frame| frame.keyframe && frame.pts <= time)
            .map(|frame| frame.pts)
            .max()
    }

    /// Where playback from `time` starts: the frame (in decode order) of the
    /// last random access point at or before `time` within the buffered
    /// range that holds `time`.
    fn start_for(&mut self, time: Micros) -> Option<usize> {
        let tolerance = self.tolerance();
        let ranges = self.ranges().to_vec();
        // A start before the first range by less than a second plays from
        // it (Chrome's "seek to start" fudge room: streams rarely start at
        // exactly zero).
        let range = ranges
            .iter()
            .find(|&&(start, end)| start - tolerance <= time && time < end)
            .or_else(|| {
                ranges
                    .first()
                    .filter(|&&(start, _)| time < start && start - time <= 1_000_000)
            })
            .copied()?;
        let target = time.max(range.0);
        let mut best: Option<usize> = None;
        for (index, frame) in self.frames.iter().enumerate() {
            if !frame.keyframe || frame.pts < range.0 || frame.pts >= range.1 {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    let current = &self.frames[current];
                    if frame.pts <= target {
                        current.pts > target || frame.pts > current.pts
                    } else {
                        current.pts > target && frame.pts < current.pts
                    }
                }
            };
            if better {
                best = Some(index);
            }
        }
        best
    }
}

/// Where a reader of one track buffer is.
#[derive(Default)]
pub(super) struct Reader {
    /// Start from the random access point for this time.
    seek: Option<Micros>,
    /// The last frame read: decode time, presentation time, decode end.
    last: Option<(Micros, Micros, Micros)>,
    /// The next frame read must be a random access point.
    need_keyframe: bool,
    /// The track description of the frames read last.
    config: Option<Arc<TrackInfo>>,
}

/// What a reader found.
pub(super) enum Read {
    Packet(Packet),
    /// Nothing to read until more is appended.
    Pending,
    /// The end of the stream.
    End,
}

impl Reader {
    pub fn new(start: Micros) -> Self {
        Self {
            seek: Some(start),
            ..Self::default()
        }
    }

    pub fn seek(&mut self, time: Micros) {
        self.seek = Some(time);
        self.last = None;
        self.need_keyframe = false;
    }

    /// Where a seek to `time` would start reading, if it is buffered.
    pub fn landing(buffer: &mut TrackBuffer, time: Micros) -> Option<Micros> {
        buffer.start_for(time).map(|index| buffer.frames[index].pts)
    }

    /// The next frame of `buffer` in decode order. `ended`: the media
    /// source has all its media data.
    pub fn read(&mut self, buffer: &mut TrackBuffer, ended: bool) -> Read {
        let mut index = if let Some(time) = self.seek {
            match buffer.start_for(time) {
                Some(index) => {
                    self.seek = None;
                    index
                }
                None => {
                    let beyond = buffer.highest_frame_end().is_none_or(|end| time >= end);
                    return if ended && beyond {
                        Read::End
                    } else {
                        Read::Pending
                    };
                }
            }
        } else if let Some((dts, pts, end)) = self.last {
            let next = buffer
                .frames
                .partition_point(|f| (f.dts, f.pts) <= (dts, pts));
            // When the frame read last is gone (removed or overwritten),
            // frames after it may depend on others that were never read.
            let present = next > 0 && {
                let frame = &buffer.frames[next - 1];
                frame.dts == dts && frame.pts == pts
            };
            if !present {
                self.need_keyframe = true;
            }
            // A gap in decode time: wait for the frames that fill it.
            if let Some(frame) = buffer.frames.get(next)
                && !self.need_keyframe
                && frame.dts > end + buffer.tolerance()
            {
                return Read::Pending;
            }
            next
        } else {
            0
        };
        if self.need_keyframe {
            while index < buffer.frames.len() && !buffer.frames[index].keyframe {
                index += 1;
            }
        }
        let Some(frame) = buffer.frames.get(index) else {
            return if ended { Read::End } else { Read::Pending };
        };
        self.need_keyframe = false;
        self.last = Some((
            frame.dts,
            frame.pts,
            frame.dts.saturating_add(frame.duration),
        ));
        let config = match &self.config {
            Some(config) if Arc::ptr_eq(config, &frame.config) => None,
            _ => {
                self.config = Some(frame.config.clone());
                Some(frame.config.clone())
            }
        };
        Read::Packet(Packet {
            track: buffer.id,
            pts: frame.pts,
            dts: frame.dts,
            duration: frame.duration,
            keyframe: frame.keyframe,
            data: frame.data.clone(),
            encryption: frame.encryption.clone(),
            discard_padding: frame.discard_padding,
            config,
        })
    }
}
