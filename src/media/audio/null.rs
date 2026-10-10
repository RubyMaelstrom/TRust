//! A silent stream that pulls audio at the device rate on its own thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Backend, Format, Position, Render};

/// Frames per pull: 10 ms.
fn period(format: Format) -> usize {
    (format.rate as usize / 100).max(1)
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

struct State {
    paused: bool,
    frames: u64,
    at: Instant,
}

pub(super) struct Null {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Null {
    pub(super) fn open(format: Format, render: Arc<dyn Render>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                paused: true,
                frames: 0,
                at: Instant::now(),
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("trust-audio-null"))
            .spawn(move || run(format, render, thread_shared))
            .ok();
        Self { shared, thread }
    }
}

fn run(format: Format, render: Arc<dyn Render>, shared: Arc<Shared>) {
    let frames = period(format);
    let mut buffer = vec![0f32; frames * usize::from(format.channels)];
    let step = Duration::from_secs_f64(frames as f64 / f64::from(format.rate));
    let mut deadline = Instant::now();
    loop {
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.paused && !shared.stop.load(Ordering::Acquire) {
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                deadline = Instant::now();
                state.at = deadline;
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        buffer.fill(0.0);
        render.render(&mut buffer);
        deadline += step;
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.frames += frames as u64;
            state.at = Instant::now();
        }
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else if now - deadline > Duration::from_millis(200) {
            // Far behind (a suspended process): do not burst to catch up.
            deadline = now;
        }
    }
}

impl Backend for Null {
    fn set_paused(&self, paused: bool) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.paused = paused;
        state.at = Instant::now();
        self.shared.wake.notify_all();
    }

    fn position(&self) -> Position {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        Position {
            frames: state.frames,
            at: state.at,
            running: !state.paused,
        }
    }

    fn name(&self) -> &'static str {
        "null"
    }
}

impl Drop for Null {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
