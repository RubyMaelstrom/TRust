//! AudioBufferSourceNode (#AudioBufferSourceNode,
//! #playback-AudioBufferSourceNode). The playhead is kept in buffer sample
//! frames (position = playhead time × buffer sample rate) so that unit-rate
//! playback addresses exact frames; between frames the playback signal is
//! linearly interpolated.

use std::sync::Arc;

use super::{Bus, Control, Params, Processor, Scope};
use crate::webaudio::AudioData;
use crate::webaudio::engine::Notice;

pub struct BufferSource {
    content: Option<Arc<AudioData>>,
    started: bool,
    start: f64,
    stop: f64,
    ended: bool,
    looping: bool,
    loop_start: f64,
    loop_end: f64,
    offset: f64,
    duration: f64,
    position: f64,
    elapsed: f64,
    begun: bool,
    entered_loop: bool,
}

impl BufferSource {
    pub fn new(looping: bool, loop_start: f64, loop_end: f64) -> Self {
        Self {
            content: None,
            started: false,
            start: f64::INFINITY,
            stop: f64::INFINITY,
            ended: false,
            looping,
            loop_start,
            loop_end,
            offset: 0.0,
            duration: f64::INFINITY,
            position: 0.0,
            elapsed: 0.0,
            begun: false,
            entered_loop: false,
        }
    }

    fn end(&mut self, scope: &mut Scope) {
        if !self.ended {
            self.ended = true;
            scope.notices.push(Notice::Ended(scope.node));
        }
    }
}

impl Processor for BufferSource {
    fn is_source(&self) -> bool {
        true
    }

    fn control(&mut self, control: Control) {
        match control {
            Control::Start {
                when,
                offset,
                duration,
            } => {
                self.started = true;
                self.start = when;
                self.offset = offset;
                if let Some(duration) = duration {
                    self.duration = duration;
                }
            }
            Control::Stop(when) if !self.ended => self.stop = when,
            Control::Buffer(content) => self.content = content,
            Control::Loop(looping) => self.looping = looping,
            Control::LoopStart(time) => self.loop_start = time,
            Control::LoopEnd(time) => self.loop_end = time,
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
        let rate = f64::from(scope.rate);
        let frame = scope.frame;
        let output = &mut outputs[0];
        if self.ended || !self.started {
            output.reset(1, frames);
            return false;
        }
        let playback_rate = f64::from(params.first(0));
        let detune = f64::from(params.first(1));
        let content = match self.content.clone() {
            Some(content) if content.length > 0 && !content.channels.is_empty() => content,
            _ => {
                output.reset(1, frames);
                if frame as f64 / rate >= self.start {
                    self.end(scope);
                }
                return false;
            }
        };
        let channels = content.channels.len();
        output.reset(channels, frames);
        let computed_rate = playback_rate * 2f64.powf(detune / 1200.0);
        let buffer_rate = f64::from(content.rate);
        let length = content.length;
        let buffer_duration = length as f64 / buffer_rate;
        let (mut loop_start, mut loop_end) = (0.0, buffer_duration);
        if self.looping {
            if self.loop_start >= 0.0 && self.loop_end > 0.0 && self.loop_start < self.loop_end {
                loop_start = self.loop_start;
                loop_end = self.loop_end.min(buffer_duration);
            }
        } else {
            self.entered_loop = false;
        }
        let loop_start_frame = loop_start * buffer_rate;
        let loop_end_frame = loop_end * buffer_rate;
        let step = computed_rate * buffer_rate / rate;
        let mut finished = false;
        let mut active = false;
        for index in 0..frames {
            let time = (frame + index as u64) as f64 / rate;
            if time < self.start {
                continue;
            }
            if time >= self.stop || self.elapsed >= self.duration {
                finished = true;
                continue;
            }
            active = true;
            if !self.begun {
                let mut offset = self.offset.min(buffer_duration);
                if self.looping && computed_rate >= 0.0 && offset >= loop_end {
                    offset = loop_end;
                }
                if computed_rate < 0.0 && self.looping && offset < loop_start {
                    offset = loop_start;
                }
                self.offset = offset;
                self.position = offset * buffer_rate;
                self.begun = true;
            }
            if self.looping {
                let offset_frame = self.offset * buffer_rate;
                if !self.entered_loop {
                    if offset_frame < loop_end_frame && self.position >= loop_start_frame {
                        self.entered_loop = true;
                    }
                    if offset_frame >= loop_end_frame && self.position < loop_end_frame {
                        self.entered_loop = true;
                    }
                }
                if self.entered_loop
                    && loop_end_frame > loop_start_frame
                    && (self.position >= loop_end_frame || self.position < loop_start_frame)
                {
                    // Wrap whole loop iterations (any number of them).
                    let span = loop_end_frame - loop_start_frame;
                    let wrapped = (self.position - loop_start_frame) % span;
                    self.position = loop_start_frame
                        + if wrapped < 0.0 {
                            wrapped + span
                        } else {
                            wrapped
                        };
                }
            }
            let position = self.position;
            if position >= 0.0 && position < length as f64 {
                // Past the last frame of a loop, interpolation continues
                // with the frames that follow loopStart.
                let at = position.floor();
                let (whole, fraction) = (at as usize, position - at);
                let next = if whole + 1 < length {
                    Some(whole + 1)
                } else if self.looping && self.entered_loop {
                    Some(loop_start_frame.floor() as usize)
                } else {
                    None
                };
                for channel in 0..channels {
                    let data = &content.channels[channel];
                    let current = f64::from(data[whole]);
                    let following = next.map_or(0.0, |next| f64::from(data[next]));
                    output.channel_mut(channel)[index] = if fraction == 0.0 {
                        data[whole]
                    } else {
                        (current + (following - current) * fraction) as f32
                    };
                }
            } else if !self.looping {
                finished = true;
            }
            self.position += step;
            self.elapsed += computed_rate / rate;
        }
        if finished || (frame + frames as u64) as f64 / rate >= self.stop {
            self.end(scope);
        }
        active
    }
}
