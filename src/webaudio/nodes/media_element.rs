//! MediaElementAudioSourceNode (#MediaElementAudioSourceNode): the media
//! element's audio, pulled from its player through a tap
//! (`media::audio::Tap`) and resampled to the context's rate. It is
//! actively processing while the element plays; a CORS-cross-origin
//! resource is heard as silence.

use super::{Bus, Params, Processor, Scope};
use crate::media::audio::{Format, Tap};
use crate::webaudio::resample::Stream;

pub struct MediaElementSource {
    tap: Tap,
    stream: Option<(Format, Stream)>,
    pulled: Vec<f32>,
}

impl MediaElementSource {
    pub fn new(tap: Tap) -> Self {
        Self {
            tap,
            stream: None,
            pulled: Vec::new(),
        }
    }
}

impl Processor for MediaElementSource {
    fn is_source(&self) -> bool {
        true
    }

    fn process(
        &mut self,
        scope: &mut Scope,
        _inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let frames = scope.frames;
        let output = &mut outputs[0];
        let Some(format) = self.tap.format() else {
            output.reset(1, frames);
            return false;
        };
        let channels = usize::from(format.channels.max(1));
        let rate = scope.rate;
        if self
            .stream
            .as_ref()
            .is_none_or(|(current, _)| *current != format)
        {
            self.stream = Some((
                format,
                Stream::new(f64::from(format.rate), f64::from(rate), channels),
            ));
        }
        let (_, stream) = self.stream.as_mut().expect("stream chosen above");
        let needed = if format.rate as f32 == rate {
            frames
        } else {
            stream.needed(frames)
        };
        self.pulled.clear();
        if needed > 0 && self.tap.pull(needed, &mut self.pulled).is_none() {
            output.reset(1, frames);
            return false;
        }
        output.reset(channels, frames);
        if format.rate as f32 == rate {
            for (index, sample) in self.pulled.iter().enumerate() {
                output.channel_mut(index % channels)[index / channels] = *sample;
            }
        } else {
            stream.push(&self.pulled);
            let mut planes: Vec<Vec<f32>> = vec![vec![0.0; frames]; channels];
            {
                let mut targets: Vec<&mut [f32]> =
                    planes.iter_mut().map(Vec::as_mut_slice).collect();
                stream.produce(frames, &mut targets);
            }
            for (channel, plane) in planes.iter().enumerate() {
                output.channel_mut(channel).copy_from_slice(plane);
            }
        }
        true
    }
}
