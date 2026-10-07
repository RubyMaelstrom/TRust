//! CSS Values 4 math functions in the numeric productions of ordinary
//! properties (`z-index: round(23, 10)`, `scale: calc(1 / 3)`,
//! `transition-delay: abs(-1s)`, `grid-template-rows: calc(2 * 1fr)`).
//!
//! #calc-type-checking: a math function is valid wherever a value of its
//! resolved type is; one resolving to <number> is also valid where only an
//! <integer> is, and is rounded to the nearest integer as it resolves. A
//! <percentage> that a property resolves against a <number> keeps the
//! percent type, so `opacity: calc(100% / 3)` is valid and
//! `opacity: calc(.25 + 25%)` is not.
//!
//! #calc-computed-value: the computed value is the calculation tree
//! simplified with all computed-value-time information (the element's font
//! and the viewport), which for these productions is a single numeric value
//! in its canonical unit. #calc-range and #calc-ieee: that top-level result
//! is clamped to the production's range, NaN is censored to zero, and an
//! infinity becomes the range's largest representable value.
//!
//! The typed evaluator is [`super::math`], shared with registered custom
//! properties and transform functions; lengths stay with layout's `Len`,
//! which resolves their percentages against the containing block.
use super::*;
use std::borrow::Cow;

/// A numeric production of a property grammar and its range
/// (CSS Values 4 #numeric-ranges).
#[derive(Clone, Copy)]
struct Production {
    kind: Type,
    min: f64,
    max: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Type {
    Number,
    /// <integer>: a <number> math function, rounded to the nearest integer.
    Integer,
    Percentage,
    /// `<number> | <percentage>` where the percentage computes to the
    /// equivalent number (`scale`, <opacity-value>).
    NumberPercentage,
    Angle,
    Time,
    Flex,
    /// A <length>, computed to an absolute length.
    Length,
    /// Only type checked here: layout resolves these (`Len`) once their
    /// percentage basis is known.
    LengthPercentage,
}

const fn production(kind: Type, min: f64, max: f64) -> Production {
    Production { kind, min, max }
}

const ANY: f64 = f64::INFINITY;
const NUMBER: Production = production(Type::Number, -ANY, ANY);
const NON_NEGATIVE: Production = production(Type::Number, 0., ANY);
const INTEGER: Production = production(Type::Integer, -ANY, ANY);
const POSITIVE_INTEGER: Production = production(Type::Integer, 1., ANY);
const NUMBER_PERCENTAGE: Production = production(Type::NumberPercentage, -ANY, ANY);
const PERCENTAGE: Production = production(Type::Percentage, -ANY, ANY);
const ANGLE: Production = production(Type::Angle, -ANY, ANY);
const TIME: Production = production(Type::Time, -ANY, ANY);
const DURATION: Production = production(Type::Time, 0., ANY);
const FLEX: Production = production(Type::Flex, 0., ANY);
const LENGTH: Production = production(Type::Length, -ANY, ANY);
const NON_NEGATIVE_LENGTH: Production = production(Type::Length, 0., ANY);
const AMOUNT: Production = production(Type::NumberPercentage, 0., ANY);
const LENGTH_PERCENTAGE: Production = production(Type::LengthPercentage, -ANY, ANY);
const FONT_WEIGHT: Production = production(Type::Number, 1., 1000.);

/// The numeric productions of `property`'s grammar that a math function can
/// resolve to, in the order one is tried against them, and whether they
/// also occur as arguments of the grammar's own functions (`minmax()`,
/// `repeat()`, transform functions) rather than only as top-level
/// components.
fn grammar(property: &str) -> Option<(&'static [Production], bool)> {
    Some(match property {
        // CSS 2 #z-index, CSS Display 3 #order-property.
        "z-index" | "order" => (&[INTEGER], false),
        // CSS Multicol 1 #cc, CSS Fragmentation 3 #widows-orphans, CSS
        // Overflow 4 #webkit-line-clamp.
        "column-count" | "orphans" | "widows" | "-webkit-line-clamp" => {
            (&[POSITIVE_INTEGER], false)
        }
        // CSS Color 4 #transparency's <opacity-value>, and the SVG, Masking
        // and Shapes properties sharing it.
        "opacity"
        | "fill-opacity"
        | "stroke-opacity"
        | "stop-opacity"
        | "flood-opacity"
        | "shape-image-threshold" => (&[NUMBER_PERCENTAGE], false),
        // CSS Flexbox 1 #flex-grow-property and #flex-shrink-property, CSS
        // Animations 1 #animation-iteration-count, CSS Fonts 5
        // #font-size-adjust-prop and CSS Sizing 4 #aspect-ratio's <ratio>.
        "flex-grow"
        | "flex-shrink"
        | "animation-iteration-count"
        | "font-size-adjust"
        | "aspect-ratio" => (&[NON_NEGATIVE], false),
        // CSS Fonts 4 #font-weight-absolute-values: <number [1,1000]>.
        "font-weight" => (&[FONT_WEIGHT], false),
        // CSS Text 3 #tab-size-property.
        "tab-size" => (&[NON_NEGATIVE, NON_NEGATIVE_LENGTH], false),
        // CSS Inline 3 #line-height-property.
        "line-height" => (&[NON_NEGATIVE, LENGTH_PERCENTAGE], false),
        // CSS Transforms 2 #individual-transforms.
        "scale" => (&[NUMBER_PERCENTAGE], false),
        "rotate" => (&[ANGLE, NUMBER], false),
        // CSS Transitions 1 #transition-duration-property and
        // #transition-delay-property; CSS Animations 1 likewise.
        "transition-delay" | "animation-delay" => (&[TIME], false),
        "transition-duration" | "animation-duration" => (&[DURATION], false),
        // CSS Grid 2 #track-sizing: <flex [0,∞]> tracks and repeat()'s
        // <integer [1,∞]> count, beside <length-percentage> sizes.
        "grid-template-rows" | "grid-template-columns" | "grid-auto-rows" | "grid-auto-columns" => {
            (&[FLEX, POSITIVE_INTEGER, LENGTH_PERCENTAGE], true)
        }
        // CSS Transforms 1 #transform-functions and Transforms 2
        // #three-d-transform-functions.
        "transform" => (&[NUMBER, ANGLE, PERCENTAGE, LENGTH_PERCENTAGE], true),
        // Filter Effects 1 #filter-functions: non-negative amounts, angles,
        // and lengths (blur radii, drop-shadow offsets).
        "filter" | "backdrop-filter" => (&[AMOUNT, ANGLE, LENGTH], true),
        _ => return None,
    })
}

/// How a resolved number is written: computed values keep the f32 value
/// layout reads; CSSOM resolved values use CSSOM
/// #serialize-a-css-component-value (at most six decimals).
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Computed,
    Resolved,
}

impl Production {
    fn kind(self) -> Kind {
        match self.kind {
            Type::Number => Kind::Number,
            Type::Integer => Kind::Integer,
            Type::NumberPercentage => Kind::NumberPercentage,
            Type::Percentage => Kind::Percentage,
            Type::Angle => Kind::Angle,
            Type::Time => Kind::Time,
            Type::Flex => Kind::Flex,
            Type::Length => Kind::Length,
            Type::LengthPercentage => Kind::LengthPercentage,
        }
    }

    /// The computed value of the component at `p`, in the production's
    /// canonical unit; `None` when it keeps a percentage for layout.
    fn resolve<'i>(self, p: &mut Parser<'i>, ctx: &Context<'_>) -> ParseResult<Option<f64>> {
        let kind = self.kind();
        let number = p.try_parse(|p| math::computed_number(p, &kind, ctx, self.min, self.max));
        if number.is_ok() || self.kind != Type::NumberPercentage {
            return number;
        }
        // A <percentage> is the equivalent number: 50% is 0.5.
        let (min, max) = (self.min * 100., self.max * 100.);
        Ok(math::computed_number(p, &Kind::Percentage, ctx, min, max)?.map(|n| n / 100.))
    }

    /// The computed value's text, or `None` for a length-percentage, which
    /// layout resolves.
    fn text(self, value: f64, format: Format) -> Option<String> {
        let number = |value: f64| match format {
            Format::Computed => math::number(value),
            Format::Resolved => crate::dom::cssom::css_number(value),
        };
        Some(match self.kind {
            // The largest representable integer (#calc-ieee).
            Type::Integer => {
                (value.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32).to_string()
            }
            Type::Number | Type::NumberPercentage => number(value),
            Type::Percentage => format!("{}%", number(value)),
            Type::Angle => format!("{}deg", number(value)),
            Type::Time => format!("{}s", number(value)),
            Type::Flex => format!("{}fr", number(value)),
            Type::Length => format!("{}px", number(value)),
            Type::LengthPercentage => return None,
        })
    }
}

/// Resolve the component at `p` against the first production it matches:
/// its value's text, `Ok(None)` when it matched a length-percentage, or `Err(())`
/// when it matches none (#calc-type-checking makes that declaration
/// invalid).
fn component<'i>(
    p: &mut Parser<'i>,
    productions: &[Production],
    ctx: &Context<'_>,
    format: Format,
) -> Result<Option<String>, ()> {
    for production in productions {
        if let Ok(value) = p.try_parse(|p| production.resolve(p, ctx)) {
            return Ok(value.and_then(|value| production.text(value, format)));
        }
    }
    Err(())
}

struct Rewrite<'a, 'c> {
    productions: &'a [Production],
    nested: bool,
    /// Also resolve numeric literals (`12turn` → `4320deg`).
    literals: bool,
    ctx: &'a Context<'c>,
    format: Format,
    out: String,
    changed: bool,
}

impl Rewrite<'_, '_> {
    /// Copy the component values at `p` to `out`, replacing each math
    /// function (and, for `literals`, numeric token) by its computed value.
    /// Returns where the copied block ends.
    fn block<'i>(
        &mut self,
        p: &mut Parser<'i>,
        depth: usize,
    ) -> ParseResult<cssparser::SourcePosition> {
        if depth > MAX_DEPTH {
            return Err(cssparser::ParseError::custom(()));
        }
        let mut start = p.position();
        loop {
            let before = p.state();
            let Ok(token) = p.next_including_whitespace_and_comments() else {
                break;
            };
            let numeric = match token {
                Token::Function(name) => crate::layout2::value::is_math_function_name(name),
                Token::Number { .. } | Token::Percentage { .. } | Token::Dimension { .. } => {
                    self.literals
                }
                _ => false,
            };
            let block = matches!(
                token,
                Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock
            );
            if numeric {
                self.out.push_str(p.slice(start..before.position()));
                p.reset(&before);
                match component(p, self.productions, self.ctx, self.format) {
                    Ok(Some(text)) => {
                        self.out.push_str(&text);
                        self.changed = true;
                    }
                    Ok(None) => self.out.push_str(p.slice_from(before.position())),
                    Err(()) if self.literals => {
                        // CSSOM only canonicalizes what it recognizes.
                        p.reset(&before);
                        if p.next_including_whitespace_and_comments().is_ok() && block {
                            p.parse_nested_block(|p| {
                                p.expect_no_error_token().map_err(Into::into)
                            })?;
                        }
                        self.out.push_str(p.slice_from(before.position()));
                    }
                    Err(()) => return Err(cssparser::ParseError::custom(())),
                }
                start = p.position();
            } else if block && self.nested {
                self.out.push_str(p.slice_from(start));
                start = p.parse_nested_block(|p| self.block(p, depth + 1))?;
            }
        }
        self.out.push_str(p.slice_from(start));
        Ok(p.position())
    }
}

/// Rewrite `property`'s `value` with its math functions resolved, or
/// `Ok(None)` when it has none to resolve.
fn rewrite(
    property: &str,
    value: &str,
    ctx: &Context<'_>,
    literals: bool,
    format: Format,
) -> Result<Option<String>, ()> {
    // Computed values are read for every property of every element: only a
    // value holding a function can need work.
    if !literals && !value.contains('(') {
        return Ok(None);
    }
    let Some((productions, nested)) = grammar(property) else {
        return Ok(None);
    };
    let mut rewrite = Rewrite {
        productions,
        nested,
        literals,
        ctx,
        format,
        out: String::with_capacity(value.len()),
        changed: false,
    };
    rewrite.block(&mut Parser::new(value), 0).map_err(|_| ())?;
    Ok(rewrite.changed.then_some(rewrite.out))
}

/// Parse-time type checking (#calc-type-checking): `value` with each math
/// function replaced by a literal of the production it resolves to, so the
/// property's literal grammar can validate the rest; `Err(())` when a math
/// function resolves to none of the property's productions. Values are
/// computed without an element (`em` is 16px), and a calculation clamps to
/// its production's range rather than invalidating the declaration
/// (#calc-range).
pub(in crate::dom) fn specified_literals<'v>(
    property: &str,
    value: &'v str,
) -> Result<Cow<'v, str>, ()> {
    Ok(
        match rewrite(
            property,
            value,
            &Context::validation(),
            false,
            Format::Computed,
        )? {
            Some(text) => Cow::Owned(text),
            None => Cow::Borrowed(value),
        },
    )
}

/// #calc-computed-value: `value` with each math function in `property`'s
/// numeric productions simplified, using the element's font and viewport
/// metrics, to the literal of its computed value. `Err(())` when one fails
/// to type check after `var()` substitution (CSS Variables 1
/// #invalid-at-computed-value-time).
pub(in crate::dom) fn computed(
    dom: &dyn Host,
    id: NodeId,
    pseudo: Option<PseudoEl>,
    property: &str,
    value: &str,
) -> Result<Option<String>, ()> {
    let ctx = Context {
        dom: Some(dom),
        id,
        pseudo,
        base: None,
        independent: false,
    };
    rewrite(property, value, &ctx, false, Format::Computed)
}

/// CSSOM #resolved-values: a computed value's numbers in their canonical
/// units, serialized per CSSOM (`12turn` → `4320deg`, `10ms` → `0.01s`).
/// `None` when nothing changes.
pub(in crate::dom) fn resolved(property: &str, value: &str) -> Option<String> {
    rewrite(
        property,
        value,
        &Context::validation(),
        true,
        Format::Resolved,
    )
    .ok()
    .flatten()
}

/// A top-level component of a declared value, classified by the property's
/// numeric productions.
enum Part<'a> {
    Numeric(Type, &'a str),
    Keyword(String),
}

/// Split `value` into its top-level components; `None` when one is neither
/// a numeric production of `property` nor a keyword.
fn parts<'a>(value: &'a str, productions: &[Production]) -> Option<Vec<Part<'a>>> {
    let mut parser = Parser::new(value);
    let mut parts = Vec::new();
    loop {
        parser.skip_whitespace();
        if parser.is_exhausted() {
            return Some(parts);
        }
        let start = parser.position();
        let numeric = productions.iter().find(|production| {
            parser
                .try_parse(|p| production.resolve(p, &Context::validation()))
                .is_ok()
        });
        if let Some(production) = numeric {
            parts.push(Part::Numeric(production.kind, parser.slice_from(start)));
        } else {
            parts.push(Part::Keyword(
                parser.expect_ident().ok()?.to_ascii_lowercase(),
            ));
        }
        if parts.len() > 4 {
            return None;
        }
    }
}

/// The value grammars of the numeric properties that no other parser checks
/// when a declaration is parsed: CSS Text 3 #tab-size-property
/// (`<number [0,∞]> | <length [0,∞]>`, so a percentage is invalid), and CSS
/// Transforms 2 #propdef-scale and #propdef-rotate. `None` for other
/// properties.
pub(in crate::dom) fn valid_value(property: &str, value: &str) -> Option<bool> {
    if !matches!(property, "tab-size" | "scale" | "rotate") {
        return None;
    }
    let (productions, _) = grammar(property)?;
    let Some(parts) = parts(value, productions) else {
        return Some(false);
    };
    let keyword = |part: &Part, names: &[&str]| match part {
        Part::Keyword(keyword) => names.contains(&keyword.as_str()),
        Part::Numeric(..) => false,
    };
    let number = |part: &Part| matches!(part, Part::Numeric(Type::Number, _));
    Some(match (property, parts.as_slice()) {
        ("tab-size", [Part::Numeric(..)]) => true,
        ("scale" | "rotate", [none]) if keyword(none, &["none"]) => true,
        ("scale", parts) => {
            (1..=3).contains(&parts.len())
                && parts.iter().all(|part| matches!(part, Part::Numeric(..)))
        }
        ("rotate", parts) => {
            // `none | <angle> | [ x | y | z | <number>{3} ] && <angle>`. A
            // unitless zero angle, which transform functions accept, is
            // kept for compatibility.
            let angle = |part: &Part| {
                matches!(
                    part,
                    Part::Numeric(Type::Angle, _) | Part::Numeric(Type::Number, "0")
                )
            };
            let axis = |parts: &[Part]| match parts {
                [] => true,
                [part] => keyword(part, &["x", "y", "z"]),
                [a, b, c] => number(a) && number(b) && number(c),
                _ => false,
            };
            match parts {
                [first, rest @ ..] if angle(first) && axis(rest) => true,
                [rest @ .., last] if angle(last) && axis(rest) => true,
                _ => false,
            }
        }
        _ => false,
    })
}

/// The value of a math function resolving to `production`, computed
/// without an element, as the animation shorthands and lists read it.
fn standalone(text: &str, production: Production) -> Option<f64> {
    if !crate::layout2::value::is_math_function(text) {
        return None;
    }
    let mut parser = Parser::new(text);
    let value = production
        .resolve(&mut parser, &Context::validation())
        .ok()
        .flatten()?;
    parser.expect_exhausted().ok()?;
    Some(value)
}

/// A math function resolving to <time>, in seconds.
pub(in crate::dom) fn math_seconds(text: &str) -> Option<f64> {
    standalone(text, TIME)
}

/// A math function resolving to <number [0,∞]> (`animation-iteration-count`).
pub(in crate::dom) fn math_count(text: &str) -> Option<f64> {
    standalone(text, NON_NEGATIVE)
}

/// Whether `text` holds a math function anywhere in its component values.
pub(in crate::dom) fn has_math_function(text: &str) -> bool {
    fn scan<'i>(p: &mut Parser<'i>, depth: usize) -> ParseResult<bool> {
        if depth > MAX_DEPTH {
            return Ok(false);
        }
        while let Ok(token) = p.next() {
            let math = match token {
                Token::Function(name) => crate::layout2::value::is_math_function_name(name),
                Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                    false
                }
                _ => continue,
            };
            if math || p.parse_nested_block(|p| scan(p, depth + 1))? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    text.contains('(') && scan(&mut Parser::new(text), 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::cssom::supports;

    #[test]
    fn math_functions_type_check_against_numeric_productions() {
        // CSS Values 4 #calc-type-checking; WPT css/css-values *-invalid.
        for (property, value) in [
            ("z-index", "round(23, 10)"),
            ("z-index", "calc(1 + 1)"),
            ("order", "calc(1.5)"),
            ("opacity", "calc(100% / 3)"),
            ("opacity", "exp(1)"),
            // Typed OM #cssnumericvalue-match: opacity resolves percentages
            // against numbers, so a percent hint is allowed.
            ("opacity", "sign(10%)"),
            ("scale", "calc(10% / 1%)"),
            ("scale", "round(Infinity, 0)"),
            ("flex-grow", "calc(-1)"),
            ("column-count", "calc(0)"),
            ("font-weight", "calc(2000)"),
            ("tab-size", "calc(2 * 4)"),
            ("tab-size", "calc(1em + 2px)"),
            ("line-height", "calc(1 + 0.5)"),
            ("line-height", "calc(100% + 2px)"),
            ("scale", "calc(1 / 3) 50%"),
            ("rotate", "asin(1)"),
            ("rotate", "x calc(30deg)"),
            ("rotate", "1 0 0 atan2(1px, 1px)"),
            ("transition-delay", "abs(-1s), calc(1s / sign(1em - 16px))"),
            ("transition-duration", "calc(-1s)"),
            ("animation-duration", "calc(NaN * 1s)"),
            ("transition", "opacity abs(1s) calc(2 * 50ms)"),
            (
                "grid-template-rows",
                "calc(3fr + 1fr * sign(42px - 2em)) 10px",
            ),
            (
                "grid-template-columns",
                "repeat(calc(2), minmax(0, calc(1fr * 2)))",
            ),
            (
                "transform",
                "rotate(asin(1)) scale(calc(50%)) translate(calc(50% + 1em))",
            ),
            (
                "filter",
                "brightness(calc(150%)) hue-rotate(calc(0.25turn)) blur(calc(1em / 4))",
            ),
        ] {
            assert!(supports(property, value), "{property}: {value}");
        }
        for (property, value) in [
            ("z-index", "calc(1px)"),
            ("z-index", "round(1, 1%)"),
            ("opacity", "calc(.25 + 25%)"),
            ("opacity", "exp(0px)"),
            ("opacity", "round(1, nearest)"),
            ("font-weight", "abs(1, 2)"),
            ("font-weight", "sign(10%)"),
            ("z-index", "calc(10% / 1%)"),
            ("tab-size", "abs(10%)"),
            ("tab-size", "10%"),
            ("tab-size", "-1"),
            ("tab-size", "1px * sign(10%)"),
            ("line-height", "calc(1px + 1)"),
            ("scale", "1deg"),
            ("rotate", "sin(1deg)"),
            ("rotate", "x y 30deg"),
            ("transition-delay", "abs(1px)"),
            ("transition-duration", "calc(1s + 1)"),
            ("grid-template-rows", "calc(1fr + 1px)"),
            ("transform", "rotate(asin())"),
            ("transform", "rotate(sin(1dag))"),
            ("transform", "rotate(sin(1deg))"),
            ("filter", "blur(calc(1deg))"),
            ("filter", "opacity(calc(1px))"),
        ] {
            assert!(!supports(property, value), "{property}: {value}");
        }
    }

    #[test]
    fn numeric_math_functions_compute_to_their_values() {
        // CSS Values 4 #calc-computed-value, #calc-range and #calc-ieee.
        let dom = Dom::parse_document(
            "<!doctype html><style>
             #a { z-index: round(23, 10); order: calc(-1.5); opacity: calc(100% / 3);
                  scale: calc(1 / sign(1em - 20px)) calc(50%); rotate: asin(1);
                  transition-delay: calc(5s + 15s * sign(42px - 2em)), 10ms;
                  grid-template-rows: calc(3fr + 1fr * sign(42px - 2em)) 10px;
                  font-size: 20px; column-count: calc(-5); flex-grow: calc(1 / 3);
                  font-weight: calc(100 * 4.5); tab-size: calc(infinity);
                  filter: brightness(calc(150%)) hue-rotate(calc(0.25turn)) blur(calc(1em / 4)) }
             #b { z-index: calc(infinity); order: calc(NaN); opacity: calc(-infinity);
                  rotate: z 1turn; scale: 50%; transition-delay: calc(-infinity * 1s);
                  line-height: calc(3 / 2); animation-duration: calc(1s / 0);
                  font-weight: calc(300 + 0 * 1em / 1px) }
             #c { z-index: calc(1 / min(0, -1 * 0)); order: calc(var(--n) * 2); --n: 3 }
             #d { z-index: 7; z-index: calc(1px) }
             #e { font-weight: calc(300 + 0 * 1ch / 1px) }
             </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div>
             <div id=e></div>",
        );
        let value = |id: &str, property: &str| {
            dom.cssom_resolved_value(dom.get_by_id(id).unwrap(), property)
                .unwrap_or_default()
        };
        for (id, property, expected) in [
            ("a", "z-index", "20"),
            ("a", "order", "-1"),
            ("a", "opacity", "0.333333"),
            ("a", "scale", "340282346638528859811704183484516925440 0.5"),
            ("a", "rotate", "90deg"),
            ("a", "transition-delay", "20s, 0.01s"),
            ("a", "grid-template-rows", "4fr 10px"),
            ("a", "column-count", "1"),
            ("a", "flex-grow", "0.333333"),
            ("a", "font-weight", "450"),
            ("a", "tab-size", "340282346638528859811704183484516925440"),
            ("b", "z-index", "2147483647"),
            ("b", "order", "0"),
            ("b", "opacity", "0"),
            ("b", "rotate", "z 360deg"),
            ("b", "scale", "0.5"),
            (
                "b",
                "transition-delay",
                "-340282346638528859811704183484516925440s",
            ),
            (
                "b",
                "animation-duration",
                "340282346638528859811704183484516925440s",
            ),
            ("b", "font-weight", "300"),
            // `ch` measures the font whose weight this computes: a unit
            // cycle, which computes as inherited.
            ("e", "font-weight", "400"),
            // 1 / 0⁻ is −∞ (#calc-ieee): min() orders 0⁻ below 0⁺.
            ("c", "z-index", "-2147483648"),
            ("c", "order", "6"),
            // An invalid declaration leaves the earlier one in force.
            ("d", "z-index", "7"),
        ] {
            assert_eq!(value(id, property), expected, "#{id} {property}");
        }
        let b = dom.get_by_id("b").unwrap();
        assert_eq!(
            dom.computed_value_resolved(b, "line-height").as_deref(),
            Some("1.5")
        );
        let a = dom.get_by_id("a").unwrap();
        // Layout reads the computed value.
        assert_eq!(
            dom.computed_value_resolved(a, "z-index").as_deref(),
            Some("20")
        );
        assert_eq!(
            dom.computed_value_resolved(a, "rotate").as_deref(),
            Some("90deg")
        );
        assert_eq!(
            dom.computed_value_resolved(a, "filter").as_deref(),
            Some("brightness(1.5) hue-rotate(90deg) blur(5px)")
        );
    }
}
