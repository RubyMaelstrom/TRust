//! Media playback: the engine behind an HTML media element (Chrome's
//! WebMediaPlayer, Firefox's MediaDecoder).
//!
//! The element (HTML #media-elements) runs on the page actor and talks to a
//! [`Player`] through commands; the player reports what HTML needs to
//! fire its events — metadata, ready-state changes, seeks completing, the
//! end — through an [`Event`] callback.
//!
//! Each player runs three threads: a control thread that reads packets
//! (blocking on the network as needed), decodes audio and schedules work; a
//! video decode thread; and the video sink's presenter. Audio is rendered on
//! the output device's thread from the audio sink's queue, and the clock
//! follows the heard audio. Queues are bounded, idle threads block, and
//! dropping the player stops everything.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::Micros;
use super::audio::{Format, Output};
use super::container::Container;
use super::decode::{self, AudioDecoder, Registry, VideoDecoder};
use super::demux::{self, ByteSource, Demuxer};
use super::track::{Packet, TrackInfo, TrackKind};

mod audio_sink;
mod clock;
mod video_sink;

pub use video_sink::{VideoSurface, set_frame_waker, surface};

use audio_sink::AudioSink;
use clock::Clock;
use video_sink::VideoSink;

/// HTML's ready states (#dom-media-readystate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReadyState {
    Nothing = 0,
    Metadata = 1,
    CurrentData = 2,
    FutureData = 3,
    EnoughData = 4,
}

/// HTML's MediaError codes the engine can report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// MEDIA_ERR_NETWORK: fetching failed after the resource was usable.
    Network = 2,
    /// MEDIA_ERR_DECODE: the media data is corrupted.
    Decode = 3,
    /// MEDIA_ERR_SRC_NOT_SUPPORTED: the resource is not usable at all.
    SrcNotSupported = 4,
}

/// A track as HTML's AudioTrack/VideoTrack describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackDescription {
    pub id: u32,
    pub kind: TrackKind,
    pub codec: String,
    pub language: String,
    pub label: String,
    /// The track plays (the selected video track, enabled audio tracks).
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// HTML "once enough of the media data has been fetched to determine
    /// the duration of the media resource and its dimensions".
    Metadata {
        duration: Option<Micros>,
        seekable: bool,
        tracks: Vec<TrackDescription>,
        /// Display size of the video, when there is one.
        video_size: Option<(u32, u32)>,
    },
    ReadyState(ReadyState),
    Duration(Option<Micros>),
    /// The video's display size changed (HTML fires `resize`).
    Resize(u32, u32),
    /// Playback stalled for lack of data while playing.
    Waiting,
    /// Playback resumed after `Waiting`.
    Playing,
    /// A seek finished at this position.
    SeekDone(Micros),
    /// The current playback position while playing, every 250 ms (HTML
    /// fires `timeupdate` every 15 to 250 ms; Chrome and Firefox use 250).
    Time(Micros),
    /// Playback reached the end of the media resource.
    Ended,
    /// Encrypted media waits for a key no session has (EME #wait-for-key).
    WaitingForKey,
    Error(ErrorKind, String),
}

/// Where a player's media comes from.
pub enum Input {
    /// A media resource read through a byte source (HTML's resource fetch
    /// algorithm). `hint` is the container the MIME type names.
    Resource {
        source: Box<dyn ByteSource>,
        hint: Option<Container>,
    },
    /// Packets from a demuxer the caller built (Media Source Extensions).
    Demuxer(Box<dyn Demuxer>),
}

#[derive(Clone)]
pub enum AudioMode {
    /// The platform's audio device (falling back to silent output), heard
    /// as silence while the user mutes it.
    Device(crate::media::audio::Mute),
    /// Silent real-time output (headless use, tests).
    Silent,
    /// Rerouted into a Web Audio graph (MediaElementAudioSourceNode).
    Tap(crate::media::audio::Tap),
}

#[derive(Clone)]
pub struct Options {
    pub audio: AudioMode,
}

enum Command {
    Play,
    Pause,
    Seek { time: Micros, fast: bool },
    Volume { volume: f32, muted: bool },
    Rate { rate: f64, preserves_pitch: bool },
    Keys(Option<super::eme::KeyRing>),
    Route(crate::media::audio::Tap),
}

type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

struct Shared {
    commands: Mutex<VecDeque<Command>>,
    /// Woken for commands, and by the sinks when they need data.
    wake: Arc<(Mutex<bool>, Condvar)>,
    clock: Arc<Clock>,
    surface: Arc<VideoSurface>,
    stop: AtomicBool,
    ready: Mutex<ReadyState>,
    duration: Mutex<Option<Micros>>,
    video_presented: Arc<AtomicU64>,
    video_dropped: Arc<AtomicU64>,
}

/// One media element's playback engine.
pub struct Player {
    shared: Arc<Shared>,
    control: Mutex<Option<JoinHandle<()>>>,
    /// A deferred player's input until it starts.
    pending: Mutex<Option<Pending>>,
}

struct Pending {
    input: Input,
    options: Options,
    events: EventSink,
}

/// Counts for HTML `getVideoPlaybackQuality()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quality {
    pub presented: u64,
    pub dropped: u64,
}

impl Player {
    pub fn new(
        input: Input,
        options: Options,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        let player = Self::deferred(input, options, events);
        player.start();
        player
    }

    /// A player that reads nothing from its input, and runs no thread, until
    /// it starts (`start`, or a `play` command): HTML
    /// #concept-media-load-resource lets a user agent wait for the user to
    /// request playback before fetching, as for the `preload` attribute's
    /// None state. Its surface, clock and commands work meanwhile.
    pub fn deferred(
        input: Input,
        options: Options,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            commands: Mutex::new(VecDeque::new()),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            clock: Arc::new(Clock::new()),
            surface: VideoSurface::new(),
            stop: AtomicBool::new(false),
            ready: Mutex::new(ReadyState::Nothing),
            duration: Mutex::new(None),
            video_presented: Arc::new(AtomicU64::new(0)),
            video_dropped: Arc::new(AtomicU64::new(0)),
        });
        Self {
            shared,
            control: Mutex::new(None),
            pending: Mutex::new(Some(Pending {
                input,
                options,
                events: Arc::new(events),
            })),
        }
    }

    /// Start reading the input and decoding, once.
    pub fn start(&self) {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(Pending {
            input,
            options,
            events,
        }) = pending
        else {
            return;
        };
        let thread_shared = self.shared.clone();
        let control = std::thread::Builder::new()
            .name(String::from("trust-media"))
            .spawn(move || {
                let events_for_panic = events.clone();
                // A panic in parsing or decoding fails this element only.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Control::run(input, options, thread_shared, events)
                }));
                if result.is_err() {
                    events_for_panic(Event::Error(
                        ErrorKind::Decode,
                        String::from("media engine failure"),
                    ));
                }
            })
            .ok();
        *self.control.lock().unwrap_or_else(|e| e.into_inner()) = control;
    }

    fn command(&self, command: Command) {
        self.shared
            .commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(command);
        let (flag, condvar) = &*self.shared.wake;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
    }

    pub fn play(&self) {
        self.command(Command::Play);
        self.start();
    }

    pub fn pause(&self) {
        // Stop the clock at once so currentTime freezes with the call.
        self.shared.clock.set_running(false);
        self.command(Command::Pause);
    }

    pub fn seek(&self, time: Micros, fast: bool) {
        self.command(Command::Seek { time, fast });
    }

    pub fn set_volume(&self, volume: f32, muted: bool) {
        self.command(Command::Volume { volume, muted });
    }

    pub fn set_rate(&self, rate: f64, preserves_pitch: bool) {
        self.command(Command::Rate {
            rate,
            preserves_pitch,
        });
    }

    /// Decrypt with these keys (the media element's MediaKeys), or none.
    pub fn set_keys(&self, keys: Option<super::eme::KeyRing>) {
        self.command(Command::Keys(keys));
    }

    /// Reroute the audio into `tap` (a MediaElementAudioSourceNode): it is
    /// no longer heard directly.
    pub fn route(&self, tap: crate::media::audio::Tap) {
        self.command(Command::Route(tap));
    }

    /// The current playback position.
    pub fn current_time(&self) -> Micros {
        self.shared.clock.now()
    }

    pub fn ready_state(&self) -> ReadyState {
        *self.shared.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn duration(&self) -> Option<Micros> {
        *self
            .shared
            .duration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn surface(&self) -> Arc<VideoSurface> {
        self.shared.surface.clone()
    }

    pub fn quality(&self) -> Quality {
        Quality {
            presented: self.shared.video_presented.load(Ordering::Relaxed),
            dropped: self.shared.video_dropped.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let (flag, condvar) = &*self.shared.wake;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
        // The control thread may be blocked on the network; it notices the
        // stop at its next wake-up and tears down its own threads. Joining
        // here would stall the page actor, so detach instead.
        drop(
            self.control
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
    }
}

enum VideoMessage {
    Packet(u64, Packet),
    EndOfStream(u64),
    Flush(u64),
}

/// Whether a decoder made for track description `a` decodes packets of `b`.
fn same_decoder_config(a: &TrackInfo, b: &TrackInfo) -> bool {
    a.codec == b.codec && a.config == b.config && a.audio == b.audio && a.encryption == b.encryption
}

/// The video decode thread.
#[allow(clippy::too_many_arguments)]
fn video_thread(
    mut decoder: Box<dyn VideoDecoder>,
    mut track: Arc<TrackInfo>,
    messages: Receiver<VideoMessage>,
    sink: Arc<VideoSink>,
    generation: Arc<AtomicU64>,
    in_flight: Arc<AtomicUsize>,
    events: EventSink,
    shared: Arc<Shared>,
) {
    let mut current = generation.load(Ordering::Acquire);
    // After a corrupt packet, skip to the next keyframe.
    let mut need_keyframe = false;
    let mut size = None;
    while let Ok(message) = messages.recv() {
        match message {
            VideoMessage::Flush(generation) => {
                crate::media_log!("video decoder {}: flush", decoder.name());
                decoder.flush();
                current = generation;
                need_keyframe = false;
            }
            VideoMessage::Packet(generation_of_packet, packet) => {
                in_flight.fetch_sub(1, Ordering::AcqRel);
                if generation_of_packet != current {
                    continue;
                }
                if need_keyframe && !packet.keyframe {
                    continue;
                }
                need_keyframe = false;
                // A new initialization segment's configuration: finish the
                // old stream's frames, then decode with a new decoder.
                if let Some(config) = &packet.config
                    && !same_decoder_config(&track, config)
                {
                    if let Ok(frames) = decoder.drain() {
                        for frame in frames {
                            if !sink.push(frame, &generation, current) {
                                break;
                            }
                        }
                    }
                    crate::media_log!("video decoder: new configuration {}", config.codec_string);
                    match Registry::get().video_decoder(config) {
                        Ok(next) => {
                            decoder = next;
                            track = config.clone();
                        }
                        Err(error) => {
                            events(Event::Error(ErrorKind::Decode, error.to_string()));
                            shared.stop.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
                match decoder.decode(&packet) {
                    Ok(frames) => {
                        for frame in frames {
                            let display = (frame.display_width, frame.display_height);
                            if size != Some(display) {
                                size = Some(display);
                                events(Event::Resize(display.0, display.1));
                            }
                            if !sink.push(frame, &generation, current) {
                                break;
                            }
                        }
                    }
                    Err(decode::Error::Corrupt(reason)) => {
                        crate::media_log!(
                            "video decoder {}: corrupt packet at {}: {reason}",
                            decoder.name(),
                            packet.pts
                        );
                        decoder.flush();
                        need_keyframe = true;
                    }
                    Err(error) => {
                        crate::media_log!(
                            "video decoder {}: failed at packet pts {} dts {} size {} key {}: {error}",
                            decoder.name(),
                            packet.pts,
                            packet.dts,
                            packet.data.len(),
                            packet.keyframe
                        );
                        events(Event::Error(ErrorKind::Decode, error.to_string()));
                        shared.stop.store(true, Ordering::Release);
                        return;
                    }
                }
            }
            VideoMessage::EndOfStream(generation_of_message) => {
                if generation_of_message != current {
                    continue;
                }
                crate::media_log!("video decoder {}: end of stream", decoder.name());
                if let Ok(frames) = decoder.drain() {
                    for frame in frames {
                        if !sink.push(frame, &generation, current) {
                            break;
                        }
                    }
                }
                sink.set_end_of_stream();
            }
        }
    }
}

struct AudioState {
    track: TrackInfo,
    decoder: Box<dyn AudioDecoder>,
    sink: Option<Arc<AudioSink>>,
    end_of_stream: bool,
    /// The demuxer has no packet yet; retried when the player wakes.
    starved: bool,
    /// An encrypted packet waiting for its key.
    blocked: Option<Packet>,
    /// Decode time of the next packet, for choosing which track to read.
    next_dts: Micros,
}

struct VideoState {
    track: TrackInfo,
    messages: SyncSender<VideoMessage>,
    sink: Arc<VideoSink>,
    in_flight: Arc<AtomicUsize>,
    /// A packet the decode thread could not take yet.
    pending: Option<Packet>,
    end_of_stream: bool,
    starved: bool,
    blocked: Option<Packet>,
    next_dts: Micros,
    threads: Vec<JoinHandle<()>>,
}

/// Video packets queued for the decode thread.
const VIDEO_PACKETS: usize = 24;

struct Control {
    demuxer: Box<dyn Demuxer>,
    audio: Option<AudioState>,
    video: Option<VideoState>,
    generation: Arc<AtomicU64>,
    shared: Arc<Shared>,
    events: EventSink,
    options: Options,
    playing: bool,
    /// Audio before this time is discarded (a seek target, or the start).
    discard_before: Micros,
    seeking: Option<Micros>,
    waiting: bool,
    ended: bool,
    volume: (f32, bool),
    rate: (f64, bool),
    /// The end of the latest packet seen, for a duration the container
    /// did not state.
    end_seen: Micros,
    last_time_event: std::time::Instant,
    /// The media element's keys (Encrypted Media Extensions).
    keys: Option<super::eme::KeyRing>,
    /// Wakes the control thread when keys arrive.
    key_waker: Arc<dyn Fn() + Send + Sync>,
    waiting_for_key: bool,
}

/// A packet ready to decode: decrypted, or the packet still waiting for its
/// key, or a decryption failure.
enum Unlocked {
    Clear(Packet),
    Blocked(Packet),
    Failed,
}

fn unlock(keys: Option<&super::eme::KeyRing>, packet: Packet) -> Unlocked {
    if packet.encryption.is_none() {
        return Unlocked::Clear(packet);
    }
    let Some(keys) = keys else {
        return Unlocked::Blocked(packet);
    };
    match keys.decrypt(&packet) {
        Ok(clear) => Unlocked::Clear(clear),
        Err(super::eme::DecryptError::NoKey) => Unlocked::Blocked(packet),
        Err(super::eme::DecryptError::Malformed) => Unlocked::Failed,
    }
}

impl Control {
    fn run(input: Input, options: Options, shared: Arc<Shared>, events: EventSink) {
        let demuxer = match input {
            Input::Resource { source, hint } => match demux::open(source, hint) {
                Ok(demuxer) => demuxer,
                Err(demux::Error::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {
                    return;
                }
                Err(error) => {
                    // HTML #concept-media-load-resource: any failure before
                    // the resource is known to be usable (network errors,
                    // an unsupported container or codecs) makes it
                    // unusable; the element moves to its next candidate.
                    events(Event::Error(ErrorKind::SrcNotSupported, error.to_string()));
                    return;
                }
            },
            Input::Demuxer(demuxer) => demuxer,
        };
        let mut demuxer = demuxer;
        if demuxer.wait_for_tracks(&shared.stop).is_err() {
            // The media source went away before it described its tracks.
            return;
        }
        let wake = shared.wake.clone();
        let waker: demux::Waker = Arc::new(move || {
            let (flag, condvar) = &*wake;
            *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
            condvar.notify_all();
        });
        demuxer.set_waker(waker.clone());
        let mut control = Self {
            demuxer,
            audio: None,
            video: None,
            generation: Arc::new(AtomicU64::new(0)),
            shared,
            events,
            options,
            playing: false,
            discard_before: 0,
            seeking: None,
            waiting: false,
            ended: false,
            volume: (1.0, false),
            rate: (1.0, true),
            end_seen: 0,
            last_time_event: std::time::Instant::now(),
            keys: None,
            key_waker: waker,
            waiting_for_key: false,
        };
        if control.setup() {
            control.main_loop();
        }
        control.shutdown();
    }

    fn emit(&self, event: Event) {
        (self.events)(event);
    }

    /// Choose tracks, create decoders and sinks, report metadata.
    fn setup(&mut self) -> bool {
        let registry = Registry::get();
        let tracks = self.demuxer.tracks().to_vec();
        let pick = |kind: TrackKind| {
            tracks
                .iter()
                .filter(|t| t.kind == kind)
                .find(|t| t.default)
                .or_else(|| tracks.iter().find(|t| t.kind == kind))
                .cloned()
        };
        let mut failures = Vec::new();
        if let Some(track) = pick(TrackKind::Audio) {
            match registry.audio_decoder(&track) {
                Ok(decoder) => {
                    self.audio = Some(AudioState {
                        track,
                        decoder,
                        sink: None,
                        end_of_stream: false,
                        starved: false,
                        blocked: None,
                        next_dts: 0,
                    })
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        if let Some(track) = pick(TrackKind::Video) {
            match registry.video_decoder(&track) {
                Ok(decoder) => {
                    let (sender, receiver) = sync_channel(VIDEO_PACKETS);
                    let sink = VideoSink::new(
                        self.shared.clock.clone(),
                        self.shared.surface.clone(),
                        self.shared.wake.clone(),
                        (
                            self.shared.video_presented.clone(),
                            self.shared.video_dropped.clone(),
                        ),
                    );
                    sink.flush(None);
                    let in_flight = Arc::new(AtomicUsize::new(0));
                    let mut threads = Vec::new();
                    let presenter = sink.clone();
                    if let Ok(handle) = std::thread::Builder::new()
                        .name(String::from("trust-media-video-out"))
                        .spawn(move || presenter.run())
                    {
                        threads.push(handle);
                    }
                    let thread_track = Arc::new(track.clone());
                    let (thread_sink, generation, flight, events, shared) = (
                        sink.clone(),
                        self.generation.clone(),
                        in_flight.clone(),
                        self.events.clone(),
                        self.shared.clone(),
                    );
                    if let Ok(handle) = std::thread::Builder::new()
                        .name(String::from("trust-media-video"))
                        .spawn(move || {
                            video_thread(
                                decoder,
                                thread_track,
                                receiver,
                                thread_sink,
                                generation,
                                flight,
                                events,
                                shared,
                            )
                        })
                    {
                        threads.push(handle);
                    }
                    self.video = Some(VideoState {
                        track,
                        messages: sender,
                        sink,
                        in_flight,
                        pending: None,
                        end_of_stream: false,
                        starved: false,
                        blocked: None,
                        next_dts: 0,
                        threads,
                    });
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        if self.audio.is_none() && self.video.is_none() {
            let reason = if failures.is_empty() {
                String::from("no audio or video track")
            } else {
                failures.join("; ")
            };
            self.emit(Event::Error(ErrorKind::SrcNotSupported, reason));
            return false;
        }
        if self.audio.is_none() {
            self.shared.clock.use_system();
        }
        let duration = self.demuxer.duration();
        *self
            .shared
            .duration
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = duration;
        let active = |id: u32| {
            self.audio.as_ref().is_some_and(|a| a.track.id == id)
                || self.video.as_ref().is_some_and(|v| v.track.id == id)
        };
        let descriptions = tracks
            .iter()
            .filter(|t| t.kind != TrackKind::Text)
            .map(|t| TrackDescription {
                id: t.id,
                kind: t.kind,
                codec: t.codec_string.clone(),
                language: t.language.clone(),
                label: t.label.clone(),
                active: active(t.id),
            })
            .collect();
        let video_size = self
            .video
            .as_ref()
            .and_then(|v| v.track.video)
            .map(|v| (v.display_width, v.display_height))
            .filter(|&(w, h)| w > 0 && h > 0);
        self.emit(Event::Metadata {
            duration,
            seekable: self.demuxer.seekable(),
            tracks: descriptions,
            video_size,
        });
        self.set_ready(ReadyState::Metadata);
        true
    }

    fn set_ready(&self, state: ReadyState) {
        let mut ready = self.shared.ready.lock().unwrap_or_else(|e| e.into_inner());
        if *ready != state {
            *ready = state;
            drop(ready);
            self.emit(Event::ReadyState(state));
        }
    }

    fn take_commands(&mut self) -> Vec<Command> {
        self.shared
            .commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Play => {
                self.playing = true;
                if self.ended {
                    // Playing again after the end restarts from the start
                    // (HTML #dom-media-play step: ended + forwards seeks to
                    // the earliest possible position; the element sends
                    // that seek itself, so just un-end here).
                    self.ended = false;
                }
                self.update_running();
            }
            Command::Pause => {
                self.playing = false;
                self.update_running();
            }
            Command::Seek { time, fast } => self.seek(time, fast),
            Command::Volume { volume, muted } => {
                self.volume = (volume, muted);
                if let Some(sink) = self.audio.as_ref().and_then(|a| a.sink.as_ref()) {
                    sink.set_volume(volume, muted);
                }
            }
            Command::Keys(keys) => {
                if let Some(keys) = &keys {
                    keys.add_waker(&self.key_waker);
                }
                self.keys = keys;
            }
            Command::Route(tap) => {
                if let Some(sink) = self.audio.as_ref().and_then(|a| a.sink.as_ref()) {
                    // The device's unplayed buffer is dropped; the clock goes
                    // on from what was rendered.
                    let output = Output::tap(sink.format(), sink.clone(), &tap, sink.rendered());
                    output.set_paused(!self.shared.clock.running());
                    sink.attach(output);
                }
                self.options.audio = AudioMode::Tap(tap);
            }
            Command::Rate {
                rate,
                preserves_pitch,
            } => {
                self.rate = (rate, preserves_pitch);
                self.shared.clock.set_rate(rate);
                if let Some(sink) = self.audio.as_ref().and_then(|a| a.sink.as_ref()) {
                    sink.set_rate(rate, preserves_pitch);
                }
                if let Some(video) = &self.video {
                    video.sink.poke();
                }
            }
        }
    }

    /// Run the clock and output only while playing, not seeking, not
    /// stalled and not ended (HTML: "potentially playing").
    fn update_running(&mut self) {
        let running = self.playing && self.seeking.is_none() && !self.ended && self.rate.0 > 0.0;
        if let Some(sink) = self.audio.as_ref().and_then(|a| a.sink.as_ref()) {
            sink.set_paused(!running);
        }
        self.shared.clock.set_running(running);
        if let Some(video) = &self.video {
            video.sink.poke();
        }
    }

    fn seek(&mut self, time: Micros, fast: bool) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.ended = false;
        self.waiting = false;
        self.waiting_for_key = false;
        let landed = match self.demuxer.seek(time.max(0)) {
            Ok(landed) => landed,
            Err(error) => {
                self.emit(Event::Error(ErrorKind::Decode, error.to_string()));
                self.shared.stop.store(true, Ordering::Release);
                return;
            }
        };
        let target = if fast { landed.min(time) } else { time };
        self.discard_before = target;
        self.seeking = Some(target);
        if let Some(audio) = self.audio.as_mut() {
            audio.decoder.flush();
            audio.end_of_stream = false;
            audio.starved = false;
            audio.blocked = None;
            audio.next_dts = landed;
            if let Some(sink) = &audio.sink {
                sink.flush(target);
            }
        }
        if let Some(video) = self.video.as_mut() {
            video.pending = None;
            video.end_of_stream = false;
            video.starved = false;
            video.blocked = None;
            video.next_dts = landed;
            video.sink.flush(Some(target));
            // The decode thread may be gone after a decode error, which
            // was reported already.
            let _ = video.messages.send(VideoMessage::Flush(generation));
        }
        self.shared.clock.reset(target);
        self.update_running();
    }

    /// The seek finishes once the frame for the target is showing and
    /// audio is queued from the target (HTML #seeking step 12: "wait until
    /// the user agent has established whether or not the media data for the
    /// new playback position is available").
    fn check_seek(&mut self) {
        let Some(target) = self.seeking else {
            return;
        };
        let video_ready = self.video.as_ref().is_none_or(|v| v.sink.seek_ready());
        let audio_ready = self.audio.as_ref().is_none_or(|a| {
            a.end_of_stream
                || a.sink
                    .as_ref()
                    .is_some_and(|s| s.queued() >= 100_000 || s.finished_queueing())
        });
        if video_ready && audio_ready {
            self.seeking = None;
            self.shared.clock.reset(target);
            self.emit(Event::SeekDone(target));
            self.update_running();
        }
    }

    /// Read and route the next packet of whichever track needs data most.
    /// Returns false when no track needs data.
    fn fill(&mut self) -> bool {
        let audio_need = self.audio.as_ref().is_some_and(|a| {
            !a.end_of_stream
                && !a.starved
                && a.sink
                    .as_ref()
                    .is_none_or(|s| s.queued() < audio_sink::TARGET_AHEAD)
        });
        let video_need = self.video.as_ref().is_some_and(|v| {
            !v.end_of_stream
                && !v.starved
                && v.pending.is_none()
                && v.in_flight.load(Ordering::Acquire) < VIDEO_PACKETS
        });
        if let Some(video) = self.video.as_mut()
            && let Some(packet) = video.pending.take()
        {
            let generation = self.generation.load(Ordering::Acquire);
            video.in_flight.fetch_add(1, Ordering::AcqRel);
            match video
                .messages
                .try_send(VideoMessage::Packet(generation, packet))
            {
                Ok(()) => return true,
                Err(TrySendError::Full(VideoMessage::Packet(_, packet))) => {
                    video.in_flight.fetch_sub(1, Ordering::AcqRel);
                    video.pending = Some(packet);
                }
                Err(_) => {
                    video.in_flight.fetch_sub(1, Ordering::AcqRel);
                }
            }
        }
        let read_audio = match (audio_need, video_need) {
            (false, false) => return false,
            (true, false) => true,
            (false, true) => false,
            // Keep both tracks at the same point of the timeline, so a
            // progressive download reads the file in order.
            (true, true) => {
                self.audio.as_ref().map_or(Micros::MAX, |a| a.next_dts)
                    <= self.video.as_ref().map_or(Micros::MAX, |v| v.next_dts)
            }
        };
        if read_audio {
            self.read_audio();
        } else {
            self.read_video();
        }
        true
    }

    fn read_audio(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let read = match audio.blocked.take() {
            Some(packet) => Ok(Some(packet)),
            None => self.demuxer.read(audio.track.id),
        };
        let packet = match read {
            Ok(packet) => packet,
            Err(demux::Error::Pending) => {
                audio.starved = true;
                return;
            }
            Err(error) => {
                self.read_failed(error);
                return;
            }
        };
        let Some(packet) = packet else {
            audio.end_of_stream = true;
            if let Ok(blocks) = audio.decoder.drain() {
                for block in blocks {
                    Self::queue_audio(
                        audio,
                        block,
                        &packet_dummy(),
                        self.discard_before,
                        self.volume,
                        self.rate,
                        &self.options,
                        &self.shared,
                    );
                }
            }
            if let Some(sink) = &audio.sink {
                sink.set_end_of_stream();
            }
            return;
        };
        let packet = match unlock(self.keys.as_ref(), packet) {
            Unlocked::Clear(packet) => packet,
            Unlocked::Blocked(packet) => {
                audio.blocked = Some(packet);
                audio.starved = true;
                if !self.waiting_for_key {
                    self.waiting_for_key = true;
                    (self.events)(Event::WaitingForKey);
                }
                return;
            }
            Unlocked::Failed => {
                self.emit(Event::Error(
                    ErrorKind::Decode,
                    String::from("an encrypted sample could not be decrypted"),
                ));
                self.shared.stop.store(true, Ordering::Release);
                return;
            }
        };
        self.waiting_for_key = false;
        audio.next_dts = packet.dts;
        self.end_seen = self.end_seen.max(packet.end());
        if let Some(config) = &packet.config
            && !same_decoder_config(&audio.track, config)
        {
            // A new configuration: play out the old decoder's audio, then
            // decode with a new one (the sink adapts its blocks).
            if let Ok(blocks) = audio.decoder.drain() {
                for block in blocks {
                    Self::queue_audio(
                        audio,
                        block,
                        &packet_dummy(),
                        self.discard_before,
                        self.volume,
                        self.rate,
                        &self.options,
                        &self.shared,
                    );
                }
            }
            match Registry::get().audio_decoder(config) {
                Ok(decoder) => {
                    audio.decoder = decoder;
                    audio.track = (**config).clone();
                }
                Err(error) => {
                    self.emit(Event::Error(ErrorKind::Decode, error.to_string()));
                    self.shared.stop.store(true, Ordering::Release);
                    return;
                }
            }
        }
        match audio.decoder.decode(&packet) {
            Ok(blocks) => {
                for block in blocks {
                    Self::queue_audio(
                        audio,
                        block,
                        &packet,
                        self.discard_before,
                        self.volume,
                        self.rate,
                        &self.options,
                        &self.shared,
                    );
                }
            }
            // A corrupt packet is skipped; audio decoders resynchronise.
            Err(decode::Error::Corrupt(_)) => {}
            Err(error) => {
                self.emit(Event::Error(ErrorKind::Decode, error.to_string()));
                self.shared.stop.store(true, Ordering::Release);
            }
        }
    }

    /// Trim, adapt and queue one decoded audio block; opens the device on
    /// the first block, whose format the stream then keeps.
    #[allow(clippy::too_many_arguments)]
    fn queue_audio(
        audio: &mut AudioState,
        mut block: super::frame::AudioBlock,
        packet: &Packet,
        discard_before: Micros,
        volume: (f32, bool),
        rate: (f64, bool),
        options: &Options,
        shared: &Arc<Shared>,
    ) {
        block.trim_end(packet.discard_padding);
        // Audio before the timeline's start (codec delay, priming) or
        // before a seek target is not played.
        let start = discard_before.max(0);
        if block.end() <= start {
            return;
        }
        if block.pts < start {
            block.trim_start(start - block.pts);
        }
        if block.frames() == 0 {
            return;
        }
        let sink = match &audio.sink {
            Some(sink) => sink.clone(),
            None => {
                let format = Format {
                    rate: block.rate,
                    channels: block.channels.clamp(1, 8),
                };
                let sink = AudioSink::new(format, shared.wake.clone());
                sink.set_volume(volume.0, volume.1);
                sink.set_rate(rate.0, rate.1);
                let output = match &options.audio {
                    AudioMode::Device(mute) => Output::open(format, mute.gate(sink.clone()))
                        .unwrap_or_else(|_| Output::null(format, sink.clone())),
                    AudioMode::Silent => Output::null(format, sink.clone()),
                    AudioMode::Tap(tap) => Output::tap(format, sink.clone(), tap, 0),
                };
                output.set_paused(!shared.clock.running());
                sink.attach(output);
                sink.flush(start.max(block.pts));
                shared.clock.use_audio(sink.clone());
                audio.sink = Some(sink.clone());
                sink
            }
        };
        sink.push(audio_sink::adapt(block, sink.format()));
    }

    fn read_video(&mut self) {
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let read = match video.blocked.take() {
            Some(packet) => Ok(Some(packet)),
            None => self.demuxer.read(video.track.id),
        };
        let packet = match read {
            Ok(packet) => packet,
            Err(demux::Error::Pending) => {
                video.starved = true;
                return;
            }
            Err(error) => {
                self.read_failed(error);
                return;
            }
        };
        let generation = self.generation.load(Ordering::Acquire);
        let Some(packet) = packet else {
            video.end_of_stream = true;
            let _ = video.messages.send(VideoMessage::EndOfStream(generation));
            return;
        };
        let packet = match unlock(self.keys.as_ref(), packet) {
            Unlocked::Clear(packet) => packet,
            Unlocked::Blocked(packet) => {
                video.blocked = Some(packet);
                video.starved = true;
                if !self.waiting_for_key {
                    self.waiting_for_key = true;
                    (self.events)(Event::WaitingForKey);
                }
                return;
            }
            Unlocked::Failed => {
                self.emit(Event::Error(
                    ErrorKind::Decode,
                    String::from("an encrypted sample could not be decrypted"),
                ));
                self.shared.stop.store(true, Ordering::Release);
                return;
            }
        };
        self.waiting_for_key = false;
        video.next_dts = packet.dts;
        self.end_seen = self.end_seen.max(packet.end());
        video.in_flight.fetch_add(1, Ordering::AcqRel);
        match video
            .messages
            .try_send(VideoMessage::Packet(generation, packet))
        {
            Ok(()) => {}
            Err(TrySendError::Full(VideoMessage::Packet(_, packet))) => {
                video.in_flight.fetch_sub(1, Ordering::AcqRel);
                video.pending = Some(packet);
            }
            Err(_) => {
                video.in_flight.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn read_failed(&mut self, error: demux::Error) {
        if let demux::Error::Io(io) = &error
            && io.kind() == std::io::ErrorKind::Interrupted
        {
            return;
        }
        let kind = match error {
            demux::Error::Io(_) => ErrorKind::Network,
            _ => ErrorKind::Decode,
        };
        self.emit(Event::Error(kind, error.to_string()));
        self.shared.stop.store(true, Ordering::Release);
    }

    /// Ready state, underflow and end of playback.
    fn update_state(&mut self) {
        if self.seeking.is_some() {
            return;
        }
        let now = self.shared.clock.now();
        let audio_eos = self.audio.as_ref().is_none_or(|a| a.end_of_stream);
        let video_eos = self.video.as_ref().is_none_or(|v| v.end_of_stream);
        let audio_queued = self
            .audio
            .as_ref()
            .and_then(|a| a.sink.as_ref())
            .map_or(0, |s| s.queued());
        let audio_current = self
            .audio
            .as_ref()
            .is_none_or(|a| audio_queued > 0 || a.end_of_stream);
        // Video state comes from the sink: the demuxer running out of
        // packets says nothing about frames being decoded.
        let video_current = self
            .video
            .as_ref()
            .is_none_or(|v| v.sink.has_current() || v.sink.drained());
        let audio_future = self.audio.as_ref().is_none_or(|a| {
            audio_queued >= 200_000
                || a.sink.as_ref().is_some_and(|s| s.finished_queueing())
                || a.end_of_stream
        });
        let video_future = self
            .video
            .as_ref()
            .is_none_or(|v| v.sink.queued() >= 1 || v.sink.end_of_stream());
        let audio_enough = self
            .audio
            .as_ref()
            .is_none_or(|a| audio_queued >= audio_sink::LOW_WATER || a.end_of_stream);
        let video_enough = self
            .video
            .as_ref()
            .is_none_or(|v| v.sink.queued() + 1 >= v.sink.capacity() || v.sink.end_of_stream());
        let state = if !(audio_current && video_current) {
            ReadyState::Metadata
        } else if !(audio_future && video_future) {
            ReadyState::CurrentData
        } else if audio_enough && video_enough {
            ReadyState::EnoughData
        } else {
            ReadyState::FutureData
        };
        // The end of playback: everything queued has been played.
        let audio_done = self
            .audio
            .as_ref()
            .is_none_or(|a| a.end_of_stream && a.sink.as_ref().is_none_or(|s| s.drained()));
        let video_done = self.video.as_ref().is_none_or(|v| v.sink.drained());
        if audio_eos && video_eos && audio_done && video_done && self.playing && !self.ended {
            let duration = *self
                .shared
                .duration
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let end = duration.unwrap_or(self.end_seen).max(now);
            if duration.is_none() {
                *self
                    .shared
                    .duration
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(end);
                self.emit(Event::Duration(Some(end)));
            }
            self.ended = true;
            self.shared.clock.reset(end);
            self.update_running();
            self.set_ready(ReadyState::EnoughData);
            self.emit(Event::Ended);
            return;
        }
        // Underflow while playing: wait for data (HTML fires `waiting`).
        let starved = self.playing
            && !self.ended
            && match (&self.audio, &self.video) {
                (Some(audio), _) => {
                    audio.sink.as_ref().is_some_and(|s| s.starved()) && !audio.end_of_stream
                }
                (None, Some(video)) => video.sink.queued() == 0 && !video.sink.end_of_stream(),
                (None, None) => false,
            };
        if self.audio.is_none() {
            self.shared.clock.set_stalled(starved);
        }
        if starved && !self.waiting && state < ReadyState::FutureData {
            self.waiting = true;
            self.set_ready(state.min(ReadyState::CurrentData));
            self.emit(Event::Waiting);
            return;
        }
        if self.waiting && state >= ReadyState::FutureData {
            self.waiting = false;
            self.set_ready(state);
            self.emit(Event::Playing);
            return;
        }
        if !self.waiting {
            self.set_ready(state);
        }
        if self.shared.clock.running()
            && self.last_time_event.elapsed() >= Duration::from_millis(250)
        {
            self.last_time_event = std::time::Instant::now();
            self.emit(Event::Time(now));
        }
    }

    fn main_loop(&mut self) {
        loop {
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            for command in self.take_commands() {
                self.apply(command);
            }
            // Work while any queue wants data, checking commands between
            // packets.
            let mut worked = false;
            for _ in 0..8 {
                if self.shared.stop.load(Ordering::Acquire)
                    || !self
                        .shared
                        .commands
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty()
                {
                    break;
                }
                if !self.fill() {
                    break;
                }
                worked = true;
            }
            self.check_seek();
            self.update_state();
            if worked {
                continue;
            }
            // Idle: sleep until a command or a sink needs data. While
            // playing, re-evaluate a few times a second for the end of
            // playback and underflow; paused and full, sleep until woken.
            let (flag, condvar) = &*self.shared.wake;
            let mut woken = flag.lock().unwrap_or_else(|e| e.into_inner());
            if !*woken {
                let timeout = if self.playing && !self.ended {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(3600)
                };
                woken = condvar
                    .wait_timeout(woken, timeout)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            *woken = false;
            drop(woken);
            // Tracks the demuxer had nothing for try again.
            if let Some(audio) = self.audio.as_mut() {
                audio.starved = false;
            }
            if let Some(video) = self.video.as_mut() {
                video.starved = false;
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(video) = self.video.take() {
            video.sink.stop();
            drop(video.messages);
            for thread in video.threads {
                let _ = thread.join();
            }
        }
        if let Some(audio) = self.audio.take() {
            drop(audio);
        }
        self.shared.surface.clear();
    }
}

/// A stand-in packet for blocks drained at the end of a stream.
fn packet_dummy() -> Packet {
    Packet {
        track: 0,
        pts: 0,
        dts: 0,
        duration: 0,
        keyframe: true,
        data: Arc::from(&[][..]),
        encryption: None,
        discard_padding: 0,
        config: None,
    }
}

#[cfg(test)]
mod tests;
