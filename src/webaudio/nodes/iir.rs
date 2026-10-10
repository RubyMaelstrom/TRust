//! IIRFilterNode (#IIRFilterNode, #IIRFilterNode-filter-definition):
//! Σ a_k y(n-k) = Σ b_k x(n-k), in double precision from an all-zero state.

use std::f64::consts::PI;
use std::sync::Arc;

use super::{Bus, Params, Processor, Scope};

/// Fixed coefficients, normalized so that a0 = 1.
#[derive(Clone, Debug)]
pub struct Coefficients {
    pub feedforward: Arc<[f64]>,
    pub feedback: Arc<[f64]>,
}

impl Coefficients {
    pub fn new(feedforward: &[f64], feedback: &[f64]) -> Self {
        let a0 = feedback[0];
        Self {
            feedforward: feedforward.iter().map(|b| b / a0).collect(),
            feedback: feedback.iter().map(|a| a / a0).collect(),
        }
    }

    /// The magnitude and phase of H(e^{jω}) at `frequency` Hz.
    pub fn response(&self, frequency: f64, rate: f64) -> (f64, f64) {
        let omega = 2.0 * PI * frequency / rate;
        let evaluate = |coefficients: &[f64]| {
            coefficients
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(re, im), (k, c)| {
                    let angle = -omega * k as f64;
                    (re + c * angle.cos(), im + c * angle.sin())
                })
        };
        let (nr, ni) = evaluate(&self.feedforward);
        let (dr, di) = evaluate(&self.feedback);
        let denominator = dr * dr + di * di;
        let (hr, hi) = (
            (nr * dr + ni * di) / denominator,
            (ni * dr - nr * di) / denominator,
        );
        (hr.hypot(hi), hi.atan2(hr))
    }
}

pub struct Iir {
    coefficients: Coefficients,
    /// Per channel: past inputs and outputs, most recent first.
    history: Vec<(Vec<f64>, Vec<f64>)>,
}

impl Iir {
    pub fn new(coefficients: Coefficients) -> Self {
        Self {
            coefficients,
            history: Vec::new(),
        }
    }
}

impl Processor for Iir {
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let input = &inputs[0];
        let output = &mut outputs[0];
        let channels = input.channels();
        output.reset(channels, scope.frames);
        let (b, a) = (&self.coefficients.feedforward, &self.coefficients.feedback);
        self.history
            .resize_with(channels, || (vec![0.0; b.len()], vec![0.0; a.len()]));
        let mut ringing = false;
        for (channel, (xs, ys)) in self.history.iter_mut().enumerate() {
            let source = input.channel(channel);
            let target = output.channel_mut(channel);
            for index in 0..scope.frames {
                xs.rotate_right(1);
                xs[0] = f64::from(source[index]);
                let mut y: f64 = b.iter().zip(xs.iter()).map(|(b, x)| b * x).sum();
                // ys[0] is y(n-1) before the rotation below.
                y -= a
                    .iter()
                    .skip(1)
                    .zip(ys.iter())
                    .map(|(a, y)| a * y)
                    .sum::<f64>();
                if y.abs() < 1e-30 {
                    y = 0.0;
                }
                ys.rotate_right(1);
                ys[0] = y;
                target[index] = y as f32;
            }
            ringing |= xs.iter().chain(ys.iter()).any(|value| value.abs() > 1e-12);
        }
        ringing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_pole_lowpass_matches_its_response() {
        let filter = Coefficients::new(&[0.5], &[1.0, -0.5]);
        // H(1) = 0.5 / (1 - 0.5) = 1 at DC.
        assert!((filter.response(0.0, 48_000.0).0 - 1.0).abs() < 1e-12);
        // At Nyquist z = -1: 0.5 / 1.5.
        assert!((filter.response(24_000.0, 48_000.0).0 - 1.0 / 3.0).abs() < 1e-12);
    }
}
