//! The rendering thread's side of AudioWorklet (#AudioWorklet): an
//! AudioWorkletGlobalScope runs on the context's rendering thread, and each
//! AudioWorkletNode's processor is called there once per render quantum
//! (#rendering-loop step 4.5.4).
//!
//! The scope itself (a JavaScript engine) is created on the rendering
//! thread by a [`Factory`] the control thread sends with the first
//! `addModule()`; this module only knows it through the [`Worklet`] trait.

use std::sync::Arc;

use super::bus::Bus;
use super::nodes::{Params, Processor, Scope};

/// Wakes the rendering thread (a message arrived for the scope's ports).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Builds the AudioWorkletGlobalScope on the rendering thread.
pub type Factory = Box<dyn FnOnce(Setup) -> Result<Box<dyn Worklet>, String> + Send>;

/// What a scope needs to know about its context.
#[derive(Clone)]
pub struct Setup {
    pub rate: f32,
    pub quantum: usize,
    pub waker: Waker,
}

/// An uncaught exception from a processor's constructor or process()
/// (#dom-audioworkletnode-onprocessorerror).
#[derive(Clone, Debug, Default)]
pub struct ProcessorError {
    pub message: String,
    pub filename: String,
    pub line: u32,
    pub column: u32,
}

/// One processor's view of a render quantum.
pub struct Call<'a> {
    pub node: u32,
    /// Per input: its mixed audio, or `None` when no actively processing
    /// node is connected to it (the processor sees zero channels).
    pub inputs: Vec<Option<&'a Bus>>,
    /// Per output: its channel count this quantum.
    pub output_channels: Vec<usize>,
    /// Per AudioParam, in descriptor order: its values (one value when
    /// constant).
    pub params: Vec<&'a [f32]>,
}

/// An AudioWorkletGlobalScope.
pub trait Worklet {
    /// Fetch and evaluate module `url`; returns the processors it
    /// registered and their parameter descriptors, as JSON.
    fn add_module(&mut self, url: &str) -> Result<String, String>;
    /// Construct the processor of AudioWorkletNode `node`
    /// (#invoking-processor-constructor); `packet` holds the serialized
    /// options and transferred port.
    fn create(&mut self, node: u32, name: &str, packet: &str) -> Result<(), ProcessorError>;
    /// The global scope's port arrived (#AudioWorklet-attributes port).
    fn global_port(&mut self, packet: &str);
    /// Call the processor's process() for this quantum, writing its
    /// outputs; returns its result (the active source flag).
    fn process(&mut self, call: Call, outputs: &mut [Bus]) -> Result<bool, ProcessorError>;
    /// The node's processor goes away.
    fn release(&mut self, node: u32);
    /// Update currentFrame and currentTime for the quantum about to render.
    fn set_time(&mut self, frame: u64);
    /// Run the scope's pending tasks (port messages) and microtasks.
    fn run_tasks(&mut self);
    /// The next host job deadline, even when audio processing is suspended.
    fn next_job_deadline(&self) -> Option<std::time::Instant> {
        None
    }
}

/// AudioWorkletNode's processing on the rendering thread.
pub struct WorkletNode {
    /// [[callable process]]: cleared by an uncaught exception.
    callable: bool,
    /// The processor's active source flag (its last process() result).
    active_source: bool,
    /// The processor exists on the scope.
    created: bool,
    /// Output channel counts from outputChannelCount, if given.
    fixed_outputs: Option<Vec<usize>>,
    /// One input and one output without outputChannelCount: the output
    /// follows computedNumberOfChannels.
    follows_input: bool,
}

impl WorkletNode {
    pub fn new(fixed_outputs: Option<Vec<usize>>, follows_input: bool) -> Self {
        Self {
            callable: true,
            active_source: true,
            created: false,
            fixed_outputs,
            follows_input,
        }
    }
}

impl Processor for WorkletNode {
    /// #AudioNode-actively-processing: while process() is callable and
    /// returned true (or before its first call).
    fn is_source(&self) -> bool {
        self.callable && self.active_source
    }

    fn control(&mut self, control: super::nodes::Control) {
        match control {
            super::nodes::Control::WorkletReady => self.created = true,
            super::nodes::Control::WorkletFailed => self.callable = false,
            _ => {}
        }
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let frames = scope.frames;
        let was_source = self.is_source();
        let output_channels: Vec<usize> = (0..outputs.len())
            .map(|output| match &self.fixed_outputs {
                Some(counts) => counts.get(output).copied().unwrap_or(1),
                None if self.follows_input => inputs.first().map_or(1, Bus::channels),
                None => 1,
            })
            .collect();
        for (output, &channels) in outputs.iter_mut().zip(&output_channels) {
            output.reset(channels.max(1), frames);
        }
        let Some(worklet) = scope.worklet.as_deref_mut() else {
            return false;
        };
        if !self.callable || !self.created {
            return self.is_source();
        }
        let call = Call {
            node: scope.node,
            inputs: inputs
                .iter()
                .zip(scope.inputs_active_each)
                .map(|(bus, &active)| active.then_some(bus))
                .collect(),
            output_channels,
            params: (0..params.len())
                .map(|index| {
                    let param = params.get(index);
                    if param.constant {
                        &param.values[..1]
                    } else {
                        &param.values[..]
                    }
                })
                .collect(),
        };
        match worklet.process(call, outputs) {
            Ok(active) => {
                self.active_source = active;
                // This quantum's output always counts; without input, the
                // result decides whether process() runs again.
                was_source || active
            }
            Err(error) => {
                // #rendering-loop: the processor outputs silence from now on.
                self.callable = false;
                self.active_source = false;
                for output in outputs.iter_mut() {
                    output.reset(1, frames);
                }
                scope.notices.push(super::engine::Notice::ProcessorError {
                    node: scope.node,
                    error,
                });
                false
            }
        }
    }
}
