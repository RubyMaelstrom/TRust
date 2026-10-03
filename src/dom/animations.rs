//! The CSS Animations origin for properties that the graphical compositor
//! path does not sample. CSS Animations 1 #animations, #keyframes,
//! #animation-definition and #events; CSS Animations 2 #event-dispatch; Web
//! Animations 1 #animation-effect-phases-and-states, #calculating-the-active-time,
//! #calculating-the-simple-iteration-progress and
//! #the-effect-value-of-a-keyframe-animation-effect; CSS Cascade 5
//! #cascade-origin. CSSWG snapshot 81c27f686901 (2026-09-06).
//!
//! `top`, `transform` and `opacity` keep the retained-display-list path
//! (`layout2::graphics::paint_animation_scope`), sampled by the frontend
//! renderer without the page actor. Every other animated property is sampled
//! here, on the page actor's document timeline, into an animation-origin
//! value that `computed_value` returns ahead of normal author declarations
//! but behind `!important` ones and transitions. Like Gecko's animation-only
//! restyle, a frame never re-runs selector matching: it re-samples the
//! retained keyframes and evicts only the changed properties of the animated
//! elements and their inheriting descendants. Paint-only changes repaint the
//! retained fragments; anything else takes the ordinary relayout path.
use super::*;
use crate::layout2::value::Vp;

mod values;
use transitions::Easing;
use values::{Color, Kind, Value};

/// Properties sampled by the graphical compositor path.
const COMPOSITOR: [&str; 3] = ["top", "transform", "opacity"];

/// Whether an `@keyframes` declaration of `property` is retained. CSS
/// Animations 1 #keyframes ignores the animation properties (except
/// `animation-timing-function`, which the parser keeps separately).
pub(super) fn keyframe_property(property: &str) -> bool {
    !(property.starts_with("animation")
        || property.starts_with("transition")
        || property.starts_with("--"))
}

/// Properties whose animation the style origin applies. Web Animations
/// #animating-properties: `display`, `direction`, `unicode-bidi` and the
/// animation/transition properties are not animatable.
fn sampled_property(property: &str) -> bool {
    keyframe_property(property)
        && !COMPOSITOR.contains(&property)
        && !matches!(
            property,
            "display"
                | "direction"
                | "unicode-bidi"
                | "will-change"
                // A compatibility alias of the -x/-y longhands, which paint
                // prefers once they are set.
                | "background-position"
        )
}

/// Properties whose changes only repaint: the painter reads them from the
/// canonical DOM, while retained fragments, box-tree construction and the
/// terminal box snapshot do not depend on them (`layout2::repaint_graphical`).
fn paint_only(property: &str) -> bool {
    matches!(
        property,
        "color"
            | "background-color"
            | "border-top-color"
            | "border-right-color"
            | "border-bottom-color"
            | "border-left-color"
            | "outline-color"
            | "text-decoration-color"
            | "caret-color"
            | "-webkit-text-fill-color"
            | "-webkit-text-stroke-color"
            | "text-shadow"
            | "box-shadow"
            | "background-position-x"
            | "background-position-y"
    )
}

/// Paint-only properties that SVG serialization also consumes (inline SVG
/// is rasterized from computed styles while the box tree is built).
fn svg_dependent(property: &str) -> bool {
    matches!(property, "color" | "caret-color")
}

pub(crate) type AnimationEvent = (NodeId, String, &'static str, f64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    None,
    Forwards,
    Backwards,
    Both,
}

impl Fill {
    fn backwards(self) -> bool {
        matches!(self, Self::Backwards | Self::Both)
    }
    fn forwards(self) -> bool {
        matches!(self, Self::Forwards | Self::Both)
    }
}

/// Web Animations #animation-effect-phases-and-states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Before,
    Active,
    After,
}

/// One coordinated `animation-*` list item (CSS Animations 1
/// #animation-definition, CSS Values 4 #linked-properties).
#[derive(Clone, Copy, Debug)]
struct Timing {
    duration: f64,
    delay: f64,
    /// `f64::INFINITY` for `infinite`.
    iterations: f64,
    direction: Direction,
    fill: Fill,
    easing: Easing,
    running: bool,
}

impl Timing {
    /// CSS Animations 1 #active-duration.
    fn active_duration(&self) -> f64 {
        if self.duration == 0.0 || self.iterations == 0.0 {
            0.0
        } else {
            self.duration * self.iterations
        }
    }

    /// The effect's end time (Web Animations #animation-effect-phases-and-states),
    /// with no end delay.
    fn end(&self) -> f64 {
        (self.delay + self.active_duration()).max(0.0)
    }

    /// Web Animations #animation-effect-phases-and-states, playing forwards.
    fn phase(&self, local: f64) -> Phase {
        let before_active = self.delay.min(self.end()).max(0.0);
        let active_after = (self.delay + self.active_duration())
            .min(self.end())
            .max(0.0);
        if local < before_active {
            Phase::Before
        } else if local >= active_after {
            Phase::After
        } else {
            Phase::Active
        }
    }

    /// Web Animations #calculating-the-active-time.
    fn active_time(&self, local: f64, phase: Phase) -> Option<f64> {
        match phase {
            Phase::Before => self.fill.backwards().then(|| (local - self.delay).max(0.0)),
            Phase::Active => Some(local - self.delay),
            Phase::After => self
                .fill
                .forwards()
                .then(|| (local - self.delay).min(self.active_duration()).max(0.0)),
            Phase::Idle => None,
        }
    }

    /// The directed iteration progress and current iteration at `local`
    /// (Web Animations #calculating-the-simple-iteration-progress,
    /// #calculating-the-current-iteration and
    /// #calculating-the-directed-progress), or `None` when the animation has
    /// no effect. CSS animations ease each keyframe interval instead of the
    /// iteration, so the effect's own easing is linear.
    fn progress(&self, local: f64) -> Option<(f64, f64)> {
        let phase = self.phase(local);
        let active = self.active_time(local, phase)?;
        let overall = if self.duration == 0.0 {
            if phase == Phase::Before {
                0.0
            } else {
                self.iterations
            }
        } else {
            active / self.duration
        };
        let mut simple = if overall.is_infinite() {
            0.0
        } else {
            overall.fract()
        };
        if simple == 0.0
            && matches!(phase, Phase::Active | Phase::After)
            && active == self.active_duration()
            && self.iterations != 0.0
        {
            simple = 1.0;
        }
        let iteration = if phase == Phase::After && self.iterations.is_infinite() {
            f64::INFINITY
        } else if simple == 1.0 {
            overall.floor() - 1.0
        } else {
            overall.floor()
        };
        let mut index = iteration;
        if self.direction == Direction::AlternateReverse {
            index += 1.0;
        }
        let forwards = match self.direction {
            Direction::Normal => true,
            Direction::Reverse => false,
            Direction::Alternate | Direction::AlternateReverse => {
                index.is_infinite() || index % 2.0 == 0.0
            }
        };
        Some((if forwards { simple } else { 1.0 - simple }, iteration))
    }

    /// CSS Animations 2 #event-dispatch: interval start and end.
    fn interval_start(&self) -> f64 {
        (-self.delay).min(self.active_duration()).max(0.0)
    }
    fn interval_end(&self) -> f64 {
        (self.end() - self.delay)
            .min(self.active_duration())
            .max(0.0)
    }
}

/// A `<single-animation>` from the `animation` shorthand. CSS Animations 1
/// #animation: the first `<time>` is the duration and the second the delay;
/// a keyword valid for another longhand not yet seen is taken by that
/// longhand rather than by `animation-name`.
#[derive(Default)]
struct Segment {
    name: Option<String>,
    duration: Option<f64>,
    delay: Option<f64>,
    easing: Option<String>,
    iterations: Option<String>,
    direction: Option<String>,
    fill: Option<String>,
    state: Option<String>,
}

impl Segment {
    fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for token in split_top_level_ws(text.trim()) {
            let lower = token.to_ascii_lowercase();
            if let Some(time) = time(&lower) {
                if out.duration.is_none() {
                    out.duration = Some(time);
                } else if out.delay.is_none() {
                    out.delay = Some(time);
                }
                continue;
            }
            match lower.as_str() {
                _ if out.easing.is_none() && Easing::parse(&lower).is_some() => {
                    out.easing = Some(lower)
                }
                "infinite" if out.iterations.is_none() => out.iterations = Some(lower),
                _ if out.iterations.is_none()
                    && lower
                        .parse::<f64>()
                        .is_ok_and(|n| n.is_finite() && n >= 0.0) =>
                {
                    out.iterations = Some(lower)
                }
                "normal" | "reverse" | "alternate" | "alternate-reverse"
                    if out.direction.is_none() =>
                {
                    out.direction = Some(lower)
                }
                "none" | "forwards" | "backwards" | "both" if out.fill.is_none() => {
                    out.fill = Some(lower)
                }
                "running" | "paused" if out.state.is_none() => out.state = Some(lower),
                _ if out.name.is_none() => out.name = Some(token.to_string()),
                _ => {}
            }
        }
        out
    }
}

fn time(value: &str) -> Option<f64> {
    let value = value.trim().to_ascii_lowercase();
    let (number, scale) = if let Some(number) = value.strip_suffix("ms") {
        (number, 0.001)
    } else {
        (value.strip_suffix('s')?, 1.0)
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .map(|number| number * scale)
}

/// The keyframe name: a `<custom-ident>` or `<string>` (CSS Animations 1
/// #typedef-keyframes-name), compared case-sensitively.
fn keyframes_name(value: &str) -> Option<String> {
    let value = value.trim();
    let name = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    (!name.is_empty() && !value.eq_ignore_ascii_case("none")).then(|| name.to_string())
}

struct Spec {
    name: String,
    timing: Timing,
}

#[derive(Clone, Debug)]
struct Frame {
    offset: f64,
    /// `None` is an implicit 0%/100% keyframe holding the underlying value.
    value: Option<Value>,
    easing: Option<Easing>,
}

#[derive(Clone, Debug)]
struct Track {
    /// `PROPS` index.
    property: u16,
    kind: Kind,
    frames: Vec<Frame>,
    underlying: Value,
}

impl Track {
    /// Web Animations #the-effect-value-of-a-keyframe-animation-effect: the
    /// interval endpoints and the eased interval distance at `progress`.
    fn interval(&self, progress: f64, default: Easing) -> (&Frame, &Frame, f32) {
        let frames = &self.frames;
        let zero = frames.iter().filter(|frame| frame.offset == 0.0).count();
        let one = frames.iter().filter(|frame| frame.offset == 1.0).count();
        if progress < 0.0 && zero > 1 {
            return (&frames[0], &frames[0], 0.0);
        }
        if progress >= 1.0 && one > 1 {
            let last = frames.last().expect("tracks have keyframes");
            return (last, last, 0.0);
        }
        let start = frames
            .iter()
            .rposition(|frame| frame.offset <= progress && frame.offset < 1.0)
            .unwrap_or_else(|| {
                frames
                    .iter()
                    .rposition(|frame| frame.offset == 0.0)
                    .unwrap_or(0)
            });
        let end = (start + 1).min(frames.len() - 1);
        let (a, b) = (&frames[start], &frames[end]);
        if start == end {
            return (a, a, 0.0);
        }
        let distance = (progress - a.offset) / (b.offset - a.offset);
        let eased = a.easing.unwrap_or(default).sample(distance as f32);
        (a, b, eased)
    }
}

struct Animation {
    name: String,
    timing: Timing,
    /// Timeline time the current time is measured from while running.
    start: f64,
    /// CSS Animations 1 #animation-play-state: the held current time.
    hold: Option<f64>,
    phase: Phase,
    iteration: f64,
    tracks: Vec<Track>,
}

impl Animation {
    fn current(&self, now: f64) -> f64 {
        self.hold.unwrap_or(now - self.start)
    }

    fn track(&self, property: u16) -> Option<&Track> {
        self.tracks.iter().find(|track| track.property == property)
    }
}

struct Element {
    animations: Vec<Animation>,
    /// The element's tree position when its animations were (re)built, for
    /// event order (Web Animations #animation-frame-loop).
    tree_order: usize,
    /// An inline SVG in the subtree consumes inherited colors at layout.
    svg: bool,
    /// The DOM revision `svg` describes; insertions below the element do not
    /// restyle it.
    svg_epoch: u64,
}

struct PendingEvent {
    event: AnimationEvent,
    time: f64,
    tree_order: usize,
    index: usize,
}

#[derive(Default)]
pub(super) struct State {
    elements: FxHashMap<NodeId, Element>,
    /// Animation-origin values by element, then `PROPS` index.
    values: FxHashMap<NodeId, Vec<(u16, String)>>,
    /// Animated elements compute their own (unshared) style rows.
    private: FxHashSet<NodeId>,
    invalid: RefCell<FxHashSet<NodeId>>,
    all_invalid: Cell<bool>,
    /// Whether the last scan saw `@keyframes` rules; without them local
    /// style changes cannot start an animation.
    keyframes: Cell<bool>,
    stamp: Option<(u64, u64, u64)>,
    now: f64,
    events: Vec<PendingEvent>,
    /// The element whose underlying (non-animated) values are being read.
    excluded: Cell<Option<NodeId>>,
    /// Changes of the animation set, for frame scheduling caches.
    generation: u64,
    /// Animation-origin changes that only need the retained paint redone.
    paint_pending: bool,
    /// Animation-origin changes that need style/layout again (a presentation
    /// revision, not a DOM mutation, so the DOM's dirty bit stays clear).
    layout_pending: bool,
    #[cfg(test)]
    pub(super) last_layout_changes: usize,
    #[cfg(test)]
    pub(super) last_paint_changes: usize,
}

impl State {
    pub(super) fn invalidate(&self, id: NodeId) {
        if self.keyframes.get() || !self.elements.is_empty() {
            self.invalid.borrow_mut().insert(id);
        }
    }

    pub(super) fn invalidate_all(&self) {
        self.all_invalid.set(true);
        self.invalid.borrow_mut().clear();
    }

    /// The animation-origin value of `name` on `id`, if any.
    pub(super) fn value(&self, id: NodeId, name: &str) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        let values = self.values.get(&id)?;
        if self.excluded.get() == Some(id) {
            return None;
        }
        let index = prop_index(name)? as u16;
        values
            .iter()
            .find(|(property, _)| *property == index)
            .map(|(_, value)| value.clone())
    }

    /// Elements with animation-origin values or private rows.
    pub(super) fn origin_elements(&self) -> FxHashSet<NodeId> {
        self.values
            .keys()
            .chain(self.private.iter())
            .copied()
            .collect()
    }

    /// Whether `id` must compute an unshared style row: its animated values
    /// change independently of elements with an equal cascade.
    pub(super) fn private_row(&self, id: NodeId) -> bool {
        !self.private.is_empty() && self.private.contains(&id)
    }

    pub(super) fn remove_node(&mut self, node: NodeId) {
        self.invalid.get_mut().remove(&node);
        self.elements.remove(&node);
        self.values.remove(&node);
        self.private.remove(&node);
    }

    pub(super) fn visit_gc_roots(&self, visit: &mut dyn FnMut(NodeId)) {
        // Pending events target their elements until dispatched.
        for event in &self.events {
            visit(event.event.0);
        }
    }

    pub(super) fn retain_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) {
        self.invalid.get_mut().retain(|&id| live(id));
        self.elements.retain(|&id, _| live(id));
        self.values.retain(|&id, _| live(id));
        self.private.retain(|&id| live(id));
        self.events.retain(|event| live(event.event.0));
    }

    pub(super) fn retained_bytes(&self) -> usize {
        let value_bytes = |value: &Value| match value {
            Value::Shadows(shadows) => shadows.capacity() * std::mem::size_of::<values::Shadow>(),
            Value::Filters(filters) => filters.capacity() * std::mem::size_of::<values::Filter>(),
            Value::Positions(positions) => positions.capacity() * 8,
            Value::Other(text) => text.capacity(),
            _ => 0,
        };
        self.elements.capacity() * std::mem::size_of::<(NodeId, Element)>()
            + self
                .elements
                .values()
                .flat_map(|element| &element.animations)
                .map(|animation| {
                    std::mem::size_of::<Animation>()
                        + animation.name.capacity()
                        + animation
                            .tracks
                            .iter()
                            .map(|track| {
                                std::mem::size_of::<Track>()
                                    + value_bytes(&track.underlying)
                                    + track
                                        .frames
                                        .iter()
                                        .map(|frame| {
                                            std::mem::size_of::<Frame>()
                                                + frame.value.as_ref().map_or(0, value_bytes)
                                        })
                                        .sum::<usize>()
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self.values.capacity() * std::mem::size_of::<(NodeId, Vec<(u16, String)>)>()
            + self
                .values
                .values()
                .flatten()
                .map(|(_, value)| std::mem::size_of::<(u16, String)>() + value.capacity())
                .sum::<usize>()
            + self.private.capacity() * std::mem::size_of::<NodeId>()
            + self.invalid.borrow().capacity() * std::mem::size_of::<NodeId>()
            + self.events.capacity() * std::mem::size_of::<PendingEvent>()
    }
}

/// What a frame requires of the presentation for one animated element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameTarget {
    pub node: NodeId,
    /// Every sampled property only repaints (Gecko's
    /// `KeyframeEffect::CanIgnoreIfNotVisible`): the animation may be
    /// throttled while it cannot be seen.
    pub paint_only: bool,
}

impl Dom {
    /// CSS Animations 1 #animation-name: the coordinated animation list,
    /// with shorter longhand lists repeated (CSS Values 4 #linked-properties).
    fn animation_specs(&self, id: NodeId) -> Vec<Spec> {
        let read = |property: &str| {
            self.cascaded(id, property)
                .map(|value| self.resolve_vars_owned(id, value))
        };
        let shorthand = read("animation");
        let names = read("animation-name");
        if shorthand.is_none() && names.is_none() {
            return Vec::new();
        }
        let segments = shorthand
            .as_deref()
            .map(|value| {
                split_top_level_commas(value)
                    .into_iter()
                    .map(Segment::parse)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let list = |property: &str| {
            read(property)
                .map(|value| {
                    split_top_level_commas(&value)
                        .into_iter()
                        .map(|item| item.trim().to_ascii_lowercase())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let names: Vec<Option<String>> = match names {
            Some(names) => split_top_level_commas(&names)
                .into_iter()
                .map(keyframes_name)
                .collect(),
            None => segments
                .iter()
                .map(|segment| segment.name.as_deref().and_then(keyframes_name))
                .collect(),
        };
        let durations = list("animation-duration");
        let delays = list("animation-delay");
        let easings = list("animation-timing-function");
        let iterations = list("animation-iteration-count");
        let directions = list("animation-direction");
        let fills = list("animation-fill-mode");
        let states = list("animation-play-state");
        let item = |values: &[String], index: usize| {
            (!values.is_empty()).then(|| values[index % values.len()].clone())
        };
        names
            .into_iter()
            .enumerate()
            .filter_map(|(index, name)| {
                let name = name?;
                let segment = segments.get(index % segments.len().max(1));
                let duration = item(&durations, index)
                    .and_then(|value| time(&value))
                    .or_else(|| segment.and_then(|segment| segment.duration))
                    .unwrap_or(0.0)
                    .max(0.0);
                let delay = item(&delays, index)
                    .and_then(|value| time(&value))
                    .or_else(|| segment.and_then(|segment| segment.delay))
                    .unwrap_or(0.0);
                let easing = item(&easings, index)
                    .or_else(|| segment.and_then(|segment| segment.easing.clone()))
                    .and_then(|value| Easing::parse(&value))
                    .unwrap_or(Easing::Bezier(0.25, 0.1, 0.25, 1.0));
                let iterations = item(&iterations, index)
                    .or_else(|| segment.and_then(|segment| segment.iterations.clone()))
                    .and_then(|value| {
                        if value == "infinite" {
                            Some(f64::INFINITY)
                        } else {
                            value
                                .parse::<f64>()
                                .ok()
                                .filter(|n| n.is_finite() && *n >= 0.0)
                        }
                    })
                    .unwrap_or(1.0);
                let direction = match item(&directions, index)
                    .or_else(|| segment.and_then(|segment| segment.direction.clone()))
                    .as_deref()
                {
                    Some("reverse") => Direction::Reverse,
                    Some("alternate") => Direction::Alternate,
                    Some("alternate-reverse") => Direction::AlternateReverse,
                    _ => Direction::Normal,
                };
                let fill = match item(&fills, index)
                    .or_else(|| segment.and_then(|segment| segment.fill.clone()))
                    .as_deref()
                {
                    Some("forwards") => Fill::Forwards,
                    Some("backwards") => Fill::Backwards,
                    Some("both") => Fill::Both,
                    _ => Fill::None,
                };
                let running = item(&states, index)
                    .or_else(|| segment.and_then(|segment| segment.state.clone()))
                    .is_none_or(|state| state != "paused");
                Some(Spec {
                    name,
                    timing: Timing {
                        duration,
                        delay,
                        iterations,
                        direction,
                        fill,
                        easing,
                        running,
                    },
                })
            })
            .collect()
    }

    /// CSS Cascade 5 #cascade-origin: important author (and inline)
    /// declarations outrank the animation origin.
    fn important_properties(&self, id: NodeId) -> FxHashSet<String> {
        let mut out = FxHashSet::default();
        if let Some(style) = self.attr(id, "style") {
            let parsed;
            let declarations = match self.cssom_inline.get(&id) {
                Some(declarations) => declarations,
                None => {
                    parsed = parse_declaration_block(style, self.in_quirks_mode(id));
                    &parsed
                }
            };
            for (name, value, important) in declarations {
                if *important {
                    out.extend(
                        expand_box_shorthand(name, value)
                            .into_iter()
                            .map(|(name, _)| name),
                    );
                }
            }
        }
        let index = self.style_index();
        if let Some(rules) = index.scopes.get(&self.tree_scope(id)) {
            for &rule in self.matched_rules(id).iter() {
                let rule = &rules[rule as usize];
                if rule_pseudo(rule).is_some() {
                    continue;
                }
                for (name, (important, _)) in &rule.decls {
                    if *important {
                        out.insert(name.clone());
                    }
                }
            }
        }
        out
    }

    /// The computed value of a keyframe declaration on `id` (CSS Animations
    /// 1 #keyframes: keyframe values compute in the element's context),
    /// with `var()` substituted and CSS-wide keywords resolved. `None` means
    /// the underlying value.
    fn keyframe_value(&self, id: NodeId, property: &str, raw: &str) -> Option<String> {
        let raw = if pending_shorthand(raw).is_some() {
            self.resolve_pending_shorthand(id, property, raw)
        } else {
            Some(raw.to_owned())
        };
        // CSS Variables 1 #invalid-at-computed-value-time: an invalid
        // substitution computes as `unset`.
        let value = raw
            .and_then(|raw| {
                if needs_var_substitution(&raw) {
                    self.substitute_vars(id, &raw, &mut Vec::new())
                } else {
                    Some(raw)
                }
            })
            .unwrap_or_else(|| "unset".into());
        let inherited = prop_index(property).is_some_and(|index| PROPS[index].inherited);
        let parent = || {
            self.style_parent(id)
                .and_then(|parent| self.computed_value_resolved(parent, property))
                .or_else(|| initial_value(property).map(str::to_owned))
        };
        match wide_keyword(&value) {
            Some(WideKeyword::Inherit) => parent(),
            Some(WideKeyword::Initial) => initial_value(property).map(str::to_owned),
            Some(WideKeyword::Unset) if inherited => parent(),
            Some(WideKeyword::Unset) => initial_value(property).map(str::to_owned),
            Some(WideKeyword::Revert) => None,
            // CSS Color 4 #currentcolor-color: `color: currentcolor` is
            // `color: inherit`.
            None if property == "color" && value.trim().eq_ignore_ascii_case("currentcolor") => {
                parent()
            }
            // CSS Fonts 4 #font-size-prop: relative sizes compute against the
            // parent's font size, never the element's own (animated) one.
            None if property == "font-size" => Some(self.font_size_keyframe(id, &value)),
            None => Some(value),
        }
    }

    fn font_size_keyframe(&self, id: NodeId, value: &str) -> String {
        let parent = self
            .style_parent(id)
            .filter(|&parent| parent != DOCUMENT)
            .map_or(FONT_SIZE_INITIAL, |parent| self.font_px(parent));
        let root = self
            .style_scope_root_element(id)
            .filter(|&root| root != id)
            .map_or(FONT_SIZE_INITIAL, |root| self.font_px(root));
        font_size_px_at(value, parent, root, self.viewport_px)
            .map_or_else(|| value.to_owned(), |px| format!("{px}px"))
    }

    /// The element's computed value without its own animations.
    fn underlying_value(&self, id: NodeId, property: &str) -> String {
        let previous = self.animations.excluded.replace(Some(id));
        let value = if property == "font-size" {
            Some(format!("{}px", self.font_px(id)))
        } else {
            self.computed_value_resolved(id, property)
        };
        self.animations.excluded.set(previous);
        value
            .or_else(|| initial_value(property).map(str::to_owned))
            .unwrap_or_default()
    }

    fn animation_tracks(
        &self,
        id: NodeId,
        name: &str,
        important: &FxHashSet<String>,
    ) -> Vec<Track> {
        let index = self.style_index();
        let Some(rule) = index.keyframes.get(name) else {
            return Vec::new();
        };
        let vp = Vp {
            w: self.viewport_px.0,
            h: self.viewport_px.1,
        };
        let easing = |offset: f32| {
            rule.easings
                .iter()
                .find(|(at, _)| *at == offset)
                .and_then(|(_, easing)| Easing::parse(easing))
        };
        let mut tracks: Vec<Track> = Vec::new();
        let mut names = rule.properties.keys().collect::<Vec<_>>();
        names.sort();
        for name in names {
            let physical = self
                .logical_property(id, None, name)
                .unwrap_or_else(|| name.clone());
            if !sampled_property(&physical) || important.contains(&physical) {
                continue;
            }
            let Some(property) = prop_index(&physical) else {
                continue;
            };
            let kind = Kind::of(&physical);
            let mut frames = rule.properties[name]
                .iter()
                .map(|frame| Frame {
                    offset: f64::from(frame.offset),
                    value: self
                        .keyframe_value(id, &physical, &frame.value)
                        .map(|value| Value::parse(self, id, kind, &value, vp)),
                    // A timing function on the 100% keyframe is ignored
                    // (#timing-functions); no interval starts there.
                    easing: easing(frame.offset),
                })
                .collect::<Vec<_>>();
            // CSS Animations 1 #keyframes: missing 0% and 100% keyframes
            // use the computed (underlying) value.
            if frames.first().is_none_or(|frame| frame.offset > 0.0) {
                frames.insert(
                    0,
                    Frame {
                        offset: 0.0,
                        value: None,
                        easing: easing(0.0),
                    },
                );
            }
            if frames.last().is_none_or(|frame| frame.offset < 1.0) {
                frames.push(Frame {
                    offset: 1.0,
                    value: None,
                    easing: None,
                });
            }
            let underlying = if frames.iter().any(|frame| frame.value.is_none()) {
                Value::parse(self, id, kind, &self.underlying_value(id, &physical), vp)
            } else {
                Value::Other(String::new())
            };
            if let Some(track) = tracks
                .iter_mut()
                .find(|track| track.property == property as u16)
            {
                // Two logical names mapping to one physical property: the
                // later name (in sorted order) replaces the earlier.
                track.frames = frames;
                track.underlying = underlying;
            } else {
                tracks.push(Track {
                    property: property as u16,
                    kind,
                    frames,
                    underlying,
                });
            }
        }
        tracks
    }

    fn subtree_has_svg(&self, id: NodeId) -> bool {
        let mut stack = vec![id];
        while let Some(node) = stack.pop() {
            if self.tag_name(node) == Some("svg") {
                return true;
            }
            self.push_composed_children(node, &mut stack);
        }
        false
    }

    /// Whether a style-origin animation applies a value or may still change
    /// one. Such a document keeps a live page actor: only the actor samples
    /// the animation origin, so a static presentation re-laid out by a
    /// frontend (images, viewport) would lose it.
    pub(crate) fn css_animations_keep_alive(&self) -> bool {
        !self.animations.values.is_empty()
            || self.animations.elements.values().any(|element| {
                element.animations.iter().any(|animation| {
                    !animation.tracks.is_empty()
                        && animation.hold.is_none()
                        && animation.phase != Phase::After
                })
            })
    }

    /// Whether the document declares a style-origin animation at all,
    /// before its first rendering update (and therefore before any sample).
    pub(crate) fn css_animations_declared(&self) -> bool {
        let index = self.style_index();
        if !index.keyframes.values().any(|rule| {
            rule.properties
                .keys()
                .any(|property| sampled_property(property))
        }) {
            return false;
        }
        self.composed_descendants(DOCUMENT).into_iter().any(|id| {
            self.tag_name(id).is_some()
                && self.animation_specs(id).iter().any(|spec| {
                    index.keyframes.get(&spec.name).is_some_and(|rule| {
                        rule.properties
                            .keys()
                            .any(|property| sampled_property(property))
                    })
                })
        })
    }

    /// The elements whose style-origin animations change with time, for
    /// frame scheduling: running animations in their active phase.
    pub(crate) fn css_animation_frame_targets(&self) -> Vec<FrameTarget> {
        self.animations
            .elements
            .iter()
            .filter_map(|(&node, element)| {
                let mut active = element
                    .animations
                    .iter()
                    .filter(|animation| {
                        animation.hold.is_none()
                            && animation.phase == Phase::Active
                            && !animation.tracks.is_empty()
                    })
                    .peekable();
                active.peek()?;
                let paint_only = active.all(|animation| {
                    animation.tracks.iter().all(|track| {
                        paint_only(PROPS[usize::from(track.property)].name)
                            && !(element.svg
                                && svg_dependent(PROPS[usize::from(track.property)].name))
                    })
                });
                Some(FrameTarget { node, paint_only })
            })
            .collect()
    }

    /// Gecko `KeyframeEffect::CanThrottleIfNotVisible` for a paint-only
    /// animated element: nothing it paints can currently be seen, because
    /// it is in a `visibility: hidden` subtree without visible descendants,
    /// under a non-animated `opacity: 0` group, or scrolled out of `view`
    /// (the viewport in document CSS px, with a margin for ink overflow such
    /// as glows). `rect` gives retained border boxes in the same space.
    /// Anything that can move content relative to the page scroll (fixed or
    /// sticky positioning, transforms, scrolled inner containers) or lacks a
    /// box counts as visible.
    pub(crate) fn css_animation_invisible(
        &self,
        node: NodeId,
        view: (f64, f64, f64, f64),
        rect: &dyn Fn(NodeId) -> Option<(f64, f64, f64, f64)>,
    ) -> bool {
        const LIMIT: usize = 512;
        const MARGIN: f64 = 128.0;
        let hidden = |id: NodeId| {
            self.computed_value_resolved(id, "visibility")
                .is_some_and(|value| matches!(value.trim(), "hidden" | "collapse"))
        };
        let subtree = |id: NodeId| {
            let mut out = Vec::new();
            let mut stack = vec![id];
            while let Some(node) = stack.pop() {
                if out.len() > LIMIT {
                    return None;
                }
                if self.tag_name(node).is_some() {
                    out.push(node);
                    self.push_composed_children(node, &mut stack);
                    if self.tag_name(node) == Some("slot") {
                        stack.extend(self.flat_slot_nodes(node));
                    }
                }
            }
            Some(out)
        };
        // Gecko nsIFrame::IsVisibleOrMayHaveVisibleDescendants.
        if hidden(node) && subtree(node).is_some_and(|nodes| nodes.into_iter().all(hidden)) {
            return true;
        }
        let mut moving = false;
        let mut current = Some(node);
        while let Some(id) = current {
            if self.tag_name(id).is_some() {
                // CanOptimizeAwayDueToOpacity: the root of an opacity:0 group
                // that cannot become visible by its own animation.
                let transparent = self
                    .computed_value_resolved(id, "opacity")
                    .and_then(|value| parse_alpha(value.trim()))
                    .is_some_and(|opacity| opacity <= 0.0);
                let compositor = || {
                    self.css_animation_definitions(id).iter().any(|definition| {
                        definition.keyframes.iter().any(|frame| {
                            frame.opacity.is_some()
                                || frame.transform.is_some()
                                || frame.top.is_some()
                        })
                    })
                };
                if transparent && !compositor() && self.transitions.value(id, "opacity").is_none() {
                    return true;
                }
                let position = self.computed_value_resolved(id, "position");
                moving |= matches!(position.as_deref().map(str::trim), Some("fixed" | "sticky"))
                    || self
                        .computed_value_resolved(id, "transform")
                        .is_some_and(|value| value.trim() != "none")
                    || compositor()
                    || (id != node
                        && (self.scroll_metric(id, 0).unwrap_or(0.0) != 0.0
                            || self.scroll_metric(id, 1).unwrap_or(0.0) != 0.0));
            }
            current = self.parent_flat(id);
        }
        if moving {
            return false;
        }
        // nsIFrame::IsScrolledOutOfView, with border boxes standing in for
        // ink overflow rectangles.
        let Some(nodes) = subtree(node) else {
            return false;
        };
        let (x, y, w, h) = view;
        let (left, top, right, bottom) = (x - MARGIN, y - MARGIN, x + w + MARGIN, y + h + MARGIN);
        let mut boxes = nodes.into_iter().filter_map(rect).peekable();
        if boxes.peek().is_none() {
            return false;
        }
        boxes.all(|(bx, by, bw, bh)| bx + bw < left || bx > right || by + bh < top || by > bottom)
    }

    /// The next timeline time (seconds) at which a running animation changes
    /// phase: its effect starts or ends, so values and events change even
    /// without per-frame sampling.
    pub(crate) fn css_animation_next_change(&self) -> Option<f64> {
        let now = self.animations.now;
        self.animations
            .elements
            .values()
            .flat_map(|element| &element.animations)
            .filter(|animation| animation.hold.is_none())
            .filter_map(|animation| {
                let local = animation.current(now);
                let timing = &animation.timing;
                let before_active = timing.delay.min(timing.end()).max(0.0);
                let active_after = (timing.delay + timing.active_duration())
                    .min(timing.end())
                    .max(0.0);
                [before_active, active_after]
                    .into_iter()
                    .find(|boundary| *boundary > local && boundary.is_finite())
                    .map(|boundary| animation.start + boundary)
            })
            .min_by(f64::total_cmp)
    }

    /// Changes to the set of animations or their phases since the caller's
    /// last look, for scheduling caches.
    pub(crate) fn css_animation_generation(&self) -> u64 {
        self.animations.generation
    }

    /// Take the presentation work produced by sampling since the last call:
    /// whether values changed that only need the retained fragments
    /// repainted (`layout2::repaint_graphical`), and whether values changed
    /// that need style and layout again.
    pub(crate) fn take_css_animation_updates(&mut self) -> (bool, bool) {
        (
            std::mem::take(&mut self.animations.paint_pending),
            std::mem::take(&mut self.animations.layout_pending),
        )
    }

    pub(crate) fn css_animation_events_pending(&self) -> bool {
        !self.animations.events.is_empty()
    }

    /// CSS Animations 2 #event-dispatch, ordered by scheduled time, then
    /// tree order and position in `animation-name` (Web Animations
    /// #animation-frame-loop, CSS Animations 2 #animation-composite-order).
    pub(crate) fn take_css_animation_events(&mut self) -> Vec<AnimationEvent> {
        let mut events = std::mem::take(&mut self.animations.events);
        events.sort_by(|a, b| {
            a.time
                .total_cmp(&b.time)
                .then(a.tree_order.cmp(&b.tree_order))
                .then(a.index.cmp(&b.index))
        });
        events.into_iter().map(|event| event.event).collect()
    }

    /// Sample every CSS animation at `seconds` on the document timeline.
    pub(crate) fn update_css_animations(&mut self, seconds: f64) {
        self.flush_style_invalidations();
        let now = seconds.max(self.animations.now);
        let stamp = (
            self.style_epoch,
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        let full =
            self.animations.stamp != Some(stamp) || self.animations.all_invalid.replace(false);
        let invalid = std::mem::take(self.animations.invalid.get_mut());
        if !full
            && invalid.is_empty()
            && (self.animations.elements.is_empty() || now == self.animations.now)
        {
            self.animations.now = now;
            return;
        }
        let diagnostic = casc_diag_on().then(std::time::Instant::now);
        let mut elements = std::mem::take(&mut self.animations.elements);
        let mut events = std::mem::take(&mut self.animations.events);
        if full || !invalid.is_empty() {
            let keyframes = !self.style_index().keyframes.is_empty();
            self.animations.keyframes.set(keyframes);
            let candidates: Vec<NodeId> = if full {
                let mut candidates = if keyframes {
                    self.composed_descendants(DOCUMENT)
                } else {
                    Vec::new()
                };
                candidates.extend(elements.keys().copied());
                candidates.sort_unstable();
                candidates.dedup();
                candidates
            } else {
                invalid.into_iter().collect()
            };
            self.rescan_animations(&mut elements, &mut events, candidates, keyframes, now);
            self.animations.stamp = Some(stamp);
        }
        for (&id, element) in elements.iter_mut() {
            if element.svg_epoch != self.epoch {
                element.svg = self.subtree_has_svg(id);
                element.svg_epoch = self.epoch;
            }
        }
        self.advance_animations(&mut elements, &mut events, now);
        let values = self.sample_animations(&elements, now);
        self.animations.elements = elements;
        self.animations.events = events;
        self.animations.now = now;
        self.apply_animation_values(values);
        if let Some(started) = diagnostic
            && started.elapsed().as_millis() > 2
        {
            eprintln!(
                "DIAGANIM total={}ms elements={} values={}",
                started.elapsed().as_millis(),
                self.animations.elements.len(),
                self.animations.values.len()
            );
        }
    }

    /// CSS Animations 1 #animations: (re)build the animations of elements
    /// whose style may have changed. Existing animations are matched by name
    /// from the end of the new list and keep their start time.
    fn rescan_animations(
        &mut self,
        elements: &mut FxHashMap<NodeId, Element>,
        events: &mut Vec<PendingEvent>,
        candidates: Vec<NodeId>,
        keyframes: bool,
        now: f64,
    ) {
        let mut participation = FxHashMap::default();
        let mut tree_order: Option<FxHashMap<NodeId, usize>> = None;
        for id in candidates {
            let mut specs = Vec::new();
            if keyframes && self.is_valid(id) && self.tag_name(id).is_some() {
                let index = self.style_index();
                specs = self
                    .animation_specs(id)
                    .into_iter()
                    .filter(|spec| index.keyframes.contains_key(&spec.name))
                    .collect::<Vec<_>>();
                // CSS Animations 1 #animations: `display: none` on the
                // element or an ancestor terminates its animations.
                if !specs.is_empty() && !transitions::participates(self, id, &mut participation) {
                    specs.clear();
                }
            }
            let previous = elements.remove(&id);
            let order = previous.as_ref().map_or(0, |element| element.tree_order);
            let mut old = previous.map(|element| element.animations);
            if specs.is_empty() {
                if let Some(old) = old {
                    self.animations.generation += 1;
                    for (index, animation) in old.iter().enumerate() {
                        cancel(events, id, animation, now, order, index);
                    }
                }
                if self.animations.private.remove(&id) {
                    self.computed_cache.borrow_mut().1.remove_node(id);
                }
                continue;
            }
            let order = *tree_order
                .get_or_insert_with(|| {
                    self.composed_descendants(DOCUMENT)
                        .into_iter()
                        .enumerate()
                        .map(|(order, node)| (node, order))
                        .collect()
                })
                .get(&id)
                .unwrap_or(&usize::MAX);
            // Animated values must not be shared with equal cascades: retire
            // the element's (possibly shared) rows before its first sample.
            if self.animations.private.insert(id) {
                let mut cache = self.computed_cache.borrow_mut();
                let mut stack = vec![id];
                while let Some(node) = stack.pop() {
                    cache.1.remove_node(node);
                    self.push_composed_children(node, &mut stack);
                }
            }
            let important = self.important_properties(id);
            let mut built: Vec<Option<Animation>> = (0..specs.len()).map(|_| None).collect();
            for (index, spec) in specs.iter().enumerate().rev() {
                let existing = old.as_mut().and_then(|old| {
                    old.iter()
                        .rposition(|animation| animation.name == spec.name)
                        .map(|position| old.remove(position))
                });
                let tracks = self.animation_tracks(id, &spec.name, &important);
                built[index] = Some(match existing {
                    Some(mut animation) => {
                        // Changed animation properties apply as if they had
                        // been specified from the start; the play state
                        // pauses or resumes the current time.
                        match (animation.hold, spec.timing.running) {
                            (None, false) => animation.hold = Some(now - animation.start),
                            (Some(hold), true) => {
                                animation.start = now - hold;
                                animation.hold = None;
                            }
                            _ => {}
                        }
                        animation.timing = spec.timing;
                        animation.tracks = tracks;
                        animation
                    }
                    None => {
                        self.animations.generation += 1;
                        Animation {
                            name: spec.name.clone(),
                            timing: spec.timing,
                            start: now,
                            hold: (!spec.timing.running).then_some(0.0),
                            phase: Phase::Idle,
                            iteration: 0.0,
                            tracks,
                        }
                    }
                });
            }
            for (index, animation) in old.unwrap_or_default().iter().enumerate() {
                self.animations.generation += 1;
                cancel(events, id, animation, now, order, index);
            }
            elements.insert(
                id,
                Element {
                    animations: built.into_iter().flatten().collect(),
                    tree_order: order,
                    svg: false,
                    svg_epoch: u64::MAX,
                },
            );
        }
    }

    /// CSS Animations 2 #event-dispatch: compare each animation's phase and
    /// current iteration with the previous frame's.
    fn advance_animations(
        &mut self,
        elements: &mut FxHashMap<NodeId, Element>,
        events: &mut Vec<PendingEvent>,
        now: f64,
    ) {
        for (&id, element) in elements.iter_mut() {
            for (index, animation) in element.animations.iter_mut().enumerate() {
                let local = animation.current(now);
                let timing = animation.timing;
                let phase = timing.phase(local);
                let iteration = timing
                    .progress(local)
                    .map_or(0.0, |(_, iteration)| iteration);
                let previous = animation.phase;
                let at = |offset: f64| {
                    if animation.hold.is_some() {
                        now
                    } else {
                        animation.start + offset
                    }
                };
                let mut push = |kind: &'static str, elapsed: f64, time: f64| {
                    events.push(PendingEvent {
                        event: (id, animation.name.clone(), kind, elapsed),
                        time,
                        tree_order: element.tree_order,
                        index,
                    });
                };
                let start = at(timing.delay.max(0.0));
                let end = at(timing.end());
                match (previous, phase) {
                    (Phase::Idle | Phase::Before, Phase::Active) => {
                        push("animationstart", timing.interval_start(), start)
                    }
                    (Phase::Idle | Phase::Before, Phase::After) => {
                        push("animationstart", timing.interval_start(), start);
                        push("animationend", timing.interval_end(), end);
                    }
                    (Phase::Active, Phase::Before) => {
                        push("animationend", timing.interval_start(), start)
                    }
                    (Phase::Active, Phase::Active) if iteration != animation.iteration => {
                        // CSS Animations 2 #animation-iteration-elapsed-time.
                        let boundary = if animation.iteration > iteration {
                            iteration + 1.0
                        } else {
                            iteration
                        };
                        let elapsed = boundary * timing.duration;
                        push("animationiteration", elapsed, at(timing.delay + elapsed));
                    }
                    (Phase::Active, Phase::After) => {
                        push("animationend", timing.interval_end(), end)
                    }
                    (Phase::After, Phase::Active) => {
                        push("animationstart", timing.interval_end(), end)
                    }
                    (Phase::After, Phase::Before) => {
                        push("animationstart", timing.interval_end(), end);
                        push("animationend", timing.interval_start(), start);
                    }
                    _ => {}
                }
                if previous != phase {
                    self.animations.generation += 1;
                }
                animation.phase = phase;
                animation.iteration = iteration;
            }
        }
    }

    /// The animation origin of every animated element at `now`. CSS
    /// Animations 1 #animations: the animation last in `animation-name`
    /// that has an effect on a property wins.
    fn sample_animations(
        &self,
        elements: &FxHashMap<NodeId, Element>,
        now: f64,
    ) -> FxHashMap<NodeId, Vec<(u16, String)>> {
        let color = prop_index("color").expect("color is tracked") as u16;
        let mut out = FxHashMap::default();
        for (&id, element) in elements {
            let mut properties = element
                .animations
                .iter()
                .flat_map(|animation| animation.tracks.iter().map(|track| track.property))
                .collect::<Vec<_>>();
            if properties.is_empty() {
                continue;
            }
            properties.sort_unstable();
            properties.dedup();
            // `currentcolor` in other properties uses this frame's color.
            if let Some(position) = properties.iter().position(|&property| property == color) {
                properties.remove(position);
                properties.insert(0, color);
            }
            let mut values: Vec<(u16, String)> = Vec::with_capacity(properties.len());
            for property in properties {
                let sampled = element.animations.iter().rev().find_map(|animation| {
                    let track = animation.track(property)?;
                    let (progress, _) = animation.timing.progress(animation.current(now))?;
                    let (a, b, t) = track.interval(progress, animation.timing.easing);
                    let start = a.value.as_ref().unwrap_or(&track.underlying);
                    let end = b.value.as_ref().unwrap_or(&track.underlying);
                    let current = || {
                        let text = values
                            .iter()
                            .find(|(property, _)| *property == color)
                            .map(|(_, value)| value.clone())
                            .or_else(|| self.computed_value_resolved(id, "color"))?;
                        Color::parse(self, id, &text)
                    };
                    Some(values::interpolate(track.kind, start, end, t, &current))
                });
                // A discrete step to an unknown underlying value leaves the
                // property to its ordinary computation.
                if let Some(value) = sampled.filter(|value| !value.is_empty()) {
                    values.push((property, value));
                }
            }
            if !values.is_empty() {
                out.insert(id, values);
            }
        }
        out
    }

    /// Publish a new animation origin and invalidate exactly what it can
    /// affect (Gecko's animation-only restyle; Stylo
    /// `replace_rules_internal`): no selector matching, no cascade.
    fn apply_animation_values(&mut self, values: FxHashMap<NodeId, Vec<(u16, String)>>) {
        let old = std::mem::take(&mut self.animations.values);
        let mut paint: Vec<(NodeId, usize)> = Vec::new();
        let mut layout: Vec<NodeId> = Vec::new();
        let background = prop_index("background-color").expect("background-color is tracked");
        let mut ids = old.keys().chain(values.keys()).copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let (before, after) = (old.get(&id), values.get(&id));
            if before == after {
                continue;
            }
            let svg = self
                .animations
                .elements
                .get(&id)
                .is_some_and(|element| element.svg);
            fn lookup(values: Option<&Vec<(u16, String)>>, property: u16) -> Option<&str> {
                values?
                    .iter()
                    .find(|(candidate, _)| *candidate == property)
                    .map(|(_, value)| value.as_str())
            }
            let mut properties = before
                .into_iter()
                .chain(after)
                .flatten()
                .map(|(property, _)| *property)
                .collect::<Vec<_>>();
            properties.sort_unstable();
            properties.dedup();
            let mut relayout = false;
            for property in properties {
                let (a, b) = (lookup(before, property), lookup(after, property));
                if a == b {
                    continue;
                }
                let name = PROPS[usize::from(property)].name;
                let mut paint_tier = paint_only(name) && !(svg && svg_dependent(name));
                if paint_tier && usize::from(property) == background {
                    // The terminal compositor decides at layout whether a
                    // background fills at all (`BoxStyle::bg`).
                    let transparent = |value: Option<&str>| {
                        value.is_none_or(|value| {
                            Value::parse(self, id, Kind::Color, value, Vp { w: 0.0, h: 0.0 })
                                .transparent()
                        })
                    };
                    paint_tier = transparent(a) == transparent(b);
                }
                if paint_tier {
                    paint.push((id, usize::from(property)));
                } else {
                    relayout = true;
                }
            }
            if relayout {
                layout.push(id);
            }
        }
        self.animations.values = values;
        #[cfg(test)]
        {
            self.animations.last_paint_changes = paint.len();
            self.animations.last_layout_changes = layout.len();
        }
        if !paint.is_empty() {
            for &(id, property) in &paint {
                self.forget_animated_value(id, property);
            }
            // A presentation revision, not a DOM mutation: neither layout
            // nor the hit-test geometry changes.
            self.layout_paint_epoch = self.layout_paint_epoch.wrapping_add(1);
            self.animations.paint_pending = true;
        }
        if !layout.is_empty() {
            {
                let mut computed = self.computed_cache.borrow_mut();
                let mut stack = layout.clone();
                while let Some(node) = stack.pop() {
                    computed.1.clear_node(node);
                    self.font_cache.borrow_mut().invalidate(node);
                    self.font_units_cache.borrow_mut().invalidate(node);
                    self.decoration_cache.borrow_mut().invalidate(node);
                    self.push_composed_children(node, &mut stack);
                    if self.tag_name(node) == Some("slot") {
                        stack.extend(self.flat_slot_nodes(node));
                    }
                }
            }
            // As for transitions: a presentation revision, never a DOM
            // mutation that live collections or observers could see.
            self.layout_presentation_epoch = self.layout_presentation_epoch.wrapping_add(1);
            self.invalidate_transition_layout(&layout);
            self.hidden_cache.get_mut().slots.clear();
            self.geometry_dirty_attributed = false;
            self.dirty_attributed = false;
            self.animations.layout_pending = true;
        }
    }

    /// Evict `property` from the computed values that inherit it from `id`.
    /// A descendant that declares its own value (other than an inheriting
    /// keyword) stops the walk below it.
    fn forget_animated_value(&self, id: NodeId, property: usize) {
        let name = PROPS[property].name;
        let inherited = PROPS[property].inherited;
        let mut computed = self.computed_cache.borrow_mut();
        computed.1.forget(id, property);
        let mut stack = Vec::new();
        self.push_composed_children(id, &mut stack);
        if self.tag_name(id) == Some("slot") {
            stack.extend(self.flat_slot_nodes(id));
        }
        while let Some(node) = stack.pop() {
            if self.tag_name(node).is_none() {
                continue;
            }
            let inherits = match self.cascaded(node, name) {
                None => inherited,
                Some(value) => match wide_keyword(&value) {
                    Some(WideKeyword::Inherit) => true,
                    Some(WideKeyword::Unset | WideKeyword::Revert) => inherited,
                    Some(WideKeyword::Initial) => false,
                    None => {
                        (name == "color" && value.trim().eq_ignore_ascii_case("currentcolor"))
                            || needs_var_substitution(&value)
                    }
                },
            };
            if !inherits {
                continue;
            }
            computed.1.forget(node, property);
            self.push_composed_children(node, &mut stack);
            if self.tag_name(node) == Some("slot") {
                stack.extend(self.flat_slot_nodes(node));
            }
        }
    }
}

/// CSS Animations 2 #event-dispatch: removing an animation that is neither
/// idle nor finished cancels it; `elapsedTime` is its active time with a
/// fill mode of both.
fn cancel(
    events: &mut Vec<PendingEvent>,
    id: NodeId,
    animation: &Animation,
    now: f64,
    tree_order: usize,
    index: usize,
) {
    if matches!(animation.phase, Phase::Idle | Phase::After) {
        return;
    }
    let local = animation.current(now);
    let timing = Timing {
        fill: Fill::Both,
        ..animation.timing
    };
    let elapsed = timing
        .active_time(local, timing.phase(local))
        .unwrap_or(0.0);
    events.push(PendingEvent {
        event: (id, animation.name.clone(), "animationcancel", elapsed),
        time: now,
        tree_order,
        index,
    });
}

/// The initial value used for `initial`, `unset` and missing keyframes.
fn initial_value(property: &str) -> Option<&'static str> {
    cssom_initial_value(property).or(match property {
        "text-shadow" | "box-shadow" | "filter" | "background-image" => Some("none"),
        "letter-spacing" | "word-spacing" => Some("normal"),
        "width" | "height" | "min-width" | "min-height" | "flex-basis" => Some("auto"),
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => Some("0px"),
        "background-position-x" | "background-position-y" => Some("0%"),
        "font-weight" => Some("400"),
        "fill" => Some("black"),
        "stroke" => Some("none"),
        "stop-color" => Some("black"),
        "fill-opacity" | "stroke-opacity" | "stop-opacity" => Some("1"),
        _ => None,
    })
}

#[cfg(test)]
mod tests;
