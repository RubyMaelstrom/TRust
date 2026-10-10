//! The video half of playback: decoded frames wait in a short queue and a
//! presenter thread publishes each one to the element's [`VideoSurface`]
//! when the clock reaches it, dropping frames that are already late (HTML
//! #playing-the-media-resource; the counts feed
//! `getVideoPlaybackQuality()`). Firefox's VideoSink works the same way.
//!
//! Frontends read surfaces at paint time: a surface holds the frame to show
//! now, with a serial that changes whenever the frame does, and a waker
//! asks the frontend to repaint.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::clock::Clock;
use crate::media::Micros;
use crate::media::frame::VideoFrame;

/// The frame a video element shows now.
pub struct VideoSurface {
    id: u64,
    state: Mutex<SurfaceState>,
}

#[derive(Default)]
struct SurfaceState {
    frame: Option<Arc<VideoFrame>>,
    serial: u64,
}

type Waker = Arc<dyn Fn(u64) + Send + Sync>;

fn registry() -> &'static Mutex<HashMap<u64, Weak<VideoSurface>>> {
    static SURFACES: OnceLock<Mutex<HashMap<u64, Weak<VideoSurface>>>> = OnceLock::new();
    SURFACES.get_or_init(Default::default)
}

fn waker() -> &'static Mutex<Option<Waker>> {
    static WAKER: OnceLock<Mutex<Option<Waker>>> = OnceLock::new();
    WAKER.get_or_init(Default::default)
}

/// Install the frontend's repaint request, called with a surface id
/// whenever that surface shows a new frame (from a media thread).
pub fn set_frame_waker(waker_fn: impl Fn(u64) + Send + Sync + 'static) {
    *waker().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(waker_fn));
}

/// The surface with `id`, while its player exists.
pub fn surface(id: u64) -> Option<Arc<VideoSurface>> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(Weak::upgrade)
}

impl VideoSurface {
    pub(crate) fn new() -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let surface = Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            state: Mutex::default(),
        });
        let mut surfaces = registry().lock().unwrap_or_else(|e| e.into_inner());
        surfaces.retain(|_, weak| weak.strong_count() > 0);
        surfaces.insert(surface.id, Arc::downgrade(&surface));
        surface
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The frame to show and its serial.
    pub fn current(&self) -> Option<(Arc<VideoFrame>, u64)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.frame.clone().map(|frame| (frame, state.serial))
    }

    pub fn serial(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).serial
    }

    fn show(&self, frame: Arc<VideoFrame>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.frame = Some(frame);
            state.serial += 1;
        }
        let waker = waker().lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(waker) = waker {
            waker(self.id);
        }
    }

    /// Show nothing (a new load: HTML shows the poster or nothing until
    /// the first frame of the new resource).
    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.frame.take().is_some() {
            state.serial += 1;
        }
    }
}

impl Drop for VideoSurface {
    fn drop(&mut self) {
        let mut surfaces = registry().lock().unwrap_or_else(|e| e.into_inner());
        if surfaces
            .get(&self.id)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            surfaces.remove(&self.id);
        }
    }
}

struct Queue {
    frames: VecDeque<Arc<VideoFrame>>,
    /// Seeking: the latest frame at or before the target so far.
    seek_target: Option<Micros>,
    seek_candidate: Option<Arc<VideoFrame>>,
    /// The frame for the seek target has been found and shown.
    seek_ready: bool,
    end_of_stream: bool,
    /// The end of the last frame shown (its pts plus duration).
    presented_until: Option<Micros>,
    /// No frame has been shown since the last flush.
    first_pending: bool,
    capacity: usize,
}

pub(crate) struct VideoSink {
    queue: Mutex<Queue>,
    changed: Condvar,
    pub(crate) surface: Arc<VideoSurface>,
    clock: Arc<Clock>,
    stop: AtomicBool,
    presented: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    need_data: Arc<(Mutex<bool>, Condvar)>,
}

impl VideoSink {
    /// `counters` are the presented and dropped frame counts the player
    /// reports (HTML getVideoPlaybackQuality).
    pub(crate) fn new(
        clock: Arc<Clock>,
        surface: Arc<VideoSurface>,
        need_data: Arc<(Mutex<bool>, Condvar)>,
        counters: (Arc<AtomicU64>, Arc<AtomicU64>),
    ) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue {
                frames: VecDeque::new(),
                seek_target: None,
                seek_candidate: None,
                seek_ready: false,
                end_of_stream: false,
                presented_until: None,
                first_pending: true,
                capacity: 6,
            }),
            changed: Condvar::new(),
            surface,
            clock,
            stop: AtomicBool::new(false),
            presented: counters.0,
            dropped: counters.1,
            need_data,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake the presenter (clock started, stopped or jumped).
    pub(crate) fn poke(&self) {
        self.changed.notify_all();
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn wake_control(&self) {
        let (flag, condvar) = &*self.need_data;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
    }

    /// Queue a decoded frame, waiting while the queue is full. Returns
    /// false when the sink stopped or `generation` became stale.
    pub(crate) fn push(&self, frame: VideoFrame, generation: &AtomicU64, expected: u64) -> bool {
        let frame = Arc::new(frame);
        let mut queue = self.lock();
        // Large pictures get a shorter queue.
        queue.capacity = if u64::from(frame.width) * u64::from(frame.height) > 1920 * 1088 {
            3
        } else {
            6
        };
        if let Some(target) = queue.seek_target {
            if frame.pts <= target {
                queue.seek_candidate = Some(frame);
                return true;
            }
            // The first frame after the target: the candidate (or this
            // frame, when the target precedes the first frame) is the one
            // to show at the target.
            let shown = queue.seek_candidate.take().unwrap_or_else(|| frame.clone());
            let same = Arc::ptr_eq(&shown, &frame);
            queue.seek_target = None;
            queue.seek_ready = true;
            queue.presented_until = Some(shown.pts + shown.duration.max(1));
            queue.first_pending = false;
            drop(queue);
            self.surface.show(shown);
            self.presented.fetch_add(1, Ordering::Relaxed);
            self.wake_control();
            queue = self.lock();
            if same {
                self.changed.notify_all();
                return true;
            }
        }
        while queue.frames.len() >= queue.capacity {
            if self.stop.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != expected {
                return false;
            }
            queue = self
                .changed
                .wait_timeout(queue, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if generation.load(Ordering::Acquire) != expected {
            return false;
        }
        queue.frames.push_back(frame);
        drop(queue);
        self.changed.notify_all();
        // The control thread re-evaluates the ready state as frames arrive.
        self.wake_control();
        true
    }

    /// The decoder delivered everything.
    pub(crate) fn set_end_of_stream(&self) {
        let mut queue = self.lock();
        queue.end_of_stream = true;
        if queue.seek_target.take().is_some() {
            // The target is past the last frame: show the last one.
            if let Some(candidate) = queue.seek_candidate.take() {
                queue.presented_until = Some(candidate.pts + candidate.duration.max(1));
                drop(queue);
                self.surface.show(candidate);
                queue = self.lock();
            }
            queue.seek_ready = true;
        }
        drop(queue);
        self.changed.notify_all();
        self.wake_control();
    }

    /// Start a seek (or a fresh start) at `target`: drop queued frames and
    /// keep the frame for the target when it is decoded.
    pub(crate) fn flush(&self, target: Option<Micros>) {
        let mut queue = self.lock();
        queue.frames.clear();
        queue.seek_target = target;
        queue.seek_candidate = None;
        queue.seek_ready = target.is_none();
        queue.end_of_stream = false;
        queue.presented_until = None;
        queue.first_pending = true;
        drop(queue);
        self.changed.notify_all();
    }

    /// Whether the frame for the last seek target is showing.
    pub(crate) fn seek_ready(&self) -> bool {
        let queue = self.lock();
        queue.seek_ready
    }

    /// Whether a frame is available at the current position (HAVE_CURRENT_DATA).
    pub(crate) fn has_current(&self) -> bool {
        if self.surface.serial() == 0 {
            return false;
        }
        let queue = self.lock();
        queue.presented_until.is_some() || !queue.frames.is_empty()
    }

    pub(crate) fn queued(&self) -> usize {
        self.lock().frames.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.lock().capacity
    }

    pub(crate) fn end_of_stream(&self) -> bool {
        self.lock().end_of_stream
    }

    /// Everything decoded has been shown.
    pub(crate) fn drained(&self) -> bool {
        let queue = self.lock();
        queue.end_of_stream && queue.frames.is_empty()
    }

    /// The presenter: publish frames as the clock reaches them.
    pub(crate) fn run(self: Arc<Self>) {
        let mut queue = self.lock();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            let running = self.clock.running();
            // Show the first frame of a load or seek at once, even paused
            // (the "first frame" browsers display under preload).
            if queue.first_pending
                && queue.seek_target.is_none()
                && let Some(frame) = queue.frames.pop_front()
            {
                queue.first_pending = false;
                queue.presented_until = Some(frame.pts + frame.duration.max(1));
                drop(queue);
                self.surface.show(frame);
                self.presented.fetch_add(1, Ordering::Relaxed);
                self.wake_control();
                queue = self.lock();
                continue;
            }
            if !running || queue.frames.is_empty() {
                queue = self.changed.wait(queue).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            let now = self.clock.now();
            // Drop frames whose successor is already due.
            while queue.frames.len() >= 2 && queue.frames[1].pts <= now {
                queue.frames.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            let next = queue.frames[0].pts;
            if next <= now {
                let frame = queue.frames.pop_front().expect("non-empty");
                queue.presented_until = Some(frame.pts + frame.duration.max(1));
                let low = queue.frames.len() < queue.capacity / 2 + 1;
                drop(queue);
                self.surface.show(frame);
                self.presented.fetch_add(1, Ordering::Relaxed);
                if low {
                    self.wake_control();
                }
                self.changed.notify_all();
                queue = self.lock();
                continue;
            }
            // Sleep until the next frame is due (in real time at the
            // current rate), re-checking at least every 50 ms in case the
            // clock moved.
            let rate = self.clock.rate().max(0.01);
            let wait = Duration::from_micros(((next - now) as f64 / rate) as u64)
                .min(Duration::from_millis(50));
            queue = self
                .changed
                .wait_timeout(queue, wait.max(Duration::from_micros(500)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
