//! StereoPannerNode (#stereopannernode, #stereopanner-algorithm): equal-power
//! panning of a mono or stereo input into stereo.

use std::f64::consts::FRAC_PI_2;

use super::{Bus, Params, Processor, Scope};

pub struct StereoPanner;

impl Processor for StereoPanner {
    fn process(
        &mut self,
        scope: &mut Scope,
        inputs: &[Bus],
        outputs: &mut [Bus],
        params: &Params,
    ) -> bool {
        let input = &inputs[0];
        let output = &mut outputs[0];
        output.reset(2, scope.frames);
        let pan = params.get(0);
        let mono = input.channels() == 1;
        for index in 0..scope.frames {
            let pan = f64::from(pan.values[index]).clamp(-1.0, 1.0);
            let x = if mono {
                (pan + 1.0) / 2.0
            } else if pan <= 0.0 {
                pan + 1.0
            } else {
                pan
            };
            let (gain_left, gain_right) = ((x * FRAC_PI_2).cos(), (x * FRAC_PI_2).sin());
            let (left, right) = if mono {
                let sample = f64::from(input.channel(0)[index]);
                (sample * gain_left, sample * gain_right)
            } else {
                let (left, right) = (
                    f64::from(input.channel(0)[index]),
                    f64::from(input.channel(1)[index]),
                );
                if pan <= 0.0 {
                    (left + right * gain_left, right * gain_right)
                } else {
                    (left * gain_left, right + left * gain_right)
                }
            };
            output.channel_mut(0)[index] = left as f32;
            output.channel_mut(1)[index] = right as f32;
        }
        false
    }
}
