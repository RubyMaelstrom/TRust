//! CSS length/percentage values for the layout2 engine, in CSS-pixel space.
//!
//! A declaration is parsed ONCE into a [`Len`] when the style snapshot is
//! built, and resolved (possibly many times) against a containing-block basis
//! during layout. Everything that can be known at parse time is folded to a
//! number then: absolute units, `em`/`rem` (the element's/root's font size is
//! fixed per element), `ch`/`ex` (from the element's font metrics), and the
//! viewport units (the viewport is fixed per pass). Only percentages stay
//! symbolic — and CSS's `calc()` grammar only permits multiplying a length by
//! a NUMBER (never length × length), so every valid `calc()` is LINEAR in the
//! percentage basis and folds to `k·basis + b`. `min()`/`max()`/`clamp()`
//! break linearity and keep a small tree.
//!
//! All math is f32 CSS px; the px→cell quantization happens once, in the
//! terminal adapter.

use crate::layout2::{Units, css_length_px};

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

/// A resolvable expression: linear in the percentage basis, a min/max/clamp
/// tree, or a calc() sum/product carrying a non-linear subtree (css-values-3
/// §8.1 allows `calc(min(…) + 10px)` — the linear fold handles everything
/// else).
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
    /// `a + sign·b` — a calc() sum with a non-linear side. Unlike the
    /// fail-open min/max fold, an unresolvable side makes the sum
    /// unresolvable (there is no partial answer to an addition).
    Sum(Box<Node>, Box<Node>, f32),
    /// `a × f` — a calc() product with a non-linear side.
    Scale(Box<Node>, f32),
}

impl Node {
    fn px(b: f32) -> Node {
        Node::Lin { k: 0.0, b }
    }

    /// Resolve against `basis` (the containing block's relevant dimension in
    /// px). `None` basis ⇒ any percentage-carrying branch is unresolvable.
    /// CSS Values 4 #comp-func / #calc-computed-value: comparison functions
    /// retain unresolved percentages until their basis is known. Dropping an
    /// operand would change the function, rather than simplify it.
    pub fn resolve(&self, basis: Option<f32>) -> Option<f32> {
        match self {
            Node::Lin { k, b } => {
                if *k == 0.0 {
                    Some(*b)
                } else {
                    basis.map(|base| k * base + b)
                }
            }
            Node::Min(args) | Node::Max(args) => {
                let mut values = args.iter();
                let first = values.next()?.resolve(basis)?;
                values.try_fold(first, |value, arg| {
                    let arg = arg.resolve(basis)?;
                    Some(if matches!(self, Node::Min(_)) {
                        value.min(arg)
                    } else {
                        value.max(arg)
                    })
                })
            }
            Node::Clamp(lo, val, hi) => {
                let v = val.resolve(basis)?;
                Some(lo.resolve(basis)?.max(v.min(hi.resolve(basis)?)))
            }
            Node::Sum(a, b, sign) => Some(a.resolve(basis)? + sign * b.resolve(basis)?),
            Node::Scale(a, f) => Some(a.resolve(basis)? * f),
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
        if v.eq_ignore_ascii_case("fit-content") {
            return Some(Len::FitContent);
        }
        if let Some(inner) = strip_fn(&v.to_ascii_lowercase(), v, "fit-content(") {
            let inner = inner.trim();
            if inner.parse::<f32>().is_ok_and(|n| n != 0.) {
                return None;
            }
            let node = parse_node(inner, u, vp)?;
            if !inner.to_ascii_lowercase().starts_with("calc(")
                && node.resolve(Some(0.)).is_some_and(|n| n < 0.)
            {
                return None;
            }
            if let Node::Lin { k, .. } = node
                && k < 0.
                && !inner.to_ascii_lowercase().starts_with("calc(")
            {
                return None;
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
                .map(|a| parse_calculation(a, u, vp))
                .collect::<Option<_>>()?;
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
        let args = split_args(inner);
        let bound = |value: &str, infinite: f32| {
            if value.trim().eq_ignore_ascii_case("none") {
                Some(Node::px(infinite))
            } else {
                parse_calculation(value, u, vp)
            }
        };
        return match args.as_slice() {
            [lo, val, hi] => Some(Node::Clamp(
                Box::new(bound(lo, f32::NEG_INFINITY)?),
                Box::new(parse_calculation(val, u, vp)?),
                Box::new(bound(hi, f32::INFINITY)?),
            )),
            _ => None,
        };
    }
    leaf(v, u, vp).map(|t| match t {
        Term::Num(n) => Node::px(n), // unitless number: legacy px (quirk kept engine-wide)
        t => t.into_node(),
    })
}

fn parse_calculation(value: &str, u: Units, vp: Vp) -> Option<Node> {
    let mut parser = Calc {
        s: value.as_bytes(),
        src: value,
        pos: 0,
        u,
        vp,
    };
    let value = parser.sum()?;
    parser.skip_ws();
    (parser.pos == parser.s.len()).then_some(value)?.into_len()
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

/// One `calc()` term while folding: a dimensionless number, a length linear
/// in the percentage basis, or a non-linear length subtree (a nested
/// `min()`/`max()`/`clamp()`). CSS's type rules (css-values-3 §8.1.1) fall
/// out of the arithmetic below: length×length and X÷length are type errors
/// ⇒ `None`.
#[derive(Clone)]
enum Term {
    Num(f32),
    Len { k: f32, b: f32 },
    Tree(Node),
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
            (Term::Num(_), _) | (_, Term::Num(_)) => None,
            // A non-linear side: keep the sum as a tree.
            (a, b) => Some(Term::Tree(Node::Sum(
                Box::new(a.into_node()),
                Box::new(b.into_node()),
                sign,
            ))),
        }
    }

    fn mul(self, o: Term) -> Option<Term> {
        match (self, o) {
            (Term::Num(a), Term::Num(b)) => Some(Term::Num(a * b)),
            (Term::Num(n), Term::Len { k, b }) | (Term::Len { k, b }, Term::Num(n)) => {
                Some(Term::Len {
                    k: scale_coeff(k, n),
                    b: scale_coeff(b, n),
                })
            }
            (Term::Num(n), Term::Tree(t)) | (Term::Tree(t), Term::Num(n)) => {
                Some(Term::Tree(Node::Scale(Box::new(t), n)))
            }
            _ => None, // length × length
        }
    }

    fn div(self, o: Term) -> Option<Term> {
        match (self, o) {
            (_, Term::Num(0.0)) => None,
            (Term::Num(a), Term::Num(n)) => Some(Term::Num(a / n)),
            (Term::Len { k, b }, Term::Num(n)) => Some(Term::Len {
                k: scale_coeff(k, 1.0 / n),
                b: scale_coeff(b, 1.0 / n),
            }),
            (Term::Tree(t), Term::Num(n)) => Some(Term::Tree(Node::Scale(Box::new(t), 1.0 / n))),
            _ => None, // anything ÷ length
        }
    }

    /// The length node this term denotes (caller has ruled out `Num`).
    fn into_node(self) -> Node {
        match self {
            Term::Len { k, b } => Node::Lin { k, b },
            Term::Tree(t) => t,
            Term::Num(n) => Node::px(n),
        }
    }

    /// A finished `calc()` must be a length; a bare number is not one.
    fn into_len(self) -> Option<Node> {
        match self {
            Term::Len { k, b } => Some(Node::Lin { k, b }),
            Term::Tree(t) => Some(t),
            Term::Num(_) => None,
        }
    }
}

/// Scale one linear coefficient, keeping an exact 0 exactly 0 — a length
/// with no percentage component must stay percentage-free under
/// `calc(infinity * 1px)` (0 × ∞ is NaN, which would poison `resolve`).
fn scale_coeff(c: f32, n: f32) -> f32 {
    if c == 0.0 { 0.0 } else { c * n }
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
    // This engine currently receives one layout viewport, so the
    // small/large/dynamic qualifiers (`svh`/`lvh`/`dvw`, …) use that same
    // basis: strip the base suffix, then an optional trailing `d`/`s`/`l`.
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
        let lower = tok.to_ascii_lowercase();
        // The css-values-4 §10.6 numeric constants (calc-only idents; a bare
        // `width: pi` is invalid CSS, so these never leak to the top level).
        match lower.as_str() {
            "e" => return Some(Term::Num(std::f32::consts::E)),
            "pi" => return Some(Term::Num(std::f32::consts::PI)),
            "infinity" => return Some(Term::Num(f32::INFINITY)),
            "-infinity" => return Some(Term::Num(f32::NEG_INFINITY)),
            _ => {}
        }
        // A nested calc()/min()/max()/clamp()/var(): a linear result folds
        // into the enclosing sum; a non-linear one rides along as a tree
        // (`calc(min(50%, 300px) + 1rem)`).
        if lower.contains('(') {
            let n = parse_node(tok, self.u, self.vp)?;
            return match n {
                Node::Lin { k, b } => Some(Term::Len { k, b }),
                n => Some(Term::Tree(n)),
            };
        }
        leaf(tok, self.u, self.vp)
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
        assert!(
            val("calc(infinity * 1px)")
                .resolve(None)
                .unwrap()
                .is_infinite()
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
}
