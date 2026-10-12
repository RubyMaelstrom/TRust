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
use super::nodes::Control;
use super::ring::Ring;
use super::worklet::{Factory, Setup, Worklet};
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
    /// AudioWorklet addModule(): build the AudioWorkletGlobalScope with
    /// `factory` first if there is none yet.
    AddModule {
        token: u32,
        url: String,
        factory: Option<Factory>,
        /// The AudioWorklet's port, for a scope being built.
        port: Option<String>,
    },
    /// Construct AudioWorkletNode `node`'s processor (its graph node exists).
    CreateProcessor {
        node: u32,
        name: String,
        packet: String,
    },
    /// The AudioWorklet's port, for the global scope.
    GlobalPort(String),
}

/// Run a worklet command on the rendering thread.
fn worklet_command(
    command: Command,
    worklet: &mut Option<Box<dyn Worklet>>,
    graph: &mut Graph,
    notices: &mut Vec<Notice>,
    setup: &Setup,
) {
    match command {
        Command::AddModule {
            token,
            url,
            factory,
            port,
        } => {
            if worklet.is_none()
                && let Some(factory) = factory
            {
                match factory(setup.clone()) {
                    Ok(mut scope) => {
                        // #AudioContext-constructors: the scope's port is
                        // set before any module runs.
                        if let Some(packet) = port {
                            scope.global_port(&packet);
                        }
                        *worklet = Some(scope);
                    }
                    Err(error) => {
                        notices.push(Notice::WorkletModule {
                            token,
                            result: Err(error),
                        });
                        return;
                    }
                }
            }
            let result = match worklet.as_mut() {
                Some(scope) => scope.add_module(&url),
                None => Err(String::from("AudioWorklet is unavailable")),
            };
            notices.push(Notice::WorkletModule { token, result });
        }
        Command::CreateProcessor { node, name, packet } => {
            let result = match worklet.as_mut() {
                Some(scope) => scope.create(node, &name, &packet),
                None => Err(super::worklet::ProcessorError {
                    message: String::from("AudioWorklet is unavailable"),
                    ..Default::default()
                }),
            };
            let control = match result {
                Ok(()) => Control::WorkletReady,
                Err(error) => {
                    notices.push(Notice::ProcessorError { node, error });
                    Control::WorkletFailed
                }
            };
            graph.apply(GraphMessage::Control { node, control });
        }
        Command::GlobalPort(packet) => {
            if let Some(scope) = worklet.as_mut() {
                scope.global_port(&packet);
            }
        }
        _ => {}
    }
}

/// Render one quantum with the worklet scope's clock, tasks and released
/// processors kept in step.
fn render_quantum(
    graph: &mut Graph,
    notices: &mut Vec<Notice>,
    worklet: &mut Option<Box<dyn Worklet>>,
) {
    if let Some(scope) = worklet.as_mut() {
        // #rendering-loop step 3: the associated task queue, then the
        // quantum with currentFrame set.
        scope.run_tasks();
        scope.set_time(graph.frame);
    }
    graph.render(notices, worklet.as_deref_mut());
    if let Some(scope) = worklet.as_mut() {
        for node in graph.take_removed() {
            scope.release(node);
        }
        scope.run_tasks();
    } else {
        graph.take_removed();
    }
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
    /// A real-time context's output became audible (a quantum above the
    /// silence threshold) or silent (none for `SILENT_AFTER`): the
    /// frontend's indication that the page makes sound.
    Audible(bool),
    /// A startRendering() buffer could not be allocated.
    RenderFailed,
    /// An AudioWorkletProcessor threw (fire `processorerror`).
    ProcessorError {
        node: u32,
        error: super::worklet::ProcessorError,
    },
    /// An addModule() finished: the processors it registered (JSON) or why
    /// it failed.
    WorkletModule {
        token: u32,
        result: Result<String, String>,
    },
    /// A ScriptProcessorNode collected a buffer of input
    /// (#AudioProcessingEvent).
    ScriptProcess {
        node: u32,
        playback_time: f64,
        input: Arc<AudioData>,
    },
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
#[derive(Clone, Debug)]
pub struct Realtime {
    pub rate: f32,
    pub quantum: usize,
    /// The device's channel count (the destination's maxChannelCount).
    pub channels: u16,
    /// The latency hint, in seconds.
    pub latency: f64,
    /// `sinkId` {type: "none"}: render without a device.
    pub silent: bool,
    /// The user's mute of the device output (a muted tab), which the
    /// context renders into as ever.
    pub mute: crate::media::audio::Mute,
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

/// The worklet scope's view of its context: a message for its ports
/// wakes the rendering thread.
fn setup(shared: &Arc<Shared>, graph: &Graph) -> Setup {
    let waker_shared = shared.clone();
    Setup {
        rate: graph.rate,
        quantum: graph.quantum,
        waker: Arc::new(move || waker_shared.wake()),
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

/// Silence this long ends a real-time context's audibility.
const SILENT_AFTER: Duration = Duration::from_secs(2);
/// A sample within this of zero is silence (-80 dBFS).
const SILENCE: f32 = 1.0e-4;

/// Whether a real-time context's output makes sound, with hysteresis: one
/// quantum above the silence threshold makes it audible; `SILENT_AFTER` of
/// silent output makes it silent again.
struct OutputLevel {
    audible: bool,
    silent_frames: u64,
    silent_limit: u64,
    channels: usize,
}

impl OutputLevel {
    fn new(rate: f32, channels: usize) -> Self {
        Self {
            audible: false,
            silent_frames: 0,
            silent_limit: (f64::from(rate.max(1.0)) * SILENT_AFTER.as_secs_f64()) as u64,
            channels: channels.max(1),
        }
    }

    /// The output's audibility, when this block of interleaved samples
    /// changes it.
    fn observe(&mut self, samples: &[f32]) -> Option<bool> {
        if samples.iter().any(|sample| sample.abs() > SILENCE) {
            self.silent_frames = 0;
            return (!std::mem::replace(&mut self.audible, true)).then_some(true);
        }
        if !self.audible {
            return None;
        }
        self.silent_frames += (samples.len() / self.channels) as u64;
        (self.silent_frames >= self.silent_limit).then(|| {
            self.audible = false;
            false
        })
    }

    /// The output stopped (the context suspended or closed).
    fn stop(&mut self) -> Option<bool> {
        self.silent_frames = 0;
        std::mem::take(&mut self.audible).then_some(false)
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
    let mut output_level = OutputLevel::new(config.rate, channels);
    let mut notices = Vec::new();
    let mut worklet: Option<Box<dyn Worklet>> = None;
    let setup = setup(shared, &graph);
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
                            Output::open_with_latency(
                                format,
                                latency,
                                config.mute.gate(feed.clone()),
                            )
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
                    if let Some(audible) = output_level.stop() {
                        notices.push(Notice::Audible(audible));
                    }
                    notices.push(Notice::Suspended(token));
                }
                Command::Close(token) => {
                    // #AudioContext-methods close(): release the device and
                    // stop rendering for good.
                    if let Some(audible) = output_level.stop() {
                        notices.push(Notice::Audible(audible));
                    }
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
                command => worklet_command(command, &mut worklet, &mut graph, &mut notices, &setup),
            }
        }
        while running && ring.free() >= room {
            render_quantum(&mut graph, &mut notices, &mut worklet);
            device_bus.reset(channels, quantum);
            if let Some(output) = graph.destination_output() {
                device_bus.mix_from(output, graph.destination_interpretation());
            }
            for frame in 0..quantum {
                for channel in 0..channels {
                    interleaved[frame * channels + channel] = device_bus.channel(channel)[frame];
                }
            }
            if let Some(audible) = output_level.observe(&interleaved) {
                notices.push(Notice::Audible(audible));
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
    let mut worklet: Option<Box<dyn Worklet>> = None;
    let setup = setup(shared, &graph);
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
                command => worklet_command(command, &mut worklet, &mut graph, &mut notices, &setup),
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
            render_quantum(&mut graph, &mut notices, &mut worklet);
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
            // Script answers (a ScriptProcessorNode's events) must reach
            // the control thread before rendering waits for them.
            if shared.pending() || !notices.is_empty() {
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

    #[test]
    fn output_audibility_has_hysteresis_and_ends_with_the_output() {
        // 2 channels at 1000 Hz: two seconds of silence are 2000 frames.
        let mut level = OutputLevel::new(1000.0, 2);
        let silent = vec![0.0f32; 256];
        let mut loud = silent.clone();
        loud[17] = 0.25;
        assert_eq!(level.observe(&silent), None, "silence starts silent");
        assert_eq!(level.observe(&loud), Some(true));
        assert_eq!(level.observe(&loud), None);
        // Quiet passages shorter than two seconds keep it audible.
        for _ in 0..14 {
            assert_eq!(level.observe(&silent), None);
        }
        assert_eq!(level.observe(&loud), None);
        let mut ended = None;
        for _ in 0..16 {
            ended = ended.or(level.observe(&silent));
        }
        assert_eq!(ended, Some(false), "two seconds of silence end it");
        assert_eq!(level.observe(&[f32::NAN; 4]), None, "NaN is not sound");
        assert_eq!(level.observe(&loud), Some(true));
        assert_eq!(level.stop(), Some(false), "a suspended context is silent");
        assert_eq!(level.stop(), None);
    }
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
                mute: Default::default(),
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
        // Rendering reports what the device hears along the way.
        let suspended = loop {
            match notices.recv_timeout(Duration::from_secs(5)) {
                Ok(Notice::Audible(_)) => {}
                other => break other,
            }
        };
        assert!(matches!(suspended, Ok(Notice::Suspended(2))));
        let held = engine.frame();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(engine.frame(), held);
        engine.send(Command::Close(3));
        let closed = loop {
            match notices.recv_timeout(Duration::from_secs(5)) {
                Ok(Notice::Audible(_)) => {}
                other => break other,
            }
        };
        assert!(matches!(closed, Ok(Notice::Closed(3))));
    }
}
