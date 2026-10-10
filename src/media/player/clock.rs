//! The media clock: the current playback position (HTML #current-playback-position)
//! as the sinks see it.
//!
//! With audio the clock follows what has been heard (Firefox's
//! AudioSinkWrapper: never lead the audible audio, never run backwards);
//! without audio it follows the system's monotonic clock scaled by the
//! playback rate.

use std::sync::Mutex;
use std::time::Instant;

use super::audio_sink::AudioSink;
use crate::media::Micros;

enum Source {
    /// Media time from heard audio.
    Audio(std::sync::Arc<AudioSink>),
    /// Media time `media` at `instant`, advancing at `rate`.
    System { instant: Instant, media: Micros },
}

struct State {
    source: Source,
    rate: f64,
    running: bool,
    /// The last value reported, for monotonicity until the next reset.
    last: Micros,
    /// Video-only playback waits while frames are missing.
    stalled: bool,
}

pub(crate) struct Clock {
    state: Mutex<State>,
}

impl Clock {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                source: Source::System {
                    instant: Instant::now(),
                    media: 0,
                },
                rate: 1.0,
                running: false,
                last: 0,
                stalled: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow `sink` from now on.
    pub(crate) fn use_audio(&self, sink: std::sync::Arc<AudioSink>) {
        let mut state = self.lock();
        state.source = Source::Audio(sink);
    }

    /// Follow the system clock from now on.
    pub(crate) fn use_system(&self) {
        let mut state = self.lock();
        let media = state.last;
        state.source = Source::System {
            instant: Instant::now(),
            media,
        };
    }

    fn raw(state: &State, now: Instant) -> Micros {
        match &state.source {
            Source::Audio(sink) => sink.media_time(now).unwrap_or(state.last),
            Source::System { instant, media } => {
                if !state.running || state.stalled {
                    *media
                } else {
                    let elapsed = now.saturating_duration_since(*instant).as_secs_f64();
                    media + (elapsed * state.rate * 1_000_000.0) as Micros
                }
            }
        }
    }

    /// The current playback position.
    pub(crate) fn now(&self) -> Micros {
        let mut state = self.lock();
        let value = Self::raw(&state, Instant::now());
        // Never run backwards between resets.
        if value > state.last {
            state.last = value;
        }
        state.last
    }

    /// Start or stop the clock (the sinks start and stop their output).
    pub(crate) fn set_running(&self, running: bool) {
        let mut state = self.lock();
        if state.running == running {
            return;
        }
        let now = Instant::now();
        let value = Self::raw(&state, now).max(state.last);
        state.last = value;
        if let Source::System { instant, media } = &mut state.source {
            *instant = now;
            *media = value;
        }
        state.running = running;
    }

    pub(crate) fn running(&self) -> bool {
        self.lock().running
    }

    /// Video-only playback: hold the clock while no frame is available.
    pub(crate) fn set_stalled(&self, stalled: bool) {
        let mut state = self.lock();
        if state.stalled == stalled {
            return;
        }
        let now = Instant::now();
        let value = Self::raw(&state, now).max(state.last);
        state.last = value;
        if let Source::System { instant, media } = &mut state.source {
            *instant = now;
            *media = value;
        }
        state.stalled = stalled;
    }

    pub(crate) fn set_rate(&self, rate: f64) {
        let mut state = self.lock();
        let now = Instant::now();
        let value = Self::raw(&state, now).max(state.last);
        state.last = value;
        if let Source::System { instant, media } = &mut state.source {
            *instant = now;
            *media = value;
        }
        state.rate = rate;
    }

    pub(crate) fn rate(&self) -> f64 {
        self.lock().rate
    }

    /// Jump to `time` (a completed seek, or the start of playback).
    pub(crate) fn reset(&self, time: Micros) {
        let mut state = self.lock();
        state.last = time;
        if let Source::System { instant, media } = &mut state.source {
            *instant = Instant::now();
            *media = time;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_runs_scales_and_resets() {
        let clock = Clock::new();
        assert_eq!(clock.now(), 0);
        clock.set_running(true);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let first = clock.now();
        assert!((40_000..200_000).contains(&first), "{first}");
        clock.set_rate(2.0);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let second = clock.now();
        assert!(second - first >= 80_000, "{first} {second}");
        clock.set_running(false);
        let paused = clock.now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(clock.now(), paused);
        clock.reset(5_000_000);
        assert_eq!(clock.now(), 5_000_000);
        clock.set_running(true);
        clock.set_stalled(true);
        let stalled = clock.now();
        assert!(stalled - 5_000_000 < 5_000);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(clock.now(), stalled);
    }
}
