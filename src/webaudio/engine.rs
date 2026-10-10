//! A BaseAudioContext's rendering thread (#control-thread-and-rendering-thread).
//!
//! The control thread queues [`Command`]s; the rendering thread atomically
//! swaps the queue before each render quantum and runs them in order
//! (#rendering-loop step 2), then renders. Its reports ([`Notice`]s) go back
//! through the context's notifier, which queues them as tasks on the
//! control thread.
//!
//! An AudioContext's thread renders ahead of its audio device into a
//! [`Ring`]; the device's thread drains it and wakes the renderer when a
//! render quantum's worth of space is free, so a running context renders
//! at the device's pace and a suspended one sleeps. An
//! OfflineAudioContext's thread renders as fast as it can into the buffer
//! that startRendering() resolves with.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use super::AudioData;
use super::bus::Bus;
use super::graph::{Graph, GraphMessage};
use super::ring::Ring;
use crate::media::audio::{Format, Output, Render};

/// A control message.
pub enum Command {
    Graph(GraphMessage),
    /// Start (or resume) rendering: acquire the device and run.
    Start(u32),
    Suspend(u32),
    Close(u32),
    /// OfflineAudioContext startRendering(): render `length` more frames.
    Render(usize),
    /// OfflineAudioContext suspend(): stop before rendering this frame.
    SuspendAt(u64),
}

/// A report from the rendering thread.
#[derive(Debug)]
pub enum Notice {
    /// An AudioScheduledSourceNode stopped playing (fire `ended`).
    Ended(u32),
    /// The rendering thread state became running for Start(token).
    Running(u32),
    Suspended(u32),
    Closed(u32),
    /// An OfflineAudioContext reached a scheduled suspend frame.
    OfflineSuspended(u64),
    /// A suspend frame arrived after rendering had passed it.
    SuspendMissed(u64),
    /// A startRendering() job's buffer.
    Rendered(Arc<AudioData>),
    /// A startRendering() buffer could not be allocated.
    RenderFailed,
    /// A decodeAudioData() job finished (`None`: the bytes could not be
    /// decoded).
    Decoded {
        job: u64,
        data: Option<Arc<AudioData>>,
    },
}

pub type Notify = Box<dyn Fn(Vec<Notice>) + Send>;

struct Shared {
    queue: Mutex<Vec<Command>>,
    frame: AtomicU64,
    stop: AtomicBool,
    closed: AtomicBool,
    thread: OnceLock<Thread>,
    device: Mutex<Option<Output>>,
    ring: OnceLock<Arc<Ring>>,
    /// Frames the device pulled, and how many of them came from the graph.
    pulled: AtomicU64,
    delivered: AtomicU64,
}

impl Shared {
    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    fn take(&self) -> Vec<Command> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn pending(&self) -> bool {
        !self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// How an AudioContext renders.
#[derive(Clone, Copy, Debug)]
pub struct Realtime {
    pub rate: f32,
    pub quantum: usize,
    /// The device's channel count (the destination's maxChannelCount).
    pub channels: u16,
    /// The latency hint, in seconds.
    pub latency: f64,
    /// `sinkId` {type: "none"}: render without a device.
    pub silent: bool,
}

/// A context's rendering thread, owned by the control thread.
pub struct Engine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub rate: f32,
    pub quantum: usize,
    base_latency: f64,
}

impl Engine {
    fn spawn(
        rate: f32,
        quantum: usize,
        base_latency: f64,
        run: impl FnOnce(Arc<Shared>, Graph) + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            frame: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            thread: OnceLock::new(),
            device: Mutex::new(None),
            ring: OnceLock::new(),
            pulled: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
        });
        let graph = Graph::new(rate, quantum);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("trust-webaudio"))
            .spawn(move || {
                let _ = thread_shared.thread.set(std::thread::current());
                run(thread_shared, graph);
            })
            .ok();
        if let Some(thread) = &thread {
            let _ = shared.thread.set(thread.thread().clone());
        }
        Self {
            shared,
            thread,
            rate,
            quantum,
            base_latency,
        }
    }

    /// An AudioContext's rendering thread (suspended until Start).
    pub fn realtime(config: Realtime, notify: Notify) -> Self {
        // The ring holds the latency hint's worth of audio (at least two
        // render quanta), which also covers the device's pulls.
        let quantum = config.quantum.max(1);
        let hint = (config.latency * f64::from(config.rate)).ceil() as usize;
        let frames = hint.max(2 * quantum).div_ceil(quantum) * quantum;
        let base_latency = frames as f64 / f64::from(config.rate);
        Self::spawn(config.rate, quantum, base_latency, move |shared, graph| {
            run_realtime(&shared, graph, config, frames, notify)
        })
    }

    /// An OfflineAudioContext's rendering thread.
    pub fn offline(rate: f32, quantum: usize, channels: usize, notify: Notify) -> Self {
        Self::spawn(rate, quantum.max(1), 0.0, move |shared, graph| {
            run_offline(&shared, graph, channels, notify)
        })
    }

    /// Queue a control message (dropped once the context is closed).
    pub fn send(&self, command: Command) {
        if self.shared.closed.load(Ordering::Acquire) {
            return;
        }
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(command);
        self.shared.wake();
    }

    /// [[current frame]].
    pub fn frame(&self) -> u64 {
        self.shared.frame.load(Ordering::Acquire)
    }

    /// `baseLatency`: the ring between the graph and the device.
    pub fn base_latency(&self) -> f64 {
        self.base_latency
    }

    /// `outputLatency`: audio handed to the device but not yet heard.
    pub fn output_latency(&self) -> f64 {
        let device = self.shared.device.lock().unwrap_or_else(|e| e.into_inner());
        let Some(output) = device.as_ref() else {
            return 0.0;
        };
        let rate = output.format().rate;
        let heard = output.position().frames_at(Instant::now(), rate);
        let pulled = self.shared.pulled.load(Ordering::Acquire);
        pulled.saturating_sub(heard) as f64 / f64::from(rate)
    }

    /// getOutputTimestamp()'s contextTime: the context time of the frame
    /// being heard now.
    pub fn output_time(&self) -> f64 {
        let device = self.shared.device.lock().unwrap_or_else(|e| e.into_inner());
        let Some(output) = device.as_ref() else {
            return 0.0;
        };
        let rate = output.format().rate;
        let heard = output.position().frames_at(Instant::now(), rate);
        let pulled = self.shared.pulled.load(Ordering::Acquire);
        let delivered = self.shared.delivered.load(Ordering::Acquire);
        delivered.saturating_sub(pulled.saturating_sub(heard)) as f64 / f64::from(self.rate)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.closed.store(true, Ordering::Release);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The device side: drains the ring, pads an underrun with silence and
/// wakes the renderer when there is room for a render quantum.
struct Feed {
    shared: Arc<Shared>,
    ring: Arc<Ring>,
    channels: usize,
    room: usize,
}

impl Render for Feed {
    fn render(&self, out: &mut [f32]) -> usize {
        let got = self.ring.pop(out);
        out[got..].fill(0.0);
        let frames = out.len() / self.channels;
        self.shared
            .pulled
            .fetch_add(frames as u64, Ordering::AcqRel);
        self.shared
            .delivered
            .fetch_add((got / self.channels) as u64, Ordering::AcqRel);
        if self.ring.free() >= self.room {
            self.shared.wake();
        }
        frames
    }
}

fn run_realtime(
    shared: &Arc<Shared>,
    mut graph: Graph,
    config: Realtime,
    frames: usize,
    notify: Notify,
) {
    let channels = usize::from(config.channels.max(1));
    let quantum = graph.quantum;
    let room = quantum * channels;
    let ring = Arc::new(Ring::new(frames * channels));
    let _ = shared.ring.set(ring.clone());
    let mut device_bus = Bus::new(channels, quantum);
    let mut interleaved = vec![0f32; room];
    let mut notices = Vec::new();
    let mut running = false;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        for command in shared.take() {
            match command {
                Command::Graph(message) => graph.apply(message),
                Command::Start(token) => {
                    let mut device = shared.device.lock().unwrap_or_else(|e| e.into_inner());
                    if device.is_none() {
                        // #acquiring: the default device, or (when it fails
                        // or the sink is "none") a silent real-time sink
                        // that emulates one.
                        let format = Format {
                            rate: config.rate.round().max(1.0) as u32,
                            channels: channels as u16,
                        };
                        let feed: Arc<dyn Render> = Arc::new(Feed {
                            shared: shared.clone(),
                            ring: ring.clone(),
                            channels,
                            room,
                        });
                        let latency = Duration::from_secs_f64(config.latency.clamp(0.005, 1.0));
                        *device = Some(if config.silent {
                            Output::null(format, feed)
                        } else {
                            Output::open_with_latency(format, latency, feed.clone())
                                .unwrap_or_else(|_| Output::null(format, feed))
                        });
                    }
                    if let Some(output) = device.as_ref() {
                        output.set_paused(false);
                    }
                    running = true;
                    notices.push(Notice::Running(token));
                }
                Command::Suspend(token) => {
                    if let Some(output) = shared
                        .device
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                    {
                        output.set_paused(true);
                    }
                    running = false;
                    notices.push(Notice::Suspended(token));
                }
                Command::Close(token) => {
                    // #AudioContext-methods close(): release the device and
                    // stop rendering for good.
                    shared.closed.store(true, Ordering::Release);
                    shared
                        .device
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    notices.push(Notice::Closed(token));
                    notify(std::mem::take(&mut notices));
                    return;
                }
                Command::Render(_) | Command::SuspendAt(_) => {}
            }
        }
        while running && ring.free() >= room {
            graph.render(&mut notices);
            device_bus.reset(channels, quantum);
            if let Some(output) = graph.destination_output() {
                device_bus.mix_from(output, graph.destination_interpretation());
            }
            for frame in 0..quantum {
                for channel in 0..channels {
                    interleaved[frame * channels + channel] = device_bus.channel(channel)[frame];
                }
            }
            ring.push(&interleaved);
            shared.frame.store(graph.frame, Ordering::Release);
            if shared.pending() {
                break;
            }
        }
        if !notices.is_empty() {
            notify(std::mem::take(&mut notices));
        }
        if shared.pending()
            || shared.stop.load(Ordering::Acquire)
            || (running && ring.free() >= room)
        {
            continue;
        }
        std::thread::park();
    }
}

struct Job {
    length: usize,
    written: usize,
    channels: Vec<Vec<f32>>,
}

fn run_offline(shared: &Arc<Shared>, mut graph: Graph, channels: usize, notify: Notify) {
    let quantum = graph.quantum;
    let mut jobs: VecDeque<Job> = VecDeque::new();
    let mut suspends: BTreeSet<u64> = BTreeSet::new();
    let mut notices = Vec::new();
    let mut running = false;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        for command in shared.take() {
            match command {
                Command::Graph(message) => graph.apply(message),
                Command::Render(length) => {
                    let mut data = Vec::with_capacity(channels);
                    let mut failed = false;
                    for _ in 0..channels {
                        let mut channel = Vec::new();
                        if channel.try_reserve_exact(length).is_err() {
                            failed = true;
                            break;
                        }
                        channel.resize(length, 0.0);
                        data.push(channel);
                    }
                    if failed {
                        notices.push(Notice::RenderFailed);
                    } else {
                        jobs.push_back(Job {
                            length,
                            written: 0,
                            channels: data,
                        });
                        running = true;
                    }
                }
                Command::Start(token) => {
                    running = true;
                    notices.push(Notice::Running(token));
                }
                Command::SuspendAt(frame) => {
                    if frame < graph.frame {
                        notices.push(Notice::SuspendMissed(frame));
                    } else {
                        suspends.insert(frame);
                    }
                }
                Command::Suspend(_) => {}
                Command::Close(token) => {
                    shared.closed.store(true, Ordering::Release);
                    notices.push(Notice::Closed(token));
                    notify(std::mem::take(&mut notices));
                    return;
                }
            }
        }
        // #rendering-loop for an OfflineAudioContext: render until a
        // suspend frame or the end of the job, taking control messages
        // between quanta.
        while running && let Some(job) = jobs.front_mut() {
            if suspends.remove(&graph.frame) {
                running = false;
                notices.push(Notice::OfflineSuspended(graph.frame));
                break;
            }
            graph.render(&mut notices);
            let count = quantum.min(job.length - job.written);
            if let Some(output) = graph.destination_output() {
                for (channel, data) in job.channels.iter_mut().enumerate().take(output.channels()) {
                    data[job.written..job.written + count]
                        .copy_from_slice(&output.channel(channel)[..count]);
                }
            }
            job.written += count;
            shared.frame.store(graph.frame, Ordering::Release);
            if job.written == job.length {
                let job = jobs.pop_front().expect("front job");
                notices.push(Notice::Rendered(Arc::new(AudioData {
                    rate: graph.rate,
                    length: job.length,
                    channels: job
                        .channels
                        .into_iter()
                        .map(Vec::into_boxed_slice)
                        .collect(),
                })));
                if jobs.is_empty() {
                    running = false;
                }
                break;
            }
            if shared.pending() || notices.len() >= 64 {
                break;
            }
        }
        if !notices.is_empty() {
            notify(std::mem::take(&mut notices));
        }
        if shared.pending() || (running && !jobs.is_empty()) {
            continue;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webaudio::bus::{ChannelConfig, CountMode, Interpretation};
    use crate::webaudio::graph::NodeInit;
    use crate::webaudio::nodes::{Control, Destination, Oscillator};
    use crate::webaudio::param::{Descriptor, Rate};
    use crate::webaudio::wave::{Builtin, Wave};
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    fn destination(channels: usize) -> Command {
        Command::Graph(GraphMessage::Create {
            id: 1,
            init: Box::new(NodeInit {
                processor: Box::new(Destination),
                inputs: 1,
                outputs: 1,
                channel: ChannelConfig {
                    count: channels,
                    mode: CountMode::Explicit,
                    interpretation: Interpretation::Speakers,
                },
                params: Vec::new(),
            }),
        })
    }

    fn sine(id: u32) -> Vec<Command> {
        let descriptor = Descriptor {
            default: 1000.0,
            min: -4000.0,
            max: 4000.0,
            rate: Rate::A,
            compound: true,
        };
        vec![
            Command::Graph(GraphMessage::Create {
                id,
                init: Box::new(NodeInit {
                    processor: Box::new(Oscillator::new(Wave::builtin(Builtin::Sine))),
                    inputs: 0,
                    outputs: 1,
                    channel: ChannelConfig {
                        count: 2,
                        mode: CountMode::Max,
                        interpretation: Interpretation::Speakers,
                    },
                    params: vec![
                        (id + 1, descriptor, Arc::new(AtomicU32::new(0))),
                        (
                            id + 2,
                            Descriptor {
                                default: 0.0,
                                ..descriptor
                            },
                            Arc::new(AtomicU32::new(0)),
                        ),
                    ],
                }),
            }),
            Command::Graph(GraphMessage::Connect {
                from: id,
                output: 0,
                to: 1,
                input: 0,
            }),
            Command::Graph(GraphMessage::Control {
                node: id,
                control: Control::Start {
                    when: 0.0,
                    offset: 0.0,
                    duration: None,
                },
            }),
        ]
    }

    fn collector() -> (Notify, mpsc::Receiver<Notice>) {
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        (
            Box::new(move |notices| {
                let sender = sender.lock().unwrap();
                for notice in notices {
                    let _ = sender.send(notice);
                }
            }),
            receiver,
        )
    }

    #[test]
    fn offline_rendering_suspends_and_completes() {
        let (notify, notices) = collector();
        let engine = Engine::offline(8000.0, 128, 1, notify);
        engine.send(destination(1));
        for command in sine(10) {
            engine.send(command);
        }
        engine.send(Command::SuspendAt(256));
        engine.send(Command::Render(1000));
        let timeout = Duration::from_secs(10);
        assert!(matches!(
            notices.recv_timeout(timeout),
            Ok(Notice::OfflineSuspended(256))
        ));
        assert_eq!(engine.frame(), 256);
        engine.send(Command::Start(7));
        assert!(matches!(
            notices.recv_timeout(timeout),
            Ok(Notice::Running(7))
        ));
        let Ok(Notice::Rendered(data)) = notices.recv_timeout(timeout) else {
            panic!("no buffer");
        };
        assert_eq!((data.length, data.channels.len()), (1000, 1));
        // A 1 kHz sine at 8 kHz: sin(2π/8) at frame 1.
        assert!((data.channels[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        assert_eq!(engine.frame(), 1024);
    }

    #[test]
    fn realtime_rendering_follows_the_device() {
        let (notify, notices) = collector();
        let engine = Engine::realtime(
            Realtime {
                rate: 48_000.0,
                quantum: 128,
                channels: 2,
                latency: 0.02,
                silent: true,
            },
            notify,
        );
        engine.send(destination(2));
        for command in sine(10) {
            engine.send(command);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(engine.frame(), 0, "suspended until started");
        engine.send(Command::Start(1));
        assert!(matches!(
            notices.recv_timeout(Duration::from_secs(5)),
            Ok(Notice::Running(1))
        ));
        std::thread::sleep(Duration::from_millis(300));
        let frame = engine.frame();
        // About 0.3 s of audio (a loaded machine may lag), never far ahead.
        assert!(frame > 4_800 && frame < 48_000, "{frame}");
        engine.send(Command::Suspend(2));
        assert!(matches!(
            notices.recv_timeout(Duration::from_secs(5)),
            Ok(Notice::Suspended(2))
        ));
        let held = engine.frame();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(engine.frame(), held);
        engine.send(Command::Close(3));
        assert!(matches!(
            notices.recv_timeout(Duration::from_secs(5)),
            Ok(Notice::Closed(3))
        ));
    }
}
