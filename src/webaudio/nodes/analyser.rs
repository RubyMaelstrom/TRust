//! AnalyserNode (#AnalyserNode): the input passes through unchanged while
//! its mono down-mix (#time-domain-down-mixing) is kept: the last 32768
//! frames, shared with the control thread, which computes the current
//! time-domain and frequency data (#fft-windowing-and-smoothing-over-time)
//! when script asks.

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::{Bus, Params, Processor, Scope};
use crate::webaudio::bus::Interpretation;

pub const MAX_FFT: usize = 32768;

pub struct AnalyserState {
    samples: Vec<f32>,
    write: usize,
    /// Render quanta written (the identity of "the same render quantum").
    quanta: u64,
    // The control thread's frequency analysis state.
    /// \hat{X}_{-1}[k], the previous block.
    previous: Vec<f64>,
    /// The quantum and FFT size the cached decibels were computed for.
    computed: Option<(u64, usize)>,
    decibels: Vec<f32>,
    fft: Option<(usize, Arc<dyn Fft<f64>>)>,
}

pub type Shared = Arc<Mutex<AnalyserState>>;

pub fn shared() -> Shared {
    Arc::new(Mutex::new(AnalyserState {
        samples: vec![0.0; MAX_FFT],
        write: 0,
        quanta: 0,
        previous: Vec::new(),
        computed: None,
        decibels: Vec::new(),
        fft: None,
    }))
}

impl AnalyserState {
    /// The current time-domain data: the most recent `size` frames.
    pub fn time_domain(&self, size: usize) -> Vec<f32> {
        let size = size.min(MAX_FFT);
        (0..size)
            .map(|index| self.samples[(self.write + MAX_FFT - size + index) % MAX_FFT])
            .collect()
    }

    /// fftSize changed: smoothing restarts from zeros.
    pub fn reset(&mut self) {
        self.previous.clear();
        self.computed = None;
    }

    /// The current frequency data in dB (frequencyBinCount values), the
    /// same within one render quantum.
    pub fn frequency(&mut self, size: usize, smoothing: f64) -> &[f32] {
        if self.computed == Some((self.quanta, size)) {
            return &self.decibels;
        }
        let input = self.time_domain(size);
        let n = input.len();
        // Blackman window (alpha = 0.16).
        let mut data: Vec<Complex<f64>> = input
            .iter()
            .enumerate()
            .map(|(index, &sample)| {
                let phase = 2.0 * PI * index as f64 / n as f64;
                let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
                Complex::new(f64::from(sample) * window, 0.0)
            })
            .collect();
        let fft = match &self.fft {
            Some((planned, fft)) if *planned == n => fft.clone(),
            _ => {
                let fft = FftPlanner::new().plan_fft_forward(n);
                self.fft = Some((n, fft.clone()));
                fft
            }
        };
        fft.process(&mut data);
        let bins = n / 2;
        self.previous.resize(bins, 0.0);
        self.decibels.clear();
        for (bin, previous) in self.previous.iter_mut().enumerate() {
            let magnitude = data[bin].norm() / n as f64;
            let mut smoothed = smoothing * *previous + (1.0 - smoothing) * magnitude;
            if !smoothed.is_finite() {
                smoothed = 0.0;
            }
            *previous = smoothed;
            self.decibels.push((20.0 * smoothed.log10()) as f32);
        }
        self.computed = Some((self.quanta, size));
        &self.decibels
    }
}

pub struct Analyser {
    shared: Shared,
    mono: Bus,
}

impl Analyser {
    pub fn new(shared: Shared) -> Self {
        Self {
            shared,
            mono: Bus::default(),
        }
    }
}

impl Processor for Analyser {
    /// Processed every quantum, so silence enters the analysis as it would
    /// a continuous stream (#AudioContext-methods suspend() note).
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
        let input = &inputs[0];
        outputs[0].copy_from(input);
        self.mono.reset(1, scope.frames);
        if scope.inputs_active {
            self.mono.mix_from(input, Interpretation::Speakers);
        }
        let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let write = state.write;
        for (offset, &sample) in self.mono.channel(0).iter().enumerate() {
            state.samples[(write + offset) % MAX_FFT] = sample;
        }
        state.write = (write + scope.frames) % MAX_FFT;
        state.quanta += 1;
        scope.inputs_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_peaks_in_its_bin() {
        let shared = shared();
        {
            let mut state = shared.lock().unwrap();
            for index in 0..MAX_FFT {
                // Bin 64 of a 2048-point FFT.
                state.samples[index] = (2.0 * PI * 64.0 * index as f64 / 2048.0).sin() as f32;
            }
            state.quanta = 1;
        }
        let mut state = shared.lock().unwrap();
        let decibels = state.frequency(2048, 0.0).to_vec();
        let peak = decibels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(peak, 64);
        // A full-scale sine under a Blackman window: 20 log10(0.42 / 2).
        assert!(
            (decibels[64] - (20.0 * (0.21f64).log10()) as f32).abs() < 0.1,
            "{}",
            decibels[64]
        );
        // Smoothing: a repeated call in the same quantum returns the same
        // data.
        let again = state.frequency(2048, 0.8).to_vec();
        assert_eq!(decibels, again);
    }
}
