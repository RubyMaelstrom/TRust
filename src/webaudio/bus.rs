//! Render-quantum audio buffers and Web Audio's channel mixing
//! (#channel-up-mixing-and-down-mixing).

use std::f64::consts::FRAC_1_SQRT_2;

/// One render quantum of audio: `channels` planar channels of `frames`
/// samples each.
#[derive(Clone, Debug, Default)]
pub struct Bus {
    data: Vec<f32>,
    channels: usize,
    frames: usize,
}

/// `channelInterpretation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpretation {
    Speakers,
    Discrete,
}

/// `channelCountMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountMode {
    Max,
    ClampedMax,
    Explicit,
}

/// A node input's channel configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
    pub count: usize,
    pub mode: CountMode,
    pub interpretation: Interpretation,
}

impl ChannelConfig {
    /// computedNumberOfChannels for inputs with these channel counts
    /// (#dom-audionode-channelcountmode).
    pub fn computed(&self, inputs: impl Iterator<Item = usize>) -> usize {
        let largest = inputs.max().unwrap_or(1).max(1);
        match self.mode {
            CountMode::Max => largest,
            CountMode::ClampedMax => largest.min(self.count),
            CountMode::Explicit => self.count,
        }
    }
}

impl Bus {
    pub fn new(channels: usize, frames: usize) -> Self {
        Self {
            data: vec![0.0; channels * frames],
            channels,
            frames,
        }
    }

    /// One silent channel: what a node that is not actively processing
    /// outputs (#AudioNode-actively-processing).
    pub fn silent(frames: usize) -> Self {
        Self::new(1, frames)
    }

    /// Reshape to `channels` silent channels, reusing the allocation.
    pub fn reset(&mut self, channels: usize, frames: usize) {
        self.channels = channels;
        self.frames = frames;
        self.data.clear();
        self.data.resize(channels * frames, 0.0);
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channel(&self, index: usize) -> &[f32] {
        &self.data[index * self.frames..(index + 1) * self.frames]
    }

    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        &mut self.data[index * self.frames..(index + 1) * self.frames]
    }

    /// Whether every sample is zero.
    pub fn is_silent(&self) -> bool {
        self.data.iter().all(|&sample| sample == 0.0)
    }

    /// Up- or down-mix `source` into this bus and sum it in.
    pub fn mix_from(&mut self, source: &Bus, interpretation: Interpretation) {
        let (from, to) = (source.channels, self.channels);
        let frames = self.frames.min(source.frames);
        // Double-precision gains, rounded once per sample.
        let mut add = |target: usize, input: usize, scale: f64| {
            let start = target * self.frames;
            let source = source.channel(input);
            let out = &mut self.data[start..start + frames];
            if scale == 1.0 {
                for (out, sample) in out.iter_mut().zip(source) {
                    *out += sample;
                }
            } else {
                for (out, sample) in out.iter_mut().zip(source) {
                    *out = (f64::from(*out) + f64::from(*sample) * scale) as f32;
                }
            }
        };
        if from == to {
            for channel in 0..to {
                add(channel, channel, 1.0);
            }
            return;
        }
        if interpretation == Interpretation::Speakers {
            const H: f64 = FRAC_1_SQRT_2;
            match (from, to) {
                // Mono up-mixes.
                (1, 2) | (1, 4) => {
                    add(0, 0, 1.0);
                    add(1, 0, 1.0);
                    return;
                }
                (1, 6) => {
                    add(2, 0, 1.0);
                    return;
                }
                // Stereo to quad and 5.1: front channels.
                (2, 4) | (2, 6) => {
                    add(0, 0, 1.0);
                    add(1, 1, 1.0);
                    return;
                }
                (4, 6) => {
                    add(0, 0, 1.0);
                    add(1, 1, 1.0);
                    add(4, 2, 1.0);
                    add(5, 3, 1.0);
                    return;
                }
                // Down-mixes.
                (2, 1) => {
                    add(0, 0, 0.5);
                    add(0, 1, 0.5);
                    return;
                }
                (4, 1) => {
                    for channel in 0..4 {
                        add(0, channel, 0.25);
                    }
                    return;
                }
                (6, 1) => {
                    add(0, 0, H);
                    add(0, 1, H);
                    add(0, 2, 1.0);
                    add(0, 4, 0.5);
                    add(0, 5, 0.5);
                    return;
                }
                (4, 2) => {
                    add(0, 0, 0.5);
                    add(0, 2, 0.5);
                    add(1, 1, 0.5);
                    add(1, 3, 0.5);
                    return;
                }
                (6, 2) => {
                    add(0, 0, 1.0);
                    add(0, 2, H);
                    add(0, 4, H);
                    add(1, 1, 1.0);
                    add(1, 2, H);
                    add(1, 5, H);
                    return;
                }
                (6, 4) => {
                    add(0, 0, 1.0);
                    add(0, 2, H);
                    add(1, 1, 1.0);
                    add(1, 2, H);
                    add(2, 4, 1.0);
                    add(3, 5, 1.0);
                    return;
                }
                _ => {}
            }
        }
        // "discrete", and speaker layouts without a defined mix: fill
        // channels in order, dropping or zero-filling the rest.
        for channel in 0..from.min(to) {
            add(channel, channel, 1.0);
        }
    }

    /// Sum `sources` into one bus of `channels` channels (an AudioNode
    /// input, #rendering-loop).
    pub fn mix_inputs<'a>(
        &mut self,
        sources: impl Iterator<Item = &'a Bus>,
        channels: usize,
        frames: usize,
        interpretation: Interpretation,
    ) {
        self.reset(channels, frames);
        for source in sources {
            self.mix_from(source, interpretation);
        }
    }

    /// Copy `other`'s shape and samples.
    pub fn copy_from(&mut self, other: &Bus) {
        self.channels = other.channels;
        self.frames = other.frames;
        self.data.clear();
        self.data.extend_from_slice(&other.data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(channels: &[&[f32]]) -> Bus {
        let mut bus = Bus::new(channels.len(), channels[0].len());
        for (index, data) in channels.iter().enumerate() {
            bus.channel_mut(index).copy_from_slice(data);
        }
        bus
    }

    #[test]
    fn speaker_mixes_follow_the_spec_tables() {
        let stereo = bus(&[&[1.0], &[3.0]]);
        let mut mono = Bus::new(1, 1);
        mono.mix_from(&stereo, Interpretation::Speakers);
        assert_eq!(mono.channel(0), [2.0]);
        let surround = bus(&[&[1.0], &[2.0], &[4.0], &[8.0], &[16.0], &[32.0]]);
        let mut down = Bus::new(2, 1);
        down.mix_from(&surround, Interpretation::Speakers);
        let h = FRAC_1_SQRT_2;
        assert_eq!(
            f64::from(down.channel(0)[0]),
            f64::from((1.0 + h * 4.0 + h * 16.0) as f32)
        );
        assert!((f64::from(down.channel(1)[0]) - (2.0 + h * (4.0 + 32.0))).abs() < 1e-5);
        let mut up = Bus::new(6, 1);
        up.mix_from(&bus(&[&[5.0]]), Interpretation::Speakers);
        assert_eq!([up.channel(0)[0], up.channel(2)[0]], [0.0, 5.0]);
        // Discrete: fill in order, drop the rest.
        let mut discrete = Bus::new(1, 1);
        discrete.mix_from(&stereo, Interpretation::Discrete);
        assert_eq!(discrete.channel(0), [1.0]);
    }

    #[test]
    fn channel_count_modes() {
        let explicit = ChannelConfig {
            count: 2,
            mode: CountMode::Explicit,
            interpretation: Interpretation::Speakers,
        };
        assert_eq!(explicit.computed([1, 6].into_iter()), 2);
        let clamped = ChannelConfig {
            mode: CountMode::ClampedMax,
            ..explicit
        };
        assert_eq!(clamped.computed([1, 6].into_iter()), 2);
        assert_eq!(clamped.computed([1].into_iter()), 1);
        let max = ChannelConfig {
            mode: CountMode::Max,
            ..explicit
        };
        assert_eq!(max.computed([1, 6].into_iter()), 6);
        assert_eq!(max.computed(std::iter::empty()), 1);
    }
}
