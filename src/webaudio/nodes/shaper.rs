//! WaveShaperNode (#WaveShaperNode): the curve, linearly interpolated over
//! [-1, 1], optionally at 2× or 4× the sample rate between windowed-sinc
//! interpolation and decimation filters.

use std::f64::consts::PI;
use std::sync::Arc;

use super::{Bus, Control, Params, Processor, Scope};

/// A lowpass FIR for one oversampling factor: cutoff at the original
/// Nyquist frequency, 16 zero crossings each side, Blackman window.
fn filter(factor: usize) -> Vec<f64> {
    let half = 16 * factor;
    (0..=2 * half)
        .map(|index| {
            let n = index as f64 - half as f64;
            let x = n / factor as f64;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (PI * x).sin() / (PI * x)
            };
            let phase = 2.0 * PI * index as f64 / (2 * half) as f64;
            let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            sinc * window / factor as f64
        })
        .collect()
}

#[derive(Default)]
struct Channel {
    /// Oversampled input history for the interpolation filter, and shaped
    /// history for the decimation filter.
    up: Vec<f64>,
    down: Vec<f64>,
}

pub struct WaveShaper {
    curve: Option<Arc<[f32]>>,
    factor: usize,
    taps: Vec<f64>,
    channels: Vec<Channel>,
}

impl WaveShaper {
    pub fn new(curve: Option<Arc<[f32]>>, factor: usize) -> Self {
        let mut shaper = Self {
            curve,
            factor: 1,
            taps: Vec::new(),
            channels: Vec::new(),
        };
        shaper.set_factor(factor);
        shaper
    }

    fn set_factor(&mut self, factor: usize) {
        let factor = if matches!(factor, 2 | 4) { factor } else { 1 };
        if factor != self.factor {
            self.factor = factor;
            self.taps = if factor > 1 {
                filter(factor)
            } else {
                Vec::new()
            };
            self.channels.clear();
        }
    }

    fn shape(curve: &[f32], x: f64) -> f64 {
        let n = curve.len();
        let v = (n - 1) as f64 / 2.0 * (x + 1.0);
        if v < 0.0 || v.is_nan() {
            return f64::from(curve[0]);
        }
        if v >= (n - 1) as f64 {
            return f64::from(curve[n - 1]);
        }
        let k = v.floor();
        let f = v - k;
        let k = k as usize;
        (1.0 - f) * f64::from(curve[k]) + f * f64::from(curve[k + 1])
    }
}

/// Push `value` into a history of `length` samples (most recent last) and
/// return the FIR output.
fn convolve(history: &mut Vec<f64>, taps: &[f64], value: f64) -> f64 {
    if history.len() != taps.len() {
        history.resize(taps.len(), 0.0);
    }
    history.rotate_left(1);
    *history.last_mut().expect("taps") = value;
    history
        .iter()
        .zip(taps.iter().rev())
        .map(|(x, h)| x * h)
        .sum()
}

impl Processor for WaveShaper {
    /// A curve that maps silence to a non-zero value produces that DC value
    /// without any input (#WaveShaperNode note).
    fn is_source(&self) -> bool {
        self.curve
            .as_ref()
            .is_some_and(|curve| Self::shape(curve, 0.0) != 0.0)
    }

    fn control(&mut self, control: Control) {
        match control {
            Control::Curve(curve) => self.curve = curve,
            Control::Oversample(factor) => self.set_factor(factor),
            _ => {}
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
        let Some(curve) = self.curve.clone() else {
            output.copy_from(input);
            return scope.inputs_active;
        };
        let channels = input.channels();
        output.reset(channels, scope.frames);
        if self.factor == 1 {
            for channel in 0..channels {
                for (out, &x) in output
                    .channel_mut(channel)
                    .iter_mut()
                    .zip(input.channel(channel))
                {
                    *out = Self::shape(&curve, f64::from(x)) as f32;
                }
            }
            return scope.inputs_active || self.is_source();
        }
        let factor = self.factor;
        self.channels.resize_with(channels, Channel::default);
        let mut ringing = false;
        for (index, state) in self.channels.iter_mut().enumerate() {
            let source = input.channel(index);
            let target = output.channel_mut(index);
            for (frame, &x) in source.iter().enumerate() {
                let mut kept = 0.0;
                for phase in 0..factor {
                    // Zero-stuffed input, gain restored by the factor.
                    let stuffed = if phase == 0 {
                        f64::from(x) * factor as f64
                    } else {
                        0.0
                    };
                    let up = convolve(&mut state.up, &self.taps, stuffed);
                    let shaped = Self::shape(&curve, up);
                    let down = convolve(&mut state.down, &self.taps, shaped);
                    if phase == 0 {
                        kept = down;
                    }
                }
                target[frame] = kept as f32;
            }
            ringing |= state.up.iter().any(|value| value.abs() > 1e-12);
        }
        scope.inputs_active || ringing || self.is_source()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_interpolation_follows_the_spec() {
        let curve: Arc<[f32]> = Arc::from([-1.0f32, 0.0, 1.0]);
        assert_eq!(WaveShaper::shape(&curve, 0.5), 0.5);
        assert_eq!(WaveShaper::shape(&curve, -3.0), -1.0);
        assert_eq!(WaveShaper::shape(&curve, 1.0), 1.0);
        let even: Arc<[f32]> = Arc::from([0.0f32, 2.0]);
        assert_eq!(WaveShaper::shape(&even, 0.0), 1.0);
        assert!(WaveShaper::new(Some(even), 1).is_source());
    }

    #[test]
    fn identity_curve_survives_oversampling() {
        // Unit gain at DC for the decimation filter; the interpolation
        // filter's zero-stuffed input carries the factor.
        let dc: f64 = filter(4).iter().sum();
        assert!((dc - 1.0).abs() < 1e-3, "{dc}");
        let mut shaper = WaveShaper::new(Some(Arc::from([-1.0f32, 1.0])), 4);
        let mut notices = Vec::new();
        let mut scope = Scope {
            worklet: None,
            inputs_active_each: &[true],
            frame: 0,
            rate: 48_000.0,
            frames: 128,
            node: 1,
            inputs_active: true,
            connected: true,
            notices: &mut notices,
        };
        let mut input = Bus::new(1, 128);
        input.channel_mut(0).fill(0.25);
        let mut outputs = [Bus::default()];
        let params = Params {
            slab: &[],
            ids: &[],
        };
        for _ in 0..3 {
            shaper.process(
                &mut scope,
                std::slice::from_ref(&input),
                &mut outputs,
                &params,
            );
        }
        assert!(
            (outputs[0].channel(0)[127] - 0.25).abs() < 1e-3,
            "{}",
            outputs[0].channel(0)[127]
        );
    }
}
