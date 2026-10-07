//! CSS Properties and Values API Level 1.
//!
//! Registration, syntax strings, and computed-value matching follow the local
//! Houdini snapshot 954df531741a9d2da94aec82b073b3f9b5af5573 (2026-09-06),
//! #determining-registration, #at-property-rule, #the-registerproperty-function,
//! #consume-syntax-definition, and #calculation-of-computed-values.
//! That draft permits omitted descriptors and comma-separated registration
//! names. Its multi-name CSSOM issue (#14227) is still unresolved.
mod gradients;
mod math;
mod matrix;
pub(super) mod numeric;
mod resolution;
mod values;
pub(super) use matrix::transform_list_matrix;
pub(super) use resolution::State;
pub(super) use resolution::{registry_bytes, substitute};

use super::*;
use cssparser::{Parser, Token};
use std::sync::Arc;

pub(super) type Registry = FxHashMap<String, Arc<Registration>>;
pub(super) type ParseResult<T> = Result<T, cssparser::ParseError<()>>;
const MAX_DEPTH: usize = 64;
const MAX_COMPONENTS: usize = 4096;

#[derive(Clone, Debug)]
pub(super) struct Registration {
    pub syntax_text: String,
    pub syntax: Syntax,
    pub inherits: bool,
    pub initial: Option<String>,
    pub base: Option<url::Url>,
}

#[derive(Clone, Debug)]
pub(super) enum Syntax {
    Universal,
    Alternatives(Vec<Component>),
}

#[derive(Clone, Debug)]
pub(super) struct Component {
    kind: Kind,
    multiplier: Option<char>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Length,
    Number,
    Percentage,
    LengthPercentage,
    String,
    Color,
    Image,
    Url,
    Integer,
    Angle,
    Time,
    Resolution,
    /// <flex>, used by grid track lists; not a registration syntax name.
    Flex,
    /// A <number> where <percentage>s resolve against numbers (CSS Color 4
    /// <opacity-value>, Transforms 2 `scale`), so a calculation may hold
    /// one (`sign(10%)`); not a registration syntax name.
    NumberPercentage,
    TransformFunction,
    TransformList,
    CustomIdent,
    Ident(String),
    /// Used by conic-gradient geometry, not a registration syntax name.
    AnglePercentage,
}

impl Kind {
    fn data_type(name: &str) -> Option<Self> {
        Some(match name {
            "<length>" => Self::Length,
            "<number>" => Self::Number,
            "<percentage>" => Self::Percentage,
            "<length-percentage>" => Self::LengthPercentage,
            "<string>" => Self::String,
            "<color>" => Self::Color,
            "<image>" => Self::Image,
            "<url>" => Self::Url,
            "<integer>" => Self::Integer,
            "<angle>" => Self::Angle,
            "<time>" => Self::Time,
            "<resolution>" => Self::Resolution,
            "<transform-function>" => Self::TransformFunction,
            "<transform-list>" => Self::TransformList,
            "<custom-ident>" => Self::CustomIdent,
            _ => return None,
        })
    }
}

/// Whether `text` is a `<color>` (named, legacy, modern, relative or system
/// color, or `currentcolor`) as the color properties accept it.
pub(super) fn is_color(text: &str) -> bool {
    values::computed_color(text).is_some()
}

/// CSS Conditional 5 #container-lengths: container query length units in a
/// property value compute to absolute lengths, each against its axis's
/// nearest eligible query container (recorded as a dependency, so layout
/// settles again when that container resizes) or the small viewport size.
/// `None` when `value` has no such unit. Strings and URLs are untouched.
pub(in crate::dom) fn resolve_container_units<B: StyleBackend + ?Sized>(
    dom: &ComputeView<'_, B>,
    id: NodeId,
    value: &str,
) -> Option<String> {
    if !value
        .as_bytes()
        .windows(2)
        .any(|pair| pair.eq_ignore_ascii_case(b"cq"))
    {
        return None;
    }
    let ctx = Context {
        dom: Some(dom),
        id,
        pseudo: None,
        base: None,
        independent: false,
    };
    fn rewrite<'i>(
        p: &mut Parser<'i>,
        ctx: &Context<'_>,
        out: &mut String,
        changed: &mut bool,
    ) -> ParseResult<cssparser::SourcePosition> {
        let mut start = p.position();
        loop {
            let before = p.position();
            let Ok(token) = p.next_including_whitespace_and_comments() else {
                break;
            };
            match token.clone() {
                Token::Dimension { value, unit, .. } => {
                    let unit = unit.to_ascii_lowercase();
                    if matches!(
                        unit.as_str(),
                        "cqw" | "cqh" | "cqi" | "cqb" | "cqmin" | "cqmax"
                    ) && let Some(scale) = values::length_scale(&unit, ctx)
                    {
                        out.push_str(p.slice(start..before));
                        out.push_str(&math::number(f64::from(value) * scale));
                        out.push_str("px");
                        start = p.position();
                        *changed = true;
                    }
                }
                Token::Function(_)
                | Token::ParenthesisBlock
                | Token::SquareBracketBlock
                | Token::CurlyBracketBlock => {
                    out.push_str(p.slice_from(start));
                    start = p.parse_nested_block(|p| rewrite(p, ctx, out, changed))?;
                }
                _ => {}
            }
        }
        out.push_str(p.slice_from(start));
        Ok(p.position())
    }
    let mut parser = Parser::new(value);
    let mut out = String::with_capacity(value.len());
    let mut changed = false;
    rewrite(&mut parser, &ctx, &mut out, &mut changed).ok()?;
    changed.then_some(out)
}

/// The computed value of a standard property's `<length-percentage>` (CSS
/// Values 4 #calc-computed-value): lengths become absolute px with the
/// element's (or pseudo-element's) font and viewport metrics, percentages
/// stay, and a math function is simplified with that information. A result
/// without a percentage is one numeric value, clamped to `[minimum, ∞]`
/// (#calc-range; NaN censored to zero, #calc-ieee); one with a percentage
/// keeps its sorted calculation (#calc-serialize). `None` when `value` is not
/// one `<length-percentage>` (a keyword, or a token this grammar rejects).
pub(in crate::dom) fn computed_length_percentage<B: StyleBackend + ?Sized>(
    dom: &ComputeView<'_, B>,
    id: NodeId,
    pseudo: Option<PseudoEl>,
    value: &str,
    minimum: f64,
) -> Option<String> {
    let ctx = Context {
        dom: Some(dom),
        id,
        pseudo,
        base: None,
        independent: false,
    };
    let mut parser = Parser::new(value);
    let computed = math::parse_range(
        &mut parser,
        &Kind::LengthPercentage,
        &ctx,
        0,
        minimum,
        f64::INFINITY,
    )
    .ok()?;
    parser.expect_exhausted().ok()?;
    Some(computed)
}

/// CSS Color 4 #resolving-color-values for CSSOM: sRGB-family colors as
/// `rgb()`/`rgba()`, other spaces in their own notation. `None` for
/// `currentcolor`, system colors and non-colors, which the caller resolves.
pub(crate) fn resolved_color(text: &str) -> Option<String> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("currentcolor") || system_color(text) {
        return None;
    }
    values::computed_color(text)
}

fn system_color(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "canvas"
            | "canvastext"
            | "field"
            | "fieldtext"
            | "buttonface"
            | "buttontext"
            | "buttonborder"
            | "linktext"
            | "visitedtext"
            | "activetext"
            | "graytext"
            | "highlight"
            | "highlighttext"
            | "selecteditem"
            | "selecteditemtext"
            | "mark"
            | "marktext"
            | "accentcolor"
            | "accentcolortext"
    )
}

pub(super) fn ident(text: &str) -> Option<String> {
    let mut parser = Parser::new(text);
    let name = parser.expect_ident_cloned().ok()?.to_string();
    parser.expect_exhausted().ok()?;
    Some(name)
}

fn custom_ident(name: &str) -> bool {
    wide_keyword(name).is_none()
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "default" | "revert-rule"
        )
}

pub(super) fn identifier_text(name: &str) -> String {
    let mut out = String::new();
    cssparser::serialize_identifier(name, &mut out).unwrap();
    out
}

pub(super) fn string_text(value: &str) -> String {
    let mut out = String::new();
    cssparser::serialize_string(value, &mut out).unwrap();
    out
}

impl Syntax {
    pub fn parse(text: &str) -> Option<Self> {
        let whitespace = |c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}');
        let mut rest = text.trim_matches(whitespace);
        if rest == "*" {
            return Some(Self::Universal);
        }
        let mut components = Vec::new();
        loop {
            rest = rest.trim_start_matches(whitespace);
            let (kind, consumed) = if rest.starts_with('<') {
                let end = rest.find('>')? + 1;
                (Kind::data_type(&rest[..end])?, end)
            } else {
                let mut parser = Parser::new(rest);
                // Syntax strings are code-point grammars: comments are not
                // whitespace and must not silently disappear here.
                let Token::Ident(name) = parser.next_including_whitespace_and_comments().ok()?
                else {
                    return None;
                };
                let name = name.to_string();
                if !custom_ident(&name) {
                    return None;
                }
                (Kind::Ident(name), parser.position().byte_index())
            };
            rest = &rest[consumed..];
            let multiplier = if kind != Kind::TransformList && rest.starts_with(['+', '#']) {
                let multiplier = rest.chars().next();
                rest = &rest[1..];
                multiplier
            } else {
                None
            };
            components.push(Component { kind, multiplier });
            if components.len() > MAX_COMPONENTS {
                return None;
            }
            rest = rest.trim_start_matches(whitespace);
            if rest.is_empty() {
                return Some(Self::Alternatives(components));
            }
            rest = rest.strip_prefix('|')?;
        }
    }

    pub fn compute(&self, text: &str, context: &Context<'_>) -> Option<String> {
        if !valid_tokens(text)
            || ident(text)
                .as_deref()
                .is_some_and(|s| wide_keyword(s).is_some())
        {
            return None;
        }
        match self {
            Self::Universal => {
                if context.independent && has_substitution(text) {
                    return None;
                }
                Some(text.to_owned())
            }
            Self::Alternatives(components) => components.iter().find_map(|component| {
                let mut parser = Parser::new(text);
                let mut parts = Vec::new();
                loop {
                    if parts.len() >= MAX_COMPONENTS {
                        return None;
                    }
                    parts.push(values::parse(&mut parser, &component.kind, context, 0).ok()?);
                    if component.multiplier.is_none() || parser.is_exhausted() {
                        break;
                    }
                    if component.multiplier == Some('#') {
                        parser.expect_comma().ok()?;
                    }
                }
                parser.expect_exhausted().ok()?;
                Some(parts.join(if component.multiplier == Some('#') {
                    ", "
                } else {
                    " "
                }))
            }),
        }
    }
}

/// What computing a registered property's value reads from its element:
/// font, line and viewport metrics for relative lengths (CSS Values 4
/// #relative-lengths) and query containers for container lengths (CSS
/// Conditional 5 #container-lengths).
pub(in crate::dom) trait Host {
    fn style_scope_root_element(&self, id: NodeId) -> Option<NodeId>;
    fn font_px(&self, id: NodeId) -> f32;
    fn root_font_px(&self) -> f32;
    fn pseudo_layout_value(&self, id: NodeId, which: PseudoEl, name: &str) -> Option<String>;
    fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String>;
    fn style_parent(&self, id: NodeId) -> Option<NodeId>;
    fn viewport_px(&self) -> (f32, f32);
    fn device_pixel_ratio(&self) -> f32;
    fn record_container_read(&self, subject: NodeId, container: NodeId, axes: u8, units: bool);
    fn container_size(&self, container: NodeId) -> Option<[f32; 2]>;
}

impl<B: StyleBackend + ?Sized> Host for ComputeView<'_, B> {
    fn style_scope_root_element(&self, id: NodeId) -> Option<NodeId> {
        ComputeView::style_scope_root_element(self, id)
    }
    fn font_px(&self, id: NodeId) -> f32 {
        ComputeView::font_px(self, id)
    }
    fn root_font_px(&self) -> f32 {
        ComputeView::root_font_px(self)
    }
    fn pseudo_layout_value(&self, id: NodeId, which: PseudoEl, name: &str) -> Option<String> {
        ComputeView::pseudo_layout_value(self, id, which, name)
    }
    fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        ComputeView::computed_value_resolved(self, id, name)
    }
    fn style_parent(&self, id: NodeId) -> Option<NodeId> {
        self.0.style_parent(id)
    }
    fn viewport_px(&self) -> (f32, f32) {
        self.0.viewport_px()
    }
    fn device_pixel_ratio(&self) -> f32 {
        self.0.device_pixel_ratio()
    }
    fn record_container_read(&self, subject: NodeId, container: NodeId, axes: u8, units: bool) {
        self.0
            .record_container_read(subject, container, axes, units)
    }
    fn container_size(&self, container: NodeId) -> Option<[f32; 2]> {
        self.0.container_size(container)
    }
}

pub(super) struct Context<'a> {
    pub dom: Option<&'a dyn Host>,
    pub id: NodeId,
    pub pseudo: Option<PseudoEl>,
    pub base: Option<&'a url::Url>,
    pub independent: bool,
}

impl Context<'_> {
    fn validation() -> Self {
        Self {
            dom: None,
            id: DOCUMENT,
            pseudo: None,
            base: None,
            independent: false,
        }
    }
    fn independent() -> Self {
        Self {
            independent: true,
            ..Self::validation()
        }
    }
}

/// Reuse CSS Values' typed math evaluator for transform numbers and angles.
/// Percentage scale values are numbers divided by 100 (Transforms 2
/// #individual-transforms), and a scale calculation may hold a percentage
/// that resolves against a number (`scale(sign(10%))`). Length percentages
/// keep their used-value basis in layout's retained length expressions
/// instead of being flattened here.
pub(super) fn transform_number(text: &str, angle: bool, percentage: bool) -> Option<f32> {
    if angle && text.trim() == "0" {
        return Some(0.);
    }
    let parse = |kind: Kind| {
        let mut parser = Parser::new(text);
        let (min, max) = (f64::NEG_INFINITY, f64::INFINITY);
        let value = math::computed_number(&mut parser, &kind, &Context::validation(), min, max)
            .ok()
            .flatten()?;
        parser.expect_exhausted().ok()?;
        Some(value as f32).filter(|n| n.is_finite())
    };
    if angle {
        parse(Kind::Angle)
    } else if percentage {
        parse(Kind::NumberPercentage).or_else(|| parse(Kind::Percentage).map(|n| n / 100.))
    } else {
        parse(Kind::Number)
    }
}

/// CSS Transforms 1 #transform-property for a value holding a math function:
/// `none` or a <transform-list> whose functions' arguments type check
/// (Transforms 2 #transform-functions), so `rotate(sin(1deg))`, a number
/// where an angle belongs, is invalid. Layout parses the remaining values.
pub(super) fn valid_transform(text: &str) -> bool {
    if !numeric::has_math_function(text) {
        return true;
    }
    let mut parser = Parser::new(text);
    values::parse(&mut parser, &Kind::TransformList, &Context::validation(), 0).is_ok()
        && parser.expect_exhausted().is_ok()
}

/// Scan component values with the CSS Syntax tokenizer. Strings, escaped
/// identifiers, URL tokens and nested blocks must not be searched as raw text.
pub(super) fn valid_tokens(text: &str) -> bool {
    fn scan<'i>(parser: &mut Parser<'i>, depth: usize) -> ParseResult<()> {
        if depth > MAX_DEPTH {
            return Err(cssparser::ParseError::custom(()));
        }
        while !parser.is_exhausted() {
            let token = parser.next()?.clone();
            if token.is_parse_error()
                || depth == 0 && matches!(token, Token::Semicolon | Token::Delim('!'))
            {
                return Err(cssparser::ParseError::custom(()));
            }
            if matches!(
                token,
                Token::Function(_)
                    | Token::ParenthesisBlock
                    | Token::CurlyBracketBlock
                    | Token::SquareBracketBlock
            ) {
                parser.parse_nested_block(|p| scan(p, depth + 1))?;
            }
        }
        Ok(())
    }
    scan(&mut Parser::new(text), 0).is_ok()
}

fn has_substitution(text: &str) -> bool {
    fn scan<'i>(p: &mut Parser<'i>, depth: usize) -> ParseResult<bool> {
        if depth > MAX_DEPTH {
            return Ok(true);
        }
        let mut found = false;
        while !p.is_exhausted() {
            let token = p.next()?.clone();
            if let Token::Function(name) = &token {
                found |= matches!(
                    name.to_ascii_lowercase().as_str(),
                    "var" | "env" | "attr" | "inherit" | "random" | "random-item"
                );
            }
            if matches!(
                token,
                Token::Function(_)
                    | Token::ParenthesisBlock
                    | Token::CurlyBracketBlock
                    | Token::SquareBracketBlock
            ) {
                found |= p.parse_nested_block(|p| scan(p, depth + 1))?;
            }
        }
        Ok(found)
    }
    scan(&mut Parser::new(text), 0).unwrap_or(true)
}

pub(super) struct PropertyRule {
    pub names: Vec<String>,
    pub registration: Registration,
}

/// `after` starts immediately after the @ delimiter. Tokenization recognizes
/// escaped/case-insensitive at-keywords and keeps strings in the prelude data.
pub(super) fn consume_rule(after: &str) -> Option<(Option<PropertyRule>, &str)> {
    let mut p = Parser::new(after);
    if !p.expect_ident().ok()?.eq_ignore_ascii_case("property") {
        return None;
    }
    let start = p.position().byte_index();
    loop {
        p.skip_whitespace();
        let position = p.position().byte_index();
        match p.next() {
            Ok(Token::CurlyBracketBlock) => {
                let (body, tail) = take_block(&after[position..]);
                return Some((PropertyRule::parse(&after[start..position], body), tail));
            }
            Ok(Token::Semicolon) => return Some((None, &after[p.position().byte_index()..])),
            Err(_) => return Some((None, "")),
            _ => {}
        }
    }
}

impl PropertyRule {
    pub fn parse(prelude: &str, body: &str) -> Option<Self> {
        let mut p = Parser::new(prelude);
        let names = p
            .parse_comma_separated(|p| {
                let name = p.expect_ident_cloned()?.to_string();
                if name.starts_with("--") && name != "--" {
                    Ok(name)
                } else {
                    Err(cssparser::ParseError::<()>::custom(()))
                }
            })
            .ok()?;
        p.expect_exhausted().ok()?;
        let mut syntax_text = String::from("*");
        let mut syntax = Syntax::Universal;
        let mut inherits = true;
        let mut initial_candidates = Vec::new();
        for declaration in split_top_level(body, ';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            let Some(name) = ident(name) else {
                continue;
            };
            let value = value.trim();
            if !valid_tokens(value) {
                continue;
            }
            match name.to_ascii_lowercase().as_str() {
                "syntax" => {
                    let mut p = Parser::new(value);
                    if let Ok(value) = p.expect_string_cloned()
                        && p.expect_exhausted().is_ok()
                        && let Some(parsed) = Syntax::parse(&value)
                    {
                        syntax_text = value.to_string();
                        syntax = parsed;
                    }
                }
                "inherits" => match ident(value).map(|v| v.to_ascii_lowercase()).as_deref() {
                    Some("true") => inherits = true,
                    Some("false") => inherits = false,
                    _ => {}
                },
                "initial-value" => initial_candidates.push(value),
                _ => {}
            }
        }
        // Invalid descriptors are ignored, including a later invalid value.
        // The descriptor's original token stream remains observable in CSSOM;
        // only values used by elements are converted to computed values.
        let initial = initial_candidates
            .into_iter()
            .rev()
            .find(|value| syntax.compute(value, &Context::independent()).is_some())
            .map(str::to_owned);
        Some(Self {
            names,
            registration: Registration {
                syntax_text,
                syntax,
                inherits,
                initial,
                base: None,
            },
        })
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({ "t": "property", "name": self.names.join(", "), "names": self.names,
            "syntax": self.registration.syntax_text, "inherits": self.registration.inherits,
            "initialValue": self.registration.initial })
    }
}

impl Registration {
    pub fn retained_bytes(&self) -> usize {
        self.syntax_text.capacity()
            + self.initial.as_ref().map_or(0, String::capacity)
            + self.base.as_ref().map_or(0, |v| v.as_str().len())
            + match &self.syntax {
                Syntax::Universal => 0,
                Syntax::Alternatives(v) => {
                    v.capacity() * std::mem::size_of::<Component>()
                        + v.iter()
                            .map(|c| {
                                if let Kind::Ident(v) = &c.kind {
                                    v.capacity()
                                } else {
                                    0
                                }
                            })
                            .sum::<usize>()
                }
            }
    }
}

#[cfg(test)]
mod tests;
