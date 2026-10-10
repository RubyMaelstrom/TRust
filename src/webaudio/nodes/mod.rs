//! AudioNode processing (#rendering-loop "processing an input buffer" and
//! "computing a block of audio"), one [`Processor`] per node type.

use std::sync::Arc;

use super::AudioData;
use super::bus::Bus;
use super::engine::Notice;
use super::param::Param;
use super::wave::Wave;

pub mod analyser;
pub mod biquad;
mod buffer_source;
mod channels;
mod compressor;
mod constant;
pub mod convolver;
mod delay;
mod gain;
pub mod iir;
mod media_element;
mod oscillator;
pub mod script;
mod shaper;
mod stereo_panner;

pub use analyser::Analyser;
pub use biquad::Biquad;
pub use buffer_source::BufferSource;
pub use channels::{Merger, Splitter};
pub use compressor::Compressor;
pub use constant::ConstantSource;
pub use convolver::Convolver;
pub use delay::Delay;
pub use gain::Gain;
pub use iir::Iir;
pub use media_element::MediaElementSource;
pub use oscillator::Oscillator;
pub use script::ScriptProcessor;
pub use shaper::WaveShaper;
pub use stereo_panner::StereoPanner;

/// What a processor knows about the render quantum it computes.
pub struct Scope<'a> {
    /// [[current frame]] at the start of the quantum.
    pub frame: u64,
    pub rate: f32,
    /// The render quantum size.
    pub frames: usize,
    /// The node's id, for notices.
    pub node: u32,
    /// Whether any node connected to an input is actively processing.
    pub inputs_active: bool,
    /// Whether anything is connected to the node's inputs or outputs.
    pub connected: bool,
    pub notices: &'a mut Vec<Notice>,
}

impl Scope<'_> {
    /// The context time of frame `index` of this quantum.
    pub fn time(&self, index: usize) -> f64 {
        (self.frame + index as u64) as f64 / f64::from(self.rate)
    }
}

/// A node's AudioParams, in the order its processor defines.
pub struct Params<'a> {
    pub(super) slab: &'a [Option<super::graph::ParamSlot>],
    pub(super) ids: &'a [usize],
}

impl Params<'_> {
    pub fn get(&self, index: usize) -> &Param {
        &self.slab[self.ids[index]]
            .as_ref()
            .expect("a node's parameter outlives it")
            .param
    }

    /// The parameter's value for this quantum (a k-rate value, or the first
    /// a-rate value).
    pub fn first(&self, index: usize) -> f32 {
        self.get(index).values[0]
    }
}

/// A node-specific control message (attribute setters and methods).
#[derive(Clone)]
pub enum Control {
    /// AudioScheduledSourceNode start(when[, offset[, duration]]).
    Start {
        when: f64,
        offset: f64,
        duration: Option<f64>,
    },
    Stop(f64),
    /// OscillatorNode `type` / setPeriodicWave().
    Wave(Arc<Wave>),
    /// AudioBufferSourceNode's acquired buffer content.
    Buffer(Option<Arc<AudioData>>),
    Loop(bool),
    LoopStart(f64),
    LoopEnd(f64),
    /// BiquadFilterNode `type` (a `biquad::Kind` index).
    FilterType(u64),
    /// WaveShaperNode `curve`.
    Curve(Option<Arc<[f32]>>),
    /// WaveShaperNode `oversample` factor (1, 2 or 4).
    Oversample(usize),
    /// ConvolverNode `buffer`, prepared on the control thread.
    Response(Option<Arc<convolver::Response>>),
}

/// One node type's processing.
pub trait Processor: Send {
    /// Whether the node decides when it is actively processing (a source
    /// node, #AudioNode-actively-processing), rather than following its
    /// inputs.
    fn is_source(&self) -> bool {
        false
    }

    /// Compute one render quantum into `outputs` (each output sets its own
    /// channel count). A source returns whether it is actively processing
    /// during this quantum; any other node is called only while an input
    /// is actively processing or its last call returned `true`, and
    /// returns whether its output may still be non-silent with silent
    /// input (its tail).
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool;

    fn control(&mut self, _control: Control) {}

    /// Whether this is a DelayNode, which can break a cycle.
    fn is_delay(&self) -> bool {
        false
    }

    /// A cycle-breaking DelayNode's DelayReader: produce this quantum from
    /// what was written at least a render quantum ago; returns whether it
    /// is actively processing.
    fn read(&mut self, _scope: &mut Scope, _outputs: &mut [Bus], _params: &Params) -> bool {
        false
    }

    /// Its DelayWriter: record this quantum's input.
    fn write(&mut self, _scope: &mut Scope, _inputs: &[Bus], _params: &Params) {}
}

/// AudioDestinationNode: records its input (the graph reads its output).
pub struct Destination;

impl Processor for Destination {
    fn process(
        &mut self,
        _scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        outputs[0].copy_from(&inputs[0]);
        false
    }
}

/// A node whose processing is not implemented yet: it outputs silence.
pub struct Silent {
    pub channels: usize,
}

impl Processor for Silent {
    fn is_source(&self) -> bool {
        true
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        _inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        for output in outputs {
            output.reset(self.channels.max(1), scope.frames);
        }
        false
    }
}
