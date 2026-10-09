//! CSS Fonts 4 properties beyond family, size and weight: grammars,
//! canonical specified and computed values, and the `font-variant`,
//! `font-synthesis` and `font` shorthands (csswg-drafts 81c27f686901;
//! CSS Fonts 5 for `font-size-adjust`). CSSOM #serializing-css-values:
//! keywords serialize in grammar order, in their shortest form.

use cssparser::{Parser, Token};

use crate::text::typography::{
    EAST_ASIAN_VARIANTS, WIDTH_KEYWORDS, opentype_tag, parse_angle_deg, parse_caps,
    parse_east_asian, parse_feature_settings, parse_ligatures, parse_numeric,
    parse_variation_settings,
};

/// The longhands this module defines, with their initial values. All are
/// inherited (#font-width-prop … #font-size-adjust-prop).
pub(in crate::dom) const LONGHANDS: [(&str, &str); 17] = [
    ("font-width", "normal"),
    ("font-kerning", "auto"),
    ("font-optical-sizing", "auto"),
    ("font-variant-ligatures", "normal"),
    ("font-variant-caps", "normal"),
    ("font-variant-numeric", "normal"),
    ("font-variant-east-asian", "normal"),
    ("font-variant-position", "normal"),
    ("font-variant-alternates", "normal"),
    ("font-variant-emoji", "normal"),
    ("font-feature-settings", "normal"),
    ("font-variation-settings", "normal"),
    ("font-synthesis-weight", "auto"),
    ("font-synthesis-style", "auto"),
    ("font-synthesis-small-caps", "auto"),
    ("font-synthesis-position", "auto"),
    ("font-size-adjust", "none"),
];

/// `font-variant`'s longhands in grammar order.
const VARIANT_LONGHANDS: [&str; 7] = [
    "font-variant-ligatures",
    "font-variant-caps",
    "font-variant-alternates",
    "font-variant-numeric",
    "font-variant-east-asian",
    "font-variant-position",
    "font-variant-emoji",
];

/// `font-synthesis`'s longhands with their keywords.
const SYNTHESIS_LONGHANDS: [(&str, &str); 4] = [
    ("font-synthesis-weight", "weight"),
    ("font-synthesis-style", "style"),
    ("font-synthesis-small-caps", "small-caps"),
    ("font-synthesis-position", "position"),
];

/// #font-prop: the reset-only sub-properties of `font` (the Reset
/// Implicitly group TRust implements).
pub(in crate::dom) const FONT_RESETS: [(&str, &str); 12] = [
    ("font-feature-settings", "normal"),
    ("font-kerning", "auto"),
    ("font-optical-sizing", "auto"),
    ("font-size-adjust", "none"),
    ("font-variant-alternates", "normal"),
    ("font-variant-east-asian", "normal"),
    ("font-variant-emoji", "normal"),
    ("font-variant-ligatures", "normal"),
    ("font-variant-numeric", "normal"),
    ("font-variant-position", "normal"),
    ("font-variation-settings", "normal"),
    ("font-width", "normal"),
];

/// Whether this module owns `name`'s grammar.
pub(in crate::dom) fn owns(name: &str) -> bool {
    LONGHANDS.iter().any(|(n, _)| *n == name)
}

pub(in crate::dom) fn initial(name: &str) -> Option<&'static str> {
    LONGHANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, initial)| *initial)
}

/// The canonical specified value of longhand `name`, or `None` when
/// `value` does not match its grammar. `value` has no CSS-wide keyword or
/// `var()`.
pub(in crate::dom) fn specified(name: &str, value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let one_of = |words: &[&str]| words.contains(&lower.as_str()).then(|| lower.clone());
    match name {
        // #font-width-prop: `normal | <percentage [0,∞]> | <keyword>`.
        "font-width" => {
            if WIDTH_KEYWORDS.iter().any(|(k, _)| *k == lower) {
                return Some(lower);
            }
            let percent = percentage(value)?;
            (percent >= 0.0).then(|| format!("{}%", number(percent)))
        }
        "font-style" => font_style(value),
        "font-kerning" => one_of(&["auto", "normal", "none"]),
        "font-optical-sizing" => one_of(&["auto", "none"]),
        "font-variant-ligatures" => parse_ligatures(&words(&lower)?).map(|l| {
            if l.none {
                return "none".into();
            }
            let mut out = Vec::new();
            for (slot, on, off) in [
                (l.common, "common-ligatures", "no-common-ligatures"),
                (
                    l.discretionary,
                    "discretionary-ligatures",
                    "no-discretionary-ligatures",
                ),
                (
                    l.historical,
                    "historical-ligatures",
                    "no-historical-ligatures",
                ),
                (l.contextual, "contextual", "no-contextual"),
            ] {
                if let Some(enabled) = slot {
                    out.push(if enabled { on } else { off });
                }
            }
            keywords(out)
        }),
        "font-variant-caps" => parse_caps(&lower).map(|_| lower.clone()),
        "font-variant-numeric" => parse_numeric(&words(&lower)?).map(|n| {
            let mut out = Vec::new();
            if let Some(lining) = n.lining {
                out.push(if lining {
                    "lining-nums"
                } else {
                    "oldstyle-nums"
                });
            }
            if let Some(tabular) = n.tabular {
                out.push(if tabular {
                    "tabular-nums"
                } else {
                    "proportional-nums"
                });
            }
            if let Some(stacked) = n.stacked {
                out.push(if stacked {
                    "stacked-fractions"
                } else {
                    "diagonal-fractions"
                });
            }
            if n.ordinal {
                out.push("ordinal");
            }
            if n.slashed_zero {
                out.push("slashed-zero");
            }
            keywords(out)
        }),
        "font-variant-east-asian" => parse_east_asian(&words(&lower)?).map(|e| {
            let mut out = Vec::new();
            if let Some(tag) = e.variant {
                out.extend(
                    EAST_ASIAN_VARIANTS
                        .iter()
                        .filter(|(_, t)| *t == tag)
                        .map(|(k, _)| *k),
                );
            }
            if let Some(full) = e.full_width {
                out.push(if full {
                    "full-width"
                } else {
                    "proportional-width"
                });
            }
            if e.ruby {
                out.push("ruby");
            }
            keywords(out)
        }),
        "font-variant-position" => one_of(&["normal", "sub", "super"]),
        "font-variant-alternates" => alternates(value),
        "font-variant-emoji" => one_of(&["normal", "text", "emoji", "unicode"]),
        "font-feature-settings" => feature_settings_specified(value),
        "font-variation-settings" => variation_settings_specified(value),
        "font-synthesis-weight" | "font-synthesis-small-caps" | "font-synthesis-position" => {
            one_of(&["auto", "none"])
        }
        "font-synthesis-style" => one_of(&["auto", "none", "oblique-only"]),
        "font-size-adjust" => size_adjust(value),
        _ => None,
    }
}

/// The computed value of longhand `name` from its canonical specified
/// value: `font-width` as a percentage, `font-style` angles in degrees,
/// the settings as maps sorted by tag; others as specified.
pub(in crate::dom) fn computed(name: &str, value: &str) -> String {
    match name {
        "font-width" => WIDTH_KEYWORDS
            .iter()
            .find(|(k, _)| *k == value)
            .map_or_else(|| value.to_string(), |(_, p)| format!("{}%", number(*p))),
        "font-style" => match value.strip_prefix("oblique") {
            Some(angle) if !angle.trim().is_empty() => {
                match parse_angle_deg(angle.trim()) {
                    // The lack of an angle represents 14deg.
                    Some(deg) => {
                        if deg == 14.0 {
                            "oblique".into()
                        } else {
                            format!("oblique {}deg", number(deg))
                        }
                    }
                    None => value.to_string(),
                }
            }
            _ => value.to_string(),
        },
        "font-feature-settings" => parse_feature_settings(value).map_or_else(
            || value.to_string(),
            |items| {
                if items.is_empty() {
                    return "normal".into();
                }
                items
                    .iter()
                    .map(|(tag, v)| feature_item(tag, *v))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        "font-variation-settings" => parse_variation_settings(value).map_or_else(
            || value.to_string(),
            |items| {
                if items.is_empty() {
                    return "normal".into();
                }
                items
                    .iter()
                    .map(|(tag, v)| format!("{} {}", tag_string(tag), number(*v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        _ => value.to_string(),
    }
}

/// `font-variant`: `normal | none | [ <ligatures> || <caps> ||
/// <alternates> || <numeric> || <east-asian> || <position> || <emoji> ]`.
/// "Like other shorthands, using font-variant resets unspecified
/// font-variant subproperties to their initial values"; `none` sets
/// `font-variant-ligatures: none`.
pub(in crate::dom) fn expand_variant(value: &str) -> Option<Vec<(String, String)>> {
    let lower = value.trim().to_ascii_lowercase();
    let mut out: Vec<(String, String)> = VARIANT_LONGHANDS
        .iter()
        .map(|name| (name.to_string(), String::from("normal")))
        .collect();
    match lower.as_str() {
        "normal" => return Some(out),
        "none" => {
            out[0].1 = "none".into();
            return Some(out);
        }
        _ => {}
    }
    let mut parts: [Vec<String>; 7] = Default::default();
    for component in component_values(value.trim())? {
        let word = component.to_ascii_lowercase();
        let slot = if word.contains('(') || word == "historical-forms" {
            2
        } else if parse_ligatures(&word).is_some_and(|l| !l.none) && word != "normal" {
            0
        } else if parse_caps(&word).is_some() && word != "normal" {
            1
        } else if parse_numeric(&word).is_some() && word != "normal" {
            3
        } else if parse_east_asian(&word).is_some() && word != "normal" {
            4
        } else if matches!(word.as_str(), "sub" | "super") {
            5
        } else if matches!(word.as_str(), "text" | "emoji" | "unicode") {
            6
        } else {
            return None;
        };
        parts[slot].push(if slot == 2 { component } else { word });
    }
    for (index, words) in parts.iter().enumerate() {
        if words.is_empty() {
            continue;
        }
        if matches!(index, 1 | 5 | 6) && words.len() > 1 {
            return None;
        }
        out[index].1 = specified(VARIANT_LONGHANDS[index], &words.join(" "))?;
    }
    Some(out)
}

/// `font-variant` from its longhands' computed values; `None` when they
/// cannot be expressed by it.
pub(in crate::dom) fn serialize_variant(
    longhand: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let values: Vec<String> = VARIANT_LONGHANDS
        .iter()
        .map(|name| longhand(name).unwrap_or_else(|| "normal".into()))
        .collect();
    if values.iter().all(|v| v == "normal") {
        return Some("normal".into());
    }
    if values[0] == "none" {
        return values[1..]
            .iter()
            .all(|v| v == "normal")
            .then(|| "none".into());
    }
    Some(
        values
            .into_iter()
            .filter(|v| v != "normal")
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// `font-synthesis`: `none | [ weight || style || small-caps || position ]`.
pub(in crate::dom) fn expand_synthesis(value: &str) -> Option<Vec<(String, String)>> {
    let lower = value.trim().to_ascii_lowercase();
    let mut enabled = [false; 4];
    if lower != "none" {
        for word in lower.split_ascii_whitespace() {
            let index = SYNTHESIS_LONGHANDS.iter().position(|(_, k)| *k == word)?;
            if std::mem::replace(&mut enabled[index], true) {
                return None;
            }
        }
        if !enabled.iter().any(|e| *e) {
            return None;
        }
    }
    Some(
        SYNTHESIS_LONGHANDS
            .iter()
            .zip(enabled)
            .map(|((name, _), on)| {
                (
                    name.to_string(),
                    String::from(if on { "auto" } else { "none" }),
                )
            })
            .collect(),
    )
}

/// `font-synthesis` from its longhands; `None` when `font-synthesis-style`
/// is `oblique-only`, which the shorthand cannot express.
pub(in crate::dom) fn serialize_synthesis(
    longhand: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut out = Vec::new();
    for (name, keyword) in SYNTHESIS_LONGHANDS {
        match longhand(name).as_deref().unwrap_or("auto") {
            "auto" => out.push(keyword),
            "none" => {}
            _ => return None,
        }
    }
    Some(if out.is_empty() {
        "none".into()
    } else {
        out.join(" ")
    })
}

/// #font-prop: the components before the size, `[ <'font-style'> ||
/// <font-variant-css2> || <'font-weight'> || <font-width-css3> ]`, as
/// (font-style, font-variant-caps, font-weight, font-width) values; `normal`
/// fills whichever is still free. `None` when the tokens do not match.
/// (font-style, font-variant-caps, font-weight, font-width).
pub(in crate::dom) type FontPrefix = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(in crate::dom) fn font_prefix(tokens: &[&str]) -> Option<FontPrefix> {
    let (mut style, mut variant, mut weight, mut width) = (None, None, None, None);
    let mut normals = 0;
    let mut index = 0;
    let set = |slot: &mut Option<String>, value: String| slot.replace(value).is_none();
    while index < tokens.len() {
        let token = tokens[index];
        let word = token.to_ascii_lowercase();
        let fresh = match word.as_str() {
            "normal" => {
                normals += 1;
                true
            }
            "italic" => set(&mut style, word),
            "oblique" => {
                // "the 25deg in this rule must be immediately following the
                // oblique keyword".
                let angle = tokens.get(index + 1).filter(|next| {
                    parse_angle_deg(next).is_some()
                        && next.starts_with(|c: char| !c.is_ascii_alphabetic())
                });
                let value = match angle {
                    Some(angle) => {
                        index += 1;
                        font_style(&format!("oblique {angle}"))?
                    }
                    None => word,
                };
                set(&mut style, value)
            }
            "small-caps" => set(&mut variant, word),
            "bold" | "bolder" | "lighter" => set(&mut weight, word),
            _ if WIDTH_KEYWORDS.iter().any(|(k, _)| *k == word) => set(&mut width, word),
            _ if token
                .parse::<f32>()
                .is_ok_and(|n| (1.0..=1000.0).contains(&n)) =>
            {
                set(&mut weight, token.to_string())
            }
            _ => return None,
        };
        if !fresh {
            return None;
        }
        index += 1;
    }
    let free = [&style, &variant, &weight, &width]
        .iter()
        .filter(|slot| slot.is_none())
        .count();
    (normals <= free).then_some((style, variant, weight, width))
}

/// #font-prop: the Set Explicitly sub-properties, in expansion order.
pub(in crate::dom) const FONT_SET: [&str; 7] = [
    "font-style",
    "font-variant-caps",
    "font-weight",
    "font-width",
    "font-size",
    "line-height",
    "font-family",
];

/// #font-prop: `font` from its sub-properties' values (specified or
/// computed); `None` when it cannot represent them: a reset-only
/// sub-property is not initial, `font-variant-caps` is not a CSS 2 value
/// or `font-width` not a CSS Fonts 3 keyword.
pub(in crate::dom) fn serialize_font(longhand: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    for (name, initial) in FONT_RESETS {
        if name != "font-width" && longhand(name).is_some_and(|value| value != initial) {
            return None;
        }
    }
    let style = longhand("font-style")?;
    let weight = longhand("font-weight")?;
    let size = longhand("font-size")?;
    let line_height = longhand("line-height")?;
    let family = longhand("font-family")?;
    let variant = longhand("font-variant-caps").unwrap_or_else(|| "normal".into());
    let width = longhand("font-width").unwrap_or_else(|| "normal".into());
    let width = match WIDTH_KEYWORDS
        .iter()
        .find(|(keyword, percent)| *keyword == width || format!("{}%", number(*percent)) == width)
    {
        Some(("normal", _)) => None,
        Some((keyword, _)) => Some(*keyword),
        None => return None,
    };
    let mut parts = Vec::new();
    if style != "normal" {
        parts.push(style);
    }
    match variant.as_str() {
        "normal" => {}
        "small-caps" => parts.push(variant),
        _ => return None,
    }
    if !matches!(weight.as_str(), "normal" | "400") {
        parts.push(weight);
    }
    if let Some(width) = width {
        parts.push(width.into());
    }
    parts.push(size);
    if line_height != "normal" {
        parts.push("/".into());
        parts.push(line_height);
    }
    parts.push(family);
    Some(parts.join(" "))
}

/// CSS Font Loading 3 #fontface-interface: the serialization of a FontFace
/// attribute's value "parsed according to the grammar for the corresponding
/// @font-face descriptor", or `None` when it does not match (a
/// `SyntaxError`). `name` is the descriptor's name.
pub(crate) fn font_face_descriptor(name: &str, value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    // A range of one value serializes as that value (#serializing-at-rules).
    let range = |valid: &dyn Fn(&str) -> bool| -> Option<String> {
        let words: Vec<&str> = lower.split_ascii_whitespace().collect();
        let mut probe = crate::font_system::FaceDescriptors::default();
        if !probe.apply(name, &lower) {
            return None;
        }
        match words.as_slice() {
            [one] => Some(one.to_string()),
            [a, b] if valid(a) && valid(b) => {
                let mut single = crate::font_system::FaceDescriptors::default();
                let mut other = crate::font_system::FaceDescriptors::default();
                single.apply(name, a);
                other.apply(name, b);
                Some(if single == other {
                    a.to_string()
                } else {
                    format!("{a} {b}")
                })
            }
            _ => None,
        }
    };
    match name {
        "font-family" => crate::http::font_family_name(value),
        "src" => (!crate::http::font_sources(value).is_empty()).then(|| value.to_string()),
        "unicode-range" => crate::http::unicode_ranges(value).map(|_| {
            value
                .split(',')
                .map(|range| range.trim().to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(", ")
        }),
        "font-weight" | "font-stretch" | "font-width" => range(&|_| true),
        "font-style" => {
            let mut probe = crate::font_system::FaceDescriptors::default();
            if !probe.apply(name, &lower) {
                return None;
            }
            let words: Vec<&str> = lower.split_ascii_whitespace().collect();
            Some(match words.as_slice() {
                ["oblique", a, b] if parse_angle_deg(a) == parse_angle_deg(b) => {
                    format!("oblique {a}")
                }
                _ => words.join(" "),
            })
        }
        "font-variant" => expand_variant(value).and_then(|longhands| {
            serialize_variant(&|name: &str| {
                longhands
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            })
        }),
        "font-feature-settings" | "font-variation-settings" => specified(name, value),
        "font-display" | "size-adjust" | "ascent-override" | "descent-override"
        | "line-gap-override" => {
            let mut probe = crate::font_system::FaceDescriptors::default();
            probe
                .apply(name, &lower)
                .then(|| lower.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
        }
        _ => None,
    }
}

/// #font-style-prop: `normal | italic | oblique <angle [-90deg,90deg]>?`.
pub(in crate::dom) fn font_style(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "normal" | "italic" | "oblique" => return Some(lower),
        _ => {}
    }
    let angle = lower.strip_prefix("oblique")?;
    if !angle.starts_with(char::is_whitespace) {
        return None;
    }
    let angle = angle.trim();
    // A unitless zero is not an <angle> here (CSS Values 4 #angles).
    if !angle.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        || angle.parse::<f32>().is_ok()
    {
        return None;
    }
    let degrees = parse_angle_deg(angle)?;
    (-90.0..=90.0)
        .contains(&degrees)
        .then(|| format!("oblique {angle}"))
}

/// #font-variant-alternates-prop.
fn alternates(value: &str) -> Option<String> {
    if value.eq_ignore_ascii_case("normal") {
        return Some("normal".into());
    }
    const ORDER: [&str; 7] = [
        "stylistic",
        "historical-forms",
        "styleset",
        "character-variant",
        "swash",
        "ornaments",
        "annotation",
    ];
    let mut slots: [Option<String>; 7] = Default::default();
    let mut parser = Parser::new(value);
    while !parser.is_exhausted() {
        let token = parser.next().ok()?.clone();
        let (index, text) = match token {
            Token::Ident(word) if word.eq_ignore_ascii_case("historical-forms") => {
                (1, String::from("historical-forms"))
            }
            Token::Function(name) => {
                let name = name.to_ascii_lowercase();
                let index = ORDER
                    .iter()
                    .position(|o| *o == name && name != "historical-forms")?;
                let list = matches!(name.as_str(), "styleset" | "character-variant");
                let names: Vec<String> = parser
                    .parse_nested_block(|p| {
                        let names = p.parse_comma_separated(|p| {
                            Ok::<_, cssparser::ParseError<()>>(p.expect_ident()?.to_string())
                        })?;
                        Ok::<_, cssparser::ParseError<()>>(names)
                    })
                    .ok()?;
                if names.is_empty() || (!list && names.len() > 1) {
                    return None;
                }
                let names: Vec<String> = names.iter().map(|n| super::identifier_text(n)).collect();
                (index, format!("{name}({})", names.join(", ")))
            }
            _ => return None,
        };
        if slots[index].replace(text).is_some() {
            return None;
        }
    }
    let out: Vec<String> = slots.into_iter().flatten().collect();
    (!out.is_empty()).then(|| out.join(" "))
}

/// #font-feature-settings-prop specified value: each `<feature-tag-value>`
/// in its shortest form (`1` and `on` omitted, `off` as `0`), in order.
fn feature_settings_specified(value: &str) -> Option<String> {
    if value.eq_ignore_ascii_case("normal") {
        return Some("normal".into());
    }
    parse_feature_settings(value)?;
    let mut parser = Parser::new(value);
    let items = parser
        .parse_comma_separated(|p| {
            let tag = p.expect_string()?.to_string();
            let value = match p.next() {
                Err(_) => 1,
                Ok(Token::Ident(word)) if word.eq_ignore_ascii_case("on") => 1,
                Ok(Token::Ident(_)) => 0,
                Ok(Token::Number {
                    int_value: Some(n), ..
                }) => *n as u32,
                Ok(_) => return Err(cssparser::ParseError::<()>::unexpected_token()),
            };
            Ok::<_, cssparser::ParseError<()>>((tag, value))
        })
        .ok()?;
    Some(
        items
            .into_iter()
            .map(|(tag, value)| feature_item(&opentype_tag(&tag).unwrap_or(*b"    "), value))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn variation_settings_specified(value: &str) -> Option<String> {
    if value.eq_ignore_ascii_case("normal") {
        return Some("normal".into());
    }
    parse_variation_settings(value)?;
    let mut parser = Parser::new(value);
    let items = parser
        .parse_comma_separated(|p| {
            let tag = p.expect_string()?.to_string();
            let value = p.expect_number()?;
            Ok::<_, cssparser::ParseError<()>>((tag, value))
        })
        .ok()?;
    Some(
        items
            .into_iter()
            .map(|(tag, value)| {
                format!(
                    "{} {}",
                    tag_string(&opentype_tag(&tag).unwrap_or(*b"    ")),
                    number(value)
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// CSS Fonts 5 #font-size-adjust-prop: `none | [ ex-height | cap-height |
/// ch-width | ic-width | ic-height ]? [ from-font | <number [0,∞]> ]`, the
/// default `ex-height` omitted (shortest serialization).
fn size_adjust(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "none" {
        return Some(lower);
    }
    let words: Vec<&str> = lower.split_ascii_whitespace().collect();
    let (metric, amount) = match words.as_slice() {
        [amount] => ("ex-height", *amount),
        [metric, amount] => (*metric, *amount),
        _ => return None,
    };
    crate::text::typography::SizeAdjustMetric::parse(metric)?;
    let amount = if amount == "from-font" {
        amount.to_string()
    } else {
        let n: f32 = amount.parse().ok()?;
        if !n.is_finite() || n < 0.0 {
            return None;
        }
        number(n)
    };
    Some(if metric == "ex-height" {
        amount
    } else {
        format!("{metric} {amount}")
    })
}

fn feature_item(tag: &[u8; 4], value: u32) -> String {
    match value {
        1 => tag_string(tag),
        _ => format!("{} {value}", tag_string(tag)),
    }
}

fn tag_string(tag: &[u8; 4]) -> String {
    super::string_text(&String::from_utf8_lossy(tag))
}

/// A CSS `<number>` serialization.
fn number(value: f32) -> String {
    let rounded = (f64::from(value) * 1e6).round() / 1e6;
    let mut text = format!("{rounded}");
    if text == "-0" {
        text = "0".into();
    }
    text
}

fn percentage(value: &str) -> Option<f32> {
    let number: f32 = value.strip_suffix('%')?.trim().parse().ok()?;
    number.is_finite().then_some(number)
}

/// The space-separated keywords of a keyword list, or `None` when it
/// holds anything else.
fn words(value: &str) -> Option<String> {
    let words: Vec<&str> = value.split_ascii_whitespace().collect();
    (!words.is_empty()
        && words.iter().all(|w| {
            w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }))
    .then(|| words.join(" "))
}

fn keywords(words: Vec<&str>) -> String {
    if words.is_empty() {
        "normal".into()
    } else {
        words.join(" ")
    }
}

/// Top-level component values of `value`: words and whole functions.
fn component_values(value: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut parser = Parser::new(value);
    while !parser.is_exhausted() {
        let start = parser.position();
        let token = parser.next().ok()?.clone();
        if let Token::Function(_) = token {
            parser
                .parse_nested_block(|p| {
                    while p.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<()>>(())
                })
                .ok()?;
        } else if !matches!(token, Token::Ident(_)) {
            return None;
        }
        out.push(parser.slice_from(start).trim().to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specified_values_serialize_in_grammar_order() {
        assert_eq!(
            specified("font-variant-numeric", "tabular-nums LINING-nums").as_deref(),
            Some("lining-nums tabular-nums")
        );
        assert_eq!(
            specified("font-variant-ligatures", "no-contextual common-ligatures").as_deref(),
            Some("common-ligatures no-contextual")
        );
        assert_eq!(
            specified("font-variant-east-asian", "ruby jis04").as_deref(),
            Some("jis04 ruby")
        );
        assert!(specified("font-variant-numeric", "lining-nums oldstyle-nums").is_none());
        assert!(specified("font-variant-ligatures", "none common-ligatures").is_none());
        assert_eq!(
            specified("font-variant-alternates", "swash(Fancy) historical-forms").as_deref(),
            Some("historical-forms swash(Fancy)")
        );
        assert!(specified("font-variant-alternates", "swash(a, b)").is_none());
        assert_eq!(
            specified("font-variant-alternates", "styleset(a,b)").as_deref(),
            Some("styleset(a, b)")
        );
    }

    #[test]
    fn widths_compute_to_percentages() {
        assert_eq!(
            specified("font-width", "Condensed").as_deref(),
            Some("condensed")
        );
        assert_eq!(computed("font-width", "condensed"), "75%");
        assert_eq!(specified("font-width", "150%").as_deref(), Some("150%"));
        assert!(specified("font-width", "-1%").is_none());
        assert!(specified("font-width", "wider").is_none());
    }

    #[test]
    fn oblique_angles_compute_to_degrees() {
        assert_eq!(
            specified("font-style", "oblique 10deg").as_deref(),
            Some("oblique 10deg")
        );
        assert_eq!(computed("font-style", "oblique 0.5turn"), "oblique 180deg");
        assert_eq!(computed("font-style", "oblique 14deg"), "oblique");
        assert_eq!(computed("font-style", "oblique -10deg"), "oblique -10deg");
        assert!(specified("font-style", "oblique 91deg").is_none());
        assert!(specified("font-style", "oblique 10").is_none());
        assert!(specified("font-style", "italic 10deg").is_none());
    }

    #[test]
    fn settings_serialize_shortest_then_compute_sorted() {
        assert_eq!(
            specified(
                "font-feature-settings",
                "\"smcp\" on, 'liga' off, \"tnum\" 2"
            )
            .as_deref(),
            Some("\"smcp\", \"liga\" 0, \"tnum\" 2")
        );
        assert_eq!(
            computed("font-feature-settings", "\"smcp\", \"liga\" 0, \"smcp\" 0"),
            "\"liga\" 0, \"smcp\" 0"
        );
        assert_eq!(
            computed("font-variation-settings", "\"wght\" 700, \"XHGT\" 0.7"),
            "\"XHGT\" 0.7, \"wght\" 700"
        );
        assert!(specified("font-variation-settings", "\"wght\" 700 \"wdth\" 80").is_none());
    }

    #[test]
    fn size_adjust_omits_the_default_metric() {
        assert_eq!(
            specified("font-size-adjust", "ex-height 0.5").as_deref(),
            Some("0.5")
        );
        assert_eq!(
            specified("font-size-adjust", "cap-height from-font").as_deref(),
            Some("cap-height from-font")
        );
        assert!(specified("font-size-adjust", "-0.5").is_none());
        assert!(specified("font-size-adjust", "ex-height").is_none());
    }

    #[test]
    fn variant_shorthand_resets_and_serializes() {
        let expanded = expand_variant("small-caps tabular-nums").unwrap();
        assert_eq!(expanded[1].1, "small-caps");
        assert_eq!(expanded[3].1, "tabular-nums");
        assert_eq!(expanded[0].1, "normal");
        let none = expand_variant("none").unwrap();
        assert_eq!(none[0].1, "none");
        assert!(expand_variant("small-caps petite-caps").is_none());
        assert!(expand_variant("none small-caps").is_none());
        let get = |values: Vec<(String, String)>| {
            move |name: &str| {
                values
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            }
        };
        assert_eq!(
            serialize_variant(&get(expand_variant("super small-caps").unwrap())).as_deref(),
            Some("small-caps super")
        );
        assert_eq!(serialize_variant(&get(none)).as_deref(), Some("none"));
    }

    #[test]
    fn synthesis_shorthand_maps_keywords_to_longhands() {
        let expanded = expand_synthesis("style small-caps").unwrap();
        assert_eq!(
            expanded.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>(),
            ["none", "auto", "auto", "none"]
        );
        assert!(expand_synthesis("weight weight").is_none());
        let get = |name: &str| (name == "font-synthesis-style").then(|| "oblique-only".into());
        assert_eq!(serialize_synthesis(&get), None);
    }

    #[test]
    fn font_prefix_assigns_each_component_once() {
        let tokens = ["condensed", "oblique", "25deg", "753"];
        let (style, variant, weight, width) = font_prefix(&tokens).unwrap();
        assert_eq!(style.as_deref(), Some("oblique 25deg"));
        assert_eq!(variant, None);
        assert_eq!(weight.as_deref(), Some("753"));
        assert_eq!(width.as_deref(), Some("condensed"));
        assert!(font_prefix(&["italic", "italic"]).is_none());
        assert!(font_prefix(&["normal", "normal", "normal", "normal", "normal"]).is_none());
        assert!(font_prefix(&["normal", "small-caps"]).is_some());
    }

    fn document(css: &str, body: &str) -> crate::dom::Dom {
        crate::dom::Dom::parse_document(&format!(
            "<head><style>{css}</style></head><body>{body}</body>"
        ))
    }

    #[test]
    fn font_longhands_cascade_inherit_and_compute() {
        let dom = document(
            "#a{font-stretch:condensed;font-style:oblique 0.25turn;\
             font-variant:small-caps tabular-nums no-common-ligatures;\
             font-feature-settings:'smcp' on,\"liga\" 0,\"smcp\" 0;\
             font-variation-settings:'wght' 700,\"XHGT\" 0.5;font-kerning:NONE;\
             font-optical-sizing:none;font-synthesis:weight;font-size-adjust:cap-height .6}\
             #bad{font-width:75%;font-width:-1%;font-variant-numeric:lining-nums;\
             font-variant-numeric:lining-nums oldstyle-nums;font-feature-settings:\"liga\";\
             font-feature-settings:liga}",
            "<div id=a><span id=b>x</span></div><div id=bad>y</div>",
        );
        let get = |id: &str, name: &str| {
            dom.cssom_resolved_value(dom.get_by_id(id).unwrap(), name)
                .unwrap_or_default()
        };
        for id in ["a", "b"] {
            assert_eq!(get(id, "font-stretch"), "75%");
            assert_eq!(get(id, "font-width"), "75%");
            assert_eq!(get(id, "font-style"), "oblique 90deg");
            assert_eq!(
                get(id, "font-variant"),
                "no-common-ligatures small-caps tabular-nums"
            );
            assert_eq!(get(id, "font-variant-caps"), "small-caps");
            assert_eq!(get(id, "font-variant-position"), "normal");
            assert_eq!(get(id, "font-feature-settings"), "\"liga\" 0, \"smcp\" 0");
            assert_eq!(
                get(id, "font-variation-settings"),
                "\"XHGT\" 0.5, \"wght\" 700"
            );
            assert_eq!(get(id, "font-kerning"), "none");
            assert_eq!(get(id, "font-optical-sizing"), "none");
            assert_eq!(get(id, "font-synthesis"), "weight");
            assert_eq!(get(id, "font-synthesis-style"), "none");
            assert_eq!(get(id, "font-size-adjust"), "cap-height 0.6");
        }
        // Invalid declarations leave the earlier valid ones in force.
        assert_eq!(get("bad", "font-width"), "75%");
        assert_eq!(get("bad", "font-variant-numeric"), "lining-nums");
        assert_eq!(get("bad", "font-feature-settings"), "\"liga\"");
        // Initial values.
        let dom = document("", "<p id=p>z</p>");
        let get = |name: &str| dom.cssom_resolved_value(dom.get_by_id("p").unwrap(), name);
        assert_eq!(get("font-stretch").as_deref(), Some("100%"));
        assert_eq!(get("font-variant").as_deref(), Some("normal"));
        assert_eq!(
            get("font-synthesis").as_deref(),
            Some("weight style small-caps position")
        );
        assert_eq!(get("font-feature-settings").as_deref(), Some("normal"));
        assert_eq!(get("font-size-adjust").as_deref(), Some("none"));
        assert_eq!(get("font-kerning").as_deref(), Some("auto"));
    }

    #[test]
    fn the_font_shorthand_sets_and_resets_its_sub_properties() {
        // #font-prop: Set Explicitly and Reset Implicitly sub-properties are
        // reset, then set; font-synthesis cascades independently.
        let dom = document(
            "#c{font-variant-numeric:tabular-nums;font-kerning:none;font-synthesis:none;\
             font:italic small-caps bold condensed 20px/2 serif}\
             #d{font:oblique 25deg 753 12px serif}\
             #e{font-variant-numeric:tabular-nums}",
            "<div id=c>c</div><div id=d>d</div><div id=e>e</div>",
        );
        let get = |id: &str, name: &str| {
            dom.cssom_resolved_value(dom.get_by_id(id).unwrap(), name)
                .unwrap_or_default()
        };
        assert_eq!(get("c", "font-variant-numeric"), "normal");
        assert_eq!(get("c", "font-kerning"), "auto");
        assert_eq!(get("c", "font-synthesis"), "none");
        assert_eq!(get("c", "font-variant-caps"), "small-caps");
        assert_eq!(get("c", "font-width"), "75%");
        assert_eq!(
            get("c", "font"),
            "italic small-caps 700 condensed 20px / 40px serif"
        );
        assert_eq!(get("d", "font-style"), "oblique 25deg");
        assert_eq!(get("d", "font-weight"), "753");
        // A reset-only sub-property that is not initial makes `font`
        // unrepresentable.
        assert_eq!(get("e", "font"), "");
    }

    #[test]
    fn declaration_blocks_serialize_the_font_shorthands() {
        let block = |text: &str, property: &str| {
            let declarations = crate::dom::cssom::tests_parse(text);
            crate::dom::cssom::tests_get(property, &declarations)
        };
        assert_eq!(
            block("font-variant: super small-caps", "font-variant"),
            "small-caps super"
        );
        assert_eq!(
            block("font-variant: none", "font-variant-ligatures"),
            "none"
        );
        assert_eq!(
            block("font-synthesis: style weight", "font-synthesis"),
            "weight style"
        );
        assert_eq!(
            block("font: bold condensed 12px/1.5 serif", "font"),
            "bold condensed 12px / 1.5 serif"
        );
        assert_eq!(block("font: 12px serif; font-kerning: none", "font"), "");
        assert_eq!(block("font-stretch: expanded", "font-width"), "expanded");
        assert_eq!(block("font-stretch: expanded", "font-stretch"), "expanded");
    }

    #[test]
    fn supports_queries_accept_the_font_grammars() {
        for (property, value) in [
            ("font-variant-numeric", "tabular-nums slashed-zero"),
            ("font-stretch", "50%"),
            ("font-width", "ultra-expanded"),
            ("font-variant", "none"),
            ("font-variant", "small-caps historical-forms ruby"),
            ("font-feature-settings", "\"liga\" off"),
            ("font-variation-settings", "\"wght\" 650"),
            ("font-style", "oblique -10deg"),
            ("font-size-adjust", "ic-width from-font"),
            ("font-synthesis", "small-caps position"),
            ("font-synthesis-style", "oblique-only"),
            ("font-variant-emoji", "unicode"),
            ("font-kerning", "normal"),
            ("font-optical-sizing", "auto"),
        ] {
            assert!(
                crate::dom::cssom::supports(property, value),
                "{property}: {value}"
            );
        }
        for (property, value) in [
            ("font-variant-caps", "tiny-caps"),
            ("font-stretch", "-5%"),
            ("font-feature-settings", "liga"),
            ("font-style", "oblique 100deg"),
            ("font-size-adjust", "-1"),
            ("font-synthesis", "none weight"),
            ("font-variant", "normal small-caps"),
        ] {
            assert!(
                !crate::dom::cssom::supports(property, value),
                "{property}: {value}"
            );
        }
    }

    #[test]
    fn font_face_attributes_parse_as_their_descriptors() {
        let d = font_face_descriptor;
        assert_eq!(d("font-weight", "700 700").as_deref(), Some("700"));
        assert_eq!(d("font-weight", "100 900").as_deref(), Some("100 900"));
        assert_eq!(d("font-weight", "bolder"), None);
        assert_eq!(d("font-stretch", "Condensed").as_deref(), Some("condensed"));
        assert_eq!(
            d("font-style", "oblique 10deg 10deg").as_deref(),
            Some("oblique 10deg")
        );
        assert_eq!(d("font-style", "oblique 10"), None);
        assert_eq!(d("font-display", "SWAP").as_deref(), Some("swap"));
        assert_eq!(d("size-adjust", "90%").as_deref(), Some("90%"));
        assert_eq!(d("ascent-override", "-1%"), None);
        assert_eq!(
            d("unicode-range", "u+0-7f, U+4??").as_deref(),
            Some("U+0-7F, U+4??")
        );
        assert_eq!(
            d("font-feature-settings", "\"liga\" on").as_deref(),
            Some("\"liga\"")
        );
        assert_eq!(
            d("font-variant", "tabular-nums small-caps").as_deref(),
            Some("small-caps tabular-nums")
        );
        assert_eq!(d("font-family", "My Font").as_deref(), Some("My Font"));
        assert_eq!(d("font-family", "serif"), None);
        assert!(d("src", "url(a.woff2) format(\"woff2\")").is_some());
        assert_eq!(d("src", "http://example.com/a.woff"), None);
    }

    #[test]
    fn from_font_computes_to_the_first_available_fonts_metric() {
        let dom = document(
            "#f{font-size-adjust:from-font;font-family:serif}",
            "<p id=f>f</p>",
        );
        let value = dom
            .cssom_resolved_value(dom.get_by_id("f").unwrap(), "font-size-adjust")
            .unwrap();
        let number: f32 = value.parse().unwrap();
        assert!(number > 0.3 && number < 0.8, "{value}");
    }
}
