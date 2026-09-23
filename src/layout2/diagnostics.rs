//! Opt-in operation accounting. Timings are exclusive: time in a nested
//! instrumented operation is charged to that operation, not every ancestor.
//! Disabled layout performs no clock reads or profile allocations.

use std::cell::RefCell;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(super) enum Op {
    Block,
    BlockCompute,
    Item,
    ItemCompute,
    Intrinsic,
    IntrinsicCompute,
    Inline,
    CacheRead,
    CacheWrite,
    Offset,
    Positioned,
}

const OPS: [Op; 11] = [
    Op::Block,
    Op::BlockCompute,
    Op::Item,
    Op::ItemCompute,
    Op::Intrinsic,
    Op::IntrinsicCompute,
    Op::Inline,
    Op::CacheRead,
    Op::CacheWrite,
    Op::Offset,
    Op::Positioned,
];

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Sample {
    pub calls: usize,
    pub exclusive: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Profile {
    pub samples: [Sample; OPS.len()],
    pub uncacheable: usize,
}

impl Profile {
    pub fn print(&self) {
        for op in OPS {
            let sample = self.samples[op as usize];
            eprint!(
                " {op:?}={}/{:.1}us",
                sample.calls,
                sample.exclusive.as_secs_f64() * 1e6
            );
        }
        eprintln!();
    }
}

struct Active {
    op: Op,
    start: Instant,
    children: Duration,
}

#[derive(Default)]
struct State {
    enabled: bool,
    profile: Profile,
    stack: Vec<Active>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub(super) struct Session(bool);

impl Session {
    pub fn start() -> Self {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| std::env::var_os("TRUST_LAYOUT_PROFILE").is_some());
        let owner = enabled
            && STATE.with_borrow_mut(|s| {
                if s.enabled {
                    return false;
                }
                s.enabled = true;
                s.profile = Profile::default();
                true
            });
        Self(owner)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        let profile = STATE.with_borrow_mut(|s| {
            debug_assert!(s.stack.is_empty());
            s.enabled = false;
            s.profile
        });
        eprint!("LAYOUT_PROFILE uncacheable={}", profile.uncacheable);
        profile.print();
    }
}

pub(super) struct Span(bool);

#[inline]
pub(super) fn enter(op: Op) -> Span {
    Span(STATE.with_borrow_mut(|s| {
        if !s.enabled {
            return false;
        }
        s.stack.push(Active {
            op,
            start: Instant::now(),
            children: Duration::ZERO,
        });
        true
    }))
}

impl Drop for Span {
    #[inline]
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        STATE.with_borrow_mut(|s| {
            let active = s.stack.pop().expect("paired layout diagnostic span");
            let elapsed = active.start.elapsed();
            let sample = &mut s.profile.samples[active.op as usize];
            sample.calls += 1;
            sample.exclusive += elapsed.saturating_sub(active.children);
            if let Some(parent) = s.stack.last_mut() {
                parent.children += elapsed;
            }
        });
    }
}

pub(super) fn uncacheable() {
    STATE.with_borrow_mut(|s| {
        if s.enabled {
            s.profile.uncacheable += 1;
        }
    });
}

#[cfg(test)]
pub(super) fn measure<R>(f: impl FnOnce() -> R) -> (R, Profile) {
    let previous = STATE.with_borrow_mut(|s| {
        std::mem::replace(
            s,
            State {
                enabled: true,
                ..Default::default()
            },
        )
    });
    let result = f();
    let profile = STATE.with_borrow_mut(|s| std::mem::replace(s, previous).profile);
    (result, profile)
}
