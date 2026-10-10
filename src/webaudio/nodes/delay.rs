//! DelayNode (#DelayNode, #DelayNode-processing): output(t) =
//! input(t - delayTime(t)), read from a delay line with linear
//! interpolation.
//!
//! Outside a cycle the node writes its input and then reads. In a cycle the
//! graph splits it: its DelayReader ([`Processor::read`]) runs as a source
//! from audio written at least one render quantum ago (delayTime is clamped
//! to a render quantum), and its DelayWriter ([`Processor::write`]) records
//! the input after the rest of the graph.

use super::{Bus, Params, Processor, Scope};
use crate::webaudio::bus::Interpretation;

pub struct Delay {
    /// One ring of `size` frames per channel.
    line: Bus,
    size: usize,
    write: usize,
    max_frames: f64,
    interpretation: Interpretation,
    /// Frames written so far, and the end of the last non-silent input.
    written: u64,
    audible_until: u64,
}

impl Delay {
    pub fn new(max_delay: f64, rate: f32, quantum: usize) -> Self {
        let max_frames = max_delay * f64::from(rate);
        let size = (max_frames.ceil() as usize).max(quantum) + quantum + 2;
        Self {
            line: Bus::new(1, size),
            size,
            write: 0,
            max_frames,
            interpretation: Interpretation::Speakers,
            written: 0,
            audible_until: 0,
        }
    }

    /// Adopt the input's channel layout, mixing the delayed audio into it
    /// (#DelayNode: "upmixed or downmixed before being combined").
    fn layout(&mut self, channels: usize) {
        if channels != self.line.channels() {
            let mut line = Bus::new(channels, self.size);
            line.mix_from(&self.line, self.interpretation);
            self.line = line;
        }
    }

    fn record(&mut self, input: &Bus, frames: usize) {
        self.layout(input.channels());
        let mut audible = false;
        for channel in 0..input.channels() {
            let source = input.channel(channel);
            let line = self.line.channel_mut(channel);
            for (offset, &sample) in source.iter().enumerate().take(frames) {
                audible |= sample != 0.0;
                line[(self.write + offset) % self.size] = sample;
            }
        }
        self.write = (self.write + frames) % self.size;
        self.written += frames as u64;
        if audible {
            self.audible_until = self.written;
        }
    }

    /// Read this quantum; frame `i` was (or will be) written at `base + i`.
    fn play(&self, scope: &Scope, output: &mut Bus, params: &Params, base: usize, minimum: f64) {
        let rate = f64::from(scope.rate);
        let delay = params.get(0);
        let channels = self.line.channels();
        output.reset(channels, scope.frames);
        for index in 0..scope.frames {
            let frames = (f64::from(delay.values[index]) * rate)
                .clamp(minimum, self.max_frames.max(minimum));
            let mut position = (base + index) as f64 - frames;
            while position < 0.0 {
                position += self.size as f64;
            }
            let whole = position.floor();
            let fraction = position - whole;
            let at = whole as usize % self.size;
            let next = (at + 1) % self.size;
            for channel in 0..channels {
                let line = self.line.channel(channel);
                let (a, b) = (f64::from(line[at]), f64::from(line[next]));
                output.channel_mut(channel)[index] = (a + (b - a) * fraction) as f32;
            }
        }
    }
}

impl Processor for Delay {
    fn is_delay(&self) -> bool {
        true
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let base = self.write;
        self.record(&inputs[0], scope.frames);
        self.play(scope, &mut outputs[0], params, base, 0.0);
        // Delayed audio is still to come.
        (self.written - self.audible_until) as f64 <= self.max_frames + 1.0
    }

    fn read(&mut self, scope: &mut Scope, outputs: &mut [Bus], params: &Params) -> bool {
        self.play(
            scope,
            &mut outputs[0],
            params,
            self.write,
            scope.frames as f64,
        );
        // #AudioNode-actively-processing: a DelayNode in a cycle is active
        // while its output is not denormal-small.
        let output = &outputs[0];
        (0..output.channels()).any(|channel| {
            output
                .channel(channel)
                .iter()
                .any(|sample| sample.abs() >= f32::MIN_POSITIVE)
        })
    }

    fn write(&mut self, scope: &mut Scope, inputs: &[Bus], _params: &Params) {
        self.record(&inputs[0], scope.frames);
    }
}
