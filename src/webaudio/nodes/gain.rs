//! GainNode (#GainNode): output = input × gain, sample-accurately.

use super::{Bus, Params, Processor, Scope};

pub struct Gain;

impl Processor for Gain {
    fn process(
        &mut self,
        _scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let output = &mut outputs[0];
        output.copy_from(&inputs[0]);
        let gain = params.get(0);
        if gain.constant {
            let value = gain.values[0];
            if value != 1.0 {
                for channel in 0..output.channels() {
                    output
                        .channel_mut(channel)
                        .iter_mut()
                        .for_each(|sample| *sample *= value);
                }
            }
        } else {
            for channel in 0..output.channels() {
                for (sample, value) in output.channel_mut(channel).iter_mut().zip(&gain.values) {
                    *sample *= value;
                }
            }
        }
        false
    }
}
