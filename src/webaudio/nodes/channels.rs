//! ChannelSplitterNode (#ChannelSplitterNode) and ChannelMergerNode
//! (#ChannelMergerNode).

use super::{Bus, Params, Processor, Scope};

/// Output n is channel n of the input (whose channel count, explicit and
/// discrete, is the number of outputs).
pub struct Splitter;

impl Processor for Splitter {
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let input = &inputs[0];
        for (channel, output) in outputs.iter_mut().enumerate() {
            output.reset(1, scope.frames);
            if channel < input.channels() {
                output
                    .channel_mut(0)
                    .copy_from_slice(input.channel(channel));
            }
        }
        false
    }
}

/// Channel n of the output is input n (each input mixed to mono); an
/// unconnected or silent input is one silent channel.
pub struct Merger;

impl Processor for Merger {
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        _params: &Params,
    ) -> bool {
        let output = &mut outputs[0];
        output.reset(inputs.len(), scope.frames);
        for (channel, input) in inputs.iter().enumerate() {
            output
                .channel_mut(channel)
                .copy_from_slice(input.channel(0));
        }
        false
    }
}
