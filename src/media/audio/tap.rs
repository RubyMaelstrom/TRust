//! An output that a Web Audio graph pulls instead of a device
//! (MediaElementAudioSourceNode: "audio playback from the HTMLMediaElement
//! will be re-routed into the processing graph").
//!
//! The element's player renders into a [`Tap`] like into any [`Output`]; a
//! MediaElementAudioSourceNode pulls from it at its context's pace. While
//! nothing pulls (the context is suspended or closed), the element plays on
//! with its output ignored (#AudioContext-methods suspend()): a thread
//! consumes its audio in real time, so its clock never stalls.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Backend, Format, Position, Render};

/// How long the graph may go without pulling before the element's audio is
/// consumed in real time instead.
const IDLE: Duration = Duration::from_millis(40);

struct Source {
    format: Format,
    render: Arc<dyn Render>,
    /// Identifies the output that attached it.
    serial: u64,
}

struct State {
    source: Option<Source>,
    serial: u64,
    paused: bool,
    /// Frames consumed (by the graph or in real time), from the attaching
    /// output's base.
    frames: u64,
    at: Instant,
    last_pull: Option<Instant>,
    /// The fallback consumer thread is running.
    consumer: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// The element's resource is CORS-cross-origin (or not yet known not
    /// to be): the graph hears silence (#MediaElementAudioSourceOptions-security).
    opaque: std::sync::atomic::AtomicBool,
}

/// The rerouting point of one media element, shared by its players'
/// outputs and its MediaElementAudioSourceNode.
#[derive(Clone)]
pub struct Tap(Arc<Shared>);

impl Default for Tap {
    fn default() -> Self {
        Self::new()
    }
}

impl Tap {
    pub fn new() -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                source: None,
                serial: 0,
                paused: true,
                frames: 0,
                at: Instant::now(),
                last_pull: None,
                consumer: false,
            }),
            wake: Condvar::new(),
            opaque: std::sync::atomic::AtomicBool::new(true),
        }))
    }

    /// Whether the graph must hear silence instead of the element.
    pub fn set_opaque(&self, opaque: bool) {
        self.0
            .opaque
            .store(opaque, std::sync::atomic::Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The format of the element's audio while it plays.
    pub fn format(&self) -> Option<Format> {
        let state = self.lock();
        if state.paused {
            return None;
        }
        state.source.as_ref().map(|source| source.format)
    }

    /// The graph's side: up to `frames` frames of the playing element's
    /// audio, interleaved, appended to `out`; `None` while nothing plays.
    pub fn pull(&self, frames: usize, out: &mut Vec<f32>) -> Option<Format> {
        let mut state = self.lock();
        if state.paused {
            return None;
        }
        let source = state.source.as_ref()?;
        let format = source.format;
        let render = source.render.clone();
        let start = out.len();
        out.resize(start + frames * usize::from(format.channels), 0.0);
        render.render(&mut out[start..]);
        if self.0.opaque.load(std::sync::atomic::Ordering::Acquire) {
            out[start..].fill(0.0);
        }
        let now = Instant::now();
        state.frames += frames as u64;
        state.at = now;
        state.last_pull = Some(now);
        Some(format)
    }

    /// Attach a player's renderer; returns the output's serial.
    fn attach(&self, format: Format, render: Arc<dyn Render>, base: u64) -> u64 {
        let mut state = self.lock();
        state.serial += 1;
        let serial = state.serial;
        state.source = Some(Source {
            format,
            render,
            serial,
        });
        state.frames = base;
        state.at = Instant::now();
        state.paused = true;
        if !state.consumer {
            state.consumer = true;
            let tap = self.clone();
            if std::thread::Builder::new()
                .name(String::from("trust-audio-tap"))
                .spawn(move || tap.consume())
                .is_err()
            {
                state.consumer = false;
            }
        }
        serial
    }

    fn detach(&self, serial: u64) {
        let mut state = self.lock();
        if state
            .source
            .as_ref()
            .is_some_and(|source| source.serial == serial)
        {
            state.source = None;
            state.paused = true;
        }
        self.0.wake.notify_all();
    }

    /// Consume in real time while the element plays and the graph does not
    /// pull; sleep while it is paused or detached. Exits once detached.
    fn consume(&self) {
        let mut buffer = Vec::new();
        let mut state = self.lock();
        loop {
            if state.source.is_none() {
                state.consumer = false;
                return;
            }
            if state.paused {
                state = self.0.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            let now = Instant::now();
            let idle = state
                .last_pull
                .is_none_or(|pull| now.duration_since(pull) >= IDLE);
            if idle && let Some(source) = state.source.as_ref() {
                let rate = f64::from(source.format.rate);
                let due = (now.duration_since(state.at).as_secs_f64() * rate) as usize;
                if due > 0 {
                    // A long stall (a suspended process) does not burst.
                    let due = due.min(source.format.rate as usize / 5);
                    buffer.clear();
                    buffer.resize(due * usize::from(source.format.channels), 0.0);
                    source.render.render(&mut buffer);
                    state.frames += due as u64;
                    state.at = now;
                }
            }
            state = self
                .0
                .wake
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// The output a player opens on a tapped element.
pub(super) struct TapOutput {
    tap: Tap,
    serial: u64,
}

impl TapOutput {
    pub(super) fn open(format: Format, render: Arc<dyn Render>, tap: &Tap, base: u64) -> Self {
        let serial = tap.attach(format, render, base);
        Self {
            tap: tap.clone(),
            serial,
        }
    }
}

impl Backend for TapOutput {
    fn set_paused(&self, paused: bool) {
        let mut state = self.tap.lock();
        if state
            .source
            .as_ref()
            .is_some_and(|source| source.serial == self.serial)
        {
            state.paused = paused;
            state.at = Instant::now();
            state.last_pull = None;
        }
        self.tap.0.wake.notify_all();
    }

    fn position(&self) -> Position {
        let state = self.tap.lock();
        Position {
            frames: state.frames,
            at: state.at,
            running: !state.paused,
        }
    }

    fn name(&self) -> &'static str {
        "web-audio"
    }
}

impl Drop for TapOutput {
    fn drop(&mut self) {
        self.tap.detach(self.serial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Counter(AtomicU64);

    impl Render for Counter {
        fn render(&self, out: &mut [f32]) -> usize {
            out.fill(0.5);
            self.0.fetch_add(out.len() as u64, Ordering::Relaxed);
            out.len()
        }
    }

    #[test]
    fn the_graph_pulls_and_idle_time_is_consumed() {
        let tap = Tap::new();
        let render = Arc::new(Counter(AtomicU64::new(0)));
        let format = Format {
            rate: 1000,
            channels: 1,
        };
        let output = TapOutput::open(format, render.clone(), &tap, 500);
        tap.set_opaque(false);
        let mut out = Vec::new();
        assert!(tap.pull(10, &mut out).is_none(), "paused");
        output.set_paused(false);
        assert_eq!(tap.pull(10, &mut out), Some(format));
        assert_eq!(out, vec![0.5; 10]);
        assert_eq!(output.position().frames, 510);
        // Without pulls the element keeps time in real time.
        std::thread::sleep(Duration::from_millis(200));
        let position = output.position();
        assert!(position.frames >= 600, "{}", position.frames);
        drop(output);
        assert!(tap.pull(10, &mut out).is_none(), "detached");
    }
}
