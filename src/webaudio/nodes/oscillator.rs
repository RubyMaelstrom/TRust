//! OscillatorNode (#OscillatorNode, #waveform-generation): a band-limited
//! PeriodicWave table read at computedOscFrequency = frequency ×
//! 2^(detune / 1200), clamped to the Nyquist range.

use std::sync::Arc;

use super::{Bus, Control, Params, Processor, Scope};
use crate::webaudio::engine::Notice;
use crate::webaudio::wave::{TABLE_SIZE, Wave};

pub struct Oscillator {
    wave: Arc<Wave>,
    started: bool,
    start: f64,
    stop: f64,
    ended: bool,
    playing: bool,
    /// Position in the period, in cycles.
    phase: f64,
    last_frequency: f64,
    level: usize,
    table: Option<Arc<[f32]>>,
}

impl Oscillator {
    pub fn new(wave: Arc<Wave>) -> Self {
        Self {
            wave,
            started: false,
            start: f64::INFINITY,
            stop: f64::INFINITY,
            ended: false,
            playing: false,
            phase: 0.0,
            last_frequency: f64::NAN,
            level: usize::MAX,
            table: None,
        }
    }
}

impl Processor for Oscillator {
    fn is_source(&self) -> bool {
        true
    }

    fn control(&mut self, control: Control) {
        match control {
            Control::Start { when, .. } => {
                self.started = true;
                self.start = when;
            }
            Control::Stop(when) if !self.ended => self.stop = when,
            Control::Wave(wave) => {
                self.wave = wave;
                self.table = None;
                self.last_frequency = f64::NAN;
            }
            _ => {}
        }
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        _inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let frames = scope.frames;
        let output = &mut outputs[0];
        output.reset(1, frames);
        if !self.started || self.ended {
            return false;
        }
        let (frequency, detune) = (params.get(0), params.get(1));
        let rate = f64::from(scope.rate);
        let nyquist = rate / 2.0;
        let mut active = false;
        let samples = output.channel_mut(0);
        for (index, sample) in samples.iter_mut().enumerate() {
            let time = scope.time(index);
            if time < self.start || time >= self.stop {
                continue;
            }
            active = true;
            let computed = (f64::from(frequency.values[index])
                * 2f64.powf(f64::from(detune.values[index]) / 1200.0))
            .clamp(-nyquist, nyquist);
            if !self.playing {
                // Phase zero at the node's exact start time.
                self.playing = true;
                self.phase = (time - self.start) * computed;
                self.phase -= self.phase.floor();
            }
            if computed != self.last_frequency || self.table.is_none() {
                self.last_frequency = computed;
                let level = self.wave.level(computed, nyquist);
                if level != self.level || self.table.is_none() {
                    self.level = level;
                    self.table = Some(self.wave.table(level));
                }
            }
            let table = self.table.as_ref().expect("table chosen above");
            let position = self.phase * TABLE_SIZE as f64;
            let at = position.floor();
            let index = at as usize;
            let (a, b) = (f64::from(table[index]), f64::from(table[index + 1]));
            *sample = (a + (b - a) * (position - at)) as f32;
            self.phase += computed / rate;
            self.phase -= self.phase.floor();
        }
        if (scope.frame + frames as u64) as f64 / rate >= self.stop {
            self.ended = true;
            scope.notices.push(Notice::Ended(scope.node));
        }
        active
    }
}
