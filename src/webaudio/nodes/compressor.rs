//! DynamicsCompressorNode (#DynamicsCompressorOptions-processing).
//!
//! The user-agent-defined parts are: a soft knee that is quadratic in
//! decibels between threshold and threshold + knee (continuous, with slope
//! 1/ratio where it meets the ratio segment); a detector curve
//! r(a) = d (1 + a) / 2 with a 1 ms smoothing coefficient d; an attack curve
//! c_a (1 + x) / 2 on x = detector average / compressor gain in [0, 1]; and
//! a release curve 1 + c_r · 2 / (1 + x) with c_r reaching +10 dB over the
//! release time. The pre-delay is the 6 ms DelayNode of the internal graph.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{Bus, Params, Processor, Scope};

pub struct Compressor {
    /// The `reduction` attribute's value, shared with the control thread.
    reduction: Arc<AtomicU32>,
    detector_average: f64,
    compressor_gain: f64,
    delay: Vec<Vec<f32>>,
    delay_write: usize,
}

fn linear_to_decibels(value: f64) -> f64 {
    if value == 0.0 {
        -1000.0
    } else {
        20.0 * value.log10()
    }
}

fn decibels_to_linear(value: f64) -> f64 {
    10f64.powf(value / 20.0)
}

fn curve(x: f64, threshold: f64, knee: f64, ratio: f64) -> f64 {
    if x <= threshold {
        return x;
    }
    if knee > 0.0 && x < threshold + knee {
        return x + (1.0 / ratio - 1.0) * (x - threshold) * (x - threshold) / (2.0 * knee);
    }
    threshold + knee / 2.0 + knee / (2.0 * ratio) + (x - threshold - knee) / ratio
}

impl Compressor {
    pub fn new(reduction: Arc<AtomicU32>) -> Self {
        reduction.store(0f32.to_bits(), Ordering::Relaxed);
        Self {
            reduction,
            detector_average: 0.0,
            compressor_gain: 1.0,
            delay: Vec::new(),
            delay_write: 0,
        }
    }
}

impl Processor for Compressor {
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let input = &inputs[0];
        let frames = scope.frames;
        let rate = f64::from(scope.rate);
        let [threshold, knee, ratio, attack, release] =
            [0, 1, 2, 3, 4].map(|index| f64::from(params.first(index)));
        let channels = input.channels();
        let delay_frames = 0.006 * rate;
        if self.delay.len() != channels {
            let size = delay_frames.ceil() as usize + frames + 2;
            self.delay.resize_with(channels, || vec![0.0; size]);
        }
        let makeup = (1.0 / decibels_to_linear(curve(0.0, threshold, knee, ratio))).powf(0.6);
        let detector = 1.0 - (-1.0 / (0.001 * rate).max(1.0)).exp();
        let attack_rate = 1.0 - (-1.0 / (attack * rate).max(1.0)).exp();
        let release_step = 10f64.powf(0.5 / (release * rate).max(1.0)) - 1.0;
        let mut detector_average = self.detector_average;
        let mut compressor_gain = self.compressor_gain;
        let mut reduction_gain = compressor_gain * makeup;
        let output = &mut outputs[0];
        output.reset(channels, frames);
        let size = self.delay[0].len();
        let mut audible = false;
        for index in 0..frames {
            let mut level = 0.0f64;
            for channel in 0..channels {
                level = level.max(f64::from(input.channel(channel)[index]).abs());
            }
            let mut attenuation = 1.0;
            if level >= 0.0001 {
                let shaped =
                    decibels_to_linear(curve(linear_to_decibels(level), threshold, knee, ratio));
                attenuation = shaped / level;
            }
            let releasing = attenuation > compressor_gain;
            detector_average +=
                (attenuation - detector_average) * detector * (1.0 + attenuation) / 2.0;
            detector_average = detector_average.min(1.0);
            let x = if compressor_gain > 0.0 {
                detector_average / compressor_gain
            } else {
                1.0
            };
            if releasing {
                compressor_gain =
                    (compressor_gain * (1.0 + release_step * 2.0 / (1.0 + x.max(0.0)))).min(1.0);
            } else {
                compressor_gain +=
                    (detector_average - compressor_gain) * attack_rate * (1.0 + x.clamp(0.0, 1.0))
                        / 2.0;
            }
            reduction_gain = compressor_gain * makeup;
            // The pre-delayed signal, read with linear interpolation.
            let write = self.delay_write;
            for channel in 0..channels {
                self.delay[channel][write] = input.channel(channel)[index];
            }
            let mut read = write as f64 - delay_frames;
            while read < 0.0 {
                read += size as f64;
            }
            let base = read.floor();
            let fraction = read - base;
            let base = base as usize;
            let next = if base + 1 == size { 0 } else { base + 1 };
            for channel in 0..channels {
                let line = &self.delay[channel];
                let (a, b) = (f64::from(line[base]), f64::from(line[next]));
                let value = ((a + (b - a) * fraction) * reduction_gain) as f32;
                audible |= value != 0.0;
                output.channel_mut(channel)[index] = value;
            }
            self.delay_write = if write + 1 == size { 0 } else { write + 1 };
        }
        self.detector_average = detector_average;
        self.compressor_gain = compressor_gain;
        self.reduction.store(
            (linear_to_decibels(reduction_gain) as f32).to_bits(),
            Ordering::Relaxed,
        );
        // The pre-delay still holds input, or the gain is still releasing.
        audible
            || compressor_gain < 0.999_999
            || self
                .delay
                .iter()
                .any(|line| line.iter().any(|&sample| sample != 0.0))
    }
}
