//! ScriptProcessorNode (#ScriptProcessorNode, deprecated but widely used):
//! the input is collected in `bufferSize` chunks, each sent to script as an
//! `audioprocess` event (#AudioProcessingEvent); the output buffers script
//! fills come back and play two buffers later (double buffering, as in
//! Chrome), so script has a buffer's duration to answer. Events go out only
//! while the node has a connection.
//!
//! An OfflineAudioContext waits for each answer, so its rendering does not
//! depend on how quickly script runs.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Bus, Params, Processor, Scope};
use crate::webaudio::AudioData;
use crate::webaudio::engine::Notice;

/// How long an offline context waits for one answer before playing
/// silence (a page that stopped answering).
const OFFLINE_WAIT: Duration = Duration::from_secs(10);

/// The output buffers script returned, shared with the control thread.
#[derive(Default)]
pub struct SharedState {
    queue: Mutex<VecDeque<Arc<AudioData>>>,
    ready: Condvar,
    closed: AtomicBool,
}

pub type Shared = Arc<SharedState>;

impl SharedState {
    /// Script filled an output buffer.
    pub fn answer(&self, data: Arc<AudioData>) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(data);
        self.ready.notify_all();
    }

    /// The context is going away: stop waiting for answers.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
    }
}

pub struct ScriptProcessor {
    size: usize,
    outputs: usize,
    offline: bool,
    shared: Shared,
    input: Vec<Vec<f32>>,
    /// Output samples to play, per channel.
    fifo: Vec<VecDeque<f32>>,
    /// Events sent whose output has not come back.
    pending: usize,
}

impl ScriptProcessor {
    pub fn new(size: usize, inputs: usize, outputs: usize, offline: bool, shared: Shared) -> Self {
        Self {
            size,
            outputs,
            offline,
            shared,
            input: vec![Vec::with_capacity(size); inputs.max(1)],
            fifo: vec![std::iter::repeat_n(0.0, 2 * size).collect(); outputs.max(1)],
            pending: 0,
        }
    }

    fn drain(&mut self) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(data) = queue.pop_front() {
            self.pending = self.pending.saturating_sub(1);
            for (channel, fifo) in self.fifo.iter_mut().enumerate() {
                match data.channels.get(channel) {
                    Some(samples) => fifo.extend(samples.iter().copied()),
                    None => fifo.extend(std::iter::repeat_n(0.0, data.length)),
                }
            }
        }
        // A late answer does not let latency grow without bound.
        for fifo in &mut self.fifo {
            let excess = fifo.len().saturating_sub(4 * self.size);
            fifo.drain(..excess);
        }
    }

    /// Offline: wait until script has answered enough to play `frames`.
    fn wait(&mut self, frames: usize) {
        let deadline = Instant::now() + OFFLINE_WAIT;
        while self.fifo[0].len() < frames && self.pending > 0 {
            {
                let queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.is_empty() {
                    let now = Instant::now();
                    if now >= deadline || self.shared.closed.load(Ordering::Acquire) {
                        self.pending = 0;
                        return;
                    }
                    let _ = self
                        .shared
                        .ready
                        .wait_timeout(queue, (deadline - now).min(Duration::from_millis(50)));
                }
            }
            self.drain();
        }
    }
}

impl Processor for ScriptProcessor {
    fn is_source(&self) -> bool {
        true
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let frames = scope.frames;
        let output = &mut outputs[0];
        // #ScriptProcessorNode: active (and dispatching) only while connected.
        if !scope.connected {
            output.reset(1, frames);
            return false;
        }
        self.drain();
        if self.offline {
            self.wait(frames);
        }
        output.reset(self.outputs.max(1), frames);
        for (channel, fifo) in self.fifo.iter_mut().enumerate() {
            let target = output.channel_mut(channel);
            for sample in target.iter_mut() {
                *sample = fifo.pop_front().unwrap_or(0.0);
            }
        }
        let input = &inputs[0];
        let mut taken = 0;
        while taken < frames {
            let count = (self.size - self.input[0].len()).min(frames - taken);
            for (channel, collected) in self.input.iter_mut().enumerate() {
                if channel < input.channels() {
                    collected.extend_from_slice(&input.channel(channel)[taken..taken + count]);
                } else {
                    collected.extend(std::iter::repeat_n(0.0, count));
                }
            }
            taken += count;
            if self.input[0].len() == self.size {
                let end = scope.frame + taken as u64;
                let channels = self
                    .input
                    .iter_mut()
                    .map(|collected| {
                        std::mem::replace(collected, Vec::with_capacity(self.size))
                            .into_boxed_slice()
                    })
                    .collect();
                self.pending += 1;
                scope.notices.push(Notice::ScriptProcess {
                    node: scope.node,
                    // This chunk's output plays one buffer after the next.
                    playback_time: (end + self.size as u64) as f64 / f64::from(scope.rate),
                    input: Arc::new(AudioData {
                        rate: scope.rate,
                        length: self.size,
                        channels,
                    }),
                });
            }
        }
        true
    }
}
