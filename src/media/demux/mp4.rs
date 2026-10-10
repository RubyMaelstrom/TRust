//! Progressive MP4 (ISO BMFF) playback: sample tables of a `moov`, or the
//! movie fragments of a fragmented file read in order.

use std::collections::VecDeque;
use std::sync::Arc;

use super::{ByteSource, Demuxer, Error};
use crate::media::isobmff::{self, BoxHeader, Movie};
use crate::media::track::{Packet, TrackInfo};
use crate::media::{Micros, ticks_to_micros};

/// Upper bound on a movie box read into memory.
const MAX_MOOV: u64 = 128 << 20;
/// Upper bound on a movie fragment box.
const MAX_MOOF: u64 = 16 << 20;
/// Upper bound on one sample.
const MAX_SAMPLE: u32 = 64 << 20;

struct FragmentIndexEntry {
    offset: u64,
    /// Earliest presentation time of each track in the fragment.
    times: Vec<(u32, Micros)>,
    /// Latest presentation end of any sample in the fragment.
    end: Micros,
}

pub(super) struct Mp4Demuxer {
    source: Box<dyn ByteSource>,
    movie: Movie,
    tracks: Vec<TrackInfo>,
    /// Non-fragmented: the next sample index of each track.
    cursors: Vec<usize>,
    /// Fragmented: file offset of the next top-level box to examine.
    fragment_cursor: u64,
    first_fragment: Option<u64>,
    queues: Vec<VecDeque<Packet>>,
    /// Decode time (media ticks) where each track's next fragment starts,
    /// for fragments without `tfdt`.
    next_decode_time: Vec<i64>,
    fragment_index: Vec<FragmentIndexEntry>,
    index_complete: bool,
    ended: bool,
    /// The presentation end of the last fragment, for fragmented files that
    /// state no duration (an init segment written before the media).
    measured_duration: Option<Micros>,
}

impl Mp4Demuxer {
    pub(super) fn open(mut source: Box<dyn ByteSource>) -> Result<Self, Error> {
        let mut offset = 0u64;
        let mut moov = None;
        let mut first_fragment = None;
        let mut sidx = None;
        // Walk top-level boxes by their headers alone, so a movie box at the
        // end of the file is found without reading the media data.
        for _ in 0..100_000 {
            let header_bytes = source.read_up_to(offset, 32)?;
            let Some(header) = BoxHeader::parse(&header_bytes)? else {
                break;
            };
            let size = header
                .size
                .or_else(|| source.len().map(|len| len.saturating_sub(offset)));
            match &header.kind {
                b"moov" => {
                    let Some(size) = size else {
                        return Err(Error::Malformed(String::from("open-ended movie box")));
                    };
                    if size > MAX_MOOV {
                        return Err(Error::Unsupported(String::from(
                            "movie box larger than 128 MiB",
                        )));
                    }
                    let payload = source.read_exact_at(
                        offset + header.header_len,
                        (size - header.header_len) as usize,
                    )?;
                    moov = Some(isobmff::parse_moov(&payload)?);
                }
                b"moof" => {
                    if first_fragment.is_none() {
                        first_fragment = Some(offset);
                    }
                }
                b"sidx" if sidx.is_none() => {
                    if let Some(size) = size.filter(|&s| s <= 1 << 20) {
                        let payload = source.read_exact_at(
                            offset + header.header_len,
                            (size - header.header_len) as usize,
                        )?;
                        sidx = isobmff::parse_sidx(&payload, offset + size).ok();
                    }
                }
                _ => {}
            }
            let fragmented = moov.as_ref().is_some_and(|m: &Movie| m.fragmented);
            if moov.is_some() && (!fragmented || first_fragment.is_some()) {
                break;
            }
            let Some(size) = size else {
                break;
            };
            if size == 0 {
                break;
            }
            offset = offset.saturating_add(size);
            if source.len().is_some_and(|len| offset >= len) {
                break;
            }
        }
        let Some(movie) = moov else {
            return Err(Error::Malformed(String::from("no movie box")));
        };
        let tracks: Vec<TrackInfo> = movie.tracks.iter().map(|t| t.info.clone()).collect();
        let count = tracks.len();
        let mut demuxer = Self {
            source,
            cursors: vec![0; count],
            fragment_cursor: first_fragment.unwrap_or(u64::MAX),
            first_fragment,
            queues: (0..count).map(|_| VecDeque::new()).collect(),
            next_decode_time: vec![0; count],
            fragment_index: Vec::new(),
            index_complete: false,
            ended: false,
            measured_duration: None,
            tracks,
            movie,
        };
        if demuxer.movie.fragmented
            && let Some((timescale, references)) = sidx
        {
            // A segment index locates every fragment without reading them.
            for reference in references.iter().filter(|r| !r.index) {
                let time = ticks_to_micros(
                    reference.time.min(i64::MAX as u64) as i64,
                    u64::from(timescale),
                );
                let end = ticks_to_micros(
                    reference
                        .time
                        .saturating_add(u64::from(reference.duration))
                        .min(i64::MAX as u64) as i64,
                    u64::from(timescale),
                );
                demuxer.fragment_index.push(FragmentIndexEntry {
                    offset: reference.offset,
                    times: demuxer.tracks.iter().map(|t| (t.id, time)).collect(),
                    end,
                });
            }
            demuxer.index_complete = !demuxer.fragment_index.is_empty();
        }
        // Like Firefox, measure a fragmented file without a stated duration
        // from its last fragment, when the whole file is reachable.
        if demuxer.movie.fragmented
            && demuxer.movie.duration.is_none()
            && demuxer.source.seekable()
            && demuxer.source.len().is_some()
        {
            demuxer.build_fragment_index()?;
            demuxer.measured_duration = demuxer.fragment_index.iter().map(|e| e.end).max();
        }
        Ok(demuxer)
    }

    fn track_index(&self, id: u32) -> Option<usize> {
        self.tracks.iter().position(|track| track.id == id)
    }

    fn read_sample(&mut self, index: usize) -> Result<Option<Packet>, Error> {
        let track = &self.movie.tracks[index];
        let Some(&sample) = track.samples.get(self.cursors[index]) else {
            return Ok(None);
        };
        self.cursors[index] += 1;
        if sample.size > MAX_SAMPLE {
            return Err(Error::Unsupported(String::from(
                "sample larger than 64 MiB",
            )));
        }
        let data = self
            .source
            .read_exact_at(sample.offset, sample.size as usize)?;
        let track = &self.movie.tracks[index];
        Ok(Some(Packet {
            track: track.info.id,
            pts: track.presentation_micros(sample.dts, i64::from(sample.cts_offset)),
            dts: track.decode_micros(sample.dts),
            duration: ticks_to_micros(i64::from(sample.duration), u64::from(track.timescale)),
            keyframe: sample.sync,
            data: Arc::from(data),
            encryption: None,
            discard_padding: 0,
            config: None,
        }))
    }

    /// Parse the next movie fragment into the track queues. `false` at the
    /// end of the file.
    fn next_fragment(&mut self) -> Result<bool, Error> {
        loop {
            if self.ended || self.fragment_cursor == u64::MAX {
                return Ok(false);
            }
            let offset = self.fragment_cursor;
            let header_bytes = self.source.read_up_to(offset, 32)?;
            let Some(header) = BoxHeader::parse(&header_bytes)? else {
                self.ended = true;
                return Ok(false);
            };
            let Some(size) = header
                .size
                .or_else(|| self.source.len().map(|len| len.saturating_sub(offset)))
            else {
                self.ended = true;
                return Ok(false);
            };
            self.fragment_cursor = offset.saturating_add(size);
            if &header.kind != b"moof" {
                continue;
            }
            if size > MAX_MOOF {
                return Err(Error::Unsupported(String::from(
                    "movie fragment larger than 16 MiB",
                )));
            }
            let payload = self.source.read_exact_at(
                offset + header.header_len,
                (size - header.header_len) as usize,
            )?;
            let fragment = isobmff::parse_moof(&payload, offset, &self.movie)?;
            let mut times = Vec::new();
            let mut end = Micros::MIN;
            for traf in fragment.tracks {
                let Some(index) = self.track_index(traf.track_id) else {
                    continue;
                };
                let track = &self.movie.tracks[index];
                let rebase = if traf.decode_time.is_some() {
                    0
                } else {
                    self.next_decode_time[index]
                };
                let mut earliest = Micros::MAX;
                let mut latest = Micros::MIN;
                for sample in traf.samples {
                    if sample.size > MAX_SAMPLE {
                        return Err(Error::Unsupported(String::from(
                            "sample larger than 64 MiB",
                        )));
                    }
                    let position = offset as i64 + sample.moof_offset;
                    if position < 0 {
                        return Err(Error::Malformed(String::from("fragment data offset")));
                    }
                    let data = self
                        .source
                        .read_exact_at(position as u64, sample.size as usize)?;
                    let dts = sample.dts + rebase;
                    let pts = track.presentation_micros(dts, sample.cts_offset);
                    earliest = earliest.min(pts);
                    latest = latest.max(
                        pts + ticks_to_micros(
                            i64::from(sample.duration),
                            u64::from(track.timescale),
                        ),
                    );
                    self.next_decode_time[index] = dts + i64::from(sample.duration);
                    self.queues[index].push_back(Packet {
                        track: track.info.id,
                        pts,
                        dts: track.decode_micros(dts),
                        duration: ticks_to_micros(
                            i64::from(sample.duration),
                            u64::from(track.timescale),
                        ),
                        keyframe: sample.sync,
                        data: Arc::from(data),
                        encryption: sample.encryption,
                        discard_padding: 0,
                        config: None,
                    });
                }
                if earliest != Micros::MAX {
                    times.push((track.info.id, earliest));
                    end = end.max(latest);
                }
            }
            if !self.index_complete && !self.fragment_index.iter().any(|e| e.offset == offset) {
                self.fragment_index
                    .push(FragmentIndexEntry { offset, times, end });
            }
            return Ok(true);
        }
    }

    /// Complete the fragment index by reading every remaining fragment's
    /// headers (fragments are small; their media data is skipped).
    fn build_fragment_index(&mut self) -> Result<(), Error> {
        if self.index_complete {
            return Ok(());
        }
        let Some(mut offset) = self.first_fragment else {
            self.index_complete = true;
            return Ok(());
        };
        let mut next_decode_time = vec![0i64; self.tracks.len()];
        let mut entries = Vec::new();
        loop {
            let header_bytes = self.source.read_up_to(offset, 32)?;
            let Some(header) = BoxHeader::parse(&header_bytes)? else {
                break;
            };
            let Some(size) = header
                .size
                .or_else(|| self.source.len().map(|len| len.saturating_sub(offset)))
                .filter(|&size| size > 0)
            else {
                break;
            };
            if &header.kind == b"moof" && size <= MAX_MOOF {
                let payload = self.source.read_exact_at(
                    offset + header.header_len,
                    (size - header.header_len) as usize,
                )?;
                let fragment = isobmff::parse_moof(&payload, offset, &self.movie)?;
                let mut times = Vec::new();
                let mut end = Micros::MIN;
                for traf in &fragment.tracks {
                    let Some(index) = self.track_index(traf.track_id) else {
                        continue;
                    };
                    let track = &self.movie.tracks[index];
                    let rebase = if traf.decode_time.is_some() {
                        0
                    } else {
                        next_decode_time[index]
                    };
                    if let Some(first) = traf.samples.first() {
                        times.push((
                            track.info.id,
                            track.presentation_micros(first.dts + rebase, first.cts_offset),
                        ));
                    }
                    if let Some(last) = traf.samples.last() {
                        next_decode_time[index] = last.dts + rebase + i64::from(last.duration);
                    }
                    for sample in &traf.samples {
                        let pts = track.presentation_micros(sample.dts + rebase, sample.cts_offset);
                        end = end.max(
                            pts + ticks_to_micros(
                                i64::from(sample.duration),
                                u64::from(track.timescale),
                            ),
                        );
                    }
                }
                entries.push(FragmentIndexEntry { offset, times, end });
            }
            offset = offset.saturating_add(size);
            if self.source.len().is_some_and(|len| offset >= len) {
                break;
            }
        }
        self.fragment_index = entries;
        self.index_complete = true;
        Ok(())
    }
}

impl Demuxer for Mp4Demuxer {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn duration(&self) -> Option<Micros> {
        self.movie
            .duration
            .or_else(|| self.tracks.iter().filter_map(|t| t.duration).max())
            .or(self.measured_duration)
    }

    fn read(&mut self, track: u32) -> Result<Option<Packet>, Error> {
        let Some(index) = self.track_index(track) else {
            return Ok(None);
        };
        if !self.movie.fragmented {
            return self.read_sample(index);
        }
        loop {
            if let Some(packet) = self.queues[index].pop_front() {
                return Ok(Some(packet));
            }
            if !self.next_fragment()? {
                return Ok(None);
            }
            // Bound memory for tracks nobody reads (an audio track of a
            // muted video, say): keep at most a few seconds queued.
            for (other, queue) in self.queues.iter_mut().enumerate() {
                if other != index && queue.len() > 2000 {
                    queue.drain(..queue.len() - 2000);
                }
            }
        }
    }

    fn seek(&mut self, time: Micros) -> Result<Micros, Error> {
        if !self.movie.fragmented {
            let mut earliest = time;
            for (index, track) in self.movie.tracks.iter().enumerate() {
                let mut chosen = 0usize;
                let mut chosen_time = Micros::MIN;
                for (i, sample) in track.samples.iter().enumerate() {
                    if !sample.sync {
                        continue;
                    }
                    let pts = track.presentation_micros(sample.dts, i64::from(sample.cts_offset));
                    if pts <= time {
                        chosen = i;
                        chosen_time = pts;
                    } else {
                        break;
                    }
                }
                self.cursors[index] = chosen;
                if chosen_time != Micros::MIN {
                    earliest = earliest.min(chosen_time);
                } else {
                    earliest = earliest.min(0);
                }
            }
            return Ok(earliest.max(0));
        }
        self.build_fragment_index()?;
        let mut target = None;
        for entry in &self.fragment_index {
            let start = entry.times.iter().map(|&(_, t)| t).min().unwrap_or(0);
            if start <= time || target.is_none() {
                target = Some((entry.offset, start));
            }
            if start > time {
                break;
            }
        }
        for queue in &mut self.queues {
            queue.clear();
        }
        self.ended = false;
        match target {
            Some((offset, start)) => {
                self.fragment_cursor = offset;
                Ok(start.min(time))
            }
            None => {
                self.fragment_cursor = self.first_fragment.unwrap_or(u64::MAX);
                Ok(0)
            }
        }
    }

    fn seekable(&self) -> bool {
        self.source.seekable()
    }
}
