//! Audio output for media elements (and, later, Web Audio).
//!
//! Like Firefox's cubeb and Chrome's audio service, each playing element
//! gets its own stream on the platform's mixer, which mixes and resamples:
//!
//! - Linux: the PulseAudio native protocol ([`pulse`]), which PulseAudio and
//!   PipeWire (pipewire-pulse) both serve, in pure Rust; then ALSA through
//!   `libasound.so.2` loaded at run time ([`alsa`]), whose default device is
//!   normally a mixing plugin.
//! - Windows: WASAPI shared mode on the default endpoint ([`wasapi`]).
//! - Elsewhere, and when no device is available: a silent real-time sink
//!   ([`null`]) that consumes audio at the device rate, so the media clock
//!   still advances (the same timeline headless playback uses).
//!
//! A stream pulls interleaved `f32` frames from a [`Render`] on the
//! backend's thread and reports which frames have become audible
//! ([`Position`]); the media clock is derived from that report (HTML
//! #media-timeline: the audio clock is the reference, as in every browser).

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

#[cfg(target_os = "linux")]
mod alsa;
mod null;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(windows)]
mod wasapi;

/// The sample format of a stream: interleaved 32-bit float.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

impl Format {
    pub fn frame_bytes(self) -> usize {
        usize::from(self.channels) * 4
    }
}

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audio output: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// The producer of a stream's audio, called on the device thread. It must
/// not block.
pub trait Render: Send + Sync {
    /// Fill `out` with interleaved frames and return how many frames it
    /// produced; the remainder of `out` is left as silence.
    fn render(&self, out: &mut [f32]) -> usize;
}

/// How much of a stream has been heard: `frames` frames had left the
/// speaker as of `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub frames: u64,
    pub at: Instant,
    /// The stream is running, so the position advances with real time
    /// after `at`.
    pub running: bool,
}

impl Position {
    /// Interpolate to `now` at `rate` frames per second.
    pub fn frames_at(&self, now: Instant, rate: u32) -> u64 {
        if !self.running || now <= self.at {
            return self.frames;
        }
        let elapsed = now.duration_since(self.at);
        self.frames + (elapsed.as_secs_f64() * f64::from(rate)) as u64
    }
}

trait Backend: Send + Sync {
    fn set_paused(&self, paused: bool);
    fn position(&self) -> Position;
    fn name(&self) -> &'static str;
}

/// Whether outputs go to an audio device; a process without one (the
/// headless renderer) turns it off and plays in silent real time.
static DEVICE_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Play all media of this process silently (`trust-headless`).
pub fn disable_device_output() {
    DEVICE_OUTPUT.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// One output stream.
pub struct Output {
    backend: Box<dyn Backend>,
    format: Format,
}

impl Output {
    /// Open a stream on the platform's default device, starting paused.
    /// Falls back through the platform backends; fails when none works.
    pub fn open(format: Format, render: Arc<dyn Render>) -> Result<Self, Error> {
        if format.rate == 0 || format.channels == 0 || format.channels > 32 {
            return Err(Error(format!("invalid format {format:?}")));
        }
        if !DEVICE_OUTPUT.load(std::sync::atomic::Ordering::Relaxed)
            || std::env::var_os("TRUST_AUDIO").is_some_and(|value| value == "null")
        {
            return Ok(Self::null(format, render));
        }
        let mut errors = Vec::new();
        #[cfg(target_os = "linux")]
        {
            match pulse::Pulse::open(format, render.clone()) {
                Ok(backend) => {
                    return Ok(Self {
                        backend: Box::new(backend),
                        format,
                    });
                }
                Err(error) => errors.push(error.0),
            }
            match alsa::Alsa::open(format, render.clone()) {
                Ok(backend) => {
                    return Ok(Self {
                        backend: Box::new(backend),
                        format,
                    });
                }
                Err(error) => errors.push(error.0),
            }
        }
        #[cfg(windows)]
        {
            match wasapi::Wasapi::open(format, render.clone()) {
                Ok(backend) => {
                    return Ok(Self {
                        backend: Box::new(backend),
                        format,
                    });
                }
                Err(error) => errors.push(error.0),
            }
        }
        let _ = &render;
        if errors.is_empty() {
            errors.push(String::from("no audio backend on this platform"));
        }
        Err(Error(errors.join("; ")))
    }

    /// A silent stream that consumes audio in real time (no device).
    pub fn null(format: Format, render: Arc<dyn Render>) -> Self {
        Self {
            backend: Box::new(null::Null::open(format, render)),
            format,
        }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn set_paused(&self, paused: bool) {
        self.backend.set_paused(paused);
    }

    pub fn position(&self) -> Position {
        self.backend.position()
    }

    pub fn backend(&self) -> &'static str {
        self.backend.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// Silence that counts the frames it was asked for.
    struct Silence {
        frames: AtomicU64,
        channels: usize,
    }

    impl Render for Silence {
        fn render(&self, out: &mut [f32]) -> usize {
            out.fill(0.0);
            let frames = out.len() / self.channels;
            self.frames.fetch_add(frames as u64, Ordering::Relaxed);
            frames
        }
    }

    fn exercise(output: &Output, render: &Silence) {
        let format = output.format();
        output.set_paused(false);
        // A quarter second must be heard (a loaded machine may start the
        // stream late), and never more than was rendered.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (position, heard) = loop {
            std::thread::sleep(Duration::from_millis(50));
            let position = output.position();
            let heard = position.frames_at(Instant::now(), format.rate);
            if heard >= u64::from(format.rate) / 4 || Instant::now() >= deadline {
                break (position, heard);
            }
        };
        assert!(
            heard >= u64::from(format.rate) / 4,
            "{}: heard {heard}",
            output.backend()
        );
        assert!(
            position.frames <= render.frames.load(Ordering::Relaxed) + u64::from(format.rate) / 10
        );
        output.set_paused(true);
        let paused = output.position();
        std::thread::sleep(Duration::from_millis(150));
        let later = output.position();
        assert!(!later.running, "{}", output.backend());
        assert!(
            later.frames_at(Instant::now(), format.rate)
                <= paused.frames_at(Instant::now(), format.rate) + u64::from(format.rate) / 20
        );
    }

    #[test]
    fn null_output_consumes_in_real_time() {
        let render = Arc::new(Silence {
            frames: AtomicU64::new(0),
            channels: 2,
        });
        let output = Output::null(
            Format {
                rate: 48_000,
                channels: 2,
            },
            render.clone(),
        );
        exercise(&output, &render);
    }

    /// WASAPI's clock advances while playing and holds while paused, when
    /// the machine has an output endpoint.
    #[cfg(windows)]
    #[test]
    fn wasapi_output_reports_its_clock() {
        let render = Arc::new(Silence {
            frames: AtomicU64::new(0),
            channels: 2,
        });
        let Ok(backend) = wasapi::Wasapi::open(
            Format {
                rate: 48_000,
                channels: 2,
            },
            render.clone(),
        ) else {
            eprintln!("skipping: no WASAPI endpoint");
            return;
        };
        let output = Output {
            backend: Box::new(backend),
            format: Format {
                rate: 48_000,
                channels: 2,
            },
        };
        exercise(&output, &render);
    }

    /// Plays silence through the PulseAudio-protocol server when one is
    /// running (PipeWire or PulseAudio); skipped otherwise.
    #[cfg(target_os = "linux")]
    #[test]
    fn pulse_output_reports_server_timing() {
        if pulseaudio::socket_path_from_env().is_none() {
            return;
        }
        let render = Arc::new(Silence {
            frames: AtomicU64::new(0),
            channels: 2,
        });
        let Ok(backend) = pulse::Pulse::open(
            Format {
                rate: 44_100,
                channels: 2,
            },
            render.clone(),
        ) else {
            return;
        };
        let output = Output {
            backend: Box::new(backend),
            format: Format {
                rate: 44_100,
                channels: 2,
            },
        };
        exercise(&output, &render);
    }
}
