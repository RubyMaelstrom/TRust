//! ConvolverNode (#ConvolverNode): linear convolution with an impulse
//! response, without added latency, in the channel configurations of
//! #Convolution-channel-configurations (mono, stereo and "true stereo"
//! four-channel responses).
//!
//! Uniformly partitioned overlap-save convolution in two stages: the first
//! 8 render quanta of the response in render-quantum blocks, the rest in
//! blocks of 8 quanta, whose contribution is computed when an input block
//! completes and played over the following block.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::{Bus, Control, Params, Processor, Scope};
use crate::webaudio::AudioData;

/// The tail stage's block, in render quanta.
const TAIL_QUANTA: usize = 8;

/// #dom-convolvernode-normalize: calculateNormalizationScale().
pub fn normalization_scale(data: &AudioData) -> f32 {
    const GAIN_CALIBRATION: f64 = 0.00125;
    const GAIN_CALIBRATION_SAMPLE_RATE: f64 = 44100.0;
    const MIN_POWER: f64 = 0.000125;
    let mut power: f64 = data
        .channels
        .iter()
        .map(|channel| {
            channel
                .iter()
                .map(|&sample| f64::from(sample) * f64::from(sample))
                .sum::<f64>()
        })
        .sum();
    power = (power / (data.channels.len() * data.length) as f64).sqrt();
    if !power.is_finite() || power < MIN_POWER {
        power = MIN_POWER;
    }
    let mut scale = 1.0 / power * GAIN_CALIBRATION;
    if data.rate != 0.0 {
        scale *= GAIN_CALIBRATION_SAMPLE_RATE / f64::from(data.rate);
    }
    if data.channels.len() == 4 {
        scale *= 0.5;
    }
    scale as f32
}

struct StageResponse {
    block: usize,
    /// [response channel][partition] spectra of 2·block points.
    partitions: Vec<Vec<Vec<Complex32>>>,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl StageResponse {
    fn new(channels: &[Vec<f32>], block: usize, planner: &mut FftPlanner<f32>) -> Self {
        let size = 2 * block;
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        let partitions = channels
            .iter()
            .map(|response| {
                response
                    .chunks(block)
                    .map(|part| {
                        let mut spectrum = vec![Complex32::new(0.0, 0.0); size];
                        for (slot, &sample) in spectrum.iter_mut().zip(part) {
                            slot.re = sample;
                        }
                        forward.process(&mut spectrum);
                        spectrum
                    })
                    .collect()
            })
            .collect();
        Self {
            block,
            partitions,
            forward,
            inverse,
        }
    }

    fn count(&self) -> usize {
        self.partitions.first().map_or(0, Vec::len)
    }
}

/// An impulse response prepared for convolution (on the control thread).
pub struct Response {
    channels: usize,
    length: usize,
    head: StageResponse,
    tail: Option<StageResponse>,
}

impl Response {
    /// Partition `data` (1, 2 or 4 channels), scaled when `normalize`.
    pub fn new(data: &AudioData, normalize: bool, quantum: usize) -> Option<Self> {
        if data.length == 0 || !matches!(data.channels.len(), 1 | 2 | 4) {
            return None;
        }
        let scale = if normalize {
            normalization_scale(data)
        } else {
            1.0
        };
        let block = quantum.max(1);
        let split = block * TAIL_QUANTA;
        let mut planner = FftPlanner::new();
        let scaled = |range: std::ops::Range<usize>| -> Vec<Vec<f32>> {
            data.channels
                .iter()
                .map(|channel| {
                    channel[range.clone()]
                        .iter()
                        .map(|sample| sample * scale)
                        .collect()
                })
                .collect()
        };
        let head = StageResponse::new(&scaled(0..data.length.min(split)), block, &mut planner);
        let tail = (data.length > split)
            .then(|| StageResponse::new(&scaled(split..data.length), split, &mut planner));
        Some(Self {
            channels: data.channels.len(),
            length: data.length,
            head,
            tail,
        })
    }
}

/// One stage's input history: per input channel, the previous block and
/// a ring of input spectra (newest at `position`).
struct StageState {
    previous: [Vec<f32>; 2],
    spectra: [Vec<Vec<Complex32>>; 2],
    position: usize,
    scratch: Vec<Complex32>,
    accumulator: Vec<Complex32>,
}

impl StageState {
    fn new(stage: &StageResponse) -> Self {
        let size = 2 * stage.block;
        let ring = || vec![vec![Complex32::new(0.0, 0.0); size]; stage.count().max(1)];
        Self {
            previous: [vec![0.0; stage.block], vec![0.0; stage.block]],
            spectra: [ring(), ring()],
            position: 0,
            scratch: vec![Complex32::new(0.0, 0.0); size],
            accumulator: vec![Complex32::new(0.0, 0.0); size],
        }
    }

    /// Take one block of each input channel, then compute each output
    /// path's block: Σ over its (input, response) pairs and partitions.
    fn run(
        &mut self,
        stage: &StageResponse,
        input: [&[f32]; 2],
        paths: &[&[(usize, usize)]],
        out: &mut [Vec<f32>],
    ) {
        let block = stage.block;
        let count = stage.count().max(1);
        self.position = (self.position + 1) % count;
        for (channel, input) in input.iter().enumerate() {
            let spectrum = &mut self.spectra[channel][self.position];
            for (slot, &sample) in spectrum
                .iter_mut()
                .zip(self.previous[channel].iter().chain(input.iter()))
            {
                *slot = Complex32::new(sample, 0.0);
            }
            stage.forward.process(spectrum);
            self.previous[channel].copy_from_slice(&input[..block]);
        }
        let scale = 1.0 / (2 * block) as f32;
        for (path, target) in paths.iter().zip(out.iter_mut()) {
            self.accumulator.fill(Complex32::new(0.0, 0.0));
            for &(input, response) in path.iter() {
                for (partition, spectrum) in stage.partitions[response].iter().enumerate() {
                    let index = (self.position + count - partition) % count;
                    for ((sum, x), h) in self
                        .accumulator
                        .iter_mut()
                        .zip(&self.spectra[input][index])
                        .zip(spectrum)
                    {
                        *sum += x * h;
                    }
                }
            }
            self.scratch.copy_from_slice(&self.accumulator);
            stage.inverse.process(&mut self.scratch);
            target.resize(block, 0.0);
            for (sample, value) in target.iter_mut().zip(&self.scratch[block..]) {
                *sample = value.re * scale;
            }
        }
    }
}

pub struct Convolver {
    response: Option<Arc<Response>>,
    head: Option<StageState>,
    tail: Option<StageState>,
    /// The tail's input block being collected, and its output being played.
    tail_input: [Vec<f32>; 2],
    tail_output: Vec<Vec<f32>>,
    tail_position: usize,
    head_output: Vec<Vec<f32>>,
    /// Frames since the last non-silent input.
    quiet: usize,
}

impl Convolver {
    pub fn new() -> Self {
        Self {
            response: None,
            head: None,
            tail: None,
            tail_input: [Vec::new(), Vec::new()],
            tail_output: Vec::new(),
            tail_position: 0,
            head_output: Vec::new(),
            quiet: usize::MAX,
        }
    }

    fn set(&mut self, response: Option<Arc<Response>>) {
        self.head = response
            .as_ref()
            .map(|response| StageState::new(&response.head));
        self.tail = response
            .as_ref()
            .and_then(|response| response.tail.as_ref().map(StageState::new));
        let split = response
            .as_ref()
            .and_then(|response| response.tail.as_ref().map(|tail| tail.block))
            .unwrap_or(0);
        self.tail_input = [vec![0.0; split], vec![0.0; split]];
        self.tail_output = vec![vec![0.0; split]; 2];
        self.tail_position = 0;
        self.response = response;
        self.quiet = usize::MAX;
    }
}

impl Default for Convolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for Convolver {
    fn control(&mut self, control: Control) {
        if let Control::Response(response) = control {
            self.set(response);
        }
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let input = &inputs[0];
        let output = &mut outputs[0];
        let frames = scope.frames;
        let Some(response) = self.response.clone() else {
            // No buffer: silence (one channel).
            output.reset(1, frames);
            return false;
        };
        let stereo_input = input.channels() >= 2;
        let channels = [
            input.channel(0),
            input.channel(if stereo_input { 1 } else { 0 }),
        ];
        let paths: &[&[(usize, usize)]] = match (response.channels, stereo_input) {
            (1, false) => &[&[(0, 0)]],
            (1, true) => &[&[(0, 0)], &[(1, 0)]],
            (2, _) => &[&[(0, 0)], &[(1, 1)]],
            _ => &[&[(0, 0), (1, 2)], &[(0, 1), (1, 3)]],
        };
        let audible = channels
            .iter()
            .any(|channel| channel.iter().any(|&sample| sample != 0.0));
        self.quiet = if audible {
            0
        } else {
            self.quiet.saturating_add(frames)
        };
        output.reset(paths.len(), frames);
        self.head_output.resize(paths.len(), Vec::new());
        if let Some(head) = &mut self.head {
            head.run(&response.head, channels, paths, &mut self.head_output);
            for (channel, data) in self.head_output.iter().enumerate().take(paths.len()) {
                output.channel_mut(channel).copy_from_slice(&data[..frames]);
            }
        }
        if let (Some(tail), Some(stage)) = (&mut self.tail, &response.tail) {
            let position = self.tail_position;
            for channel in 0..paths.len() {
                for (sample, add) in output
                    .channel_mut(channel)
                    .iter_mut()
                    .zip(&self.tail_output[channel][position..position + frames])
                {
                    *sample += add;
                }
            }
            for (collected, channel) in self.tail_input.iter_mut().zip(channels) {
                collected[position..position + frames].copy_from_slice(channel);
            }
            self.tail_position += frames;
            if self.tail_position == stage.block {
                self.tail_position = 0;
                let [first, second] = &self.tail_input;
                tail.run(stage, [first, second], paths, &mut self.tail_output);
            }
        }
        // Input still echoing through the response.
        self.quiet <= response.length + 2 * frames * (TAIL_QUANTA + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webaudio::nodes::Params;

    fn run(response: &AudioData, input: &[f32], quantum: usize) -> Vec<f32> {
        let mut convolver = Convolver::new();
        convolver.set(Response::new(response, false, quantum).map(Arc::new));
        let mut notices = Vec::new();
        let params = Params {
            slab: &[],
            ids: &[],
        };
        let mut result = Vec::new();
        for chunk in input.chunks(quantum) {
            let mut bus = Bus::new(1, quantum);
            bus.channel_mut(0)[..chunk.len()].copy_from_slice(chunk);
            let mut outputs = [Bus::default()];
            let mut scope = Scope {
                frame: 0,
                rate: 48_000.0,
                frames: quantum,
                node: 1,
                inputs_active: true,
                connected: true,
                notices: &mut notices,
            };
            convolver.process(
                &mut scope,
                std::slice::from_ref(&bus),
                &mut outputs,
                &params,
            );
            result.extend_from_slice(outputs[0].channel(0));
        }
        result
    }

    #[test]
    fn matches_direct_convolution_across_both_stages() {
        let quantum = 16;
        let length = 16 * TAIL_QUANTA * 3 + 5;
        let ir: Vec<f32> = (0..length)
            .map(|n| ((n * 7919) % 23) as f32 / 23.0 - 0.5)
            .collect();
        let response = AudioData {
            rate: 48_000.0,
            length,
            channels: vec![ir.clone().into_boxed_slice()],
        };
        let input: Vec<f32> = (0..quantum * 40)
            .map(|n| ((n * 104_729) % 17) as f32 / 17.0 - 0.5)
            .collect();
        let output = run(&response, &input, quantum);
        for (n, &value) in output.iter().enumerate() {
            let expected: f32 = (0..=n.min(length - 1)).map(|k| ir[k] * input[n - k]).sum();
            assert!(
                (value - expected).abs() < 1e-3,
                "{n}: {value} vs {expected}"
            );
        }
    }

    #[test]
    fn normalization_follows_the_reference_algorithm() {
        let data = AudioData {
            rate: 44_100.0,
            length: 2,
            channels: vec![vec![1.0, 1.0].into_boxed_slice()],
        };
        assert!((normalization_scale(&data) - 0.00125).abs() < 1e-9);
    }
}
