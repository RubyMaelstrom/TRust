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
mod resolution;
mod values;
pub(super) use resolution::State;
pub(super) use resolution::{registry_bytes, substitute};

use super::*;
use cssparser::{Parser, ParserInput, Token};
use std::rc::Rc;

pub(super) type Registry = FxHashMap<String, Rc<Registration>>;
pub(super) type ParseResult<'i, T> = Result<T, cssparser::ParseError<'i, ()>>;
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

pub(super) fn ident(text: &str) -> Option<String> {
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
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
                let mut input = ParserInput::new(rest);
                let mut parser = Parser::new(&mut input);
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
                let mut input = ParserInput::new(text);
                let mut parser = Parser::new(&mut input);
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

pub(super) struct Context<'a> {
    pub dom: Option<&'a Dom>,
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

/// Scan component values with the CSS Syntax tokenizer. Strings, escaped
/// identifiers, URL tokens and nested blocks must not be searched as raw text.
pub(super) fn valid_tokens(text: &str) -> bool {
    fn scan<'i>(parser: &mut Parser<'i, '_>, depth: usize) -> ParseResult<'i, ()> {
        if depth > MAX_DEPTH {
            return Err(parser.new_custom_error(()));
        }
        while !parser.is_exhausted() {
            let token = parser.next()?.clone();
            if token.is_parse_error()
                || depth == 0 && matches!(token, Token::Semicolon | Token::Delim('!'))
            {
                return Err(parser.new_custom_error(()));
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
    scan(&mut Parser::new(&mut ParserInput::new(text)), 0).is_ok()
}

fn has_substitution(text: &str) -> bool {
    fn scan<'i>(p: &mut Parser<'i, '_>, depth: usize) -> ParseResult<'i, bool> {
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
    scan(&mut Parser::new(&mut ParserInput::new(text)), 0).unwrap_or(true)
}

pub(super) struct PropertyRule {
    pub names: Vec<String>,
    pub registration: Registration,
}

/// `after` starts immediately after the @ delimiter. Tokenization recognizes
/// escaped/case-insensitive at-keywords and keeps strings in the prelude data.
pub(super) fn consume_rule(after: &str) -> Option<(Option<PropertyRule>, &str)> {
    let mut input = ParserInput::new(after);
    let mut p = Parser::new(&mut input);
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
        let mut input = ParserInput::new(prelude);
        let mut p = Parser::new(&mut input);
        let names = p
            .parse_comma_separated(|p| {
                let name = p.expect_ident_cloned()?.to_string();
                if name.starts_with("--") && name != "--" {
                    Ok(name)
                } else {
                    Err(p.new_custom_error::<_, ()>(()))
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
                    let mut input = ParserInput::new(value);
                    let mut p = Parser::new(&mut input);
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
