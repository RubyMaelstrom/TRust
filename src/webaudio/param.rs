//! The rendering thread's half of an AudioParam (#AudioParam): its
//! automation timeline and #computation-of-value.
//!
//! Script validates each automation method on the control thread (argument
//! ranges and curve overlaps, which throw synchronously) and sends the event
//! here, stamped with the control thread's `currentTime`. Values are
//! computed in double precision and stored as `f32`, like the timeline the
//! spec describes in terms of `float` attributes and `double` times.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// `AutomationRate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    A,
    K,
}

/// An automation method, as the control thread sends it.
#[derive(Clone, Debug)]
pub enum Automation {
    /// The `value` setter: [[current value]] and setValueAtTime(value, now).
    Value(f32),
    SetValue {
        value: f32,
        time: f64,
    },
    LinearRamp {
        value: f32,
        time: f64,
    },
    ExponentialRamp {
        value: f32,
        time: f64,
    },
    SetTarget {
        target: f32,
        time: f64,
        time_constant: f32,
    },
    SetCurve {
        values: Arc<[f32]>,
        time: f64,
        duration: f64,
    },
    Cancel(f64),
    CancelAndHold(f64),
    Rate(Rate),
}

#[derive(Clone, Debug)]
enum Kind {
    Set,
    Linear,
    Exponential,
    Target(f64),
    Curve {
        values: Arc<[f32]>,
        duration: f64,
        /// cancelAndHoldAtTime() truncation: from `cut` on, `cut_value`.
        cut: f64,
        cut_value: f64,
    },
}

#[derive(Clone, Debug)]
struct Event {
    kind: Kind,
    time: f64,
    value: f64,
    /// A ramp that follows setTargetAtTime() starts from where that
    /// automation is when the ramp is inserted (or just before it starts).
    start: Option<(f64, f64)>,
    /// setTargetAtTime()'s starting value, computed on first use.
    v0: Cell<Option<f64>>,
}

impl Event {
    fn new(kind: Kind, time: f64, value: f64) -> Self {
        Self {
            kind,
            time,
            value,
            start: None,
            v0: Cell::new(None),
        }
    }

    fn is_ramp(&self) -> bool {
        matches!(self.kind, Kind::Linear | Kind::Exponential)
    }

    fn end(&self) -> f64 {
        match self.kind {
            Kind::Curve { duration, cut, .. } => (self.time + duration).min(cut),
            _ => self.time,
        }
    }
}

/// An AudioParam's automation events in time order (#dfn-automation-event).
#[derive(Clone, Debug)]
pub struct Timeline {
    events: Vec<Event>,
    /// The intrinsic value before the first event (the `value` attribute's,
    /// or the value held when events were canceled).
    base: f64,
}

impl Timeline {
    pub fn new(default: f32) -> Self {
        Self {
            events: Vec::new(),
            base: f64::from(default),
        }
    }

    fn changed(&self) {
        for event in &self.events {
            event.v0.set(None);
        }
    }

    /// #AudioParam: an event at the same time as existing events follows
    /// them.
    fn insert(&mut self, event: Event) -> usize {
        let mut index = self.events.len();
        while index > 0 && self.events[index - 1].time > event.time {
            index -= 1;
        }
        self.events.insert(index, event);
        self.changed();
        index
    }

    /// Apply an automation method. `now` is the control thread's
    /// `currentTime` when it was called; `current` is [[current value]].
    pub fn apply(&mut self, automation: Automation, now: f64, current: &mut f32) {
        match automation {
            Automation::Value(value) => {
                *current = value;
                self.insert(Event::new(Kind::Set, now, f64::from(value)));
            }
            Automation::SetValue { value, time } => {
                self.insert(Event::new(Kind::Set, time, f64::from(value)));
            }
            Automation::LinearRamp { value, time } => {
                self.ramp(Kind::Linear, value, time, now, *current)
            }
            Automation::ExponentialRamp { value, time } => {
                self.ramp(Kind::Exponential, value, time, now, *current)
            }
            Automation::SetTarget {
                target,
                time,
                time_constant,
            } => {
                self.insert(Event::new(
                    Kind::Target(f64::from(time_constant)),
                    time,
                    f64::from(target),
                ));
            }
            Automation::SetCurve {
                values,
                time,
                duration,
            } => {
                self.insert(Event::new(
                    Kind::Curve {
                        values,
                        duration,
                        cut: f64::INFINITY,
                        cut_value: 0.0,
                    },
                    time,
                    0.0,
                ));
            }
            Automation::Cancel(time) => {
                // #dom-audioparam-cancelscheduledvalues: the intrinsic value
                // stays where it is until set or automated again.
                self.base = f64::from(*current);
                self.events.retain(|event| {
                    event.time < time
                        && !matches!(event.kind, Kind::Curve { duration, .. } if time <= event.time + duration)
                });
                self.changed();
            }
            Automation::CancelAndHold(time) => self.cancel_and_hold(time),
            Automation::Rate(_) => {}
        }
    }

    fn ramp(&mut self, kind: Kind, value: f32, time: f64, now: f64, current: f32) {
        let mut index = self.events.len();
        while index > 0 && self.events[index - 1].time > time {
            index -= 1;
        }
        let mut event = Event::new(kind, time, f64::from(value));
        if index == 0 {
            // No preceding event: as if setValueAtTime(value, currentTime).
            self.insert(Event::new(Kind::Set, now, f64::from(current)));
            self.insert(event);
            return;
        }
        let previous = &self.events[index - 1];
        if matches!(previous.kind, Kind::Target(_)) {
            // A ramp after SetTarget starts where that automation is now, or,
            // when it has not started, just before it starts.
            event.start = Some(if previous.time > now {
                (previous.time, self.evaluate(index - 1, previous.time))
            } else {
                (now, self.value_at(now))
            });
        }
        self.insert(event);
    }

    /// #dom-audioparam-cancelandholdattime
    fn cancel_and_hold(&mut self, time: f64) {
        let first = self.events.iter().rposition(|event| event.time <= time);
        let second = first.map_or(0, |first| first + 1);
        let held = self.value_at(time);
        if self.events.get(second).is_some_and(Event::is_ramp) {
            let event = &mut self.events[second];
            event.time = time;
            event.value = held;
        } else if let Some(first) = first {
            let event = &mut self.events[first];
            match &mut event.kind {
                Kind::Target(_) => {
                    self.events
                        .insert(first + 1, Event::new(Kind::Set, time, held));
                }
                Kind::Curve {
                    duration,
                    cut,
                    cut_value,
                    ..
                } if time <= event.time + *duration => {
                    *cut = time;
                    *cut_value = held;
                }
                _ => {}
            }
        }
        self.events.retain(|event| event.time <= time);
        self.changed();
    }

    fn curve_value(event: &Event, time: f64) -> f64 {
        let Kind::Curve {
            values,
            duration,
            cut,
            cut_value,
        } = &event.kind
        else {
            return event.value;
        };
        if time >= *cut {
            return *cut_value;
        }
        let count = values.len();
        if time >= event.time + duration {
            return f64::from(values[count - 1]);
        }
        let position = (count - 1) as f64 / duration * (time - event.time);
        let k = position.floor();
        if k >= (count - 1) as f64 {
            return f64::from(values[count - 1]);
        }
        let k = k.max(0.0) as usize;
        let (a, b) = (f64::from(values[k]), f64::from(values[k + 1]));
        a + (b - a) * (position - k as f64)
    }

    fn end_value(&self, index: usize) -> f64 {
        let event = &self.events[index];
        match event.kind {
            Kind::Curve { .. } => Self::curve_value(event, event.end()),
            Kind::Target(_) => {
                let next = self
                    .events
                    .get(index + 1)
                    .map_or(event.time, |next| next.time);
                self.target_value(index, next)
            }
            _ => event.value,
        }
    }

    fn target_value(&self, index: usize, time: f64) -> f64 {
        let event = &self.events[index];
        let Kind::Target(time_constant) = event.kind else {
            return event.value;
        };
        let v0 = match event.v0.get() {
            Some(v0) => v0,
            None => {
                let v0 = self.evaluate(index, event.time);
                event.v0.set(Some(v0));
                v0
            }
        };
        if time_constant == 0.0 {
            return if time >= event.time { event.value } else { v0 };
        }
        event.value + (v0 - event.value) * (-(time - event.time) / time_constant).exp()
    }

    /// The value at `time` of the timeline formed by `events[..limit]`.
    fn evaluate(&self, limit: usize, time: f64) -> f64 {
        if limit == 0 {
            return self.base;
        }
        let events = &self.events[..limit];
        let last = events
            .partition_point(|event| event.time <= time)
            .checked_sub(1);
        let next = events.get(last.map_or(0, |last| last + 1));
        if let Some(next) = next.filter(|next| next.is_ramp()) {
            let (t0, v0) = match (next.start, last) {
                (Some(start), _) => start,
                (None, Some(last)) => (self.events[last].end(), self.end_value(last)),
                (None, None) => (f64::NEG_INFINITY, self.base),
            };
            if time >= t0 {
                let (t1, v1) = (next.time, next.value);
                if t1 <= t0 {
                    return v1;
                }
                if t0 == f64::NEG_INFINITY {
                    return v0;
                }
                let fraction = (time - t0) / (t1 - t0);
                if matches!(next.kind, Kind::Linear) {
                    return v0 + (v1 - v0) * fraction;
                }
                if v0 == 0.0 || (v0 > 0.0) != (v1 > 0.0) {
                    return v0;
                }
                return v0 * (v1 / v0).powf(fraction);
            }
        }
        let Some(last) = last else {
            return self.base;
        };
        let event = &self.events[last];
        match event.kind {
            Kind::Target(_) => self.target_value(last, time),
            Kind::Curve { .. } => Self::curve_value(event, time),
            _ => event.value,
        }
    }

    pub fn value_at(&self, time: f64) -> f64 {
        self.evaluate(self.events.len(), time)
    }

    /// Whether the intrinsic value is the same for every time in
    /// `[from, to]`, and that value.
    fn constant_over(&self, from: f64, to: f64) -> Option<f64> {
        if self.events.is_empty() {
            return Some(self.base);
        }
        let last = self
            .events
            .partition_point(|event| event.time <= from)
            .checked_sub(1);
        if self
            .events
            .get(last.map_or(0, |last| last + 1))
            .is_some_and(|next| next.time <= to || next.is_ramp())
        {
            return None;
        }
        match last.map(|last| &self.events[last]) {
            None => Some(self.base),
            Some(event) => match event.kind {
                Kind::Set | Kind::Linear | Kind::Exponential if event.start.is_none() => {
                    Some(event.value)
                }
                Kind::Target(_) => None,
                Kind::Curve { .. } if event.end() <= from => Some(Self::curve_value(event, from)),
                _ => None,
            },
        }
    }

    /// Drop events that can no longer affect values at or after `time`: an
    /// event goes once its successor has begun by then. The value the
    /// dropped events reach at the new first event becomes the base, which
    /// is all a setTargetAtTime() there reads as its start.
    pub fn prune(&mut self, time: f64) {
        let mut drop = 0;
        while drop + 1 < self.events.len() && self.events[drop + 1].time <= time {
            drop += 1;
        }
        if drop > 0 {
            self.base = self.evaluate(drop, self.events[drop].time);
            self.events.drain(..drop);
        }
    }
}

/// An AudioParam on the rendering thread.
#[derive(Debug)]
pub struct Param {
    pub timeline: Timeline,
    pub rate: Rate,
    pub default: f32,
    pub min: f32,
    pub max: f32,
    /// A compound parameter is clamped by its node after combination
    /// (#computation-of-value step 3).
    pub compound: bool,
    /// [[current value]], shared with the control thread.
    pub current: Arc<AtomicU32>,
    current_value: f32,
    /// computedValue for this render quantum.
    pub values: Vec<f32>,
    /// Every value in `values` is the same.
    pub constant: bool,
}

/// How to create a parameter.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor {
    pub default: f32,
    pub min: f32,
    pub max: f32,
    pub rate: Rate,
    pub compound: bool,
}

impl Param {
    /// `current` is [[current value]] as the control thread sees it. That
    /// thread created it holding defaultValue and may have set the value
    /// since (#dom-audioparam-value), before this message arrived; it
    /// changes here only from the next render quantum's computation.
    pub fn new(descriptor: Descriptor, current: Arc<AtomicU32>) -> Self {
        Self {
            timeline: Timeline::new(descriptor.default),
            rate: descriptor.rate,
            default: descriptor.default,
            min: descriptor.min,
            max: descriptor.max,
            compound: descriptor.compound,
            current,
            current_value: descriptor.default,
            values: Vec::new(),
            constant: true,
        }
    }

    pub fn apply(&mut self, automation: Automation, now: f64) {
        if let Automation::Rate(rate) = automation {
            self.rate = rate;
            return;
        }
        self.timeline
            .apply(automation, now, &mut self.current_value);
    }

    /// paramIntrinsicValue for the render quantum starting at `frame`
    /// (#computation-of-value steps 1-2); sets [[current value]].
    pub fn compute_intrinsic(&mut self, frame: u64, rate: f32, frames: usize) {
        self.values.resize(frames, 0.0);
        let rate = f64::from(rate);
        let start = frame as f64 / rate;
        let end = (frame + frames as u64 - 1) as f64 / rate;
        if let Some(value) = self.timeline.constant_over(start, end) {
            self.values.fill(value as f32);
            self.constant = true;
        } else if self.rate == Rate::K {
            self.values.fill(self.timeline.value_at(start) as f32);
            self.constant = true;
        } else {
            for (index, value) in self.values.iter_mut().enumerate() {
                *value = self.timeline.value_at((frame + index as u64) as f64 / rate) as f32;
            }
            self.constant = false;
        }
        self.current_value = self.values[0];
        self.current
            .store(self.current_value.to_bits(), Ordering::Relaxed);
        // Keep a second of history for automation messages stamped with an
        // earlier currentTime.
        self.timeline.prune(start - 1.0);
    }

    /// Add the input AudioParam buffer (already down-mixed to mono), then
    /// replace NaN and clamp a simple parameter (#computation-of-value).
    pub fn finish(&mut self, input: Option<&[f32]>) {
        if let Some(input) = input {
            if self.rate == Rate::K {
                let value = self.values[0] + input[0];
                self.values.fill(value);
            } else {
                for (value, add) in self.values.iter_mut().zip(input) {
                    *value += add;
                }
                self.constant = false;
            }
        }
        let (low, high, compound, default) = (self.min, self.max, self.compound, self.default);
        let fix = |value: &mut f32| {
            if value.is_nan() {
                *value = default;
            } else if !compound && (*value < low || *value > high) {
                *value = if *value < low { low } else { high };
            }
        };
        if self.constant {
            let mut value = self.values[0];
            fix(&mut value);
            self.values.fill(value);
        } else {
            self.values.iter_mut().for_each(fix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(default: f32) -> Param {
        Param::new(
            Descriptor {
                default,
                min: -1000.0,
                max: 1000.0,
                rate: Rate::A,
                compound: false,
            },
            Arc::new(AtomicU32::new(0)),
        )
    }

    fn block(param: &mut Param, frame: u64, rate: f32, frames: usize) -> Vec<f32> {
        param.compute_intrinsic(frame, rate, frames);
        param.finish(None);
        param.values.clone()
    }

    #[test]
    fn linear_and_exponential_ramps() {
        let mut p = param(0.0);
        p.apply(
            Automation::SetValue {
                value: 1.0,
                time: 0.0,
            },
            0.0,
        );
        p.apply(
            Automation::LinearRamp {
                value: 5.0,
                time: 4.0,
            },
            0.0,
        );
        assert_eq!(block(&mut p, 0, 1.0, 6), [1.0, 2.0, 3.0, 4.0, 5.0, 5.0]);
        let mut p = param(0.0);
        p.apply(
            Automation::SetValue {
                value: 1.0,
                time: 0.0,
            },
            0.0,
        );
        p.apply(
            Automation::ExponentialRamp {
                value: 16.0,
                time: 4.0,
            },
            0.0,
        );
        assert_eq!(block(&mut p, 0, 1.0, 5), [1.0, 2.0, 4.0, 8.0, 16.0]);
        assert_eq!(f32::from_bits(p.current.load(Ordering::Relaxed)), 1.0);
    }

    #[test]
    fn a_value_set_before_the_rendering_thread_has_the_param_survives() {
        // #dom-audioparam-value: setting `value` assigns [[current value]]
        // at once. The rendering thread learning of the parameter later must
        // not put defaultValue back (getFrequencyResponse reads it).
        let current = Arc::new(AtomicU32::new(500f32.to_bits()));
        let mut p = Param::new(
            Descriptor {
                default: 350.0,
                min: 0.0,
                max: 1000.0,
                rate: Rate::A,
                compound: false,
            },
            current.clone(),
        );
        assert_eq!(f32::from_bits(current.load(Ordering::Relaxed)), 500.0);
        p.apply(Automation::Value(500.0), 0.0);
        assert_eq!(block(&mut p, 0, 1.0, 2), [500.0, 500.0]);
        assert_eq!(f32::from_bits(current.load(Ordering::Relaxed)), 500.0);
    }

    #[test]
    fn target_curve_cancel_and_hold() {
        let mut p = param(1.0);
        p.apply(
            Automation::SetTarget {
                target: 0.0,
                time: 1.0,
                time_constant: 1.0,
            },
            0.0,
        );
        let values = block(&mut p, 0, 1.0, 3);
        assert_eq!(values[..2], [1.0, 1.0]);
        assert!((values[2] - (-1f64).exp() as f32).abs() < 1e-7);
        let mut p = param(0.0);
        p.apply(
            Automation::SetCurve {
                values: Arc::from([0.0f32, 10.0, 20.0]),
                time: 0.0,
                duration: 4.0,
            },
            0.0,
        );
        p.apply(Automation::CancelAndHold(2.0), 0.0);
        assert_eq!(block(&mut p, 0, 1.0, 5), [0.0, 5.0, 10.0, 10.0, 10.0]);
    }

    #[test]
    fn nan_inputs_become_the_default_and_simple_params_clamp() {
        let mut p = param(3.0);
        p.compute_intrinsic(0, 1.0, 2);
        p.finish(Some(&[f32::NAN, 5000.0]));
        assert_eq!(p.values, [3.0, 1000.0]);
    }

    #[test]
    fn pruning_keeps_values() {
        let mut p = param(0.0);
        for step in 0..10 {
            p.apply(
                Automation::LinearRamp {
                    value: (step * step) as f32,
                    time: f64::from(step),
                },
                0.0,
            );
        }
        p.apply(
            Automation::SetTarget {
                target: 2.0,
                time: 4.5,
                time_constant: 0.5,
            },
            0.0,
        );
        p.apply(
            Automation::LinearRamp {
                value: 7.0,
                time: 20.0,
            },
            0.0,
        );
        let sample = |p: &Param| -> Vec<f64> {
            (0..60)
                .map(|t| p.timeline.value_at(5.0 + f64::from(t) / 4.0))
                .collect()
        };
        let before = sample(&p);
        p.timeline.prune(5.0);
        assert_eq!(p.timeline.events.len(), 6);
        assert_eq!(before, sample(&p));
        // A later edit recomputes the setTarget start from the base.
        p.apply(
            Automation::SetValue {
                value: 1.0,
                time: 30.0,
            },
            0.0,
        );
        assert_eq!(before, sample(&p));
    }
}
