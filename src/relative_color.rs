//! CSS Color 5 relative color syntax (#relative-colors, #resolving-rcs), from
//! the local 2026-09-06 CSSWG snapshot (81c27f686901): resolve
//! `<function>(from <origin> …)` to the absolute color it computes to.
//!
//! The origin is converted to the function's processing space; its component
//! keywords are numbers there (missing analogous components carry forward as
//! `none`, and are zero inside math). `rgb()`/`hsl()`/`hwb()` compute to
//! `color(srgb …)`, the others to their own function. An omitted alpha is the
//! origin's alpha; alpha is clamped. A `currentcolor` origin is not resolved
//! (its computed value would have to stay relative), so it is rejected.
//!
//! `color-mix()` (CSS Color 5 #color-mix) likewise computes to its mixed
//! absolute color (#serial-color-mix); a `currentcolor` argument is rejected
//! for the same reason.

use color::{ColorSpaceTag as Space, DynamicColor, Flags, HueDirection, Missing};
use cssparser::{ParseError, Parser, Token};

/// Nested relative origins and math nesting are bounded like other values.
const MAX_DEPTH: usize = 16;

type Res<T> = Result<T, ParseError<()>>;

/// The channel grammar of one relative color function.
struct Form {
    space: Space,
    keywords: [&'static str; 3],
    /// Index of the `<hue>` channel, which takes angles but not percentages.
    hue: Option<usize>,
    /// The number each channel's 100% resolves to.
    percent: [f64; 3],
    /// Keyword numbers per processing-space component (`rgb()` uses 0–255).
    scale: f64,
    /// `rgb()`, `hsl()` and `hwb()` compute to `color(srgb …)`.
    srgb_output: bool,
}

fn function_form(name: &str) -> Option<Form> {
    let form = |space, keywords, hue, percent, scale, srgb_output| Form {
        space,
        keywords,
        hue,
        percent,
        scale,
        srgb_output,
    };
    Some(match name {
        "rgb" | "rgba" => form(Space::Srgb, ["r", "g", "b"], None, [255.; 3], 255., true),
        "hsl" | "hsla" => form(Space::Hsl, ["h", "s", "l"], Some(0), [100.; 3], 1., true),
        "hwb" => form(Space::Hwb, ["h", "w", "b"], Some(0), [100.; 3], 1., true),
        "lab" => form(
            Space::Lab,
            ["l", "a", "b"],
            None,
            [100., 125., 125.],
            1.,
            false,
        ),
        "lch" => form(
            Space::Lch,
            ["l", "c", "h"],
            Some(2),
            [100., 150., 1.],
            1.,
            false,
        ),
        "oklab" => form(
            Space::Oklab,
            ["l", "a", "b"],
            None,
            [1., 0.4, 0.4],
            1.,
            false,
        ),
        "oklch" => form(
            Space::Oklch,
            ["l", "c", "h"],
            Some(2),
            [1., 0.4, 1.],
            1.,
            false,
        ),
        _ => return None,
    })
}

/// `color(from <origin> <colorspace> …)`: predefined RGB spaces use `r g b`,
/// the XYZ spaces `x y z`; 100% is 1 for every channel.
fn color_function_form(space: &str) -> Option<Form> {
    let space: Space = space.parse().ok()?;
    let keywords = match space {
        Space::Srgb
        | Space::LinearSrgb
        | Space::DisplayP3
        | Space::A98Rgb
        | Space::ProphotoRgb
        | Space::Rec2020 => ["r", "g", "b"],
        Space::XyzD50 | Space::XyzD65 => ["x", "y", "z"],
        _ => return None,
    };
    Some(Form {
        space,
        keywords,
        hue: None,
        percent: [1.; 3],
        scale: 1.,
        srgb_output: false,
    })
}

/// Resolve a relative color or `color-mix()`. `None` means `text` is neither
/// at all; `Some(None)` means it is one but invalid (or uses `currentcolor`).
pub(crate) fn resolve(text: &str) -> Option<Option<String>> {
    // Paint and style parse many colors; skip tokenizing the ordinary ones.
    let contains = |needle: &[u8]| {
        text.as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    };
    if !contains(b"from") && !contains(b"color-mix") {
        return None;
    }
    resolve_at(text, 0)
}

/// A `<color>` as an absolute color, resolving relative colors and
/// `color-mix()`; `None` for anything else or an invalid color.
pub(crate) fn parse(text: &str) -> Option<DynamicColor> {
    match resolve(text) {
        Some(resolved) => color::parse_color(&resolved?).ok(),
        None => color::parse_color(text).ok(),
    }
}

/// The kind of value a math function computed to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MathValue {
    Number(f64),
    Percent(f64),
    Degrees(f64),
}

/// CSS Values 4 #math: evaluate a `calc()`, `min()`, `max()` or `clamp()`
/// of numbers, percentages or angles (any `var()` already substituted).
/// `None` for anything else, or for a sum mixing those types.
pub(crate) fn math_value(text: &str) -> Option<MathValue> {
    let mut p = Parser::new(text.trim());
    let name = p.expect_function().ok()?.to_ascii_lowercase();
    let typed = p
        .parse_nested_block(|p| math_function(p, &name, &[], 0))
        .ok()?;
    if !p.is_exhausted() || !typed.value.is_finite() {
        return None;
    }
    Some(match typed.unit {
        Unit::Number => MathValue::Number(typed.value),
        Unit::Percent => MathValue::Percent(typed.value),
        Unit::Angle => MathValue::Degrees(typed.value),
    })
}

fn resolve_at(text: &str, depth: usize) -> Option<Option<String>> {
    let mut p = Parser::new(text.trim());
    let name = p.expect_function().ok()?.to_ascii_lowercase();
    if name == "color-mix" {
        if depth > MAX_DEPTH {
            return Some(None);
        }
        let mixed = p.parse_nested_block(|p| mix_body(p, depth)).ok();
        return Some(mixed.filter(|_| p.is_exhausted()));
    }
    if name != "color" && function_form(&name).is_none() {
        return None;
    }
    let relative = p
        .parse_nested_block(|p| {
            let relative = p.try_parse(|p| p.expect_ident_matching("from")).is_ok();
            while p.next().is_ok() {}
            Ok::<_, ParseError<()>>(relative)
        })
        .unwrap_or(false);
    if !relative {
        return None;
    }
    if depth > MAX_DEPTH {
        return Some(None);
    }
    // Re-tokenize: the probe above consumed the block.
    let mut p = Parser::new(text.trim());
    if p.expect_function().is_err() {
        return Some(None);
    }
    let resolved = p
        .parse_nested_block(|p| relative_body(p, &name, depth))
        .ok();
    Some(resolved.filter(|_| p.is_exhausted()))
}

fn relative_body<'i>(p: &mut Parser<'i>, name: &str, depth: usize) -> Res<String> {
    p.expect_ident_matching("from")?;
    let origin_text = component_text(p)?;
    let origin =
        origin_color(&origin_text, depth).ok_or_else(|| cssparser::ParseError::custom(()))?;
    let (form, space_name) = if name == "color" {
        let space = p.expect_ident_cloned()?.to_ascii_lowercase();
        let form = color_function_form(&space).ok_or_else(|| cssparser::ParseError::custom(()))?;
        // "xyz" is an alias; serialize the canonical name.
        let canonical = if space == "xyz" {
            "xyz-d65".to_string()
        } else {
            space
        };
        (form, Some(canonical))
    } else {
        (
            function_form(name).ok_or_else(|| cssparser::ParseError::custom(()))?,
            None,
        )
    };

    // Component keywords: the origin in the processing space.
    let converted = origin.convert(form.space);
    let missing = converted.flags.missing();
    let mut keywords: Vec<(&str, Option<f64>)> = Vec::with_capacity(4);
    for (index, keyword) in form.keywords.iter().enumerate() {
        let value = (!missing.contains(index)).then(|| {
            let value = f64::from(converted.components[index]);
            if form.hue == Some(index) {
                value.rem_euclid(360.)
            } else {
                value * form.scale
            }
        });
        keywords.push((*keyword, value));
    }
    let origin_alpha = (!missing.contains(3)).then(|| f64::from(converted.components[3]));
    keywords.push(("alpha", origin_alpha));

    let mut components = [0f32; 4];
    let mut result_missing = Missing::EMPTY;
    for (index, component) in components.iter_mut().enumerate().take(3) {
        let channel = Channel::Component {
            hue: form.hue == Some(index),
            percent: form.percent[index],
        };
        match channel_value(p, &keywords, channel, depth)? {
            Some(value) => *component = (value / form.scale) as f32,
            None => result_missing.insert(index),
        }
    }
    let alpha = if p.try_parse(|p| p.expect_delim('/')).is_ok() {
        channel_value(p, &keywords, Channel::Alpha, depth)?
    } else {
        origin_alpha
    };
    match alpha {
        Some(alpha) => components[3] = alpha.clamp(0., 1.) as f32,
        None => result_missing.insert(3),
    }
    if !p.is_exhausted() || !components.iter().all(|value| value.is_finite()) {
        return Err(cssparser::ParseError::custom(()));
    }
    // The result is then clamped like a parsed absolute color (CSS Color 4
    // #the-hsl-notation, #specifying-lab-lch, #specifying-oklab-oklch): hues
    // modulo 360, lightness within its reference range, chroma non-negative.
    if let Some(hue) = form.hue.filter(|&hue| !result_missing.contains(hue)) {
        components[hue] = components[hue].rem_euclid(360.);
    }
    let lightness_max = match form.space {
        Space::Lab | Space::Lch => Some(100.),
        Space::Oklab | Space::Oklch => Some(1.),
        _ => None,
    };
    if let Some(max) = lightness_max.filter(|_| !result_missing.contains(0)) {
        components[0] = components[0].clamp(0., max);
    }
    if matches!(form.space, Space::Lch | Space::Oklch) && !result_missing.contains(1) {
        components[1] = components[1].max(0.);
    }
    let color = DynamicColor {
        cs: form.space,
        flags: Flags::from_missing(result_missing),
        components,
    };
    Ok(if form.srgb_output {
        serialize("color(srgb", color.convert(Space::Srgb))
    } else if let Some(space) = space_name {
        serialize(&format!("color({space}"), color)
    } else {
        serialize(&format!("{name}("), color)
    })
}

/// CSS Color 5 #color-mix:
/// `color-mix( <color-interpolation-method>? , [ <color> && <percentage [0,100]>? ]# )`,
/// mixed as #color-mix-result calculates it and serialized in the form
/// #serial-color-mix gives for the mixing color space.
fn mix_body<'i>(p: &mut Parser<'i>, depth: usize) -> Res<String> {
    // #color-mix-space: Oklab unless a method is given; polar hues default
    // to the `shorter` interpolation method.
    let (space, hue) = p
        .try_parse(|p| {
            let method = interpolation_method(p)?;
            p.expect_comma()?;
            Ok::<_, ParseError<()>>(method)
        })
        .unwrap_or((Space::Oklab, HueDirection::Shorter));
    let items = p.parse_comma_separated(|p| {
        let mut percentage = p.try_parse(|p| mix_percentage(p, depth)).ok();
        let text = component_text(p)?;
        let color = origin_color(&text, depth).ok_or_else(|| cssparser::ParseError::custom(()))?;
        if percentage.is_none() {
            percentage = p.try_parse(|p| mix_percentage(p, depth)).ok();
        }
        Ok((color, percentage))
    })?;
    // CSS Values 5 #normalize-mix-percentages, with forced normalization.
    let specified = items
        .iter()
        .filter_map(|(_, percentage)| *percentage)
        .sum::<f64>()
        .min(100.);
    let omitted = items
        .iter()
        .filter(|(_, percentage)| percentage.is_none())
        .count();
    let mut percentages: Vec<f64> = items
        .iter()
        .map(|(_, percentage)| percentage.unwrap_or((100. - specified) / omitted as f64))
        .collect();
    let total: f64 = percentages.iter().sum();
    if total > 0. {
        for percentage in &mut percentages {
            *percentage *= 100. / total;
        }
    }
    let leftover = (100. - total).max(0.);
    // #color-mix-result: mix each item into the result of the ones before it,
    // by its share of their combined percentage (half when that is zero).
    let mut mixed = items[0].0.convert(space);
    let mut weight = percentages[0];
    for ((color, _), &percentage) in items.iter().zip(&percentages).skip(1) {
        let combined = weight + percentage;
        let progress = if combined > 0. {
            percentage / combined
        } else {
            0.5
        };
        mixed = mixed.interpolate(*color, space, hue).eval(progress as f32);
        weight = combined;
    }
    mixed.components[3] *= (1. - leftover / 100.) as f32;
    if !mixed.components.iter().all(|value| value.is_finite()) {
        return Err(cssparser::ParseError::custom(()));
    }
    Ok(match space {
        // Without missing components an HSL or HWB mix serializes as sRGB.
        Space::Hsl | Space::Hwb if mixed.flags.missing().is_empty() => {
            serialize("color(srgb", mixed.convert(Space::Srgb))
        }
        Space::Hsl => serialize("hsl(", mixed),
        Space::Hwb => serialize("hwb(", mixed),
        Space::Lab => serialize("lab(", mixed),
        Space::Lch => serialize("lch(", mixed),
        Space::Oklab => serialize("oklab(", mixed),
        Space::Oklch => serialize("oklch(", mixed),
        _ => serialize(&format!("color({}", space_name(space)), mixed),
    })
}

/// CSS Color 4 #color-interpolation-method:
/// `in [ <rectangular-color-space> | <polar-color-space> <hue-interpolation-method>? ]`.
fn interpolation_method<'i>(p: &mut Parser<'i>) -> Res<(Space, HueDirection)> {
    p.expect_ident_matching("in")?;
    let name = p.expect_ident_cloned()?.to_ascii_lowercase();
    let space = [
        Space::Srgb,
        Space::LinearSrgb,
        Space::DisplayP3,
        Space::A98Rgb,
        Space::ProphotoRgb,
        Space::Rec2020,
        Space::Lab,
        Space::Oklab,
        Space::XyzD50,
        Space::XyzD65,
        Space::Hsl,
        Space::Hwb,
        Space::Lch,
        Space::Oklch,
    ]
    .into_iter()
    .find(|&space| space_name(space) == name || (name == "xyz" && space == Space::XyzD65))
    .ok_or_else(|| cssparser::ParseError::custom(()))?;
    let mut hue = HueDirection::Shorter;
    if matches!(space, Space::Hsl | Space::Hwb | Space::Lch | Space::Oklch)
        && let Ok(direction) = p.try_parse(|p| {
            let direction = match p.expect_ident()?.to_ascii_lowercase().as_str() {
                "shorter" => HueDirection::Shorter,
                "longer" => HueDirection::Longer,
                "increasing" => HueDirection::Increasing,
                "decreasing" => HueDirection::Decreasing,
                _ => return Err(cssparser::ParseError::custom(())),
            };
            p.expect_ident_matching("hue")?;
            Ok::<_, ParseError<()>>(direction)
        })
    {
        hue = direction;
    }
    Ok((space, hue))
}

/// A mix item's `<percentage [0,100]>`; a math function's result is clamped
/// to the range (CSS Values 4 #calc-range), a literal outside it is invalid.
fn mix_percentage<'i>(p: &mut Parser<'i>, depth: usize) -> Res<f64> {
    match p.next()?.clone() {
        Token::Percentage { unit_value, .. } => {
            let value = f64::from(unit_value) * 100.;
            if (0. ..=100.).contains(&value) {
                Ok(value)
            } else {
                Err(cssparser::ParseError::custom(()))
            }
        }
        Token::Function(ref name) => {
            let name = name.to_ascii_lowercase();
            let typed = p.parse_nested_block(|p| math_function(p, &name, &[], depth + 1))?;
            if typed.unit == Unit::Percent && typed.value.is_finite() {
                Ok(typed.value.clamp(0., 100.))
            } else {
                Err(cssparser::ParseError::custom(()))
            }
        }
        _ => Err(cssparser::ParseError::custom(())),
    }
}

/// The CSS name of a color space, as `color()` and color-interpolation
/// methods spell it.
fn space_name(space: Space) -> &'static str {
    match space {
        Space::Srgb => "srgb",
        Space::LinearSrgb => "srgb-linear",
        Space::DisplayP3 => "display-p3",
        Space::A98Rgb => "a98-rgb",
        Space::ProphotoRgb => "prophoto-rgb",
        Space::Rec2020 => "rec2020",
        Space::Lab => "lab",
        Space::Lch => "lch",
        Space::Oklab => "oklab",
        Space::Oklch => "oklch",
        Space::Hsl => "hsl",
        Space::Hwb => "hwb",
        Space::XyzD50 => "xyz-d50",
        _ => "xyz-d65",
    }
}

fn serialize(prefix: &str, color: DynamicColor) -> String {
    let missing = color.flags.missing();
    let channel = |index: usize| {
        if missing.contains(index) {
            "none".to_string()
        } else {
            color.components[index].to_string()
        }
    };
    let space = if prefix.ends_with('(') { "" } else { " " };
    format!(
        "{prefix}{space}{} {} {} / {})",
        channel(0),
        channel(1),
        channel(2),
        channel(3)
    )
}

/// The origin `<color>`: an absolute color, or a nested relative color.
fn origin_color(text: &str, depth: usize) -> Option<DynamicColor> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("currentcolor") {
        return None;
    }
    let absolute = match resolve_at(text, depth + 1) {
        Some(resolved) => resolved?,
        None => text.to_string(),
    };
    if absolute.eq_ignore_ascii_case("transparent") {
        return Some(DynamicColor {
            cs: Space::Srgb,
            flags: Flags::default(),
            components: [0.; 4],
        });
    }
    color::parse_color(&absolute).ok()
}

/// The source text of the next component value (a token or whole function).
fn component_text<'i>(p: &mut Parser<'i>) -> Res<String> {
    let start = p.position();
    if matches!(p.next()?, Token::Function(_)) {
        p.parse_nested_block(|p| {
            while p.next().is_ok() {}
            Ok::<_, ParseError<()>>(())
        })?;
    }
    Ok(p.slice_from(start).to_string())
}

#[derive(Clone, Copy)]
enum Channel {
    /// A color channel: `<hue>` (number or angle) or number/percentage.
    Component { hue: bool, percent: f64 },
    /// `<alpha-value>`: number or percentage, 100% = 1.
    Alpha,
}

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Number,
    Percent,
    /// Degrees.
    Angle,
}

#[derive(Clone, Copy)]
struct Typed {
    value: f64,
    unit: Unit,
}

/// One channel argument, resolved to the processing space's number, or
/// `None` for `none` (directly, or a keyword naming a missing component).
fn channel_value<'i>(
    p: &mut Parser<'i>,
    keywords: &[(&str, Option<f64>)],
    channel: Channel,
    depth: usize,
) -> Res<Option<f64>> {
    let state = p.state();
    if let Ok(name) = p.expect_ident_cloned() {
        if name.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        return match keyword(keywords, &name) {
            Some(value) => Ok(value),
            None => Err(cssparser::ParseError::custom(())),
        };
    }
    p.reset(&state);
    let typed = term(p, keywords, depth)?;
    let value = match (channel, typed.unit) {
        (_, Unit::Number) => typed.value,
        (Channel::Component { hue: true, .. }, Unit::Angle) => typed.value,
        (
            Channel::Component {
                hue: false,
                percent,
            },
            Unit::Percent,
        ) => typed.value / 100. * percent,
        (Channel::Alpha, Unit::Percent) => typed.value / 100.,
        _ => return Err(cssparser::ParseError::custom(())),
    };
    Ok(Some(value))
}

fn keyword(keywords: &[(&str, Option<f64>)], name: &str) -> Option<Option<f64>> {
    keywords
        .iter()
        .find(|(keyword, _)| name.eq_ignore_ascii_case(keyword))
        .map(|&(_, value)| value)
}

/// A primary value: a literal, a component keyword (missing ⇒ 0, CSS Color 5
/// #relative-syntax), a math constant, or a math function / parenthesized sum.
fn term<'i>(p: &mut Parser<'i>, keywords: &[(&str, Option<f64>)], depth: usize) -> Res<Typed> {
    if depth > MAX_DEPTH {
        return Err(cssparser::ParseError::custom(()));
    }
    let token = p.next()?.clone();
    let number = |value: f64| Typed {
        value,
        unit: Unit::Number,
    };
    Ok(match token {
        Token::Number { value, .. } => number(f64::from(value)),
        Token::Percentage { unit_value, .. } => Typed {
            value: f64::from(unit_value) * 100.,
            unit: Unit::Percent,
        },
        Token::Dimension {
            value, ref unit, ..
        } => {
            let value = f64::from(value);
            let degrees = match unit.to_ascii_lowercase().as_str() {
                "deg" => value,
                "rad" => value.to_degrees(),
                "grad" => value * 0.9,
                "turn" => value * 360.,
                _ => return Err(cssparser::ParseError::custom(())),
            };
            Typed {
                value: degrees,
                unit: Unit::Angle,
            }
        }
        Token::Ident(ref name) => match keyword(keywords, name) {
            Some(value) => number(value.unwrap_or(0.)),
            None => match name.to_ascii_lowercase().as_str() {
                "e" => number(std::f64::consts::E),
                "pi" => number(std::f64::consts::PI),
                _ => return Err(cssparser::ParseError::custom(())),
            },
        },
        Token::ParenthesisBlock => p.parse_nested_block(|p| sum(p, keywords, depth + 1))?,
        Token::Function(ref name) => {
            let name = name.to_ascii_lowercase();
            p.parse_nested_block(|p| math_function(p, &name, keywords, depth + 1))?
        }
        _ => return Err(cssparser::ParseError::custom(())),
    })
}

fn math_function<'i>(
    p: &mut Parser<'i>,
    name: &str,
    keywords: &[(&str, Option<f64>)],
    depth: usize,
) -> Res<Typed> {
    let result = match name {
        "calc" => sum(p, keywords, depth)?,
        "min" | "max" | "clamp" => {
            let args = p.parse_comma_separated(|p| sum(p, keywords, depth))?;
            let unit = args[0].unit;
            if args.iter().any(|arg| arg.unit != unit) || name == "clamp" && args.len() != 3 {
                return Err(cssparser::ParseError::custom(()));
            }
            let value = match name {
                "min" => args
                    .iter()
                    .map(|arg| arg.value)
                    .fold(f64::INFINITY, f64::min),
                "max" => args
                    .iter()
                    .map(|arg| arg.value)
                    .fold(f64::NEG_INFINITY, f64::max),
                _ => args[1].value.min(args[2].value).max(args[0].value),
            };
            Typed { value, unit }
        }
        _ => return Err(cssparser::ParseError::custom(())),
    };
    if !p.is_exhausted() {
        return Err(cssparser::ParseError::custom(()));
    }
    Ok(result)
}

/// `<calc-sum>`: products joined by `+`/`-`, which must share a type.
fn sum<'i>(p: &mut Parser<'i>, keywords: &[(&str, Option<f64>)], depth: usize) -> Res<Typed> {
    let mut left = product(p, keywords, depth)?;
    loop {
        let state = p.state();
        let sign = match p.next() {
            Ok(Token::Delim('+')) => 1.,
            Ok(Token::Delim('-')) => -1.,
            _ => {
                p.reset(&state);
                return Ok(left);
            }
        };
        let right = product(p, keywords, depth)?;
        if right.unit != left.unit {
            return Err(cssparser::ParseError::custom(()));
        }
        left.value += sign * right.value;
    }
}

/// `<calc-product>`: a product needs a plain number on one side, and a
/// quotient a plain number divisor (CSS Values 4 type checking).
fn product<'i>(p: &mut Parser<'i>, keywords: &[(&str, Option<f64>)], depth: usize) -> Res<Typed> {
    let mut left = term(p, keywords, depth)?;
    loop {
        let state = p.state();
        let multiply = match p.next() {
            Ok(Token::Delim('*')) => true,
            Ok(Token::Delim('/')) => false,
            _ => {
                p.reset(&state);
                return Ok(left);
            }
        };
        let right = term(p, keywords, depth)?;
        left = if multiply {
            match (left.unit, right.unit) {
                (Unit::Number, unit) | (unit, Unit::Number) => Typed {
                    value: left.value * right.value,
                    unit,
                },
                _ => return Err(cssparser::ParseError::custom(())),
            }
        } else if right.unit == Unit::Number {
            Typed {
                value: left.value / right.value,
                unit: left.unit,
            }
        } else {
            return Err(cssparser::ParseError::custom(()));
        };
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    fn resolved(text: &str) -> Option<String> {
        resolve(text).expect("relative color")
    }

    /// The resolved text and its numeric components (space names, `/` and
    /// `none` are skipped).
    fn components(text: &str) -> (String, Vec<f64>) {
        let value = resolved(text).unwrap_or_else(|| panic!("{text} should resolve"));
        let body = &value[value.find('(').unwrap() + 1..value.len() - 1];
        let numbers = body
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        (value, numbers)
    }

    fn close(actual: &[f64], expected: &[f64]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?} vs {expected:?}");
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 0.002, "{actual:?} vs {expected:?}");
        }
    }

    #[test]
    fn non_relative_and_invalid_forms_are_distinguished() {
        assert_eq!(resolve("rgb(1 2 3)"), None);
        assert_eq!(resolve("red"), None);
        assert_eq!(resolve("calc(1 + 2)"), None);
        // CSS Color 5 #relative-syntax: modern syntax only, keywords per
        // function, no type mixing, hue rejects percentages.
        for invalid in [
            "rgb(from rebeccapurple, r, g, b)",
            "rgb(from rebeccapurple r 10deg 10)",
            "rgb(from rebeccapurple l g b)",
            "rgb(from rebeccapurple calc(r + 1%) g b)",
            "hsl(from rebeccapurple 10% s l)",
            "hsl(from rebeccapurple calc(h + 1deg) s l)",
            "lab(from lab(25 20 50) l 10deg 10)",
            "color(from red bogus r g b)",
            "rgb(from currentcolor r g b)",
            "rgb(from rebeccapurple r g b extra)",
        ] {
            assert_eq!(resolve(invalid), Some(None), "{invalid}");
        }
    }

    #[test]
    fn relative_colors_resolve_in_their_processing_space() {
        // Expected values follow WPT css-color/parsing/color-computed-relative-color.
        let (value, values) = components("rgb(from rebeccapurple r g b)");
        assert!(value.starts_with("color(srgb "), "{value}");
        close(&values, &[0.4, 0.2, 0.6, 1.]);
        close(
            &components("rgb(from rgb(20% 40% 60% / 80%) r g b)").1,
            &[0.2, 0.4, 0.6, 0.8],
        );
        close(
            &components("rgb(from rebeccapurple b alpha r / g)").1,
            &[0.6, 1. / 255., 0.4, 1.],
        );
        close(
            &components("rgb(from rebeccapurple r calc(g * 2) 10)").1,
            &[0.4, 0.4, 10. / 255., 1.],
        );
        close(
            &components("rgb(from rebeccapurple calc((r / 255) * 100%) g b / calc(alpha * 50%))").1,
            &[0.4, 0.2, 0.6, 0.5],
        );
        close(
            &components("hsl(from rebeccapurple h s l)").1,
            &[0.4, 0.2, 0.6, 1.],
        );
        close(
            &components("hsl(from hsl(20 30 40 / 0.8) calc(h + 1) calc(s + 1) calc(l + 1) / calc(alpha + 0.01))").1,
            &[0.537, 0.372, 0.283, 0.81],
        );
        close(
            &components("hwb(from rebeccapurple h w b)").1,
            &[0.4, 0.2, 0.6, 1.],
        );
        let (value, values) = components("lab(from lab(25 20 50 / 40%) l a b)");
        assert!(value.starts_with("lab("), "{value}");
        close(&values, &[25., 20., 50., 0.4]);
        close(
            &components("oklch(from oklch(0.7 0.1 300) l c calc(h + 90))").1,
            &[0.7, 0.1, 30., 1.],
        );
        // WPT: the result is clamped like a parsed color.
        close(
            &components("oklch(from oklch(0.7 0.45 30 / 40%) 2 3 400 / 500)").1,
            &[1., 3., 40., 1.],
        );
        let (value, values) =
            components("color(from color(display-p3 0.7 0.5 0.3) display-p3 r g calc(b + 0.5))");
        assert!(value.starts_with("color(display-p3 "), "{value}");
        close(&values, &[0.7, 0.5, 0.8, 1.]);
        assert!(
            resolved("color(from red xyz x y z)")
                .unwrap()
                .starts_with("color(xyz-d65 ")
        );
        // Nested origins and `none`.
        close(
            &components("rgb(from rgb(from rebeccapurple r g b) r g b)").1,
            &[0.4, 0.2, 0.6, 1.],
        );
        let (value, values) = components("rgb(from rebeccapurple r g none / none)");
        assert!(value.ends_with(" none / none)"), "{value}");
        close(&values, &[0.4, 0.2]);
        // Discourse's browser check.
        assert!(resolved("hsl(from white h s l)").is_some());
    }

    #[test]
    fn color_mix_computes_its_mixed_color() {
        // CSS Color 5 #color-mix-result and #serial-color-mix; expected values
        // follow WPT css-color/parsing/color-computed-color-mix-function.
        let (value, values) = components("color-mix(in sRGB, #fff 40%, transparent)");
        assert!(value.starts_with("color(srgb "), "{value}");
        close(&values, &[1., 1., 1., 0.4]);
        close(
            &components("color-mix(in hsl, hsl(120deg 10% 20%) 25%, hsl(30deg 30% 40%))").1,
            &[0.4375, 0.415625, 0.2625, 1.],
        );
        close(
            &components("color-mix(in hsl longer hue, hsl(40deg 50% 50%), hsl(60deg 50% 50%))").1,
            &[0.25, 1. / 3., 0.75, 1.],
        );
        // Percentages summing below 100% scale up and multiply the alpha.
        let (value, values) = components(
            "color-mix(in oklch, oklch(0.1 0.2 30deg) 12.5%, oklch(0.5 0.6 70deg) 37.5%)",
        );
        assert!(value.starts_with("oklch("), "{value}");
        close(&values, &[0.4, 0.5, 60., 0.5]);
        // #color-mix-space: Oklab without an interpolation method.
        assert_eq!(
            resolved("color-mix(red, blue)"),
            resolved("color-mix(in oklab, red, blue)")
        );
        assert!(
            resolved("color-mix(red, blue)")
                .unwrap()
                .starts_with("oklab(")
        );
        // More than two colors, nesting, math and a 0% sum (CSS Values 5
        // #normalize-mix-percentages leaves 100% for transparent).
        close(
            &components("color-mix(in srgb, red, lime, blue)").1,
            &[1. / 3., 1. / 3., 1. / 3., 1.],
        );
        close(
            &components("color-mix(in srgb, color-mix(in srgb, red, blue), white)").1,
            &[0.75, 0.5, 0.75, 1.],
        );
        close(
            &components("color-mix(in srgb, red calc(25% + 50%), blue)").1,
            &[0.75, 0., 0.25, 1.],
        );
        close(
            &components("color-mix(in srgb, red 0%, blue 0%)").1,
            &[0.5, 0., 0.5, 0.],
        );
        close(
            &components("rgb(from color-mix(in srgb, red, blue) r g b)").1,
            &[0.5, 0., 0.5, 1.],
        );
        for invalid in [
            "color-mix(in srgb, red -10%, blue)",
            "color-mix(in srgb, red 150%, blue)",
            "color-mix(in hsl hue, red, blue)",
            "color-mix(in hsl shorter, red, blue)",
            "color-mix(in srgb longer hue, red, blue)",
            "color-mix(in srgb red, blue)",
            "color-mix(in srgb, red blue)",
            "color-mix(in srgb, red, blue, in srgb)",
            "color-mix(in bogus, red, blue)",
            "color-mix(in srgb, currentcolor, red)",
        ] {
            assert_eq!(resolve(invalid), Some(None), "{invalid}");
        }
    }
}
