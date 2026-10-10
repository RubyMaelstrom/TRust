//! Progressive WebM/Matroska playback over the incremental parser.

use std::collections::VecDeque;

use super::{ByteSource, Demuxer, Error};
use crate::media::Micros;
use crate::media::track::{Packet, TrackInfo};
use crate::media::webm::{self, CuePoint, Event, Parser};

/// Bytes pushed into the parser per read.
const CHUNK: usize = 64 << 10;
/// Packets a track nobody is reading may hold before older ones are
/// dropped (bounding memory for, e.g., an unselected audio track).
const MAX_QUEUE: usize = 2000;

pub(super) struct WebmDemuxer {
    source: Box<dyn ByteSource>,
    parser: Parser,
    tracks: Vec<TrackInfo>,
    duration: Option<Micros>,
    queues: Vec<VecDeque<Packet>>,
    segment_data: u64,
    first_cluster: Option<u64>,
    cues: Option<Vec<CuePoint>>,
    cues_position: Option<u64>,
    ended: bool,
}

impl WebmDemuxer {
    pub(super) fn open(source: Box<dyn ByteSource>) -> Result<Self, Error> {
        let mut demuxer = Self {
            source,
            parser: Parser::new(),
            tracks: Vec::new(),
            duration: None,
            queues: Vec::new(),
            segment_data: 0,
            first_cluster: None,
            cues: None,
            cues_position: None,
            ended: false,
        };
        // Read until the Tracks element has been parsed.
        let mut header_seen = false;
        for _ in 0..4096 {
            if header_seen {
                break;
            }
            let events = demuxer.pump()?;
            for event in events {
                if let Event::Header(_) = event {
                    header_seen = true;
                }
            }
            if demuxer.ended && !header_seen {
                break;
            }
        }
        if !header_seen {
            return Err(Error::Malformed(String::from("no Tracks element")));
        }
        if demuxer.tracks.is_empty() {
            return Err(Error::Unsupported(String::from("no playable tracks")));
        }
        Ok(demuxer)
    }

    /// Push the next chunk of the resource through the parser and file its
    /// events. Returns the events for callers that wait for one.
    fn pump(&mut self) -> Result<Vec<Event>, Error> {
        if self.ended {
            return Ok(Vec::new());
        }
        let position = self.parser.position();
        let bytes = self.source.read_up_to(position, CHUNK)?;
        let mut events = Vec::new();
        if bytes.is_empty() {
            self.ended = true;
            return Ok(events);
        }
        self.parser.push(&bytes, &mut events)?;
        for event in &events {
            self.handle(event);
        }
        Ok(events)
    }

    fn handle(&mut self, event: &Event) {
        match event {
            Event::SegmentStart { data_offset } => self.segment_data = *data_offset,
            Event::Header(header) => {
                self.tracks = header.tracks.clone();
                self.duration = header.duration;
                self.queues = self.tracks.iter().map(|_| VecDeque::new()).collect();
            }
            Event::SeekHead(entries) => {
                if let Some(&(_, position)) = entries.iter().find(|(id, _)| *id == webm::CUES_ID) {
                    self.cues_position = Some(position);
                }
            }
            Event::Cues(cues) => {
                if self.cues.is_none() {
                    self.cues = Some(cues.clone());
                }
            }
            Event::ClusterStart { offset, .. } => {
                if self.first_cluster.is_none() {
                    self.first_cluster = Some(*offset);
                }
            }
            Event::Frame(frame) => {
                let Some(index) = self.tracks.iter().position(|t| t.id == frame.track) else {
                    return;
                };
                let queue = &mut self.queues[index];
                if queue.len() >= MAX_QUEUE {
                    queue.pop_front();
                }
                queue.push_back(Packet {
                    track: frame.track,
                    pts: frame.pts,
                    dts: frame.pts,
                    duration: frame.duration.unwrap_or(0),
                    keyframe: frame.keyframe,
                    data: frame.data.clone(),
                    encryption: frame.encryption.clone(),
                    discard_padding: frame.discard_padding,
                    config: None,
                });
            }
            Event::DocType(_) | Event::ClusterEnd => {}
        }
    }

    /// Load the Cues a SeekHead points at, if they have not been seen.
    fn load_cues(&mut self) -> Result<(), Error> {
        if self.cues.is_some() {
            return Ok(());
        }
        let Some(position) = self.cues_position else {
            return Ok(());
        };
        let offset = self.segment_data + position;
        let header = self.source.read_up_to(offset, 16)?;
        let Some(size) = webm::element_size(&header) else {
            return Ok(());
        };
        if size > 16 << 20 {
            return Ok(());
        }
        let bytes = self.source.read_exact_at(offset, size as usize)?;
        self.cues = Some(webm::parse_cues_element(
            &bytes,
            self.parser.timestamp_scale(),
        )?);
        Ok(())
    }
}

impl Demuxer for WebmDemuxer {
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
            if let Some(packet) = self.queues[index].pop_front() {
                return Ok(Some(packet));
            }
            if self.ended {
                return Ok(None);
            }
            self.pump()?;
        }
    }

    fn seek(&mut self, time: Micros) -> Result<Micros, Error> {
        self.load_cues()?;
        // Prefer cues of a video track: its keyframes are the constraint.
        let video = self
            .tracks
            .iter()
            .find(|t| t.kind == crate::media::track::TrackKind::Video)
            .map(|t| t.id);
        let target = self.cues.as_ref().and_then(|cues| {
            cues.iter()
                .filter(|cue| video.is_none_or(|id| cue.track == id) && cue.time <= time)
                .max_by_key(|cue| cue.time)
                .copied()
        });
        for queue in &mut self.queues {
            queue.clear();
        }
        self.ended = false;
        let segment_end = self.parser.segment_end();
        match target {
            Some(cue) => {
                self.parser
                    .resume_at(self.segment_data + cue.cluster_offset, segment_end);
                Ok(cue.time)
            }
            None => {
                // No index: restart from the first cluster; the player
                // decodes forward to the target.
                let first = self.first_cluster.unwrap_or(self.segment_data);
                self.parser.resume_at(first, segment_end);
                Ok(0)
            }
        }
    }

    fn seekable(&self) -> bool {
        self.source.seekable()
    }
}
