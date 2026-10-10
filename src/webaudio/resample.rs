//! Band-limited sample-rate conversion (decodeAudioData()'s "resample it
//! to the sample-rate of the BaseAudioContext"): a Blackman-windowed sinc
//! with 16 zero crossings each side, its cutoff lowered to the output's
//! Nyquist frequency when downsampling, read from a precomputed table.

use std::f64::consts::PI;
use std::sync::OnceLock;

const ZERO_CROSSINGS: usize = 16;
const STEPS: usize = 512;

/// The windowed sinc at u = i / STEPS, for 0 ≤ u ≤ ZERO_CROSSINGS.
fn kernel() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=ZERO_CROSSINGS * STEPS + 1)
            .map(|index| {
                let u = index as f64 / STEPS as f64;
                if u >= ZERO_CROSSINGS as f64 {
                    return 0.0;
                }
                let sinc = if u == 0.0 {
                    1.0
                } else {
                    (PI * u).sin() / (PI * u)
                };
                // Blackman window over [-Z, Z].
                let phase = PI * (u / ZERO_CROSSINGS as f64 + 1.0);
                let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
                (sinc * window) as f32
            })
            .collect()
    })
}

/// Convert one channel from `from` Hz to `to` Hz.
pub fn resample(input: &[f32], from: f64, to: f64) -> Vec<f32> {
    if input.is_empty() || from == to || !(from > 0.0 && to > 0.0) {
        return input.to_vec();
    }
    let table = kernel();
    let ratio = from / to;
    // The cutoff, as a fraction of the input's Nyquist frequency.
    let cutoff = (to / from).min(1.0);
    let reach = (ZERO_CROSSINGS as f64 / cutoff).ceil() as isize;
    let length = (input.len() as f64 / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(length);
    let scale = cutoff * STEPS as f64;
    for index in 0..length {
        let position = index as f64 * ratio;
        let center = position.floor() as isize;
        let first = (center - reach + 1).max(0);
        let last = (center + reach).min(input.len() as isize - 1);
        let (mut sum, mut weights) = (0.0f64, 0.0f64);
        for k in first..=last {
            let at = (position - k as f64).abs() * scale;
            let step = at as usize;
            if step >= ZERO_CROSSINGS * STEPS {
                continue;
            }
            let fraction = at - step as f64;
            let weight = f64::from(table[step])
                + (f64::from(table[step + 1]) - f64::from(table[step])) * fraction;
            sum += f64::from(input[k as usize]) * weight;
            weights += weight;
        }
        // Normalizing keeps DC exact near the ends of the signal too.
        output.push(if weights.abs() > 1e-9 {
            (sum / weights) as f32
        } else {
            0.0
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, rate: f64, length: usize) -> Vec<f32> {
        (0..length)
            .map(|n| (2.0 * PI * frequency * n as f64 / rate).sin() as f32)
            .collect()
    }

    #[test]
    fn preserves_a_tone_and_its_length() {
        let input = sine(1000.0, 44_100.0, 44_100);
        let output = resample(&input, 44_100.0, 48_000.0);
        assert_eq!(output.len(), 48_000);
        let expected = sine(1000.0, 48_000.0, 48_000);
        let error = output[1000..47_000]
            .iter()
            .zip(&expected[1000..47_000])
            .fold(0f32, |worst, (a, b)| worst.max((a - b).abs()));
        assert!(error < 1e-3, "{error}");
    }

    #[test]
    fn downsampling_removes_content_above_the_new_nyquist() {
        // 20 kHz cannot exist at 22.05 kHz; it must not alias down.
        let input = sine(20_000.0, 48_000.0, 48_000);
        let output = resample(&input, 48_000.0, 22_050.0);
        let peak = output[500..output.len() - 500]
            .iter()
            .fold(0f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak < 0.01, "{peak}");
    }
}
