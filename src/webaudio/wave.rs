//! PeriodicWave (#PeriodicWave) and the band-limited tables oscillators
//! read (#waveform-generation).
//!
//! For an oscillator frequency F only harmonics k with k|F| below the
//! Nyquist frequency contribute. Each table holds [`TABLE_SIZE`] samples of
//! one period, normalized by the wave's fixed factor
//! (#waveform-normalization), and is read with linear interpolation.
//! Tables are shared by harmonic level: exact up to 32 harmonics, then in
//! quarter-octave steps, rounded down so no table holds aliased partials.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::{Arc, Mutex, OnceLock};

pub const TABLE_SIZE: usize = 4096;
const HARMONICS: usize = TABLE_SIZE / 2;

/// A PeriodicWave's Fourier coefficients and its tables.
#[derive(Debug)]
pub struct Wave {
    a: Vec<f32>,
    b: Vec<f32>,
    factor: f64,
    tables: Mutex<HashMap<usize, Arc<[f32]>>>,
}

/// The built-in `OscillatorType`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

impl Wave {
    /// #dom-periodicwave-periodicwave: the coefficients after the
    /// constructor's defaulting (`real[0]` and `imag[0]` are ignored).
    pub fn new(real: &[f32], imag: &[f32], normalize: bool) -> Self {
        let count = real.len().min(imag.len()).min(HARMONICS);
        let mut a = vec![0.0; count];
        let mut b = vec![0.0; count];
        a[1..count].copy_from_slice(&real[1..count]);
        b[1..count].copy_from_slice(&imag[1..count]);
        let mut wave = Self {
            a,
            b,
            factor: 1.0,
            tables: Mutex::new(HashMap::new()),
        };
        if normalize && count > 1 {
            // f = max |x~(n)| over one period of the full series.
            let full = wave.synthesize(count - 1);
            let peak = full
                .iter()
                .fold(0.0f64, |peak, value| peak.max(value.abs()));
            wave.factor = if peak > 0.0 { peak } else { 1.0 };
        }
        wave
    }

    /// #oscillator-coefficients for the built-in types (shared).
    pub fn builtin(kind: Builtin) -> Arc<Wave> {
        static WAVES: OnceLock<[Arc<Wave>; 4]> = OnceLock::new();
        let waves = WAVES.get_or_init(|| {
            let make = |kind: Builtin| {
                let a = vec![0f32; HARMONICS];
                let mut b = vec![0f32; HARMONICS];
                for (n, value) in b.iter_mut().enumerate().skip(1) {
                    let n = n as f64;
                    let sign = if (n as usize).is_multiple_of(2) {
                        1.0
                    } else {
                        -1.0
                    };
                    *value = match kind {
                        Builtin::Sine => f64::from(n == 1.0),
                        Builtin::Square => 2.0 / (n * PI) * (1.0 - sign),
                        Builtin::Sawtooth => -sign * 2.0 / (n * PI),
                        Builtin::Triangle => 8.0 * (n * PI / 2.0).sin() / (PI * n * PI * n),
                    } as f32;
                }
                Arc::new(Wave::new(&a, &b, true))
            };
            [
                make(Builtin::Sine),
                make(Builtin::Square),
                make(Builtin::Sawtooth),
                make(Builtin::Triangle),
            ]
        });
        waves[kind as usize].clone()
    }

    /// x(n) = Σ a[k] cos(2πkn/N) + b[k] sin(2πkn/N), k ≤ `harmonics`, by
    /// an inverse FFT of size N.
    fn synthesize(&self, harmonics: usize) -> Vec<f64> {
        let mut re = vec![0.0; TABLE_SIZE];
        let mut im = vec![0.0; TABLE_SIZE];
        for k in 1..=harmonics.min(self.a.len().saturating_sub(1)) {
            re[k] = f64::from(self.a[k]);
            im[k] = -f64::from(self.b[k]);
        }
        fft(&mut re, &mut im);
        re
    }

    /// The harmonic level of the table for an oscillator at `frequency`.
    pub fn level(&self, frequency: f64, nyquist: f64) -> usize {
        let count = self.a.len();
        let magnitude = frequency.abs();
        let limit = if magnitude > 0.0 {
            ((nyquist / magnitude).ceil() - 1.0).max(0.0)
        } else {
            count.saturating_sub(1) as f64
        };
        let limit = limit.min(count.saturating_sub(1) as f64);
        if limit <= 32.0 {
            return limit as usize;
        }
        let level = (32.0 * 2f64.powf((4.0 * (limit / 32.0).log2()).floor() / 4.0)).floor();
        level.min(count.saturating_sub(1) as f64) as usize
    }

    /// The table for harmonic level `level`: [`TABLE_SIZE`] + 1 samples
    /// (the last repeats the first, for interpolation).
    pub fn table(&self, level: usize) -> Arc<[f32]> {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(table) = tables.get(&level) {
            return table.clone();
        }
        let samples = self.synthesize(level);
        let mut table = Vec::with_capacity(TABLE_SIZE + 1);
        table.extend(samples.iter().map(|sample| (sample / self.factor) as f32));
        table.push(table[0]);
        let table: Arc<[f32]> = table.into();
        tables.insert(level, table.clone());
        table
    }
}

/// In-place radix-2 FFT (`re.len()` a power of two).
pub fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut size = 2;
    while size <= n {
        let step = 2.0 * PI / size as f64;
        let half = size >> 1;
        for start in (0..n).step_by(size) {
            for k in 0..half {
                let (wr, wi) = ((step * k as f64).cos(), (step * k as f64).sin());
                let (i, j) = (start + k, start + k + half);
                let tr = re[j] * wr - im[j] * wi;
                let ti = re[j] * wi + im[j] * wr;
                re[j] = re[i] - tr;
                im[j] = im[i] - ti;
                re[i] += tr;
                im[i] += ti;
            }
        }
        size <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_table_is_a_normalized_sine() {
        let wave = Wave::builtin(Builtin::Sine);
        let table = wave.table(wave.level(440.0, 24_000.0));
        assert_eq!(table.len(), TABLE_SIZE + 1);
        for (n, &sample) in table.iter().enumerate().take(TABLE_SIZE).step_by(97) {
            let expected = (2.0 * PI * n as f64 / TABLE_SIZE as f64).sin();
            assert!((f64::from(sample) - expected).abs() < 1e-6, "{n}: {sample}");
        }
    }

    #[test]
    fn levels_never_alias() {
        let wave = Wave::builtin(Builtin::Sawtooth);
        for frequency in [20.0, 440.0, 1000.0, 5000.0, 23_000.0] {
            let level = wave.level(frequency, 24_000.0);
            assert!(level as f64 * frequency < 24_000.0, "{frequency}: {level}");
        }
        assert!(wave.level(0.0, 24_000.0) < HARMONICS);
    }
}
