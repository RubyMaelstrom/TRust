//! CSS Transitions 1 #starting, #reversing, #application, #complete and
//! #transition-events; CSS Easing 1. CSSWG snapshot 81c27f686901 (2026-09-06).
//!
//! The canonical style owns the transition, so CSSOM, layout, observers and
//! both frontends see the same intermediate value. Box lengths/percentages,
//! opacity and transforms have dedicated endpoints for every element; the
//! other interpolable properties (`EXTENDED`: colors, shadows, filters,
//! lengths, numbers, `visibility`, …) use the animation value model and are
//! tracked for the elements whose style transitions them.
use super::animations::values::{self, Kind};
use super::*;
use crate::layout2::value::{Len, Node as Length, Vp};

pub(super) mod transform;
use transform::Transform;

/// The animation-model color source for `currentcolor` endpoints.
type Current<'a> = &'a dyn Fn() -> Option<values::Color>;

#[derive(Clone, Debug, PartialEq)]
enum Animated {
    Linear(Linear),
    Transform(Transform),
    /// An `EXTENDED` property's computed value (Web Animations 1
    /// #animation-types through the animation value model).
    Value(Kind, values::Value),
}
impl Animated {
    /// CSS Transitions 1 #transitionable: neither not animatable nor
    /// discrete for this pair of values.
    fn compatible(&self, other: &Self, current: Current<'_>) -> bool {
        match (self, other) {
            (Self::Linear(_), Self::Linear(_)) => true,
            (Self::Transform(a), Self::Transform(b)) => a.compatible(b),
            (Self::Value(kind, a), Self::Value(_, b)) => {
                values::mix_value(*kind, a, b, 0.5, current).is_some()
            }
            _ => false,
        }
    }
    fn mix(&self, end: &Self, t: f32, current: Current<'_>) -> Self {
        match (self, end) {
            (Self::Linear(a), Self::Linear(b)) => Self::Linear(a.mix(*b, t)),
            (Self::Transform(a), Self::Transform(b)) => Self::Transform(a.mix(b, t)),
            (Self::Value(kind, a), Self::Value(_, b)) => Self::Value(
                *kind,
                values::mix_value(*kind, a, b, t, current)
                    .unwrap_or_else(|| if t < 0.5 { a.clone() } else { b.clone() }),
            ),
            _ => unreachable!("transition endpoints have matching animation types"),
        }
    }
    fn css(&self, property: usize) -> String {
        match self {
            Self::Linear(v) => v.css(property),
            Self::Transform(v) => v.css(),
            Self::Value(kind, v) => v.css(*kind),
        }
    }
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Transform(v) => v.retained_bytes(),
            Self::Value(..) => std::mem::size_of::<values::Value>(),
            _ => 0,
        }
    }
}

pub(super) const PROPERTIES: [&str; 20] = [
    "width",
    "height",
    "min-width",
    "min-height",
    "max-width",
    "max-height",
    "top",
    "right",
    "bottom",
    "left",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "opacity",
    "transform",
];
/// The other interpolable properties: Web Animations 1 #animation-types
/// other than discrete, by the animation value model (`values::Kind`).
pub(super) const EXTENDED: [&str; 46] = [
    "color",
    "background-color",
    "border-top-color",
    "border-right-color",
    "border-bottom-color",
    "border-left-color",
    "outline-color",
    "text-decoration-color",
    "caret-color",
    "-webkit-text-fill-color",
    "-webkit-text-stroke-color",
    "stop-color",
    "fill",
    "stroke",
    "box-shadow",
    "text-shadow",
    "filter",
    // Filter Effects 2 #BackdropFilterProperty: "Animation type: see prose
    // in [[filter-effects-1#animation-of-filters]]", as for `filter`.
    "backdrop-filter",
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
    "outline-width",
    "outline-offset",
    "font-size",
    "flex-basis",
    "column-gap",
    "row-gap",
    "text-indent",
    "vertical-align",
    "text-underline-offset",
    "-webkit-text-stroke-width",
    "stroke-width",
    "stroke-dashoffset",
    "letter-spacing",
    "word-spacing",
    "background-position-x",
    "background-position-y",
    "flex-grow",
    "flex-shrink",
    "z-index",
    "order",
    "fill-opacity",
    "stroke-opacity",
    "font-weight",
    "visibility",
];
/// Every transitioned property: `PROPERTIES`, then `EXTENDED`.
const COUNT: usize = PROPERTIES.len() + EXTENDED.len();

fn property_name(index: usize) -> &'static str {
    if index < PROPERTIES.len() {
        PROPERTIES[index]
    } else {
        EXTENDED[index - PROPERTIES.len()]
    }
}

fn property_index(name: &str) -> Option<usize> {
    PROPERTIES.iter().position(|p| *p == name).or_else(|| {
        EXTENDED
            .iter()
            .position(|p| *p == name)
            .map(|i| i + PROPERTIES.len())
    })
}

/// One element's `EXTENDED` endpoints.
type ExtendedValues = Box<[Option<Animated>]>;

/// One element's computed `EXTENDED` values (`None` where unwatched), as
/// of the last style change event; equal snapshots share one allocation.
type Snapshot = std::rc::Rc<[Option<Box<str>>]>;

const LONGHANDS: [&str; 4] = [
    "transition-property",
    "transition-duration",
    "transition-timing-function",
    "transition-delay",
];
type Values = [Option<Animated>; PROPERTIES.len()];
/// A transition's target (an element or one of its pseudo-elements) and
/// transitioned property.
type Key = (StyleTarget, usize);
/// (element, propertyName, type, elapsedTime, pseudoElement).
pub(crate) type TransitionEvent = (NodeId, &'static str, &'static str, f64, &'static str);

struct PendingEvent {
    event: TransitionEvent,
    time: f64,
    generation: u64,
    tree_order: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Linear {
    px: f32,
    pct: f32,
}
impl Linear {
    fn mix(self, end: Self, t: f32) -> Self {
        Self {
            px: self.px + (end.px - self.px) * t,
            pct: self.pct + (end.pct - self.pct) * t,
        }
    }
    fn css(self, property: usize) -> String {
        if property == 18 {
            return self.px.clamp(0., 1.).to_string();
        }
        if self.pct == 0. {
            format!("{}px", self.px)
        } else if self.px == 0. {
            format!("{}%", self.pct * 100.)
        } else {
            format!("calc({}% + {}px)", self.pct * 100., self.px)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Easing {
    Linear,
    Bezier(f32, f32, f32, f32),
    Steps(u32, bool, i32),
}
impl Easing {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        Some(match value.as_str() {
            "linear" => Self::Linear,
            "ease" => Self::Bezier(0.25, 0.1, 0.25, 1.),
            "ease-in" => Self::Bezier(0.42, 0., 1., 1.),
            "ease-out" => Self::Bezier(0., 0., 0.58, 1.),
            "ease-in-out" => Self::Bezier(0.42, 0., 0.58, 1.),
            "step-start" => Self::Steps(1, true, 0),
            "step-end" => Self::Steps(1, false, 0),
            _ => {
                if let Some(s) = value
                    .strip_prefix("cubic-bezier(")
                    .and_then(|s| s.strip_suffix(')'))
                {
                    let n = s
                        .split(',')
                        .map(|s| s.trim().parse::<f32>().ok())
                        .collect::<Option<Vec<_>>>()?;
                    if n.len() != 4
                        || n.iter().any(|n| !n.is_finite())
                        || !(0. ..=1.).contains(&n[0])
                        || !(0. ..=1.).contains(&n[2])
                    {
                        return None;
                    }
                    Self::Bezier(n[0], n[1], n[2], n[3])
                } else {
                    let s = value.strip_prefix("steps(")?.strip_suffix(')')?;
                    let parts = s.split(',').map(str::trim).collect::<Vec<_>>();
                    let n = parts.first()?.parse::<u32>().ok().filter(|n| *n > 0)?;
                    if parts.len() > 2 {
                        return None;
                    }
                    let (start, delta) = match parts.get(1).copied().unwrap_or("end") {
                        "start" | "jump-start" => (true, 0),
                        "end" | "jump-end" => (false, 0),
                        "jump-both" => (true, 1),
                        "jump-none" if n > 1 => (false, -1),
                        _ => return None,
                    };
                    Self::Steps(n, start, delta)
                }
            }
        })
    }
    pub(super) fn sample(self, x: f32) -> f32 {
        match self {
            Self::Linear => x,
            Self::Steps(n, start, delta) => {
                ((x * n as f32).floor() + if start { 1. } else { 0. })
                    .clamp(0., n as f32 + delta as f32)
                    / (n as f32 + delta as f32)
            }
            Self::Bezier(x1, y1, x2, y2) => {
                if x == 0. || x == 1. {
                    return x;
                }
                let curve = |t: f32, a: f32, b: f32| {
                    3. * (1. - t).powi(2) * t * a + 3. * (1. - t) * t * t * b + t * t * t
                };
                let (mut lo, mut hi) = (0., 1.);
                for _ in 0..20 {
                    let mid = (lo + hi) * 0.5;
                    if curve(mid, x1, x2) < x {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                // Y control points can overshoot; clamp only when the property's
                // own range requires it, never the timing-function output.
                curve((lo + hi) * 0.5, y1, y2)
            }
        }
    }
}

/// A <time> in seconds: a literal, or a math function resolving to <time>
/// (CSS Values 4 #calc-type-checking), as the shorthand classifies it.
fn time(value: &str) -> Option<f64> {
    if let Some(seconds) = properties::numeric::math_seconds(value) {
        return Some(seconds);
    }
    let value = value.trim().to_ascii_lowercase();
    let (n, scale) = if let Some(n) = value.strip_suffix("ms") {
        (n, 0.001)
    } else {
        (value.strip_suffix('s')?, 1.)
    };
    n.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| n * scale)
}

pub(super) fn expand(value: &str) -> Vec<(String, String)> {
    if wide_keyword(value).is_some() {
        return LONGHANDS
            .iter()
            .map(|name| ((*name).into(), value.into()))
            .collect();
    }
    let segments = split_top_level_commas(value);
    let mut lists: [Vec<String>; 4] = std::array::from_fn(|_| vec![]);
    for segment in &segments {
        let (mut property, mut duration, mut timing, mut delay) = (None, None, None, None);
        for token in split_top_level_ws(segment) {
            if let Some(t) = time(token) {
                if duration.is_none() {
                    // A negative literal is invalid; a calculation clamps
                    // (CSS Values 4 #calc-range).
                    if t < 0. && !crate::layout2::value::is_math_function(token) {
                        return vec![];
                    }
                    duration = Some(token);
                } else if delay.is_none() {
                    delay = Some(token);
                } else {
                    return vec![];
                }
            } else if Easing::parse(token).is_some() && timing.is_none() {
                timing = Some(token);
            } else if property.is_none()
                && properties::ident(token).is_some()
                && wide_keyword(token).is_none()
                && token != "default"
            {
                property = Some(token);
            } else {
                return vec![];
            }
        }
        if segment.trim().is_empty() || (segments.len() > 1 && property == Some("none")) {
            return vec![];
        }
        for (list, value) in lists.iter_mut().zip([
            property.unwrap_or("all"),
            duration.unwrap_or("0s"),
            timing.unwrap_or("ease"),
            delay.unwrap_or("0s"),
        ]) {
            list.push(value.into());
        }
    }
    LONGHANDS
        .iter()
        .zip(lists)
        .map(|(name, values)| ((*name).into(), values.join(", ")))
        .collect()
}

pub(super) fn valid_longhand(name: &str, value: &str) -> bool {
    if wide_keyword(value).is_some() || find_var_function(value).is_some() {
        return true;
    }
    let Ok(value) = properties::numeric::specified_literals(name, value) else {
        return false;
    };
    let parts = split_top_level_commas(&value)
        .into_iter()
        .map(str::trim)
        .collect::<Vec<_>>();
    !parts.is_empty()
        && parts.iter().all(|v| match name {
            "transition-duration" => time(v).is_some_and(|t| t >= 0.),
            "transition-delay" => time(v).is_some(),
            "transition-timing-function" => Easing::parse(v).is_some(),
            "transition-property" => {
                properties::ident(v).is_some()
                    && wide_keyword(v).is_none()
                    && *v != "default"
                    && (*v != "none" || parts.len() == 1)
            }
            _ => true,
        })
}

#[derive(Clone, Copy)]
struct Parameters {
    duration: f64,
    delay: f64,
    easing: Easing,
}
fn parameters(dom: &Dom, target: StyleTarget) -> [Option<Parameters>; COUNT] {
    let values = LONGHANDS.map(|p| {
        target_value(dom, target, p).unwrap_or_else(|| {
            match p {
                "transition-property" => "all",
                "transition-timing-function" => "ease",
                _ => "0s",
            }
            .into()
        })
    });
    let lists = values.each_ref().map(|v| split_top_level_commas(v));
    let mut out = [None; COUNT];
    for (i, property) in lists[0].iter().enumerate() {
        let property = property.trim();
        let value = |n: usize| {
            lists[n]
                .get(i % lists[n].len().max(1))
                .copied()
                .unwrap_or("")
        };
        let Some(duration) = time(value(1)).filter(|t| *t >= 0.) else {
            continue;
        };
        let Some(delay) = time(value(3)) else {
            continue;
        };
        let Some(easing) = Easing::parse(value(2)) else {
            continue;
        };
        let names = cssom::property_names(property);
        for (j, p) in PROPERTIES.iter().chain(EXTENDED.iter()).enumerate() {
            if property == "all" || names.iter().any(|name| name == p) {
                out[j] = Some(Parameters {
                    duration,
                    delay,
                    easing,
                });
            }
        }
    }
    out
}

struct Running {
    from: Animated,
    to: Animated,
    reversing_from: Animated,
    factor: f64,
    start: f64,
    duration: f64,
    delay: f64,
    easing: Easing,
    started: bool,
    generation: u64,
    tree_order: usize,
}
impl Running {
    fn progress(&self, now: f64) -> f32 {
        if now < self.start {
            0.
        } else if self.duration == 0. {
            1.
        } else {
            self.easing
                .sample(((now - self.start) / self.duration).clamp(0., 1.) as f32)
        }
    }
    fn value(&self, now: f64, current: Current<'_>) -> Animated {
        self.from.mix(&self.to, self.progress(now), current)
    }
    fn elapsed(&self, now: f64) -> f64 {
        (now - self.start).clamp(0., self.duration)
    }
    fn event(&self, key: Key, kind: &'static str, elapsed: f64, time: f64) -> PendingEvent {
        PendingEvent {
            event: (
                key.0.0,
                property_name(key.1),
                kind,
                elapsed,
                pseudo_selector(key.0),
            ),
            time,
            generation: self.generation,
            tree_order: self.tree_order,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    epoch: Option<u64>,
    style_stamp: (u64, u64),
    invalid: RefCell<FxHashSet<NodeId>>,
    all_invalid: Cell<bool>,
    now: f64,
    before: FxHashMap<StyleTarget, Values>,
    /// `EXTENDED` endpoints of the elements whose style transitions any of
    /// them (see `extended_interest`).
    before_extended: FxHashMap<StyleTarget, ExtendedValues>,
    /// CSS Transitions 1 #starting: the before-change style is every
    /// property's computed value as of the previous style change event, so
    /// a transition declared only in the after-change style (`a:hover {
    /// color: red; transition: color 1s }`) starts from the value before
    /// it. `before_extended` holds the elements already transitioning
    /// `EXTENDED` properties; these snapshots hold, for every other
    /// participating element, the `EXTENDED` properties that the
    /// document's transitions name (`watch`, a bit per property).
    snapshots: FxHashMap<StyleTarget, Snapshot>,
    interned: FxHashSet<Snapshot>,
    watch: u64,
    /// The style revision `watch` was collected for.
    watch_epoch: Option<u64>,
    running: FxHashMap<Key, Running>,
    completed: FxHashMap<Key, Animated>,
    values: FxHashMap<Key, String>,
    events: Vec<PendingEvent>,
    generation: u64,
    /// Endpoints computed by a parallel style pass for the next update.
    prepared: RefCell<Option<Prepared>>,
    #[cfg(test)]
    last_recomputed: usize,
    #[cfg(test)]
    pub(super) last_prepared: usize,
}

/// Per element, its endpoints (`None`: it does not participate), as a
/// parallel style pass computed them for one revision of the document
/// (style value, page font and DOM epochs).
pub(super) struct Prepared {
    pub(super) stamp: (u64, u64, u64),
    pub(super) endpoints: FxHashMap<NodeId, Option<Endpoints>>,
}

/// One element's endpoints, computed ahead of the update that reads them.
pub(super) struct Endpoints(Values);

/// `id`'s endpoints as the next update computes them (see `endpoints`).
pub(super) fn endpoints_ahead<D: crate::layout2::UnitSource + ?Sized>(
    dom: &D,
    id: NodeId,
    vp: Vp,
) -> Endpoints {
    Endpoints(endpoints(dom, id, vp))
}

impl State {
    /// Offer the next update endpoints computed elsewhere for `stamp`.
    pub(super) fn prepare(&self, prepared: Prepared) {
        *self.prepared.borrow_mut() = Some(prepared);
    }

    pub(super) fn remove_node(&mut self, node: NodeId) {
        self.invalid.get_mut().remove(&node);
        for target in std::iter::once(None)
            .chain(PseudoEl::ALL.map(Some))
            .map(|pseudo| (node, pseudo))
        {
            let tracked = self.before.remove(&target).is_some();
            self.before_extended.remove(&target);
            self.snapshots.remove(&target);
            if !tracked && target.1.is_some() {
                // A pseudo-element without endpoints has no transitions.
                continue;
            }
            for property in 0..COUNT {
                // Running and pending transition targets are native roots.
                // Completed/value caches do not root nodes and can be removed
                // directly without scanning old transitions.
                debug_assert!(!self.running.contains_key(&(target, property)));
                self.completed.remove(&(target, property));
                self.values.remove(&(target, property));
            }
        }
        macro_rules! shrink {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        shrink!(before, before_extended, snapshots, completed, values);
        let invalid = self.invalid.get_mut();
        if invalid.capacity() > invalid.len().saturating_mul(4).max(64) {
            invalid.shrink_to(invalid.len().saturating_mul(2));
        }
    }

    pub(super) fn visit_gc_roots(&self, visit: &mut dyn FnMut(NodeId)) {
        for &((node, _), _) in self.running.keys() {
            visit(node);
        }
        for event in &self.events {
            visit(event.event.0);
        }
    }

    pub(super) fn retain_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) {
        self.invalid.get_mut().retain(|&id| live(id));
        self.before.retain(|&(id, _), _| live(id));
        self.before_extended.retain(|&(id, _), _| live(id));
        self.snapshots.retain(|&(id, _), _| live(id));
        self.release_snapshots();
        self.running.retain(|((id, _), _), _| live(*id));
        self.completed.retain(|((id, _), _), _| live(*id));
        self.values.retain(|((id, _), _), _| live(*id));
        self.events.retain(|event| live(event.event.0));
        macro_rules! shrink {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        shrink!(
            before,
            before_extended,
            snapshots,
            running,
            completed,
            values,
            events
        );
        let invalid = self.invalid.get_mut();
        if invalid.capacity() > invalid.len().saturating_mul(4).max(64) {
            invalid.shrink_to(invalid.len().saturating_mul(2));
        }
    }

    pub(super) fn invalidate(&self, id: NodeId) {
        self.invalid.borrow_mut().insert(id);
    }
    pub(super) fn invalidate_all(&self) {
        self.all_invalid.set(true);
        self.invalid.borrow_mut().clear();
    }
    /// No transition is running or holds a transition-origin value.
    pub(super) fn idle(&self) -> bool {
        self.running.is_empty() && self.values.is_empty()
    }
    pub(super) fn affects_computation(&self, name: &str) -> bool {
        !self.values.is_empty() && property_index(name).is_some()
    }
    pub(super) fn value(&self, id: NodeId, name: &str) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        let property = property_index(name)?;
        self.values.get(&((id, None), property)).cloned()
    }
    /// The transition-origin value of `name` on `id`'s `which`
    /// pseudo-element.
    pub(super) fn pseudo_value(&self, id: NodeId, which: PseudoEl, name: &str) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        let property = property_index(name)?;
        self.values.get(&((id, Some(which)), property)).cloned()
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.invalid.borrow().capacity() * std::mem::size_of::<NodeId>()
            + self.before.capacity() * std::mem::size_of::<(StyleTarget, Values)>()
            + self
                .before_extended
                .values()
                .map(|v| v.len() * std::mem::size_of::<Option<Animated>>())
                .sum::<usize>()
            + self.snapshots.capacity() * std::mem::size_of::<(StyleTarget, Snapshot)>()
            + self
                .interned
                .iter()
                .map(|snapshot| {
                    snapshot.len() * std::mem::size_of::<Option<Box<str>>>()
                        + snapshot.iter().flatten().map(|v| v.len()).sum::<usize>()
                })
                .sum::<usize>()
            + self.running.capacity() * std::mem::size_of::<(Key, Running)>()
            + self.completed.capacity() * std::mem::size_of::<(Key, Animated)>()
            + self.values.capacity() * std::mem::size_of::<(Key, String)>()
            + self.values.values().map(String::capacity).sum::<usize>()
            + self.events.capacity() * std::mem::size_of::<PendingEvent>()
            + self
                .before
                .values()
                .flat_map(|v| v.iter().flatten())
                .map(Animated::retained_bytes)
                .sum::<usize>()
            + self
                .completed
                .values()
                .map(Animated::retained_bytes)
                .sum::<usize>()
            + self
                .running
                .values()
                .map(|v| {
                    v.from.retained_bytes()
                        + v.to.retained_bytes()
                        + v.reversing_from.retained_bytes()
                })
                .sum::<usize>()
    }
    /// Share an equal snapshot (most elements repeat a few computed styles).
    fn intern(&mut self, values: Vec<Option<Box<str>>>) -> Snapshot {
        if let Some(existing) = self.interned.get(values.as_slice()) {
            return existing.clone();
        }
        let snapshot: Snapshot = values.into();
        self.interned.insert(snapshot.clone());
        snapshot
    }

    /// Drop interned snapshots no element holds any more.
    fn release_snapshots(&mut self) {
        if self.interned.len() > 64 && self.interned.len() > self.snapshots.len() / 4 {
            self.interned
                .retain(|snapshot| std::rc::Rc::strong_count(snapshot) > 1);
        }
    }

    fn cancel(&mut self, key: Key, now: f64) {
        if let Some(old) = self.running.remove(&key) {
            self.events
                .push(old.event(key, "transitioncancel", old.elapsed(now), now));
        }
    }

    /// CSS Transitions 1 #starting: "If an element is no longer in the
    /// document, implementations must cancel any running transitions on it",
    /// for elements that left this arena for another of the group
    /// (`Dom::transplant_subtree`), at the time of the last update. Their
    /// `transitioncancel` events stay queued here, with the transitions'
    /// timeline; `remove_node` forgets the rest.
    pub(super) fn cancel_departed(&mut self, departed: &FxHashSet<NodeId>) {
        let now = self.now;
        let keys: Vec<Key> = self
            .running
            .keys()
            .copied()
            .filter(|((node, _), _)| departed.contains(node))
            .collect();
        for key in keys {
            self.cancel(key, now);
        }
    }
    fn advance(&mut self, now: f64) {
        let mut finished = vec![];
        for (&key, track) in &mut self.running {
            if !track.started && now >= track.start {
                track.started = true;
                self.events.push(track.event(
                    key,
                    "transitionstart",
                    (-track.delay).clamp(0., track.duration),
                    track.start.max(track.start - track.delay),
                ));
            }
            if now >= track.start + track.duration {
                self.events.push(track.event(
                    key,
                    "transitionend",
                    track.duration,
                    track.start + track.duration,
                ));
                self.completed.insert(key, track.to.clone());
                finished.push(key);
            }
        }
        for key in finished {
            self.running.remove(&key);
        }
    }
}

/// CSS Animations 2 #animation-composite-order and CSS Transitions 2: an
/// element's own transitions come first, then its pseudo-elements' in
/// `::marker`, `::before`, `::after` order (other pseudo-elements after).
fn pseudo_rank(selector: &str) -> u8 {
    match selector {
        "" => 0,
        "::marker" => 1,
        "::before" => 2,
        "::after" => 3,
        _ => 4,
    }
}

/// CSS Transitions 1 #starting: the transitioned properties' computed
/// values as animatable endpoints.
fn endpoints<D: crate::layout2::UnitSource + ?Sized>(dom: &D, id: NodeId, vp: Vp) -> Values {
    endpoints_with(
        &|name| dom.computed_value_resolved(id, name),
        crate::layout2::Units::of(dom, id),
        vp,
    )
}

/// `endpoints` of a target whose computed values `value` reads, with its
/// font-relative `units`.
fn endpoints_with(
    value: &dyn Fn(&str) -> Option<String>,
    units: crate::layout2::Units,
    vp: Vp,
) -> Values {
    std::array::from_fn(|i| {
        // Transition endpoints are computed values, not CSSOM's
        // used-value serialization. Resolve relative lengths only
        // when needed; px, percentages and initial values do not
        // require selecting/shaping a font for every element.
        let value = value(PROPERTIES[i]);
        let value = value
            .as_deref()
            .or_else(|| cssom_initial_value(PROPERTIES[i]))?
            .trim();
        if i == 19 {
            return Transform::parse(value, units, vp).map(Animated::Transform);
        }
        if i == 18 {
            return parse_alpha(value).map(|px| {
                Animated::Linear(Linear {
                    px: px.clamp(0., 1.),
                    pct: 0.,
                })
            });
        }
        if matches!(
            value,
            "auto" | "none" | "min-content" | "max-content" | "fit-content"
        ) {
            return None;
        }
        if let Some(px) = value
            .strip_suffix("px")
            .unwrap_or(value)
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
        {
            return Some(Animated::Linear(Linear { px, pct: 0. }));
        }
        if let Some(pct) = value
            .strip_suffix('%')
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite())
        {
            return Some(Animated::Linear(Linear {
                px: 0.,
                pct: pct / 100.,
            }));
        }
        match Len::parse(value, units, vp)? {
            Len::Val(Length::Lin { k, b }) => Some(Animated::Linear(Linear { px: b, pct: k })),
            _ => None,
        }
    })
}

/// The `EXTENDED` endpoints, as animation-model computed values.
fn extended_endpoints(dom: &Dom, target: StyleTarget, vp: Vp) -> ExtendedValues {
    let id = target.0;
    EXTENDED
        .iter()
        .map(|&name| {
            let value = target_value(dom, target, name);
            let value = value.as_deref().or_else(|| cssom_initial_value(name))?;
            let kind = Kind::of(name);
            Some(Animated::Value(
                kind,
                values::Value::parse(dom, id, kind, value, vp),
            ))
        })
        .collect()
}

/// The `EXTENDED` properties a `transition-property` list can name, as a
/// bit per property; `all`, an unknown or a `var()`-dependent list names
/// every one.
fn named_extended(list: &str) -> u64 {
    let all = (1u64 << EXTENDED.len()) - 1;
    if find_var_function(list).is_some() || wide_keyword(list).is_some() {
        return all;
    }
    let mut mask = 0;
    for item in split_top_level_commas(list) {
        let item = item.trim();
        if item.eq_ignore_ascii_case("all") {
            return all;
        }
        for name in cssom::property_names(&item.to_ascii_lowercase()).iter() {
            if let Some(index) = EXTENDED.iter().position(|p| p == name) {
                mask |= 1 << index;
            }
        }
    }
    mask
}

/// The `EXTENDED` properties the document's style sheets transition.
fn sheet_watch(dom: &Dom) -> u64 {
    let index = dom.style_index();
    let mut mask = 0;
    for rules in index.scopes.values() {
        for rule in rules {
            for (name, (_, value)) in &rule.decls {
                if name == "transition-property" {
                    mask |= named_extended(value);
                }
            }
        }
    }
    mask
}

/// `target`'s computed values of the watched `EXTENDED` properties.
fn snapshot_values(dom: &Dom, target: StyleTarget, watch: u64) -> Vec<Option<Box<str>>> {
    EXTENDED
        .iter()
        .enumerate()
        .map(|(index, &name)| {
            (watch & (1 << index) != 0)
                .then(|| {
                    target_value(dom, target, name)
                        .or_else(|| cssom_initial_value(name).map(str::to_owned))
                })
                .flatten()
                .map(String::into_boxed_str)
        })
        .collect()
}

/// Whether `id`'s style transitions any `EXTENDED` property: a matching
/// `transition-property` with a positive combined duration (CSS Transitions
/// 1 #starting). Only such elements keep `EXTENDED` before-change values.
fn extended_interest(params: &[Option<Parameters>; COUNT]) -> bool {
    params[PROPERTIES.len()..]
        .iter()
        .flatten()
        .any(|p| p.duration.max(0.) + p.delay > 0.)
}

/// Whether `target` declares any transition timing at all; the common
/// untransitioned element skips reading the transition lists.
fn declares_transition(dom: &Dom, target: StyleTarget) -> bool {
    ["transition-duration", "transition-delay"]
        .iter()
        .any(|name| {
            target_value(dom, target, name).is_some_and(|value| {
                split_top_level_commas(&value)
                    .iter()
                    .any(|v| v.trim() != "0s")
            })
        })
}

/// Resolve participation only along paths whose styles/tree membership changed.
/// CSS Transitions 1 #starting: detached or non-rendered elements have no
/// before/after pair. Memoizing common ancestors keeps a large dirty subtree
/// linear without traversing unrelated branches or relying on dirty-set order.
/// Ancestors are resolved first: nothing below a non-rendered ancestor needs
/// its own `display`, which layout never computes either (a full pass visits
/// every element, so hidden menus would otherwise each pay a cascade).
pub(super) fn participates<B: StyleBackend + ?Sized>(
    dom: ComputeView<'_, B>,
    node: NodeId,
    memo: &mut FxHashMap<NodeId, bool>,
) -> bool {
    let mut path = Vec::new();
    let mut cursor = Some(node);
    let mut result = loop {
        let Some(id) = cursor else { break false };
        if id == dom.document() {
            break true;
        }
        if let Some(&result) = memo.get(&id) {
            break result;
        }
        path.push(id);
        if !dom.is_valid(id) {
            break false;
        }
        cursor = dom.parent_composed(id);
    };
    for &id in path.iter().rev() {
        result = result
            && dom.is_valid(id)
            && !(dom.tag_name(id).is_some()
                && (dom.computed_display(id).as_deref() == Some("none")
                    || dom.subtree_omitted_from_box_tree(id)));
        memo.insert(id, result);
    }
    result
}

/// One transition update's shared inputs.
struct Pass<'a> {
    prepared: Option<&'a Prepared>,
    participation: FxHashMap<NodeId, bool>,
    tree_order: Option<FxHashMap<NodeId, usize>>,
    vp: Vp,
    now: f64,
    /// Targets with running transitions before this update.
    active: FxHashSet<StyleTarget>,
}

/// `target`'s computed value of `name`: the element's, or its
/// pseudo-element's (which inherits from the element).
fn target_value(dom: &Dom, (id, pseudo): StyleTarget, name: &str) -> Option<String> {
    match pseudo {
        None => dom.computed_value_resolved(id, name),
        Some(which) => dom.pseudo_layout_value(id, which, name),
    }
}

impl Dom {
    /// Whether `id`'s `which` pseudo-element keeps transition endpoints:
    /// it is generated, and (for the markers and placeholders that every
    /// list item and placeholder text control generates) some rule styles
    /// that kind of pseudo-element at all.
    fn transition_pseudo_tracked(&self, id: NodeId, which: PseudoEl) -> bool {
        match which {
            PseudoEl::Marker if !self.style_index().has_marker => false,
            PseudoEl::Placeholder if !self.style_index().has_placeholder => false,
            PseudoEl::Before | PseudoEl::After
                if self.cascaded_maps(id).pseudo(which).is_empty()
                    && self.internal_marker(id, which.baked_style_attr()).is_none() =>
            {
                false
            }
            _ => self.pseudo_generated(id, which),
        }
    }

    /// CSS Transitions 1 #starting for one target: compare its
    /// before-change and after-change styles, and start, reverse, cancel or
    /// complete its transitions.
    fn transition_target(&self, state: &mut State, pass: &mut Pass<'_>, target: StyleTarget) {
        let (id, pseudo) = target;
        let (vp, now) = (pass.vp, pass.now);
        let known = match pseudo {
            None => pass
                .prepared
                .and_then(|prepared| prepared.endpoints.get(&id)),
            Some(_) => None,
        };
        let participating = match known {
            Some(values) => values.is_some(),
            None => {
                participates(ComputeView(self), id, &mut pass.participation)
                    && pseudo.is_none_or(|which| self.pseudo_generated(id, which))
            }
        };
        if !participating {
            state.before.remove(&target);
            state.before_extended.remove(&target);
            state.snapshots.remove(&target);
            for property in 0..COUNT {
                state.cancel((target, property), now);
                state.completed.remove(&(target, property));
            }
            return;
        }
        #[cfg(test)]
        {
            state.last_recomputed += 1;
        }
        let values = match (known, pseudo) {
            (Some(Some(values)), _) => {
                #[cfg(test)]
                {
                    state.last_prepared += 1;
                }
                values.0.clone()
            }
            (_, None) => endpoints(self, id, vp),
            (_, Some(which)) => endpoints_with(
                &|name| self.pseudo_layout_value(id, which, name),
                crate::layout2::pseudo_units(self, id, which),
                vp,
            ),
        };
        // `EXTENDED` endpoints for targets transitioning them.
        let declares = declares_transition(self, target);
        let params = declares.then(|| parameters(self, target));
        let extended = params
            .as_ref()
            .is_some_and(extended_interest)
            .then(|| extended_endpoints(self, target, vp));
        // An inline or script-set transition may name properties the sheets
        // do not: watch them from now on.
        if let Some(params) = &params {
            for (j, p) in params[PROPERTIES.len()..].iter().enumerate() {
                if p.is_some_and(|p| p.duration.max(0.) + p.delay > 0.) {
                    state.watch |= 1 << j;
                }
            }
        }
        // #starting: the before-change values of a target that did not
        // transition `EXTENDED` properties come from its snapshot of the
        // previous style change event.
        let before_extended = state.before_extended.get(&target).cloned().or_else(|| {
            let snapshot = state.snapshots.get(&target)?;
            extended.as_ref()?;
            Some(
                EXTENDED
                    .iter()
                    .zip(snapshot.iter())
                    .map(|(&name, value)| {
                        let kind = Kind::of(name);
                        value.as_deref().map(|value| {
                            Animated::Value(kind, values::Value::parse(self, id, kind, value, vp))
                        })
                    })
                    .collect(),
            )
        });
        // Without `EXTENDED` interest no transition reads the old snapshot:
        // the after-change values become the next event's before-change
        // values right away.
        if extended.is_none() {
            if state.watch != 0 {
                let snapshot = state.intern(snapshot_values(self, target, state.watch));
                state.snapshots.insert(target, snapshot);
            } else {
                state.snapshots.remove(&target);
            }
        }
        // The target's `currentcolor`: its computed `color`.
        let color = || {
            target_value(self, target, "color")
                .and_then(|value| values::Color::parse(self, id, &value))
        };
        if let Some(before) = state.before.get(&target).cloned() {
            if before == values
                && before_extended == extended
                && !pass.active.contains(&target)
                && !(0..COUNT).any(|i| state.completed.contains_key(&(target, i)))
            {
                return;
            }
            let params = params.unwrap_or_else(|| parameters(self, target));
            for i in 0..COUNT {
                let key = (target, i);
                let (end, before_value) = if i < PROPERTIES.len() {
                    (values[i].clone(), before[i].clone())
                } else {
                    let j = i - PROPERTIES.len();
                    (
                        extended.as_ref().and_then(|values| values[j].clone()),
                        before_extended
                            .as_ref()
                            .and_then(|values| values[j].clone()),
                    )
                };
                let Some(end) = end else {
                    state.cancel(key, now);
                    state.completed.remove(&key);
                    continue;
                };
                let Some(p) = params[i] else {
                    state.cancel(key, now);
                    state.completed.remove(&key);
                    continue;
                };
                if state.completed.get(&key).is_some_and(|v| *v != end) {
                    state.completed.remove(&key);
                }
                if state.running.get(&key).is_some_and(|r| r.to == end) {
                    continue;
                }
                let old = state.running.remove(&key);
                let start = old.as_ref().map(|r| r.value(now, &color)).or(before_value);
                if let Some(old) = &old {
                    state
                        .events
                        .push(old.event(key, "transitioncancel", old.elapsed(now), now));
                }
                let Some(start) = start else {
                    continue;
                };
                if !start.compatible(&end, &color)
                    || start == end
                    || p.duration + p.delay <= 0.
                    || state.completed.get(&key) == Some(&end)
                {
                    continue;
                }
                let (factor, reversing_from) = if let Some(old) = &old
                    && old.reversing_from == end
                {
                    (
                        (f64::from(old.progress(now)) * old.factor + 1. - old.factor)
                            .abs()
                            .clamp(0., 1.),
                        old.to.clone(),
                    )
                } else {
                    (1., start.clone())
                };
                let delay = if p.delay < 0. {
                    p.delay * factor
                } else {
                    p.delay
                };
                let duration = p.duration * factor;
                state.completed.remove(&key);
                // Event ordering is observable, but only actual new
                // transitions need a tree-order snapshot. Ordinary local style
                // transactions never build this map.
                let tree_order = *pass
                    .tree_order
                    .get_or_insert_with(|| {
                        self.composed_descendants(self.document())
                            .into_iter()
                            .enumerate()
                            .map(|(order, node)| (node, order))
                            .collect()
                    })
                    .get(&id)
                    .expect("participating transition target is connected");
                state.running.insert(
                    key,
                    Running {
                        from: start,
                        to: end,
                        reversing_from,
                        factor,
                        start: now + delay,
                        duration,
                        delay,
                        easing: p.easing,
                        started: false,
                        generation: state.generation,
                        tree_order,
                    },
                );
                state.events.push(state.running[&key].event(
                    key,
                    "transitionrun",
                    (-delay).clamp(0., duration),
                    now,
                ));
            }
        }
        state.before.insert(target, values);
        match extended {
            Some(extended) => {
                state.before_extended.insert(target, extended);
                state.snapshots.remove(&target);
            }
            None => {
                state.before_extended.remove(&target);
            }
        }
    }
}

impl Dom {
    /// Compare prepared endpoints with the serial computation; returns the
    /// elements checked and the mismatches.
    pub(super) fn prepared_mismatches(&self, prepared: &Prepared) -> (usize, usize) {
        let vp = Vp {
            w: self.viewport_px.0,
            h: self.viewport_px.1,
        };
        let mut participation = FxHashMap::default();
        let mut mismatches = 0;
        for (&id, known) in &prepared.endpoints {
            let known = known.as_ref().map(|known| &known.0);
            let expected = participates(ComputeView(self), id, &mut participation)
                .then(|| endpoints(self, id, vp));
            if known != expected.as_ref() {
                mismatches += 1;
                if mismatches <= 8 {
                    eprintln!(
                        "STYLEVERIFY transition mismatch node={id} tag={:?} parallel={known:?} serial={expected:?}",
                        self.tag_name(id)
                    );
                }
            }
        }
        (prepared.endpoints.len(), mismatches)
    }

    #[cfg(test)]
    pub(super) fn pending_endpoint_mismatches(&self) -> Option<(usize, usize)> {
        let prepared = self.transitions.prepared.borrow();
        prepared
            .as_ref()
            .map(|prepared| self.prepared_mismatches(prepared))
    }

    pub(crate) fn css_transitions_active(&self) -> bool {
        !self.transitions.running.is_empty()
    }
    pub(crate) fn css_transition_work_pending(&self) -> bool {
        self.css_transitions_active() || !self.transitions.events.is_empty()
    }
    pub(crate) fn take_css_transition_events(&mut self) -> Vec<TransitionEvent> {
        let mut events = std::mem::take(&mut self.transitions.events);
        if events.is_empty() {
            return vec![];
        }
        // Web Animations #animation-frame-loop and CSS Transitions 2
        // #animation-composite-order: scheduled time, then owning element,
        // generation, and expanded property name. Stable sorting preserves
        // run/start/end order when their scheduled times coincide.
        let order: FxHashMap<_, _> = self
            .composed_descendants(self.document())
            .into_iter()
            .enumerate()
            .map(|(position, id)| (id, position))
            .collect();
        events.sort_by(|a, b| {
            a.time
                .total_cmp(&b.time)
                .then_with(|| {
                    order
                        .get(&a.event.0)
                        .unwrap_or(&a.tree_order)
                        .cmp(order.get(&b.event.0).unwrap_or(&b.tree_order))
                })
                .then(pseudo_rank(a.event.4).cmp(&pseudo_rank(b.event.4)))
                .then(a.generation.cmp(&b.generation))
                .then(a.event.1.cmp(b.event.1))
        });
        events.into_iter().map(|e| e.event).collect()
    }
    pub(crate) fn update_css_transitions(&mut self, seconds: f64) {
        self.flush_style_invalidations();
        let diagnostic = casc_diag_on().then(std::time::Instant::now);
        // Pull the transition origin out while computing the after-change
        // style. Interpolated values are never written into author declarations.
        let mut state = std::mem::take(&mut self.transitions);
        let now = seconds.max(state.now);
        state.advance(now);
        let stamp = (
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        let explicitly_invalid = state.all_invalid.replace(false);
        let full = state.epoch.is_none() || state.style_stamp != stamp || explicitly_invalid;
        let invalid = std::mem::take(state.invalid.get_mut());
        #[cfg(test)]
        {
            state.last_recomputed = 0;
            state.last_prepared = 0;
        }
        // Endpoints from a parallel style pass over this very revision.
        let prepared = state
            .prepared
            .get_mut()
            .take()
            .filter(|prepared| prepared.stamp == (stamp.0, stamp.1, self.epoch));
        if let Some(prepared) = &prepared
            && super::parallel_match::verify_enabled()
        {
            let (checked, mismatches) = self.prepared_mismatches(prepared);
            eprintln!("STYLEVERIFY transitions checked={checked} mismatches={mismatches}");
        }
        if state.watch_epoch != Some(self.style_epoch) {
            state.watch |= sheet_watch(self);
            state.watch_epoch = Some(self.style_epoch);
        }
        if full || !invalid.is_empty() || state.epoch != Some(self.epoch) {
            state.generation = state.generation.wrapping_add(1);
            let active = state
                .running
                .keys()
                .map(|(target, _)| *target)
                .collect::<FxHashSet<_>>();
            // CSS Transitions 1 #starting defines observable before/after
            // styles, not a compulsory document walk. The cascade already
            // invalidates every changed selector subject and inherited
            // descendant. Retain other endpoints in place: copying all 20
            // properties after every local mutation made detection O(document).
            let candidates = if full {
                let mut candidates = self
                    .composed_descendants(self.document())
                    .into_iter()
                    .collect::<FxHashSet<_>>();
                // Include formerly rendered nodes to retire detached values
                // and cancel their transitions even after broad invalidation.
                candidates.extend(state.before.keys().map(|&(id, _)| id));
                candidates
            } else {
                invalid
            };
            let vp = Vp {
                w: self.viewport_px.0,
                h: self.viewport_px.1,
            };
            let mut pass = Pass {
                prepared: prepared.as_ref(),
                participation: FxHashMap::default(),
                tree_order: None,
                vp,
                now,
                active,
            };
            for id in candidates {
                if self.tag_name(id).is_none() {
                    continue;
                }
                self.transition_target(&mut state, &mut pass, (id, None));
                // CSS Transitions 1 #starting applies to pseudo-elements too:
                // those generated, and those that had a before-change style.
                for which in PseudoEl::ALL {
                    let target = (id, Some(which));
                    if state.before.contains_key(&target)
                        || self.transition_pseudo_tracked(id, which)
                    {
                        self.transition_target(&mut state, &mut pass, target);
                    }
                }
            }
            state.epoch = Some(self.epoch);
            state.style_stamp = stamp;
            state.release_snapshots();
        }
        state.advance(now);
        let values: FxHashMap<Key, String> = state
            .running
            .iter()
            .map(|(&key, r)| {
                let color = || {
                    target_value(self, key.0, "color")
                        .and_then(|value| values::Color::parse(self, key.0.0, &value))
                };
                (key, r.value(now, &color).css(key.1))
            })
            .collect();
        if state.values != values {
            // A presentation revision, not a DOM mutation: live collections
            // and MutationObservers must not see style interpolation as edits.
            self.layout_presentation_epoch = self.layout_presentation_epoch.wrapping_add(1);
            let changed = state
                .values
                .keys()
                .chain(values.keys())
                .filter(|key| state.values.get(key) != values.get(key))
                .map(|((node, _), _)| *node)
                .collect::<FxHashSet<_>>();
            self.invalidate_transition_layout(&changed.into_iter().collect::<Vec<_>>());
            self.hidden_cache.get_mut().slots.clear();
            self.unattributed_geometry_change();
            self.dirty_attributed = false;
            self.dirty = true;
        }
        state.values = values;
        state.now = now;
        self.transitions = state;
        if let Some(start) = diagnostic
            && start.elapsed().as_millis() > 2
        {
            eprintln!(
                "DIAGTRANS total={}ms nodes={} active={}",
                start.elapsed().as_millis(),
                self.transitions.before.len(),
                self.transitions.running.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(css: &str) -> (Dom, NodeId) {
        let mut dom = Dom::parse_document(&format!(
            "<!doctype html><style>body{{margin:0;display:flow-root}}#box{{height:0px;width:0px;{css}}}.open#box{{height:100px;width:200px;margin-top:20px;padding-left:40px}}</style><div id=box></div>"
        ));
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        dom.update_css_transitions(0.);
        assert!(!dom.css_transitions_active());
        (dom, id)
    }
    fn px(dom: &Dom, id: NodeId, property: &str) -> f32 {
        dom.computed_value_resolved(id, property)
            .unwrap_or_else(|| "0px".into())
            .trim_end_matches("px")
            .parse()
            .unwrap()
    }
    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 0.001, "{a} != {b}");
    }

    /// CSS Transitions 1 #starting with the animation types of Web
    /// Animations 1 #animation-types: colors, shadows, lengths, numbers and
    /// `visibility` transition, and fire their events.
    #[test]
    fn extended_properties_transition_by_animation_type() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box{color:rgb(0,0,0);background-color:transparent;\
             box-shadow:0 0 0 rgb(0,0,0);border-top:0 solid;font-weight:400;\
             transition:color 1s linear,background-color 1s linear,box-shadow 1s linear,\
             border-top-width 1s linear,font-weight 1s linear,visibility 1s linear}\
             #box.on{color:rgb(200,100,0);background-color:rgb(0,0,200);\
             box-shadow:4px 8px 2px rgb(0,0,0);border-top-width:10px;font-weight:700;\
             visibility:hidden}</style><div id=box></div><p id=plain>p</p>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        let plain = dom.get_by_id("plain").unwrap();
        dom.update_css_transitions(0.);
        // Only elements transitioning `EXTENDED` properties keep them.
        assert!(dom.transitions.before_extended.contains_key(&(id, None)));
        assert!(!dom.transitions.before_extended.contains_key(&(plain, None)));
        dom.set_attr(id, "class", "on");
        dom.update_css_transitions(1.);
        let events = dom.take_css_transition_events();
        assert!(
            events
                .iter()
                .any(|(_, property, kind, _, _)| *property == "background-color"
                    && *kind == "transitionrun")
        );
        dom.update_css_transitions(1.5);
        let value = |dom: &Dom, property: &str| dom.computed_value_resolved(id, property);
        assert_eq!(value(&dom, "color").as_deref(), Some("rgb(100, 50, 0)"));
        assert_eq!(
            value(&dom, "background-color").as_deref(),
            Some("rgba(0, 0, 200, 0.5)")
        );
        assert_eq!(
            value(&dom, "box-shadow").as_deref(),
            Some("rgb(0, 0, 0) 2px 4px 1px 0px")
        );
        assert_eq!(value(&dom, "border-top-width").as_deref(), Some("5px"));
        assert_eq!(value(&dom, "font-weight").as_deref(), Some("550"));
        // Web Animations 1 #animating-visibility: visible until the end.
        assert_eq!(value(&dom, "visibility").as_deref(), Some("visible"));
        dom.update_css_transitions(2.);
        assert_eq!(value(&dom, "visibility").as_deref(), Some("hidden"));
        assert_eq!(value(&dom, "color").as_deref(), Some("rgb(200,100,0)"));
        assert!(!dom.css_transitions_active());
        let events = dom.take_css_transition_events();
        for property in ["color", "box-shadow", "visibility", "font-weight"] {
            assert!(
                events
                    .iter()
                    .any(|(_, p, kind, _, _)| *p == property && *kind == "transitionend"),
                "{property}: {events:?}"
            );
        }
    }

    /// CSS Transitions 1 #starting: the before-change style holds every
    /// property's value as of the previous style change event, so a
    /// transition that only the after-change style declares starts from it;
    /// one that only the before-change style declares does not run.
    #[test]
    fn transitions_declared_only_in_the_after_change_style_start() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box,#all{color:rgb(0,0,0);border-top:0 solid}\
             #box.on{color:rgb(200,100,0);transition:color 1s linear}\
             #all.on{color:rgb(0,0,200);border-top-width:8px;transition:all 1s linear}\
             #w{transition:background-color 1s linear}</style>\
             <div id=box></div><div id=all></div><p id=other></p><i id=w></i>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        let all = dom.get_by_id("all").unwrap();
        dom.update_css_transitions(0.);
        assert!(!dom.transitions.before_extended.contains_key(&(id, None)));
        dom.set_attr(id, "class", "on");
        dom.set_attr(all, "class", "on");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        let value = |dom: &Dom, id, property: &str| dom.computed_value_resolved(id, property);
        assert_eq!(value(&dom, id, "color").as_deref(), Some("rgb(100, 50, 0)"));
        assert_eq!(value(&dom, all, "color").as_deref(), Some("rgb(0, 0, 100)"));
        assert_eq!(value(&dom, all, "border-top-width").as_deref(), Some("4px"));
        let events = dom.take_css_transition_events();
        assert!(
            events.iter().any(|(node, p, kind, _, _)| *node == id
                && *p == "color"
                && *kind == "transitionrun")
        );
        // The after-change style declares no transition: no transition.
        dom.update_css_transitions(3.);
        dom.set_attr(id, "class", "");
        dom.update_css_transitions(3.5);
        assert_eq!(value(&dom, id, "color").as_deref(), Some("rgb(0,0,0)"));
        assert!(!dom.css_transitions_active());
        // A script-set inline transition of a property the sheets watch.
        let other = dom.get_by_id("other").unwrap();
        dom.set_attr(
            other,
            "style",
            "background-color:rgb(0,200,0);transition:background-color 1s linear",
        );
        dom.update_css_transitions(4.);
        dom.update_css_transitions(4.5);
        assert_eq!(
            value(&dom, other, "background-color").as_deref(),
            Some("rgba(0, 200, 0, 0.5)")
        );
    }

    /// CSS Transitions 1 #starting applies to pseudo-elements: a `::after`
    /// transition interpolates its own style (with an `EXTENDED` property
    /// whose transition only the after-change style declares), reaches
    /// layout, and fires its events with `pseudoElement`.
    #[test]
    fn pseudo_element_transitions_interpolate_their_own_style() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#a{color:rgb(0,0,0)}\
             #a::after{content:'';display:block;width:0px;color:rgb(0,0,0);\
             transition:width 1s linear}\
             #a.on::after{width:100px;color:rgb(0,0,200);\
             transition:width 1s linear,color 1s linear}\
             #a::marker{color:red}</style><div id=a></div>",
        );
        dom.set_viewport_px(640., 480.);
        let a = dom.get_by_id("a").unwrap();
        dom.update_css_transitions(0.);
        assert!(
            dom.transitions
                .before
                .contains_key(&(a, Some(PseudoEl::After)))
        );
        // Not generated: no endpoints.
        assert!(
            !dom.transitions
                .before
                .contains_key(&(a, Some(PseudoEl::Before)))
        );
        assert!(
            !dom.transitions
                .before
                .contains_key(&(a, Some(PseudoEl::Marker)))
        );
        dom.set_attr(a, "class", "on");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        let after = |dom: &Dom, p: &str| dom.pseudo_layout_value(a, PseudoEl::After, p);
        assert_eq!(after(&dom, "width").as_deref(), Some("50px"));
        assert_eq!(after(&dom, "color").as_deref(), Some("rgb(0, 0, 100)"));
        assert_eq!(dom.computed_value_resolved(a, "width"), None);
        let style =
            crate::layout2::BoxStyle::of_pseudo(&dom, a, PseudoEl::After, Vp { w: 640., h: 480. });
        assert_eq!(style.width.resolve(Some(640.)), Some(50.));
        let events = dom.take_css_transition_events();
        assert!(
            events.contains(&(a, "width", "transitionrun", 0., "::after")),
            "{events:?}"
        );
        assert!(events.iter().all(|event| event.4 == "::after"));
        dom.update_css_transitions(3.);
        assert_eq!(after(&dom, "width").as_deref(), Some("100px"));
        assert!(!dom.css_transitions_active());
    }

    #[test]
    fn backdrop_filter_transitions_like_filter() {
        // Filter Effects 2 #BackdropFilterProperty defers to Filter Effects 1
        // #interpolation-of-filters: `none` becomes the other list at its
        // initial values; a list with a <url> changes discretely.
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box{backdrop-filter:none;             transition:backdrop-filter 1s linear}             #box.on{backdrop-filter:blur(8px) saturate(3)}             #box.url{backdrop-filter:url(#f) blur(2px)}</style><div id=box></div>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        dom.update_css_transitions(0.);
        dom.set_attr(id, "class", "on");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        assert_eq!(
            dom.computed_value_resolved(id, "backdrop-filter")
                .as_deref(),
            Some("blur(4px) saturate(2)")
        );
        dom.update_css_transitions(2.);
        dom.set_attr(id, "class", "url");
        dom.update_css_transitions(2.);
        dom.update_css_transitions(2.2);
        assert_eq!(
            dom.computed_value_resolved(id, "backdrop-filter")
                .as_deref(),
            Some("url(#f) blur(2px)")
        );
    }

    /// CSS Transitions 1 #transitionable: a pair whose animation type is
    /// discrete (mismatched `inset`, `hidden`/`collapse`) changes at once.
    #[test]
    fn discrete_value_pairs_do_not_transition() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box{box-shadow:0 0 4px red;visibility:hidden;\
             transition:all 1s linear}\
             #box.on{box-shadow:inset 0 0 4px red;visibility:collapse}</style><div id=box></div>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        dom.update_css_transitions(0.);
        dom.set_attr(id, "class", "on");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.25);
        assert_eq!(
            dom.computed_value_resolved(id, "box-shadow").as_deref(),
            Some("inset 0 0 4px red")
        );
        assert_eq!(
            dom.computed_value_resolved(id, "visibility").as_deref(),
            Some("collapse")
        );
    }

    /// CSS Transforms 1 #interpolation-of-transforms: mismatched lists whose
    /// remainder holds a percentage translation still transition; the matrix
    /// interpolation waits for the reference box (`layout2::transform`).
    #[test]
    fn mismatched_lists_with_percentages_defer_matrix_interpolation() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box{width:100px;height:100px;\
             transform:translateX(10px) scale(2) translateX(50%);\
             transition:transform 1s linear}\
             #box.on{transform:translateX(20px) rotate(90deg)}</style><div id=box></div>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        dom.update_css_transitions(0.);
        dom.set_attr(id, "class", "on");
        dom.update_css_transitions(1.);
        assert!(dom.css_transitions_active(), "not discrete");
        dom.update_css_transitions(1.25);
        // The common translateX pair interpolates numerically; the rest of
        // each list (padded with an identity translation) is deferred.
        assert_eq!(
            dom.computed_value_resolved(id, "transform").as_deref(),
            Some(
                "translate(12.5px, 0px) -trust-interpolate-matrix(0.25, \
                 scale(2, 2) translate(50%, 0px), rotate(90deg) translate(0px, 0px))"
            )
        );
        // Transitioning back from the deferred value nests it.
        dom.set_attr(id, "class", "");
        dom.update_css_transitions(1.5);
        dom.update_css_transitions(1.6);
        let value = dom.computed_value_resolved(id, "transform").unwrap();
        assert_eq!(
            value.matches("-trust-interpolate-matrix(").count(),
            2,
            "{value}"
        );
        dom.update_css_transitions(10.);
        assert!(!dom.css_transitions_active());
        let value = dom.computed_value_resolved(id, "transform").unwrap();
        assert!(!value.contains("-trust-"), "{value}");
    }

    /// CSS Transforms 1 #interpolation-of-transforms with Transforms 2
    /// primitives: 3D rotations interpolate their angles, matrices and
    /// mismatched remainders decompose.
    #[test]
    fn transform_transitions_cover_3d_functions_and_mismatched_lists() {
        let sample = |from: &str, to: &str, t: f64| {
            let mut dom = Dom::parse_document(&format!(
                "<!doctype html><style>#box{{transform:{from};transition:transform 1s linear}}\
                 #box.on{{transform:{to}}}</style><div id=box></div>"
            ));
            dom.set_viewport_px(640., 480.);
            let id = dom.get_by_id("box").unwrap();
            dom.update_css_transitions(0.);
            dom.set_attr(id, "class", "on");
            dom.update_css_transitions(1.);
            dom.update_css_transitions(1. + t);
            dom.computed_value_resolved(id, "transform").unwrap()
        };
        assert_eq!(
            sample("rotateY(0deg)", "rotateY(180deg)", 1. / 3.),
            "rotate3d(0, 1, 0, 60deg)"
        );
        // translateX and translateZ share translate3d.
        assert_eq!(
            sample("translateX(10px)", "translateZ(20px)", 0.5),
            "translate3d(5px, 0px, 10px)"
        );
        assert_eq!(
            sample("matrix(1, 0, 0, 1, 0, 0)", "matrix(2, 0, 0, 2, 10, 0)", 0.5),
            "matrix(1.5, 0, 0, 1.5, 5, 0)"
        );
        // The rotations interpolate by function; the remainders as matrices.
        assert_eq!(
            sample(
                "rotate(0deg) scale(1) translate(20px)",
                "rotate(270deg) translate(10px) scale(2)",
                0.5
            ),
            "rotate(135deg) matrix(1.5, 0, 0, 1.5, 15, 0)"
        );
        // A remainder with a percentage needs layout's reference box: its
        // matrix interpolation is deferred to layout.
        assert_eq!(
            sample("scale(1) translate(10%)", "translate(10px) scale(2)", 0.5),
            "-trust-interpolate-matrix(0.5, scale(1, 1) translate(10%, 0px), \
             translate(10px, 0px) scale(2, 2))"
        );
    }

    /// `currentcolor` interpolates as the element's used color.
    #[test]
    fn currentcolor_endpoints_use_the_element_color() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#box{color:rgb(0,0,200);border-top-color:currentcolor;\
             transition:border-top-color 1s linear}\
             #box.on{border-top-color:rgb(200,0,0)}</style><div id=box></div>",
        );
        dom.set_viewport_px(640., 480.);
        let id = dom.get_by_id("box").unwrap();
        dom.update_css_transitions(0.);
        dom.set_attr(id, "class", "on");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        assert_eq!(
            dom.computed_value_resolved(id, "border-top-color")
                .as_deref(),
            Some("rgb(100, 0, 100)")
        );
    }

    #[test]
    fn local_transition_detection_does_not_visit_independent_endpoints() {
        // Exact restyle counts: a parallel test changing process-wide layout
        // inputs (the page font epoch) would expire unrelated styles.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        for siblings in [16, 1024] {
            let mut checked = false;
            for _ in 0..16 {
                // Parallel font tests can legitimately invalidate every
                // endpoint. Always check semantic results, and require a
                // complete stable-font transaction for the locality bounds.
                let fonts = crate::font_system::page_font_epoch();
                let mut dom = Dom::parse_document(&format!(
                    "<style>div{{width:10px;transition:width 1s linear}}.wide{{width:30px}}</style><main>{}</main>",
                    (0..siblings)
                        .map(|i| format!("<div id=n{i}>text</div>"))
                        .collect::<String>()
                ));
                let changed = dom.get_by_id("n0").unwrap();
                let stable = dom.get_by_id("n1").unwrap();
                dom.update_css_transitions(0.);
                assert!(dom.transitions.last_recomputed >= siblings);
                let before = dom.transitions.before.get(&(stable, None)).unwrap() as *const Values;
                dom.set_attr(changed, "class", "wide");
                dom.update_css_transitions(1.);
                let changed_work = dom.transitions.last_recomputed;
                let retained =
                    std::ptr::eq(before, dom.transitions.before.get(&(stable, None)).unwrap());
                dom.update_css_transitions(1.5);
                let sampled_work = dom.transitions.last_recomputed;
                close(px(&dom, changed, "width"), 20.);
                close(px(&dom, stable, "width"), 10.);
                dom.set_attr(stable, "data-unrelated", "value");
                dom.update_css_transitions(1.5);
                let unrelated_work = dom.transitions.last_recomputed;
                close(px(&dom, changed, "width"), 20.);
                dom.update_css_transitions(2.);
                assert!(!dom.css_transitions_active());
                if fonts != crate::font_system::page_font_epoch() {
                    continue;
                }
                assert_eq!((changed_work, sampled_work, unrelated_work), (1, 0, 1));
                assert!(retained, "independent endpoint allocation was replaced");
                checked = true;
                break;
            }
            assert!(
                checked,
                "font revisions never settled for the locality check"
            );
        }
    }

    #[test]
    fn selective_transition_endpoints_match_full_style_change_transactions() {
        let html = r#"<style>
            main { width:10px } div { width:inherit; transition:width 1s linear }
            main.wide { width:30px } .hidden { display:none }
        </style><main id=parent><div id=a></div><div id=b></div></main><aside id=other></aside>"#;
        let mut selective = Dom::parse_document(html);
        let mut full = Dom::parse_document(html);
        let a_id = selective.get_by_id("a").unwrap();
        assert_eq!(full.get_by_id("a"), Some(a_id));
        for dom in [&mut selective, &mut full] {
            dom.update_css_transitions(0.);
        }
        for step in 0..10 {
            for dom in [&mut selective, &mut full] {
                let parent = dom.get_by_id("parent").unwrap();
                let a = a_id;
                let b = dom.get_by_id("b").unwrap();
                match step {
                    0 => dom.set_attr(parent, "class", "wide"),
                    1 => dom.set_attr(b, "style", "transition:none"),
                    2 => dom.set_attr(parent, "class", "hidden"),
                    3 => dom.set_attr(parent, "class", "wide"),
                    4 => dom.set_attr(a, "style", "width:50px"),
                    5 => dom.detach(a),
                    6 => dom.append(parent, a),
                    7 => dom.set_attr(a, "style", "width:70px;transition:width .5s linear"),
                    8 => dom.set_attr(a, "style", "width:90px;transition:none"),
                    _ => dom.set_attr(parent, "class", ""),
                }
            }
            full.transitions.invalidate_all();
            for t in [step as f64 + 1., step as f64 + 1.25] {
                selective.update_css_transitions(t);
                full.update_css_transitions(t);
                for id in ["parent", "a", "b", "other"] {
                    let a = if id == "a" {
                        a_id
                    } else {
                        selective.get_by_id(id).unwrap()
                    };
                    let b = if id == "a" {
                        a_id
                    } else {
                        full.get_by_id(id).unwrap()
                    };
                    assert_eq!(
                        selective.transitions.before.get(&(a, None)),
                        full.transitions.before.get(&(b, None)),
                        "step {step}, {id}"
                    );
                    assert_eq!(
                        selective.computed_value_resolved(a, "width"),
                        full.computed_value_resolved(b, "width"),
                        "step {step}, {id}"
                    );
                }
                assert_eq!(
                    selective.take_css_transition_events(),
                    full.take_css_transition_events(),
                    "step {step}"
                );
            }
        }
    }

    #[test]
    fn container_query_changes_are_transition_style_events_without_dom_mutations() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        // CSS Conditional 5 #animated-containers: query and container-unit
        // changes participate in style change events even without DOM edits.
        let mut dom = Dom::parse_document(
            r#"<style>
                main { container-type:inline-size }
                #child { width:10px;transition:width 1s linear }
                @container (width > 150px) { #child { width:20px } }
            </style><main id=parent><div id=child></div></main>"#,
        );
        let parent = dom.get_by_id("parent").unwrap();
        let child = dom.get_by_id("child").unwrap();
        dom.update_container_sizes([(parent, [100., 50.])].into_iter().collect());
        dom.update_css_transitions(0.);
        let epoch = dom.epoch();
        assert!(dom.update_container_sizes([(parent, [200., 50.])].into_iter().collect()));
        assert_eq!(dom.epoch(), epoch);
        dom.update_css_transitions(1.);
        assert_eq!(dom.transitions.last_recomputed, 1);
        dom.update_css_transitions(1.5);
        close(px(&dom, child, "width"), 15.);
        assert!(dom.update_container_sizes([(parent, [100., 50.])].into_iter().collect()));
        dom.update_css_transitions(1.5);
        dom.update_css_transitions(1.75);
        close(px(&dom, child, "width"), 12.5);
    }
    #[test]
    fn hover_transform_transitions_pad_none_and_reverse_without_style_mutation() {
        let mut dom = Dom::parse_document(
            r#"<!doctype html><style>
            #menu{width:200px;height:100px;visibility:hidden;transform:scaleY(.75);transform-origin:center top;transition:transform .2s linear}
            #host:hover #menu{visibility:visible;transform:none}
            </style><div id=host><div id=menu>Menu</div></div>"#,
        );
        dom.set_viewport_px(800., 600.);
        let host = dom.get_by_id("host").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        dom.update_css_transitions(0.);
        dom.set_hover_chain(Some(host));
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.1);
        assert_eq!(
            dom.computed_value_resolved(menu, "transform").as_deref(),
            Some("scale(1, 0.875)")
        );
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &url::Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        assert!(layout.paint.primitives.iter().any(|p| matches!(p,crate::render::DisplayCommand::PushTransform(m) if (m.0[3]-0.875).abs()<0.001)));
        dom.set_hover_chain(None);
        dom.update_css_transitions(1.1);
        dom.update_css_transitions(1.15);
        assert_eq!(
            dom.computed_value_resolved(menu, "transform").as_deref(),
            Some("scale(1, 0.8125)")
        );
        dom.update_css_transitions(1.21);
        assert!(!dom.css_transitions_active());
        assert!(dom.attr(menu, "style").is_none());
    }

    #[test]
    fn transform_transition_preserves_percentage_translation_and_rotation_turns() {
        let (mut dom, id) = setup(
            "transform:translateY(calc(100% - 58px)) rotate(0deg);transition:transform 1s linear",
        );
        dom.set_attr(
            id,
            "style",
            "transform:translate(20px,0px) rotate(720deg) scaleX(2)",
        );
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        assert_eq!(
            dom.computed_value_resolved(id, "transform").as_deref(),
            Some("translate(10px, calc(50% + -29px)) rotate(360deg) scale(1.5, 1)")
        );
        dom.update_css_transitions(2.);
        assert!(!dom.css_transitions_active());
    }
    #[test]
    fn transition_box_lengths_change_layout_and_finish_without_dom_mutation() {
        let (mut dom, id) =
            setup("transition:height 1s linear, margin 1s linear, padding 1s linear");
        dom.set_attr(id, "class", "open");
        dom.update_css_transitions(1.);
        let epoch = dom.epoch();
        close(px(&dom, id, "height"), 0.);
        dom.update_css_transitions(1.5);
        close(px(&dom, id, "height"), 50.);
        close(px(&dom, id, "margin-top"), 10.);
        close(px(&dom, id, "padding-left"), 20.);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &url::Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(640., 480.),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        close(layout.boxes[&id].height as f32, 50.);
        close(layout.boxes[&id].top as f32, 10.);
        assert_eq!(dom.epoch(), epoch);
        dom.update_css_transitions(2.);
        close(px(&dom, id, "height"), 100.);
        assert!(!dom.css_transitions_active());
        dom.set_attr(id, "data-unrelated", "x");
        dom.update_css_transitions(3.);
        assert!(!dom.css_transitions_active());
        let events = dom
            .take_css_transition_events()
            .into_iter()
            .filter(|e| e.1 == "height")
            .map(|e| (e.2, e.3))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                ("transitionrun", 0.),
                ("transitionstart", 0.),
                ("transitionend", 1.)
            ]
        );
    }
    #[test]
    fn transition_reversal_shortens_duration_and_retargets_from_current_value() {
        let (mut dom, id) = setup("transition:height 1s linear");
        dom.set_attr(id, "class", "open");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.4);
        close(px(&dom, id, "height"), 40.);
        dom.set_attr(id, "class", "");
        dom.update_css_transitions(1.4);
        close(px(&dom, id, "height"), 40.);
        dom.update_css_transitions(1.6);
        close(px(&dom, id, "height"), 20.);
        dom.update_css_transitions(1.81);
        assert!(!dom.css_transitions_active());
        close(px(&dom, id, "height"), 0.);
        dom.set_attr(id, "style", "height:80px");
        dom.update_css_transitions(2.);
        dom.update_css_transitions(2.5);
        close(px(&dom, id, "height"), 40.);
        dom.set_attr(id, "style", "height:200px");
        dom.update_css_transitions(2.5);
        dom.update_css_transitions(3.);
        close(px(&dom, id, "height"), 120.);
    }
    #[test]
    fn transition_matching_delays_cancellation_and_new_display() {
        let (mut dom, id) = setup(
            "transition-property:all,unknown,height;transition-duration:2s,7s,1s;transition-delay:-0.25s;transition-timing-function:linear",
        );
        dom.set_attr(id, "class", "open");
        dom.update_css_transitions(1.);
        close(px(&dom, id, "height"), 25.);
        close(px(&dom, id, "width"), 25.);
        dom.set_attr(id, "style", "transition-duration:10s");
        dom.update_css_transitions(1.25);
        close(px(&dom, id, "height"), 50.);
        dom.set_attr(id, "style", "transition-property:none");
        dom.update_css_transitions(1.3);
        assert!(!dom.css_transitions_active());
        close(px(&dom, id, "height"), 100.);
        assert!(
            dom.take_css_transition_events()
                .iter()
                .any(|e| e.2 == "transitioncancel")
        );
        dom.set_attr(id, "style", "display:none");
        dom.update_css_transitions(2.);
        dom.set_attr(id, "style", "height:50px;transition:height 1s");
        dom.update_css_transitions(3.);
        assert!(!dom.css_transitions_active());
        close(px(&dom, id, "height"), 50.);
        dom.set_attr(id, "style", "height:100px;transition:height 0s linear 1s");
        dom.update_css_transitions(4.);
        close(px(&dom, id, "height"), 50.);
        assert!(dom.css_transitions_active());
        dom.update_css_transitions(5.);
        close(px(&dom, id, "height"), 100.);
        assert!(!dom.css_transitions_active());
    }
    #[test]
    fn transition_inheritance_clipping_and_dependency_invalidation() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>#parent{height:20px;overflow:hidden;transition:height 1s linear}#child{height:inherit}</style><div id=parent><div id=child></div></div>",
        );
        let parent = dom.get_by_id("parent").unwrap();
        let child = dom.get_by_id("child").unwrap();
        dom.update_css_transitions(0.);
        dom.set_attr(parent, "style", "height:0px");
        dom.update_css_transitions(1.);
        assert!(!dom.is_hidden(parent));
        dom.update_css_transitions(1.5);
        close(px(&dom, parent, "height"), 10.);
        close(px(&dom, child, "height"), 10.);
        assert!(!dom.is_hidden(parent));
        dom.set_attr(child, "data-unrelated", "x");
        dom.update_css_transitions(1.75);
        close(px(&dom, parent, "height"), 5.);
        close(px(&dom, child, "height"), 5.);
        dom.update_css_transitions(2.);
        assert!(
            !dom.is_hidden(parent),
            "a fully clipped box is still generated"
        );
        close(px(&dom, child, "height"), 0.);
        dom.set_attr(parent, "style", "height:40px");
        dom.update_css_transitions(3.);
        dom.update_css_transitions(3.5);
        assert!(!dom.is_hidden(parent));
        close(px(&dom, child, "height"), 20.);
        dom.detach(parent);
        dom.update_css_transitions(3.6);
        assert!(!dom.css_transitions_active());
        assert!(
            dom.take_css_transition_events()
                .iter()
                .any(|e| e.2 == "transitioncancel")
        );
    }

    #[test]
    fn transition_events_sort_by_time_tree_order_and_property() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>div{width:0px;height:0px;transition:width .1s linear,height .2s linear}.open{width:100px;height:100px}</style><main><div id=a></div><div id=b></div></main>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let parent = dom.nodes[a].parent.unwrap();
        // Tree order must not be confused with allocation order.
        dom.insert_before(parent, b, Some(a));
        dom.update_css_transitions(0.);
        dom.set_attr(a, "class", "open");
        dom.set_attr(b, "class", "open");
        dom.update_css_transitions(1.);
        let events = dom
            .take_css_transition_events()
            .into_iter()
            .map(|e| (e.0, e.1, e.2))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                (b, "height", "transitionrun"),
                (b, "height", "transitionstart"),
                (b, "width", "transitionrun"),
                (b, "width", "transitionstart"),
                (a, "height", "transitionrun"),
                (a, "height", "transitionstart"),
                (a, "width", "transitionrun"),
                (a, "width", "transitionstart"),
            ]
        );
        // One delayed rendering opportunity crosses both end times. The
        // shorter width transitions finish first across both elements.
        dom.update_css_transitions(2.);
        let events = dom
            .take_css_transition_events()
            .into_iter()
            .map(|e| (e.0, e.1, e.2))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                (b, "width", "transitionend"),
                (a, "width", "transitionend"),
                (b, "height", "transitionend"),
                (a, "height", "transitionend"),
            ]
        );
    }

    #[test]
    fn transition_percentages_shorthands_and_easing() {
        let (mut dom, id) = setup("width:10%;transition:width 1s linear");
        dom.set_attr(id, "style", "width:calc(30% + 20px)");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        let value = dom.computed_value_resolved(id, "width").unwrap();
        let length = Len::parse(
            &value,
            crate::layout2::Units::of(&dom, id),
            Vp { w: 640., h: 480. },
        )
        .unwrap();
        close(length.resolve(Some(100.)).unwrap(), 30.);
        close(length.resolve(Some(200.)).unwrap(), 50.);
        let parsed = expand("height 0.3s ease-in-out, margin-top 1s linear -100ms");
        assert_eq!(parsed[0].1, "height, margin-top");
        assert_eq!(parsed[3].1, "0s, -100ms");
        for invalid in ["height -1s", "none 1s, height 2s", "height 1s 2s 3s", ""] {
            assert!(expand(invalid).is_empty(), "{invalid}");
        }
        close(Easing::parse("ease-in-out").unwrap().sample(0.5), 0.5);
        close(Easing::parse("steps(4,end)").unwrap().sample(0.49), 0.25);
        close(
            Easing::parse("steps(4,jump-none)").unwrap().sample(0.5),
            2. / 3.,
        );
        assert!(
            Easing::parse("cubic-bezier(0.5,2,0.5,2)")
                .unwrap()
                .sample(0.5)
                > 1.
        );
    }
}
