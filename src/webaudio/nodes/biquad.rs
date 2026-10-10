//! BiquadFilterNode (#BiquadFilterNode, #filters-characteristics): the Audio
//! EQ Cookbook filters with the spec's coefficients, run in direct form I
//! in double precision. Where a formula degenerates (a cutoff at 0 or the
//! Nyquist frequency, Q = 0) the coefficients are its limit.

use std::f64::consts::PI;

use super::{Bus, Params, Processor, Scope};

/// `BiquadFilterType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Lowpass,
    Highpass,
    Bandpass,
    Lowshelf,
    Highshelf,
    Peaking,
    Notch,
    Allpass,
}

impl Kind {
    pub fn from_index(index: u64) -> Self {
        match index {
            1 => Self::Highpass,
            2 => Self::Bandpass,
            3 => Self::Lowshelf,
            4 => Self::Highshelf,
            5 => Self::Peaking,
            6 => Self::Notch,
            7 => Self::Allpass,
            _ => Self::Lowpass,
        }
    }
}

/// Normalized coefficients: b0, b1, b2, a1, a2 (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coefficients([f64; 5]);

impl Coefficients {
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self([b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0])
    }

    fn gain(value: f64) -> Self {
        Self([value, 0.0, 0.0, 0.0, 0.0])
    }

    /// The coefficients for `frequency` (computedFrequency, Hz), `q` and
    /// `gain` (dB) at `rate`.
    pub fn compute(kind: Kind, frequency: f64, q: f64, gain: f64, rate: f64) -> Self {
        let nyquist = rate / 2.0;
        // The cutoff as a fraction of the Nyquist frequency.
        let cutoff = (frequency / nyquist).clamp(0.0, 1.0);
        let a = 10f64.powf(gain / 40.0);
        let w0 = PI * cutoff;
        let (sin, cos) = (w0.sin(), w0.cos());
        match kind {
            Kind::Lowpass => {
                if cutoff >= 1.0 {
                    return Self::gain(1.0);
                }
                if cutoff <= 0.0 {
                    return Self::gain(0.0);
                }
                let alpha = sin / (2.0 * 10f64.powf(q / 20.0));
                Self::new(
                    (1.0 - cos) / 2.0,
                    1.0 - cos,
                    (1.0 - cos) / 2.0,
                    1.0 + alpha,
                    -2.0 * cos,
                    1.0 - alpha,
                )
            }
            Kind::Highpass => {
                if cutoff >= 1.0 {
                    return Self::gain(0.0);
                }
                if cutoff <= 0.0 {
                    return Self::gain(1.0);
                }
                let alpha = sin / (2.0 * 10f64.powf(q / 20.0));
                Self::new(
                    (1.0 + cos) / 2.0,
                    -(1.0 + cos),
                    (1.0 + cos) / 2.0,
                    1.0 + alpha,
                    -2.0 * cos,
                    1.0 - alpha,
                )
            }
            Kind::Bandpass => {
                if cutoff <= 0.0 || cutoff >= 1.0 {
                    return Self::gain(0.0);
                }
                if q <= 0.0 {
                    return Self::gain(1.0);
                }
                let alpha = sin / (2.0 * q);
                Self::new(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Kind::Notch => {
                if cutoff <= 0.0 || cutoff >= 1.0 {
                    return Self::gain(1.0);
                }
                if q <= 0.0 {
                    return Self::gain(0.0);
                }
                let alpha = sin / (2.0 * q);
                Self::new(1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Kind::Allpass => {
                if cutoff <= 0.0 || cutoff >= 1.0 {
                    return Self::gain(1.0);
                }
                if q <= 0.0 {
                    return Self::gain(-1.0);
                }
                let alpha = sin / (2.0 * q);
                Self::new(
                    1.0 - alpha,
                    -2.0 * cos,
                    1.0 + alpha,
                    1.0 + alpha,
                    -2.0 * cos,
                    1.0 - alpha,
                )
            }
            Kind::Peaking => {
                if cutoff <= 0.0 || cutoff >= 1.0 {
                    return Self::gain(1.0);
                }
                if q <= 0.0 {
                    return Self::gain(a * a);
                }
                let alpha = sin / (2.0 * q);
                Self::new(
                    1.0 + alpha * a,
                    -2.0 * cos,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos,
                    1.0 - alpha / a,
                )
            }
            Kind::Lowshelf | Kind::Highshelf => {
                let high = kind == Kind::Highshelf;
                if cutoff >= 1.0 {
                    return Self::gain(if high { 1.0 } else { a * a });
                }
                if cutoff <= 0.0 {
                    return Self::gain(if high { a * a } else { 1.0 });
                }
                // alpha_S with the shelf slope S = 1.
                let shelf_slope = 1.0;
                let alpha = sin / 2.0 * ((a + 1.0 / a) * (1.0 / shelf_slope - 1.0) + 2.0).sqrt();
                let k2 = 2.0 * a.sqrt() * alpha;
                let (plus, minus) = (a + 1.0, a - 1.0);
                if high {
                    Self::new(
                        a * (plus + minus * cos + k2),
                        -2.0 * a * (minus + plus * cos),
                        a * (plus + minus * cos - k2),
                        plus - minus * cos + k2,
                        2.0 * (minus - plus * cos),
                        plus - minus * cos - k2,
                    )
                } else {
                    Self::new(
                        a * (plus - minus * cos + k2),
                        2.0 * a * (minus - plus * cos),
                        a * (plus - minus * cos - k2),
                        plus + minus * cos + k2,
                        -2.0 * (minus + plus * cos),
                        plus + minus * cos - k2,
                    )
                }
            }
        }
    }

    /// The magnitude and phase of H(e^{jω}) at `frequency` Hz.
    pub fn response(&self, frequency: f64, rate: f64) -> (f64, f64) {
        let [b0, b1, b2, a1, a2] = self.0;
        let omega = 2.0 * PI * frequency / rate;
        // z^-1 = e^{-jω}.
        let (c1, s1) = (omega.cos(), -omega.sin());
        let (c2, s2) = ((2.0 * omega).cos(), -(2.0 * omega).sin());
        let (nr, ni) = (b0 + b1 * c1 + b2 * c2, b1 * s1 + b2 * s2);
        let (dr, di) = (1.0 + a1 * c1 + a2 * c2, a1 * s1 + a2 * s2);
        let denominator = dr * dr + di * di;
        let (hr, hi) = (
            (nr * dr + ni * di) / denominator,
            (ni * dr - nr * di) / denominator,
        );
        (hr.hypot(hi), hi.atan2(hr))
    }
}

/// computedFrequency = frequency · 2^(detune/1200), in [0, Nyquist].
pub fn computed_frequency(frequency: f32, detune: f32, rate: f32) -> f64 {
    (f64::from(frequency) * 2f64.powf(f64::from(detune) / 1200.0)).clamp(0.0, f64::from(rate) / 2.0)
}

pub struct Biquad {
    kind: Kind,
    /// Per channel: x[n-1], x[n-2], y[n-1], y[n-2].
    state: Vec<[f64; 4]>,
}

impl Biquad {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            state: Vec::new(),
        }
    }
}

impl Processor for Biquad {
    fn control(&mut self, control: super::Control) {
        if let super::Control::FilterType(index) = control {
            self.kind = Kind::from_index(index);
        }
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let input = &inputs[0];
        let output = &mut outputs[0];
        let channels = input.channels();
        output.reset(channels, scope.frames);
        self.state.resize(channels, [0.0; 4]);
        let (frequency, detune, q, gain) =
            (params.get(0), params.get(1), params.get(2), params.get(3));
        let rate = f64::from(scope.rate);
        let constant = frequency.constant && detune.constant && q.constant && gain.constant;
        let coefficients_at = |index: usize| {
            Coefficients::compute(
                self.kind,
                computed_frequency(frequency.values[index], detune.values[index], scope.rate),
                f64::from(q.values[index]),
                f64::from(gain.values[index]),
                rate,
            )
        };
        let fixed = coefficients_at(0);
        let mut ringing = false;
        for (channel, state) in self.state.iter_mut().enumerate() {
            let source = input.channel(channel);
            let target = output.channel_mut(channel);
            let [mut x1, mut x2, mut y1, mut y2] = *state;
            for index in 0..scope.frames {
                let coefficients = if constant {
                    fixed
                } else {
                    coefficients_at(index)
                };
                let [b0, b1, b2, a1, a2] = coefficients.0;
                let x = f64::from(source[index]);
                let mut y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
                if y.abs() < 1e-30 {
                    y = 0.0;
                }
                (x2, x1, y2, y1) = (x1, x, y1, y);
                target[index] = y as f32;
            }
            *state = [x1, x2, y1, y2];
            ringing |= state.iter().any(|value| value.abs() > 1e-12);
        }
        ringing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookbook_responses() {
        let rate = 48_000.0;
        let lowpass = Coefficients::compute(Kind::Lowpass, 1000.0, 0.0, 0.0, rate);
        // Q = 0 dB (linear 1): unity at the cutoff and at DC, nothing near
        // Nyquist; Q = -3.0103 dB is the Butterworth -3 dB point.
        assert!((lowpass.response(1000.0, rate).0 - 1.0).abs() < 1e-6);
        let butterworth =
            Coefficients::compute(Kind::Lowpass, 1000.0, -3.010_299_956_639_812, 0.0, rate);
        assert!((butterworth.response(1000.0, rate).0 - 0.5f64.sqrt()).abs() < 1e-6);
        assert!((lowpass.response(0.0, rate).0 - 1.0).abs() < 1e-9);
        assert!(lowpass.response(20_000.0, rate).0 < 0.01);
        let peaking = Coefficients::compute(Kind::Peaking, 1000.0, 1.0, 6.0, rate);
        assert!((peaking.response(1000.0, rate).0 - 10f64.powf(6.0 / 20.0)).abs() < 1e-6);
        let highshelf = Coefficients::compute(Kind::Highshelf, 1000.0, 1.0, -12.0, rate);
        assert!((highshelf.response(20_000.0, rate).0 - 10f64.powf(-12.0 / 20.0)).abs() < 1e-2);
        assert_eq!(
            Coefficients::compute(Kind::Lowpass, 30_000.0, 1.0, 0.0, rate),
            Coefficients::gain(1.0)
        );
        let allpass = Coefficients::compute(Kind::Allpass, 3000.0, 2.0, 0.0, rate);
        assert!((allpass.response(500.0, rate).0 - 1.0).abs() < 1e-9);
    }
}
