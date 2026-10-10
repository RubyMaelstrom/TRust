//! PulseAudio native-protocol output (served by PulseAudio and PipeWire's
//! pipewire-pulse), through the pure-Rust `pulseaudio` client: the backend
//! Firefox (cubeb's pulse-rust) and Chrome use on Linux.
//!
//! One client connection serves the process; each [`Pulse`] is a playback
//! stream on the default sink with a ~100 ms buffer (Firefox's media stream
//! latency). The server pulls audio through a callback on the client's
//! reactor thread. The audible position comes from the server's timing
//! reports (`GET_PLAYBACK_LATENCY`: read index minus sink latency), polled
//! while the stream runs and interpolated between polls.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulseaudio::protocol;

use super::{Backend, Error, Format, Position, Render};

/// The process's connection, made on first use. `None` once a connection
/// attempt failed, so later streams fall back without retrying.
fn client() -> Option<Arc<pulseaudio::Client>> {
    static CLIENT: OnceLock<Option<Arc<pulseaudio::Client>>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let name = CString::new("TRust").expect("no NUL");
            pulseaudio::Client::from_env(name).ok().map(Arc::new)
        })
        .clone()
}

fn channel_map(channels: u16) -> Option<protocol::ChannelMap> {
    use protocol::ChannelPosition::*;
    // WAVE-order defaults (the order TRust's decoders output) for the
    // layouts web media uses.
    let positions: &[protocol::ChannelPosition] = match channels {
        1 => &[Mono],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, RearLeft, RearRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, RearLeft, RearRight],
        6 => &[FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
        7 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            RearCenter,
            SideLeft,
            SideRight,
        ],
        8 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            RearLeft,
            RearRight,
            SideLeft,
            SideRight,
        ],
        _ => return None,
    };
    Some(protocol::ChannelMap::new(positions.iter().copied()))
}

struct Timing {
    frames: u64,
    at: Instant,
    running: bool,
}

struct Shared {
    timing: Mutex<Timing>,
    wake: Condvar,
    paused: AtomicBool,
    stop: AtomicBool,
}

pub(super) struct Pulse {
    stream: Arc<pulseaudio::PlaybackStream>,
    shared: Arc<Shared>,
    poller: Option<std::thread::JoinHandle<()>>,
}

impl Pulse {
    pub(super) fn open(format: Format, render: Arc<dyn Render>) -> Result<Self, Error> {
        let client = client().ok_or_else(|| Error(String::from("no PulseAudio server")))?;
        let frame_bytes = format.frame_bytes();
        let target = (format.rate as usize / 10 * frame_bytes) as u32;
        let params = protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::Float32Le,
                channels: format.channels as u8,
                sample_rate: format.rate,
            },
            channel_map: channel_map(format.channels)
                .ok_or_else(|| Error(format!("{} channels", format.channels)))?,
            cvolume: Some(protocol::ChannelVolume::norm(format.channels as u8)),
            sink_name: Some(protocol::DEFAULT_SINK.to_owned()),
            buffer_attr: protocol::stream::BufferAttr {
                max_length: u32::MAX,
                target_length: target,
                pre_buffering: u32::MAX,
                minimum_request_length: u32::MAX,
                fragment_size: u32::MAX,
            },
            flags: protocol::stream::StreamFlags {
                start_corked: true,
                adjust_latency: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut scratch = Vec::<f32>::new();
        let callback = move |buf: &mut [u8]| -> usize {
            let frames = buf.len() / frame_bytes;
            if frames == 0 {
                buf.fill(0);
                return buf.len();
            }
            scratch.clear();
            scratch.resize(frames * frame_bytes / 4, 0.0);
            render.render(&mut scratch);
            for (bytes, sample) in buf.as_chunks_mut::<4>().0.iter_mut().zip(scratch.iter()) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
            // Zero (end of stream) would end the stream; always report the
            // whole request, silence included.
            buf[frames * frame_bytes..].fill(0);
            buf.len()
        };
        use pulseaudio::AsPlaybackSource;
        let stream = futures::executor::block_on(
            client.create_playback_stream(params, callback.as_playback_source()),
        )
        .map_err(|error| Error(format!("PulseAudio stream: {error}")))?;
        let stream = Arc::new(stream);
        let shared = Arc::new(Shared {
            timing: Mutex::new(Timing {
                frames: 0,
                at: Instant::now(),
                running: false,
            }),
            wake: Condvar::new(),
            paused: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let poller = {
            let stream = stream.clone();
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(String::from("trust-audio-timing"))
                .spawn(move || poll_timing(&stream, &shared, format))
                .ok()
        };
        Ok(Self {
            stream,
            shared,
            poller,
        })
    }
}

/// Refresh the audible position every 100 ms while the stream runs; sleep
/// while it is paused.
fn poll_timing(stream: &pulseaudio::PlaybackStream, shared: &Shared, format: Format) {
    let frame_bytes = format.frame_bytes() as i64;
    loop {
        {
            let mut timing = shared.timing.lock().unwrap_or_else(|e| e.into_inner());
            while shared.paused.load(Ordering::Acquire) && !shared.stop.load(Ordering::Acquire) {
                timing = shared.wake.wait(timing).unwrap_or_else(|e| e.into_inner());
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let sent = Instant::now();
        if let Ok(latency) = futures::executor::block_on(stream.timing_info()) {
            let received = Instant::now();
            // The reply describes the server at roughly the midpoint of
            // the round trip.
            let at = sent + received.duration_since(sent) / 2;
            let read_frames = latency.read_offset.max(0) / frame_bytes;
            let sink_frames =
                (latency.sink_usec as i128 * i128::from(format.rate) / 1_000_000) as i64;
            let frames = (read_frames - sink_frames).max(0) as u64;
            let mut timing = shared.timing.lock().unwrap_or_else(|e| e.into_inner());
            // Never move backwards (HTML: the clock is monotonic).
            if frames >= timing.frames || !timing.running {
                timing.frames = timing.frames.max(frames);
            }
            timing.at = at;
            timing.running = latency.playing && !shared.paused.load(Ordering::Acquire);
        }
        let timing = shared.timing.lock().unwrap_or_else(|e| e.into_inner());
        let _ = shared
            .wake
            .wait_timeout(timing, Duration::from_millis(100))
            .unwrap_or_else(|e| e.into_inner());
    }
}

impl Backend for Pulse {
    fn set_paused(&self, paused: bool) {
        if self.shared.paused.swap(paused, Ordering::AcqRel) == paused {
            return;
        }
        let result = if paused {
            futures::executor::block_on(self.stream.cork())
        } else {
            futures::executor::block_on(self.stream.uncork())
        };
        let _ = result;
        let mut timing = self.shared.timing.lock().unwrap_or_else(|e| e.into_inner());
        if paused {
            // Freeze at the interpolated position.
            let now = Instant::now();
            timing.frames = Position {
                frames: timing.frames,
                at: timing.at,
                running: timing.running,
            }
            .frames_at(now, self.stream.sample_spec().sample_rate);
            timing.at = now;
            timing.running = false;
        }
        self.shared.wake.notify_all();
    }

    fn position(&self) -> Position {
        let timing = self.shared.timing.lock().unwrap_or_else(|e| e.into_inner());
        Position {
            frames: timing.frames,
            at: timing.at,
            running: timing.running,
        }
    }

    fn name(&self) -> &'static str {
        "pulse"
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}
