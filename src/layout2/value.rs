//! CSS length/percentage values for the layout2 engine, in CSS-pixel space.
//!
//! A declaration is parsed ONCE into a [`Len`] when the style snapshot is
//! built, and resolved (possibly many times) against a containing-block basis
//! during layout. Everything that can be known at parse time is folded to a
//! number then: absolute units, `em`/`rem` (the element's/root's font size is
//! fixed per element), `ch`/`ex` (from the element's font metrics), and the
//! viewport units (the viewport is fixed per pass). Only percentages stay
//! symbolic. Sums and products with a number keep a calculation LINEAR in
//! the percentage basis (`k·basis + b`); the other math functions of CSS
//! Values 4 §10 (comparison, stepped-value, trigonometric, exponential and
//! sign-related functions), and products or quotients of two
//! percentage-dependent values, keep a small tree that resolves once the
//! basis is known. Constant subtrees fold at parse time.
//!
//! All math is f32 CSS px; the px→cell quantization happens once, in the
//! terminal adapter.

use crate::layout2::{Units, css_length_px, css_number_prefix};

/// The viewport in CSS px for viewport-percentage units. `h == 0.0` means the
/// pass wasn't told the viewport height (a legacy/test caller): `vh`/`vmin`/
/// `vmax` stay unresolvable rather than collapsing to zero.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Vp {
    pub w: f32,
    pub h: f32,
}

/// A parsed CSS sizing value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Len {
    Auto,
    /// `none` — the initial value of `max-width`/`max-height`.
    None,
    /// Intrinsic sizing, resolved by the memoized content-size query.
    MinContent,
    MaxContent,
    FitContent,
    /// CSS Sizing 4 #sizing-values: clamp the argument between the
    /// min-content and max-content sizes, preserving percentages.
    FitContentLimit(Node),
    Val(Node),
}

/// A resolvable calculation tree (CSS Values 4 #calc-internal). A node's
/// value is in the canonical unit of its type — px, deg, s, Hz, dppx or fr —
/// and its type was checked when it was parsed. [`Len`] only holds nodes of
/// type `<length>`; number- and angle-typed nodes occur as subtrees.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    /// `k·basis + b`.
    Lin {
        k: f32,
        b: f32,
    },
    Min(Vec<Node>),
    Max(Vec<Node>),
    Clamp(Box<Node>, Box<Node>, Box<Node>),
    /// `a + sign·b` — a calc() sum with a non-linear side. Unlike the
    /// fail-open min/max fold, an unresolvable side makes the sum
    /// unresolvable (there is no partial answer to an addition).
    Sum(Box<Node>, Box<Node>, f32),
    /// `a × f` — a calc() product with a non-linear side.
    Scale(Box<Node>, f32),
    /// `a × b` and `a ÷ b` where neither side is constant.
    Mul(Box<Node>, Box<Node>),
    Div(Box<Node>, Box<Node>),
    /// A stepped-value, trigonometric, exponential or sign-related function.
    Math(MathFn, Vec<Node>),
}

impl Node {
    fn px(b: f32) -> Node {
        Node::Lin { k: 0.0, b }
    }

    /// The value of a node that needs no percentage basis.
    fn constant(&self) -> Option<f32> {
        match self {
            Node::Lin { k, b } if *k == 0.0 => Some(*b),
            _ => None,
        }
    }

    /// Resolve against `basis` (the containing block's relevant dimension in
    /// px). `None` basis ⇒ any percentage-carrying branch is unresolvable.
    /// CSS Values 4 #comp-func / #calc-computed-value: comparison functions
    /// retain unresolved percentages until their basis is known. Dropping an
    /// operand would change the function, rather than simplify it.
    ///
    /// This is the top-level calculation, so CSS Values 4 #calc-ieee censors
    /// NaN and signed zeros into an unsigned zero, and clamps an infinity to
    /// the largest length layout represents. Each consumer then applies its
    /// property's own range (`width` is never negative, for instance).
    pub fn resolve(&self, basis: Option<f32>) -> Option<f32> {
        self.eval(basis).map(|value| {
            if value.is_nan() || value == 0.0 {
                0.0
            } else {
                value.clamp(-LENGTH_LIMIT, LENGTH_LIMIT)
            }
        })
    }

    /// Evaluate inside a calculation tree, keeping IEEE-754 NaN, infinities
    /// and signed zeros (CSS Values 4 #calc-ieee).
    fn eval(&self, basis: Option<f32>) -> Option<f32> {
        match self {
            Node::Lin { k, b } => {
                if *k == 0.0 {
                    Some(*b)
                } else {
                    basis.map(|base| k * base + b)
                }
            }
            Node::Min(args) | Node::Max(args) => {
                let min = matches!(self, Node::Min(_));
                let mut values = args.iter();
                let first = values.next()?.eval(basis)?;
                values.try_fold(first, |value, arg| {
                    let arg = arg.eval(basis)?;
                    Some(if min {
                        css_min(value, arg)
                    } else {
                        css_max(value, arg)
                    })
                })
            }
            Node::Clamp(lo, val, hi) => {
                let v = val.eval(basis)?;
                Some(css_max(lo.eval(basis)?, css_min(v, hi.eval(basis)?)))
            }
            Node::Sum(a, b, sign) => Some(a.eval(basis)? + sign * b.eval(basis)?),
            Node::Scale(a, f) => Some(a.eval(basis)? * f),
            Node::Mul(a, b) => Some(a.eval(basis)? * b.eval(basis)?),
            Node::Div(a, b) => Some(a.eval(basis)? / b.eval(basis)?),
            Node::Math(function, args) => {
                let args = args
                    .iter()
                    .map(|arg| arg.eval(basis).map(f64::from))
                    .collect::<Option<Vec<_>>>()?;
                Some(function.eval(&args, LAYOUT_DEVICE_PIXEL_RATIO) as f32)
            }
        }
    }

    /// Replace a subtree that needs no percentage basis by its value.
    fn fold(self) -> Node {
        match self {
            Node::Lin { .. } => self,
            node => node.eval(None).map_or(node, Node::px),
        }
    }
}

/// CSS Values 4 #calc-ieee: NaN wins any comparison, and 0⁻ is less than 0⁺.
fn css_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a < b || a == b && a.is_sign_negative() {
        a
    } else {
        b
    }
}

fn css_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a > b || a == b && b.is_sign_negative() {
        a
    } else {
        b
    }
}

/// The largest length magnitude a calculation resolves to: 2²⁵ CSS px, the
/// range of the 1/64-px fixed-point layout units other engines use. An
/// infinite length (`calc(infinity * 1px)`) becomes this finite value, which
/// further layout arithmetic can still add to without overflowing f32.
pub(crate) const LENGTH_LIMIT: f32 = 33_554_432.0;

/// Layout parses lengths without a device, so `round(line-width, …)` snaps
/// to whole CSS pixels: the terminal's device pixel. The DOM's typed
/// evaluator, which knows the document's device pixel ratio, uses that.
const LAYOUT_DEVICE_PIXEL_RATIO: f64 = 1.0;

/// CSS Values 4 #typedef-rounding-strategy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Rounding {
    Nearest,
    Up,
    Down,
    ToZero,
    LineWidth,
}

impl Rounding {
    pub(crate) fn parse(keyword: &str) -> Option<Rounding> {
        Some(match keyword.trim().to_ascii_lowercase().as_str() {
            "nearest" => Rounding::Nearest,
            "up" => Rounding::Up,
            "down" => Rounding::Down,
            "to-zero" => Rounding::ToZero,
            "line-width" => Rounding::LineWidth,
            _ => return None,
        })
    }

    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Rounding::Nearest => "nearest",
            Rounding::Up => "up",
            Rounding::Down => "down",
            Rounding::ToZero => "to-zero",
            Rounding::LineWidth => "line-width",
        }
    }
}

/// The CSS Values 4 math functions other than `calc()` and the comparison
/// functions. Arguments and results use canonical units, so angles are
/// degrees.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum MathFn {
    Round(Rounding),
    Mod,
    Rem,
    Abs,
    Sign,
    Hypot,
    /// `sin()`/`cos()`/`tan()`: `degrees` when the argument is an `<angle>`
    /// rather than a `<number>` of radians.
    Sin {
        degrees: bool,
    },
    Cos {
        degrees: bool,
    },
    Tan {
        degrees: bool,
    },
    Asin,
    Acos,
    Atan,
    Atan2,
    Pow,
    Sqrt,
    Log,
    Exp,
}

impl MathFn {
    /// Evaluate on already type-checked arguments. `round()` receives one
    /// argument only for `line-width` snapping; a number's omitted step is
    /// supplied by the caller. `log()` takes its base as an optional second
    /// argument.
    pub(crate) fn eval(self, args: &[f64], device_pixel_ratio: f64) -> f64 {
        // #calc-ieee: any operation with a NaN argument produces NaN.
        if args.iter().any(|value| value.is_nan()) {
            return f64::NAN;
        }
        let a = args.first().copied().unwrap_or(f64::NAN);
        let b = args.get(1).copied();
        match self {
            MathFn::Round(Rounding::LineWidth) => {
                let value = match b {
                    Some(b) => round(a, b, Rounding::LineWidth),
                    None => a,
                };
                snap_as_line_width(value, device_pixel_ratio)
            }
            MathFn::Round(strategy) => round(a, b.unwrap_or(1.0), strategy),
            MathFn::Mod => {
                let b = b.unwrap_or(f64::NAN);
                // #round-infinities: an infinite B returns A unless their
                // signs (including signed zeros) differ.
                if b.is_infinite() && a.is_finite() {
                    if a.is_sign_negative() == b.is_sign_negative() {
                        a
                    } else {
                        f64::NAN
                    }
                } else {
                    let rem = a % b;
                    if rem == 0.0 {
                        0_f64.copysign(b)
                    } else if rem.is_sign_negative() != b.is_sign_negative() {
                        rem + b
                    } else {
                        rem
                    }
                }
            }
            MathFn::Rem => a % b.unwrap_or(f64::NAN),
            MathFn::Abs => a.abs(),
            MathFn::Sign => {
                if a == 0.0 {
                    a
                } else {
                    a.signum()
                }
            }
            MathFn::Hypot => args.iter().copied().fold(0.0, f64::hypot),
            MathFn::Sin { degrees } => {
                // Exact zeros at whole half turns; 0⁻ stays 0⁻
                // (#trig-infinities).
                if a == 0.0 {
                    a
                } else if half_turns(a, degrees).is_some_and(|turns| turns.fract() == 0.0) {
                    0.0
                } else {
                    radians(a, degrees).sin()
                }
            }
            MathFn::Cos { degrees } => {
                if half_turns(a, degrees).is_some_and(|turns| (turns - 0.5).rem_euclid(1.0) == 0.0)
                {
                    0.0
                } else {
                    radians(a, degrees).cos()
                }
            }
            MathFn::Tan { degrees } => {
                // #trig-infinities: the asymptotes at 90deg + N·360deg and
                // -90deg + N·360deg are +∞ and −∞ where they are exact.
                if a == 0.0 {
                    a
                } else if let Some(turns) = half_turns(a, degrees).map(|t| t.rem_euclid(2.0))
                    && (turns.fract() == 0.0 || turns.fract() == 0.5)
                {
                    if turns.fract() == 0.0 {
                        0.0
                    } else if turns == 0.5 {
                        f64::INFINITY
                    } else {
                        f64::NEG_INFINITY
                    }
                } else {
                    radians(a, degrees).tan()
                }
            }
            MathFn::Asin => a.asin().to_degrees(),
            MathFn::Acos => a.acos().to_degrees(),
            MathFn::Atan => a.atan().to_degrees(),
            MathFn::Atan2 => a.atan2(b.unwrap_or(f64::NAN)).to_degrees(),
            MathFn::Pow => a.powf(b.unwrap_or(f64::NAN)),
            MathFn::Sqrt => a.sqrt(),
            MathFn::Log => match b {
                Some(base) => a.log(base),
                None => a.ln(),
            },
            MathFn::Exp => a.exp(),
        }
    }
}

/// A finite angle as a count of half turns. Degrees are exact at each
/// multiple of 90°; for radians this recognizes multiples of `pi / 2` built
/// from the `pi` constant.
fn half_turns(a: f64, degrees: bool) -> Option<f64> {
    a.is_finite().then(|| {
        if degrees {
            a / 180.0
        } else {
            a / std::f64::consts::PI
        }
    })
}

fn radians(a: f64, degrees: bool) -> f64 {
    if degrees { a.to_radians() } else { a }
}

/// CSS Values 4 #round-func and #round-infinities.
fn round(a: f64, b: f64, strategy: Rounding) -> f64 {
    if b == 0.0 || a.is_infinite() && b.is_infinite() {
        return f64::NAN;
    }
    if a.is_infinite() {
        return a;
    }
    if b.is_infinite() {
        return match strategy {
            Rounding::Up if a > 0.0 => f64::INFINITY,
            Rounding::Down if a < 0.0 => f64::NEG_INFINITY,
            Rounding::LineWidth if a != 0.0 => f64::INFINITY.copysign(a),
            _ => 0_f64.copysign(a),
        };
    }
    if a % b == 0.0 {
        return a;
    }
    let quotient = a / b.abs();
    let multiple = match strategy {
        Rounding::Up => quotient.ceil(),
        Rounding::Down => quotient.floor(),
        Rounding::ToZero => quotient.trunc(),
        Rounding::Nearest | Rounding::LineWidth => (quotient + 0.5).floor(),
    };
    if strategy == Rounding::LineWidth && multiple == 0.0 {
        // `line-width` never rounds a non-zero value to zero.
        b.abs().copysign(a)
    } else {
        // A zero lower bound is 0⁺ and a zero upper bound is 0⁻.
        (b.abs() * multiple).copysign(if multiple == 0.0 { a } else { multiple })
    }
}

/// CSS Values 4 #snap-a-length-as-a-line-width, exactly.
fn snap_as_line_width(px: f64, device_pixel_ratio: f64) -> f64 {
    let pixels = px * device_pixel_ratio;
    if pixels == 0.0 || !pixels.is_finite() {
        px
    } else {
        pixels.signum() * pixels.abs().floor().max(1.0) / device_pixel_ratio
    }
}

/// CSS Values 4 #calc-syntax: the math function names.
const MATH_FUNCTIONS: [&str; 21] = [
    "calc", "min", "max", "clamp", "round", "mod", "rem", "sin", "cos", "tan", "asin", "acos",
    "atan", "atan2", "pow", "sqrt", "hypot", "log", "exp", "abs", "sign",
];

/// Whether `token` starts with a math function call (`calc(`, `round(`, …).
/// Grammars use this to route a component to a numeric production; the
/// production's own parser then validates the whole calculation.
pub(crate) fn is_math_function(token: &str) -> bool {
    let token = token.trim_start();
    token
        .find('(')
        .is_some_and(|open| is_math_function_name(&token[..open]))
}

pub(crate) fn is_math_function_name(name: &str) -> bool {
    MATH_FUNCTIONS
        .iter()
        .any(|function| name.eq_ignore_ascii_case(function))
}

/// `(name, arguments)` when `v` is exactly one math function call: its
/// opening parenthesis closes at the end of `v`.
fn math_call(v: &str) -> Option<(String, &str)> {
    let open = v.find('(')?;
    let name = v[..open].to_ascii_lowercase();
    if !MATH_FUNCTIONS.contains(&name.as_str()) || !v.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    for (i, byte) in v.bytes().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return (i == v.len() - 1).then(|| (name, &v[open + 1..i]));
                }
            }
            _ => {}
        }
    }
    None
}

impl Len {
    /// Parse a declared sizing value. `None` = unparseable: the caller keeps
    /// the property's initial value, exactly as a browser drops an invalid
    /// declaration at parse time.
    pub fn parse(v: &str, u: Units, vp: Vp) -> Option<Len> {
        let v = v.trim();
        if v.is_empty() {
            return None;
        }
        match v.to_ascii_lowercase().as_str() {
            "auto" => return Some(Len::Auto),
            "none" => return Some(Len::None),
            "min-content" => return Some(Len::MinContent),
            "max-content" => return Some(Len::MaxContent),
            _ => {}
        }
        if v.eq_ignore_ascii_case("fit-content") {
            return Some(Len::FitContent);
        }
        if let Some(inner) = strip_fn(&v.to_ascii_lowercase(), v, "fit-content(") {
            let inner = inner.trim();
            if inner.parse::<f32>().is_ok_and(|n| n != 0.) {
                return None;
            }
            let node = parse_node(inner, u, vp)?;
            // A negative literal is invalid; a calculation clamps instead
            // (CSS Values 4 #calc-range).
            if !is_math_function(inner) {
                if node.resolve(Some(0.)).is_some_and(|n| n < 0.) {
                    return None;
                }
                if let Node::Lin { k, .. } = node
                    && k < 0.
                {
                    return None;
                }
            }
            return Some(Len::FitContentLimit(node));
        }
        parse_node(v, u, vp).map(Len::Val)
    }

    /// Parse an `Option<String>` read from the cascade, falling back to
    /// `initial` when absent or unparseable.
    pub fn parse_or(v: Option<&str>, u: Units, vp: Vp, initial: Len) -> Len {
        v.and_then(|s| Len::parse(s, u, vp)).unwrap_or(initial)
    }

    /// A fixed pixel value (UA-stylesheet defaults).
    pub fn px(b: f32) -> Len {
        Len::Val(Node::px(b))
    }

    /// Resolve to px against `basis`. `Auto`/`None`/the intrinsic keywords
    /// resolve to `None` — the caller applies the property's auto behavior.
    pub fn resolve(&self, basis: Option<f32>) -> Option<f32> {
        match self {
            Len::Val(n) => n.resolve(basis),
            _ => None,
        }
    }

    /// Whether this is the `auto` keyword (margin arithmetic cares).
    pub fn is_auto(&self) -> bool {
        matches!(self, Len::Auto)
    }
}

/// Parse a single value into a length `Node`: a bare length/percentage, a
/// math function, or `var(--x, fallback)` (the fallback — sheets are baked
/// before layout, so an unresolved custom property's spec-correct value here
/// is its fallback; no fallback ⇒ unresolvable).
fn parse_node(v: &str, u: Units, vp: Vp) -> Option<Node> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    if let Some(inner) = strip_fn(&lower, v, "var(") {
        let fallback = inner.split_once(',')?.1.trim();
        return parse_node(fallback, u, vp);
    }
    if let Some((name, args)) = math_call(v) {
        // CSS Values 4 #calc-type-checking: a math function is valid in a
        // `<length-percentage>` context only when its type is `<length>`.
        let term = Calc::new(args, u, vp, 0).function(&name, args)?;
        return (term.dims == LENGTH).then_some(term.node);
    }
    let term = leaf(v, u, vp)?;
    match term.dims {
        LENGTH => Some(term.node),
        // A unitless number: legacy px (quirk kept engine-wide).
        NUMBER => Some(term.node),
        _ => None,
    }
}

/// The body of `name(...)` when `v` is exactly that call (matched on the
/// lowercased copy so `CALC(...)` works, sliced from the original).
fn strip_fn<'a>(lower: &str, v: &'a str, name: &str) -> Option<&'a str> {
    if lower.starts_with(name) && lower.ends_with(')') {
        Some(&v[name.len()..v.len() - 1])
    } else {
        None
    }
}

/// Split a comma-separated argument list, respecting nested parentheses.
pub(crate) fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Substitute every `var(--name, fallback)` in a value string with its
/// fallback text. CSS resolves custom properties into the token stream BEFORE
/// the property grammar parses it (css-variables-1 §3), so a value like
/// `minmax(var(--x, 16rem), var(--y, 1fr))` must become `minmax(16rem, 1fr)`
/// before a grid/track parser classifies `1fr` as a flex or `min-content` as a
/// keyword — those tokens are otherwise hidden inside the `var()` and missed.
/// We resolve to the FALLBACK (matching `parse_node`'s var handling — the
/// author custom-property registry isn't consulted at this layer; sheets whose
/// vars ARE defined are baked before layout). `var(--name)` with no fallback,
/// or an unbalanced `var(`, becomes empty (the guaranteed-invalid value, per
/// spec). Nested vars in a fallback resolve too (the loop re-scans). A `var`
/// that is part of a longer identifier is left alone.
pub(crate) fn substitute_var_fallbacks(s: &str) -> String {
    let mut s = s.to_string();
    // Bounded so a pathological self-referential fallback can't spin forever.
    for _ in 0..64 {
        let Some(open) = find_var_open(&s) else {
            return s;
        };
        // `open` indexes the '(' after "var"; find its matching ')'.
        let mut depth = 0i32;
        let mut close = None;
        for (i, &c) in s.as_bytes().iter().enumerate().skip(open) {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            // Unbalanced `var(` — invalid; drop from the `var` to end.
            s.truncate(open - 3);
            return s;
        };
        let inner = &s[open + 1..close];
        let fallback = inner
            .split_once(',')
            .map(|(_, f)| f.trim())
            .unwrap_or("")
            .to_string();
        s.replace_range(open - 3..=close, &fallback);
    }
    s
}

/// Byte index of the `(` of the first `var(` function token in `s`
/// (case-insensitive), skipping a `var` that is part of a longer identifier
/// (preceded by a name character).
fn find_var_open(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        if b[i].eq_ignore_ascii_case(&b'v')
            && b[i + 1].eq_ignore_ascii_case(&b'a')
            && b[i + 2].eq_ignore_ascii_case(&b'r')
            && b[i + 3] == b'('
        {
            let prev_is_name =
                i > 0 && matches!(b[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_');
            if !prev_is_name {
                return Some(i + 3);
            }
        }
        i += 1;
    }
    None
}

/// CSS Values 4 #calc-type-checking: a type as the exponents of its base
/// types — length, angle, time, frequency, resolution, flex. This module
/// parses `<length-percentage>` contexts, where a percentage takes the
/// length percent hint, so percentages are typed (and folded) as lengths.
type Dims = [i8; 6];
const NUMBER: Dims = [0; 6];
const LENGTH: Dims = [1, 0, 0, 0, 0, 0];
const ANGLE: Dims = [0, 1, 0, 0, 0, 0];
const TIME: Dims = [0, 0, 1, 0, 0, 0];
const FREQUENCY: Dims = [0, 0, 0, 1, 0, 0];
const RESOLUTION: Dims = [0, 0, 0, 0, 1, 0];
const FLEX: Dims = [0, 0, 0, 0, 0, 1];

/// CSS Values 4 #calc-syntax requires at least 32 nesting levels; deeper
/// calculations are invalid rather than a recursion hazard.
const MAX_DEPTH: usize = 64;

/// One typed calculation while folding (CSS Values 4 #calc-type-checking):
/// its type, and a node in that type's canonical unit.
#[derive(Clone)]
struct Term {
    dims: Dims,
    node: Node,
}

impl Term {
    fn constant(dims: Dims, value: f32) -> Term {
        Term {
            dims,
            node: Node::px(value),
        }
    }

    /// `+`/`-`: adding two types fails unless they are the same.
    fn add(self, o: Term, sign: f32) -> Option<Term> {
        if self.dims != o.dims {
            return None;
        }
        let node = match (self.node, o.node) {
            (Node::Lin { k, b }, Node::Lin { k: k2, b: b2 }) => Node::Lin {
                k: k + sign * k2,
                b: b + sign * b2,
            },
            // A non-linear side: keep the sum as a tree.
            (a, b) => Node::Sum(Box::new(a), Box::new(b), sign),
        };
        Some(Term {
            dims: self.dims,
            node,
        })
    }

    /// `*` multiplies the types; `/` multiplies by the inverted type.
    fn mul(self, o: Term, divide: bool) -> Option<Term> {
        let mut dims = self.dims;
        for (d, other) in dims.iter_mut().zip(o.dims) {
            *d = if divide {
                d.checked_sub(other)?
            } else {
                d.checked_add(other)?
            };
        }
        let node = match (self.node, o.node) {
            (a, b) if b.constant().is_some() => {
                let n = b.constant().unwrap_or(f32::NAN);
                scale(a, n, divide)
            }
            (a, b) if !divide && a.constant().is_some() => {
                scale(b, a.constant().unwrap_or(f32::NAN), false)
            }
            (a, b) if divide => Node::Div(Box::new(a), Box::new(b)),
            (a, b) => Node::Mul(Box::new(a), Box::new(b)),
        };
        Some(Term { dims, node })
    }
}

/// Multiply (or divide) a node by a constant, staying linear when it was.
/// IEEE-754 division gives a zero divisor its infinity (#calc-ieee).
fn scale(node: Node, n: f32, divide: bool) -> Node {
    let apply = |c: f32| if divide { c / n } else { c * n };
    match node {
        Node::Lin { k: 0.0, b } => Node::px(apply(b)),
        // A zero coefficient means "no such component", not a value: a
        // length with no percentage stays percentage-free under
        // `calc(infinity * 1px)` (0 × ∞ is NaN, which would poison it).
        Node::Lin { k, b } => Node::Lin {
            k: if k == 0.0 { 0.0 } else { apply(k) },
            b: if b == 0.0 { 0.0 } else { apply(b) },
        },
        Node::Scale(a, f) => Node::Scale(a, apply(f)),
        node => Node::Scale(Box::new(node), if divide { 1.0 / n } else { n }),
    }
}

/// A leaf value (CSS Values 4 #calc-type-checking "terminal value"): a
/// number, percentage, or dimension, folded to its canonical unit. Lengths
/// use the engine-wide `css_length_px` (em/rem/ch/physical units, one
/// authority) plus the viewport units.
fn leaf(v: &str, u: Units, vp: Vp) -> Option<Term> {
    let (n, unit) = css_number_prefix(v.trim())?;
    let unit = unit.to_ascii_lowercase();
    let (dims, value) = match unit.as_str() {
        "" => (NUMBER, n),
        "%" => {
            return Some(Term {
                dims: LENGTH,
                node: Node::Lin {
                    k: n / 100.0,
                    b: 0.0,
                },
            });
        }
        "deg" => (ANGLE, n),
        "grad" => (ANGLE, n * 0.9),
        "rad" => (ANGLE, n.to_degrees()),
        "turn" => (ANGLE, n * 360.0),
        "s" => (TIME, n),
        "ms" => (TIME, n / 1000.0),
        "hz" => (FREQUENCY, n),
        "khz" => (FREQUENCY, n * 1000.0),
        "dppx" | "x" => (RESOLUTION, n),
        "dpi" => (RESOLUTION, n / 96.0),
        "dpcm" => (RESOLUTION, n * 2.54 / 96.0),
        "fr" => (FLEX, n),
        unit => (
            LENGTH,
            viewport_length(n, unit, vp).or_else(|| css_length_px(v, u))?,
        ),
    };
    Some(Term::constant(dims, value))
}

/// This engine currently receives one layout viewport, so the
/// small/large/dynamic qualifiers (`svh`/`lvh`/`dvw`, …) use that same
/// basis: strip an optional leading `d`/`s`/`l`, then match the base unit.
/// An unknown viewport height leaves `vh`/`vmin`/`vmax` unresolvable.
fn viewport_length(n: f32, unit: &str, vp: Vp) -> Option<f32> {
    let unit = unit.strip_prefix(['d', 's', 'l']).unwrap_or(unit);
    let basis = match unit {
        "vw" => vp.w,
        "vh" if vp.h > 0.0 => vp.h,
        "vmin" if vp.h > 0.0 => vp.w.min(vp.h),
        "vmax" if vp.h > 0.0 => vp.w.max(vp.h),
        _ => return None,
    };
    Some(n / 100.0 * basis)
}

/// Recursive-descent calculation parser over typed `Term`s (CSS Values 4
/// #calc-syntax): `sum := product ((+|-) product)*`, `product := value
/// ((*|/) value)*`, `value := (sum) | math-function | keyword | leaf`. CSS
/// requires whitespace around `+`/`-` (disambiguating signed numbers);
/// `*`/`/` need none.
struct Calc<'a> {
    s: &'a [u8],
    src: &'a str,
    pos: usize,
    u: Units,
    vp: Vp,
    depth: usize,
}

impl<'a> Calc<'a> {
    fn new(src: &'a str, u: Units, vp: Vp, depth: usize) -> Self {
        Calc {
            s: src.as_bytes(),
            src,
            pos: 0,
            u,
            vp,
            depth,
        }
    }

    /// One whole `<calc-sum>` argument, nested one level deeper.
    fn argument(&self, src: &str) -> Option<Term> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let mut parser = Calc::new(src, self.u, self.vp, self.depth + 1);
        let term = parser.sum()?;
        parser.skip_ws();
        (parser.pos == parser.s.len()).then_some(term)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn sum(&mut self) -> Option<Term> {
        let mut acc = self.product()?;
        loop {
            let before = self.pos;
            self.skip_ws();
            let sign = match self.s.get(self.pos) {
                Some(b'+') => 1.0,
                Some(b'-') => -1.0,
                _ => return Some(acc),
            };
            // Whitespace is required on both sides of `+` and `-`.
            if self.pos == before || !self.s.get(self.pos + 1)?.is_ascii_whitespace() {
                return None;
            }
            self.pos += 1;
            let rhs = self.product()?;
            acc = acc.add(rhs, sign)?;
        }
    }

    fn product(&mut self) -> Option<Term> {
        let mut acc = self.value()?;
        loop {
            let before = self.pos;
            self.skip_ws();
            let divide = match self.s.get(self.pos) {
                Some(b'*') => false,
                Some(b'/') => true,
                _ => {
                    // Leave the whitespace for `sum` to check around `+`/`-`.
                    self.pos = before;
                    return Some(acc);
                }
            };
            self.pos += 1;
            let rhs = self.value()?;
            acc = acc.mul(rhs, divide)?;
        }
    }

    fn value(&mut self) -> Option<Term> {
        self.skip_ws();
        if self.s.get(self.pos) == Some(&b'(') {
            let start = self.pos + 1;
            let end = self.closing_paren(self.pos)?;
            self.pos = end + 1;
            return self.argument(&self.src[start..end]);
        }
        // A value token ends at top-level whitespace, `*`, `/`, or `)`;
        // function calls keep their parenthesized arguments.
        let start = self.pos;
        let mut depth = 0usize;
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b'*' | b'/' if depth == 0 => break,
                byte if depth == 0 && byte.is_ascii_whitespace() => break,
                _ => {}
            }
            self.pos += 1;
        }
        let token = &self.src[start..self.pos];
        if token.is_empty() {
            return None;
        }
        // CSS Values 4 #calc-keywords: the numeric constants, valid only
        // inside a calculation (a bare `width: pi` is invalid).
        let constant = match token.to_ascii_lowercase().as_str() {
            "e" => Some(std::f32::consts::E),
            "pi" => Some(std::f32::consts::PI),
            "infinity" => Some(f32::INFINITY),
            "-infinity" => Some(f32::NEG_INFINITY),
            "nan" => Some(f32::NAN),
            _ => None,
        };
        if let Some(value) = constant {
            return Some(Term::constant(NUMBER, value));
        }
        if token.contains('(') {
            let lower = token.to_ascii_lowercase();
            if let Some(inner) = strip_fn(&lower, token, "var(") {
                // An unresolved custom property: its fallback, typed in
                // place (`calc(var(--n, 2) * 1px)` multiplies a number).
                return self.argument(inner.split_once(',')?.1);
            }
            let (name, args) = math_call(token)?;
            if self.depth >= MAX_DEPTH {
                return None;
            }
            return Calc::new(args, self.u, self.vp, self.depth + 1).function(&name, args);
        }
        leaf(token, self.u, self.vp)
    }

    fn closing_paren(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, byte) in self.s.iter().enumerate().skip(open) {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A math function's type and folded node (CSS Values 4 #calc-syntax
    /// and the function's type rule in #calc-type-checking).
    fn function(&self, name: &str, args: &str) -> Option<Term> {
        if name == "calc" {
            return self.argument(args);
        }
        let mut args = split_args(args);
        let mut strategy = None;
        if name == "round"
            && let Some(rounding) = args.first().and_then(|first| Rounding::parse(first))
        {
            strategy = Some(rounding);
            args.remove(0);
        }
        let mut terms = Vec::with_capacity(args.len());
        for arg in &args {
            if name == "clamp" && arg.trim().eq_ignore_ascii_case("none") {
                // `none` bounds take the value argument's type below.
                terms.push(None);
            } else {
                terms.push(Some(self.argument(arg)?));
            }
        }
        if name == "clamp" {
            let [lo, Some(value), hi] = <[Option<Term>; 3]>::try_from(terms).ok()? else {
                return None;
            };
            let bound = |term: Option<Term>, infinity: f32| {
                term.unwrap_or_else(|| Term::constant(value.dims, infinity))
            };
            let (lo, hi) = (bound(lo, f32::NEG_INFINITY), bound(hi, f32::INFINITY));
            if lo.dims != value.dims || hi.dims != value.dims {
                return None;
            }
            let dims = value.dims;
            let node = Node::Clamp(Box::new(lo.node), Box::new(value.node), Box::new(hi.node));
            return Some(Term {
                dims,
                node: node.fold(),
            });
        }
        let terms: Vec<Term> = terms.into_iter().collect::<Option<_>>()?;
        let first = terms.first()?.dims;
        // The comparison, stepped-value and hypot() functions require a
        // consistent type: adding their argument types must not fail.
        let consistent = terms.iter().all(|term| term.dims == first);
        let count = terms.len();
        let (function, dims) = match name {
            "min" | "max" if consistent => {
                let nodes = terms.into_iter().map(|term| term.node).collect();
                let node = if name == "min" {
                    Node::Min(nodes)
                } else {
                    Node::Max(nodes)
                };
                return Some(Term {
                    dims: first,
                    node: node.fold(),
                });
            }
            "round" if consistent && (count == 1 || count == 2) => {
                let strategy = strategy.unwrap_or(Rounding::Nearest);
                // `line-width` rounds lengths. Only it (snapping to device
                // pixels) or a number (stepping by 1) may omit B.
                if strategy == Rounding::LineWidth && first != LENGTH
                    || count == 1 && strategy != Rounding::LineWidth && first != NUMBER
                {
                    return None;
                }
                (MathFn::Round(strategy), first)
            }
            "mod" | "rem" if consistent && count == 2 => (
                if name == "mod" {
                    MathFn::Mod
                } else {
                    MathFn::Rem
                },
                first,
            ),
            "hypot" if consistent => (MathFn::Hypot, first),
            "abs" if count == 1 => (MathFn::Abs, first),
            "sign" if count == 1 => (MathFn::Sign, NUMBER),
            "sin" | "cos" | "tan" if count == 1 && matches!(first, NUMBER | ANGLE) => {
                let degrees = first == ANGLE;
                let function = match name {
                    "sin" => MathFn::Sin { degrees },
                    "cos" => MathFn::Cos { degrees },
                    _ => MathFn::Tan { degrees },
                };
                (function, NUMBER)
            }
            "asin" | "acos" | "atan" if count == 1 && first == NUMBER => {
                let function = match name {
                    "asin" => MathFn::Asin,
                    "acos" => MathFn::Acos,
                    _ => MathFn::Atan,
                };
                (function, ANGLE)
            }
            "atan2" if consistent && count == 2 => (MathFn::Atan2, ANGLE),
            "pow" if consistent && count == 2 && first == NUMBER => (MathFn::Pow, NUMBER),
            "sqrt" if count == 1 && first == NUMBER => (MathFn::Sqrt, NUMBER),
            "exp" if count == 1 && first == NUMBER => (MathFn::Exp, NUMBER),
            "log" if consistent && (count == 1 || count == 2) && first == NUMBER => {
                (MathFn::Log, NUMBER)
            }
            _ => return None,
        };
        // Only `round()` takes a rounding strategy.
        if strategy.is_some() && !matches!(function, MathFn::Round(_)) {
            return None;
        }
        let nodes = terms.into_iter().map(|term| term.node).collect();
        Some(Term {
            dims,
            node: Node::Math(function, nodes).fold(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> Units {
        Units::default() // 16px font with a shaped `ch` basis
    }

    fn vp() -> Vp {
        Vp { w: 640.0, h: 384.0 }
    }

    fn val(s: &str) -> Len {
        Len::parse(s, u(), vp()).expect(s)
    }

    #[test]
    fn absolute_units_fold_at_parse() {
        assert_eq!(val("32px").resolve(None), Some(32.0));
        assert_eq!(val("2em").resolve(None), Some(32.0));
        assert_eq!(val("1.5rem").resolve(None), Some(24.0));
        assert_eq!(val("10vw").resolve(None), Some(64.0));
        assert_eq!(val("50vh").resolve(None), Some(192.0));
        // Unknown viewport height: vh unresolvable, not zero.
        assert_eq!(
            Len::parse("50vh", u(), Vp { w: 640.0, h: 0.0 }),
            None,
            "vh with unknown viewport height is dropped at parse"
        );
    }

    #[test]
    fn percentages_need_a_basis() {
        let l = val("75%");
        assert_eq!(l.resolve(None), None);
        assert_eq!(l.resolve(Some(400.0)), Some(300.0));
    }

    #[test]
    fn calc_folds_linear() {
        // (100% - 20px) / 4  =  0.25·basis - 5
        let l = val("calc((100% - 20px) / 4)");
        assert_eq!(l.resolve(Some(400.0)), Some(95.0));
        // Scalar math: calc(2 * 3em) = 96px.
        assert_eq!(val("calc(2 * 3em)").resolve(None), Some(96.0));
        // Type errors are dropped.
        assert_eq!(Len::parse("calc(10px * 2em)", u(), vp()), None);
        // CSS Values 4 #calc-ieee: division by zero is an infinity, not a
        // parse error.
        assert_eq!(val("calc(10px / 0)").resolve(None), Some(LENGTH_LIMIT));
        assert_eq!(
            Len::parse("calc(2 * 3)", u(), vp()),
            None,
            "a number is not a length"
        );
    }

    #[test]
    fn min_max_clamp() {
        assert_eq!(val("min(100%, 200px)").resolve(Some(400.0)), Some(200.0));
        assert_eq!(val("min(100%, 200px)").resolve(Some(100.0)), Some(100.0));
        assert_eq!(val("min(100%, 200px)").resolve(None), None);
        assert_eq!(
            val("clamp(100px, 50%, 300px)").resolve(Some(400.0)),
            Some(200.0)
        );
        assert_eq!(
            val("clamp(100px, 50%, 300px)").resolve(Some(1000.0)),
            Some(300.0)
        );
        // calc nesting a linear min-arg.
        assert_eq!(
            val("min(calc(50% + 10px), 500px)").resolve(Some(400.0)),
            Some(210.0)
        );
    }

    #[test]
    fn comparison_functions_keep_every_operand() {
        for value in [
            "min(bogus, 20px)",
            "max(20px,)",
            "clamp(bogus, 20px, 30px)",
            "clamp(10px, 20px, bogus)",
        ] {
            assert_eq!(Len::parse(value, u(), vp()), None, "{value}");
        }
        assert_eq!(val("max(100%, 20px)").resolve(None), None);
        assert_eq!(val("clamp(10%, 20px, 30px)").resolve(None), None);
        assert_eq!(val("clamp(none, 20px, none)").resolve(None), Some(20.));
        assert_eq!(val("min(50% + 10px, 200px)").resolve(Some(100.)), Some(60.));
    }

    #[test]
    fn var_uses_fallback() {
        assert_eq!(val("var(--w, 12rem)").resolve(None), Some(192.0));
        assert_eq!(Len::parse("var(--w)", u(), vp()), None);
    }

    #[test]
    fn calc_carries_nonlinear_subtrees() {
        // A nested min()/max()/clamp() inside calc() no longer drops the
        // declaration — the non-linear branch rides along as a tree.
        let l = val("calc(min(50%, 300px) + 10px)");
        assert_eq!(l.resolve(Some(400.0)), Some(210.0));
        assert_eq!(l.resolve(Some(1000.0)), Some(310.0));
        let l = val("calc(2 * min(10px, 5%))");
        assert_eq!(l.resolve(Some(400.0)), Some(20.0));
        assert_eq!(l.resolve(Some(100.0)), Some(10.0));
        let l = val("calc(min(100%, 80px) / 2)");
        assert_eq!(l.resolve(Some(40.0)), Some(20.0));
        assert_eq!(l.resolve(Some(400.0)), Some(40.0));
        // A sum with an unresolvable side is unresolvable (no partial adds).
        assert_eq!(val("calc(min(100%) + 10px)").resolve(None), None);
    }

    #[test]
    fn calc_numeric_constants() {
        // css-values-4 §10.6: e / pi / infinity, calc-only idents.
        let pi = val("calc(pi * 1px)").resolve(None).unwrap();
        assert!((pi - std::f32::consts::PI).abs() < 1e-4);
        let e = val("calc(e * 1px)").resolve(None).unwrap();
        assert!((e - std::f32::consts::E).abs() < 1e-4);
        // A top-level infinity clamps to the largest representable length.
        assert_eq!(
            val("calc(infinity * 1px)").resolve(None),
            Some(LENGTH_LIMIT)
        );
        assert_eq!(
            val("calc(-infinity * 1%)").resolve(Some(1.)),
            Some(-LENGTH_LIMIT)
        );
        // A bare number is still not a length…
        assert_eq!(Len::parse("calc(pi)", u(), vp()), None);
        // …and the constants don't leak outside calc().
        assert_eq!(Len::parse("pi", u(), vp()), None);
    }

    #[test]
    fn keywords() {
        assert!(val("auto").is_auto());
        assert_eq!(val("none"), Len::None);
        assert_eq!(val("min-content"), Len::MinContent);
        assert_eq!(
            val("fit-content(20%)"),
            Len::FitContentLimit(Node::Lin { k: 0.2, b: 0. })
        );
        assert_eq!(val("AUTO"), Len::Auto);
    }

    fn px(s: &str, basis: Option<f32>) -> f32 {
        val(s).resolve(basis).expect(s)
    }

    fn close(actual: f32, expected: f32, value: &str) {
        assert!(
            (actual - expected).abs() < 1e-3,
            "{value}: {actual} != {expected}"
        );
    }

    #[test]
    fn stepped_value_functions() {
        // CSS Values 4 #round-func: rounding strategies and their examples.
        for (value, expected) in [
            ("round(nearest, 23px, 10px)", 20.0),
            ("round(23px, 10px)", 20.0),
            ("round(25px, 10px)", 30.0),
            ("round(-25px, 10px)", -20.0),
            ("round(up, 23px, 10px)", 30.0),
            ("round(down, -23px, 10px)", -30.0),
            ("round(to-zero, -23px, 10px)", -20.0),
            ("ROUND(Up, 21px, -10px)", 30.0),
            ("round(line-width, 0.3px, 10px)", 10.0),
            ("round(line-width, 2.5px)", 2.0),
            ("round(line-width, 0.25px)", 1.0),
            ("calc(round(2.5) * 1px)", 3.0),
            ("mod(18px, 5px)", 3.0),
            ("mod(-18px, 5px)", 2.0),
            ("rem(-18px, 5px)", -3.0),
            ("calc(mod(-140deg, -90deg) / 1deg * 1px)", -50.0),
            ("calc(mod(140deg, -90deg) / 1deg * 1px)", -40.0),
            ("calc(rem(140deg, -90deg) / 1deg * 1px)", 50.0),
            ("calc(1px * mod(1, infinity))", 1.0),
            ("calc(1px * round(up, 1, infinity))", LENGTH_LIMIT),
        ] {
            assert_eq!(px(value, None), expected, "{value}");
        }
        // Percentages keep their basis inside the function.
        let l = val("round(50%, 7px)");
        assert_eq!(l.resolve(None), None);
        assert_eq!(l.resolve(Some(100.0)), Some(49.0));
        assert_eq!(val("mod(100%, 30px)").resolve(Some(100.0)), Some(10.0));
        // A length needs an explicit step; `line-width` needs a length.
        for value in [
            "round(23.7px)",
            "round(up)",
            "round(bogus, 1px, 1px)",
            "round(line-width, 2, 1)",
            "mod(1px)",
            "mod(1px, 1deg)",
            "rem(1px, 1)",
            "calc(1px * round(2, 1px))",
        ] {
            assert_eq!(Len::parse(value, u(), vp()), None, "{value}");
        }
    }

    #[test]
    fn trigonometric_exponential_and_sign_functions() {
        for (value, expected) in [
            ("calc(100px * sin(30deg))", 50.0),
            ("calc(100px * cos(60deg))", 50.0),
            ("calc(100px * tan(45deg))", 100.0),
            ("calc(100px * sin(pi / 6))", 50.0),
            ("calc(100px * cos(0.25turn) + 1px)", 1.0),
            ("calc(1px * asin(1) / 1deg)", 90.0),
            ("calc(1px * acos(-1) / 1deg)", 180.0),
            ("calc(1px * atan(1) / 1deg)", 45.0),
            ("calc(1px * atan2(1px, -1px) / 1deg)", 135.0),
            ("calc(10px * pow(2, 3))", 80.0),
            ("calc(10px * sqrt(16))", 40.0),
            ("hypot(30px, 40px)", 50.0),
            ("hypot(-3em)", 48.0),
            ("calc(10px * log(100, 10))", 20.0),
            ("calc(10px * log(e))", 10.0),
            ("calc(10px * exp(0))", 10.0),
            ("abs(-20px)", 20.0),
            ("calc(100px * sign(-5px) + 200px)", 100.0),
        ] {
            close(px(value, None), expected, value);
        }
        // Exact zeros and asymptotes (#trig-infinities).
        assert_eq!(px("calc(1px * sin(180deg))", None), 0.0);
        assert_eq!(px("calc(1px * tan(90deg))", None), LENGTH_LIMIT);
        assert_eq!(px("calc(1px * tan(-90deg))", None), -LENGTH_LIMIT);
        // sign() of a percentage depends on the resolved basis.
        let l = val("calc(100px * sign(50% - 10px) + 200px)");
        assert_eq!(l.resolve(Some(10.0)), Some(100.0));
        assert_eq!(l.resolve(Some(100.0)), Some(300.0));
        assert_eq!(val("abs(10px - 50%)").resolve(Some(100.0)), Some(40.0));
        assert_eq!(val("hypot(60%, 40px)").resolve(Some(50.0)), Some(50.0));
        for value in [
            "sin(10px)",
            "calc(1px * sin(10px))",
            "calc(1px * asin(1deg))",
            "calc(1px * pow(2px, 2))",
            "calc(1px * pow(2, 2px))",
            "calc(1px * sqrt(4px))",
            "calc(1px * log(1, 2px))",
            "calc(1px * exp(1, 2))",
            "hypot(1px, 1deg)",
            "abs(10)",
            "sign(10px)",
            "calc(1px * atan2(1px, 1))",
        ] {
            assert_eq!(Len::parse(value, u(), vp()), None, "{value}");
        }
    }

    #[test]
    fn values_4_type_algebra() {
        // #calc-type-checking: products and quotients multiply types, so a
        // ratio of lengths is a number and length² is an intermediate type.
        assert_eq!(px("calc(100px * (20px / 10px))", None), 200.0);
        assert_eq!(px("calc(2px * 3px / 1px)", None), 6.0);
        assert_eq!(px("calc(1s / 500ms * 1px)", None), 2.0);
        assert_eq!(px("calc(1px * (1dppx / 1x) * 96dpi / 1dppx)", None), 1.0);
        assert_eq!(
            val("calc(50% * 50% / 1px)").resolve(Some(100.0)),
            Some(2500.0)
        );
        assert_eq!(
            val("calc(100px * 10px / 50%)").resolve(Some(100.0)),
            Some(20.0)
        );
        assert_eq!(
            val("calc(1px * (50% / 10px))").resolve(Some(100.0)),
            Some(5.0)
        );
        for value in [
            "calc(1px * 1px)",
            "calc(1px / 1px)",
            "calc(0 + 5px)",
            "calc(5px - 5px + 10s)",
            "calc(0 * 5px + 10s)",
            "calc(1fr)",
            "calc(10px+5px)",
            "calc(10px -5px)",
            "calc(inf * 1px)",
        ] {
            assert_eq!(Len::parse(value, u(), vp()), None, "{value}");
        }
        assert_eq!(px("calc(10px\t+\n5px)", None), 15.0);
        assert_eq!(px("calc(var(--n, 2) * 3px)", None), 6.0);
        assert_eq!(px("min(10px, round(up, 7px, 5px))", None), 10.0);
    }

    #[test]
    fn top_level_calculations_censor_ieee_values() {
        // #calc-ieee: NaN and signed zeros become zero only at the top level;
        // a nested calc() passes its 0⁻ through to the enclosing division.
        assert_eq!(px("calc(1px * NaN)", None), 0.0);
        assert_eq!(px("calc(infinity * 1px - infinity * 1px)", None), 0.0);
        assert!(px("calc(-5px * 0)", None).is_sign_positive());
        assert_eq!(px("calc(1px / calc(-5 * 0))", None), -LENGTH_LIMIT);
        assert_eq!(px("calc(1px / (-5 * 0))", None), -LENGTH_LIMIT);
        assert_eq!(px("calc(1px * min(NaN, 1))", None), 0.0);
        assert_eq!(px("calc(1px / sign(min(0, -1 * 0)))", None), -LENGTH_LIMIT);
        assert_eq!(px("calc(1px * mod(-1, infinity))", None), 0.0);
        assert_eq!(px("calc(1px * pow(-8, 1 / 3))", None), 0.0);
        // At least 32 nesting levels are supported; unbounded nesting is not.
        let nested = |depth: usize| format!("{}1px{}", "calc(".repeat(depth), ")".repeat(depth));
        assert_eq!(px(&nested(32), None), 1.0);
        assert_eq!(Len::parse(&nested(200), u(), vp()), None);
    }

    #[test]
    fn math_function_recognition() {
        for token in [
            "round(1px, 2px)",
            "ROUND(1px,2px)",
            " calc(1px)",
            "atan2(1, 2)",
            "rem(5px, 2px)",
        ] {
            assert!(is_math_function(token), "{token}");
        }
        for token in [
            "2rem",
            "rem",
            "remx(1px)",
            "var(--x)",
            "fit-content(1px)",
            "1px",
        ] {
            assert!(!is_math_function(token), "{token}");
        }
    }
}
