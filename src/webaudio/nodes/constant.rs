//! ConstantSourceNode (#ConstantSourceNode): a mono output equal to its
//! `offset` AudioParam while it plays.

use super::{Bus, Control, Params, Processor, Scope};
use crate::webaudio::engine::Notice;

#[derive(Default)]
pub struct ConstantSource {
    started: bool,
    start: f64,
    stop: f64,
    ended: bool,
}

impl ConstantSource {
    pub fn new() -> Self {
        Self {
            start: f64::INFINITY,
            stop: f64::INFINITY,
            ..Self::default()
        }
    }
}

impl Processor for ConstantSource {
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
        let output = &mut outputs[0];
        output.reset(1, scope.frames);
        if !self.started || self.ended {
            return false;
        }
        let offset = params.get(0);
        let mut active = false;
        for index in 0..scope.frames {
            let time = scope.time(index);
            if time >= self.start && time < self.stop {
                active = true;
                output.channel_mut(0)[index] = offset.values[index];
            }
        }
        if (scope.frame + scope.frames as u64) as f64 / f64::from(scope.rate) >= self.stop {
            self.ended = true;
            scope.notices.push(Notice::Ended(scope.node));
        }
        active
    }
}
