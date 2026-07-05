//! CSS length/percentage values for the layout2 engine, in CSS-pixel space.
//!
//! A declaration is parsed ONCE into a [`Len`] when the style snapshot is
//! built, and resolved (possibly many times) against a containing-block basis
//! during layout. Everything that can be known at parse time is folded to a
//! number then: absolute units, `em`/`rem` (the element's/root's font size is
//! fixed per element), `ch`/`ex` (glyph metrics are the terminal's), and the
//! viewport units (the viewport is fixed per pass). Only percentages stay
//! symbolic — and CSS's `calc()` grammar only permits multiplying a length by
//! a NUMBER (never length × length), so every valid `calc()` is LINEAR in the
//! percentage basis and folds to `k·basis + b`. `min()`/`max()`/`clamp()`
//! break linearity and keep a small tree.
//!
//! All math is f32 CSS px; the px→cell quantization happens once, at the
//! paint boundary (LAYOUT_OVERHAUL_PLAN.md "Quantization").

use crate::layout::{Units, css_length_px};

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
    /// The intrinsic-sizing keywords. P0 carries them so the parser is
    /// complete; the block algorithms treat them as `Auto` until the
    /// intrinsic-size query lands (an explicit, memoized query per the plan).
    MinContent,
    MaxContent,
    FitContent,
    Val(Node),
}

/// A resolvable expression: linear in the percentage basis, or a min/max/clamp
/// tree over linear branches.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    /// `k·basis + b` px.
    Lin {
        k: f32,
        b: f32,
    },
    Min(Vec<Node>),
    Max(Vec<Node>),
    Clamp(Box<Node>, Box<Node>, Box<Node>),
}

impl Node {
    fn px(b: f32) -> Node {
        Node::Lin { k: 0.0, b }
    }

    /// Resolve against `basis` (the containing block's relevant dimension in
    /// px). `None` basis ⇒ any percentage-carrying branch is unresolvable.
    /// `min()`/`max()` skip unresolvable arguments (fail-open, matching the
    /// engine-wide rule: a bound we can't parse yields the other bound rather
    /// than dropping the whole value); an empty fold is `None`.
    pub fn resolve(&self, basis: Option<f32>) -> Option<f32> {
        match self {
            Node::Lin { k, b } => {
                if *k == 0.0 {
                    Some(*b)
                } else {
                    basis.map(|base| k * base + b)
                }
            }
            Node::Min(args) => args
                .iter()
                .filter_map(|a| a.resolve(basis))
                .reduce(f32::min),
            Node::Max(args) => args
                .iter()
                .filter_map(|a| a.resolve(basis))
                .reduce(f32::max),
            Node::Clamp(lo, val, hi) => {
                let v = val.resolve(basis)?;
                let lo = lo.resolve(basis);
                let hi = hi.resolve(basis);
                // clamp(MIN, VAL, MAX) = max(MIN, min(VAL, MAX)); degrade
                // gracefully when a bound is unresolvable.
                let v = hi.map_or(v, |h| v.min(h));
                Some(lo.map_or(v, |l| v.max(l)))
            }
        }
    }
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
        if v.to_ascii_lowercase().starts_with("fit-content") {
            return Some(Len::FitContent);
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

/// Parse a single value into a `Node`: a bare length/percentage, `calc()`,
/// `min()`/`max()`/`clamp()`, or `var(--x, fallback)` (the fallback — sheets
/// are baked before layout, so an unresolved custom property's spec-correct
/// value here is its fallback; no fallback ⇒ unresolvable).
fn parse_node(v: &str, u: Units, vp: Vp) -> Option<Node> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    if let Some(inner) = strip_fn(&lower, v, "var(") {
        let fallback = inner.split_once(',')?.1.trim();
        return parse_node(fallback, u, vp);
    }
    if let Some(inner) = strip_fn(&lower, v, "calc(") {
        let mut p = Calc {
            s: inner.as_bytes(),
            src: inner,
            pos: 0,
            u,
            vp,
        };
        let t = p.sum()?;
        p.skip_ws();
        if p.pos != p.s.len() {
            return None;
        }
        return t.into_len();
    }
    for (name, is_min) in [("min(", true), ("max(", false)] {
        if let Some(inner) = strip_fn(&lower, v, name) {
            let args: Vec<Node> = split_args(inner)
                .into_iter()
                .filter_map(|a| parse_node(a, u, vp))
                .collect();
            if args.is_empty() {
                return None;
            }
            return Some(if is_min {
                Node::Min(args)
            } else {
                Node::Max(args)
            });
        }
    }
    if let Some(inner) = strip_fn(&lower, v, "clamp(") {
        let args: Vec<Option<Node>> = split_args(inner)
            .into_iter()
            .map(|a| parse_node(a, u, vp))
            .collect();
        return match args.as_slice() {
            [lo, Some(val), hi] => Some(Node::Clamp(
                Box::new(lo.clone().unwrap_or(Node::px(f32::NEG_INFINITY))),
                Box::new(val.clone()),
                Box::new(hi.clone().unwrap_or(Node::px(f32::INFINITY))),
            )),
            _ => None,
        };
    }
    leaf(v, u, vp).map(|t| match t {
        Term::Num(n) => Node::px(n), // unitless number: legacy px (quirk kept engine-wide)
        Term::Len { k, b } => Node::Lin { k, b },
    })
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

/// One `calc()` term while folding: a dimensionless number, or a length
/// linear in the percentage basis. CSS's type rules (css-values-3 §8.1.1)
/// fall out of the arithmetic below: length×length and X÷length are type
/// errors ⇒ `None`.
#[derive(Copy, Clone)]
enum Term {
    Num(f32),
    Len { k: f32, b: f32 },
}

impl Term {
    fn add(self, o: Term, sign: f32) -> Option<Term> {
        match (self, o) {
            (Term::Num(a), Term::Num(b)) => Some(Term::Num(a + sign * b)),
            (Term::Len { k, b }, Term::Len { k: k2, b: b2 }) => Some(Term::Len {
                k: k + sign * k2,
                b: b + sign * b2,
            }),
            // number + length is a calc type error.
            _ => None,
        }
    }

    fn mul(self, o: Term) -> Option<Term> {
        match (self, o) {
            (Term::Num(a), Term::Num(b)) => Some(Term::Num(a * b)),
            (Term::Num(n), Term::Len { k, b }) | (Term::Len { k, b }, Term::Num(n)) => {
                Some(Term::Len { k: k * n, b: b * n })
            }
            _ => None, // length × length
        }
    }

    fn div(self, o: Term) -> Option<Term> {
        match (self, o) {
            (_, Term::Num(0.0)) => None,
            (Term::Num(a), Term::Num(n)) => Some(Term::Num(a / n)),
            (Term::Len { k, b }, Term::Num(n)) => Some(Term::Len { k: k / n, b: b / n }),
            _ => None, // anything ÷ length
        }
    }

    /// A finished `calc()` must be a length; a bare number is not one.
    fn into_len(self) -> Option<Node> {
        match self {
            Term::Len { k, b } => Some(Node::Lin { k, b }),
            Term::Num(_) => None,
        }
    }
}

/// A leaf value: percentage, viewport unit, or absolute length (via the
/// engine-wide `css_length_px` — em/rem/ch/physical units, one authority).
fn leaf(v: &str, u: Units, vp: Vp) -> Option<Term> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        let pct: f32 = p.trim().parse().ok()?;
        return Some(Term::Len {
            k: pct / 100.0,
            b: 0.0,
        });
    }
    // Viewport-percentage units. A terminal has no dynamic chrome, so the
    // small/large/dynamic qualifiers (`svh`/`lvh`/`dvw`, …) equal the classic
    // units: strip the base suffix, then an optional trailing `d`/`s`/`l`.
    // Longer suffixes first so `vmin` isn't caught by `vh`-less scans.
    for (suffix, basis) in [
        ("vmin", (vp.h > 0.0).then(|| vp.w.min(vp.h))),
        ("vmax", (vp.h > 0.0).then(|| vp.w.max(vp.h))),
        ("vh", (vp.h > 0.0).then_some(vp.h)),
        ("vw", Some(vp.w)),
    ] {
        if let Some(rest) = v.strip_suffix(suffix) {
            let rest = rest.strip_suffix(['d', 's', 'l']).unwrap_or(rest);
            if let Ok(n) = rest.trim().parse::<f32>() {
                return basis.map(|b| Term::Len {
                    k: 0.0,
                    b: (n / 100.0) * b,
                });
            }
        }
    }
    // A bare number would parse as px through css_length_px; keep it a Num so
    // calc scalar arithmetic types correctly. (At the top level a Num is
    // treated as px — the engine-wide legacy-attr quirk.)
    if let Ok(n) = v.parse::<f32>() {
        return Some(Term::Num(n));
    }
    css_length_px(v, u).map(|px| Term::Len { k: 0.0, b: px })
}

/// Recursive-descent `calc()` evaluator over `Term`s: `sum := product ((+|-)
/// product)*`, `product := unit ((*|/) unit)*`, `unit := (sum) | nested-fn |
/// leaf`. CSS requires whitespace around `+`/`-` (disambiguating signed
/// numbers); `*`/`/` need none.
struct Calc<'a> {
    s: &'a [u8],
    src: &'a str,
    pos: usize,
    u: Units,
    vp: Vp,
}

impl Calc<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn sum(&mut self) -> Option<Term> {
        let mut acc = self.product()?;
        loop {
            self.skip_ws();
            match self.s.get(self.pos) {
                Some(b'+') => {
                    self.pos += 1;
                    let rhs = self.product()?;
                    acc = acc.add(rhs, 1.0)?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    let rhs = self.product()?;
                    acc = acc.add(rhs, -1.0)?;
                }
                _ => return Some(acc),
            }
        }
    }

    fn product(&mut self) -> Option<Term> {
        let mut acc = self.unit()?;
        loop {
            self.skip_ws();
            match self.s.get(self.pos) {
                Some(b'*') => {
                    self.pos += 1;
                    let rhs = self.unit()?;
                    acc = acc.mul(rhs)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let rhs = self.unit()?;
                    acc = acc.div(rhs)?;
                }
                _ => return Some(acc),
            }
        }
    }

    fn unit(&mut self) -> Option<Term> {
        self.skip_ws();
        if self.s.get(self.pos) == Some(&b'(') {
            self.pos += 1;
            let v = self.sum()?;
            self.skip_ws();
            if self.s.get(self.pos) != Some(&b')') {
                return None;
            }
            self.pos += 1;
            return Some(v);
        }
        // A value token ends at a top-level space, `*`, `/`, or `)`; nested
        // function calls (`calc()`, `min()`, `var()`) keep their parens.
        let start = self.pos;
        let mut depth = 0i32;
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b' ' | b'*' | b'/' if depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        let tok = self.src[start..self.pos].trim();
        if tok.is_empty() {
            return None;
        }
        // A nested calc()/min()/max()/clamp()/var() must stay foldable to a
        // linear term to participate in an enclosing calc sum; a non-linear
        // nested min() inside calc() is deliberately unsupported (rare — the
        // caller drops the declaration, keeping the initial value).
        let lower = tok.to_ascii_lowercase();
        if lower.contains('(') {
            let n = parse_node(tok, self.u, self.vp)?;
            return match n {
                Node::Lin { k, b } => Some(Term::Len { k, b }),
                _ => None,
            };
        }
        leaf(tok, self.u, self.vp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> Units {
        Units::default() // 16px font, 8×16 cell
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
        assert_eq!(Len::parse("calc(10px / 0)", u(), vp()), None);
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
        // Unresolvable arg skipped, fail-open.
        assert_eq!(val("min(100%, 200px)").resolve(None), Some(200.0));
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
    fn var_uses_fallback() {
        assert_eq!(val("var(--w, 12rem)").resolve(None), Some(192.0));
        assert_eq!(Len::parse("var(--w)", u(), vp()), None);
    }

    #[test]
    fn keywords() {
        assert!(val("auto").is_auto());
        assert_eq!(val("none"), Len::None);
        assert_eq!(val("min-content"), Len::MinContent);
        assert_eq!(val("fit-content(20%)"), Len::FitContent);
        assert_eq!(val("AUTO"), Len::Auto);
    }
}
