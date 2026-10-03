//! Computed animation values and their interpolation for the animation
//! origin. CSS Values 4 #interpolation / #combining-values, Web Animations 1
//! #animating-properties (animation types "by computed value", "repeatable
//! list" and "discrete"), CSS Color 4 #interpolation, #interpolation-space
//! and #interpolation-alpha, CSS Backgrounds 3 #box-shadow and CSS Text
//! Decoration 4 #text-shadow-property (shadow lists), Filter Effects 1
//! #interpolation-of-filters, CSS Display 4 #visibility. CSSWG snapshot
//! 81c27f686901 (2026-09-06).

use super::super::{Dom, NodeId, split_top_level_commas, split_top_level_ws};
use crate::layout2::value::{Len, Node as Length, Vp};
use color::{ColorSpaceTag as Space, DynamicColor, HueDirection};

/// The animation type of a supported property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// `<color>`.
    Color,
    /// SVG 2 `<paint>`: interpolable when both endpoints are colors.
    Paint,
    /// `box-shadow` (spread and `inset`) or `text-shadow`.
    Shadow { spread: bool },
    /// One `<length-percentage>`; keywords such as `auto` are discrete.
    Length { negative: bool },
    /// `letter-spacing`/`word-spacing`: `normal` computes to zero.
    Spacing,
    /// `background-position-x`/`-y`: a comma-separated list of one-axis
    /// positions, each an edge keyword with an optional offset.
    Position { vertical: bool },
    /// A `<number>`, optionally clamped to [0, 1] or rounded to an integer.
    Number { unit: bool, integer: bool },
    /// `font-weight` numbers, with the absolute keywords.
    FontWeight,
    /// CSS Display 4 #visibility.
    Visibility,
    /// Filter Effects 1 `<filter-value-list>`.
    Filter,
    /// Every other animatable property.
    Discrete,
}

impl Kind {
    pub(super) fn of(property: &str) -> Self {
        match property {
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
            | "stop-color" => Self::Color,
            "fill" | "stroke" => Self::Paint,
            "box-shadow" => Self::Shadow { spread: true },
            "text-shadow" => Self::Shadow { spread: false },
            "width"
            | "height"
            | "min-width"
            | "min-height"
            | "max-width"
            | "max-height"
            | "padding-top"
            | "padding-right"
            | "padding-bottom"
            | "padding-left"
            | "border-top-width"
            | "border-right-width"
            | "border-bottom-width"
            | "border-left-width"
            | "outline-width"
            | "font-size"
            | "flex-basis"
            | "column-gap"
            | "row-gap"
            | "-webkit-text-stroke-width"
            | "stroke-width" => Self::Length { negative: false },
            "margin-top"
            | "margin-right"
            | "margin-bottom"
            | "margin-left"
            | "right"
            | "bottom"
            | "left"
            | "text-indent"
            | "outline-offset"
            | "vertical-align"
            | "text-underline-offset"
            | "stroke-dashoffset" => Self::Length { negative: true },
            "letter-spacing" | "word-spacing" => Self::Spacing,
            "background-position-x" => Self::Position { vertical: false },
            "background-position-y" => Self::Position { vertical: true },
            "flex-grow" | "flex-shrink" | "stroke-miterlimit" => Self::Number {
                unit: false,
                integer: false,
            },
            "fill-opacity" | "stroke-opacity" | "stop-opacity" => Self::Number {
                unit: true,
                integer: false,
            },
            "z-index" | "order" | "column-count" => Self::Number {
                unit: false,
                integer: true,
            },
            "font-weight" => Self::FontWeight,
            "visibility" => Self::Visibility,
            "filter" => Self::Filter,
            _ => Self::Discrete,
        }
    }
}

/// A `<color>` endpoint. `currentcolor` interpolates as its used value
/// (CSS Color 4 #interpolation), which only paint knows when both endpoints
/// keep the keyword.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Color {
    Current,
    Value {
        color: DynamicColor,
        /// CSS Color 4 #interpolation-space: hex, named, `rgb()`, `hsl()` and
        /// `hwb()` colors interpolate in gamma-encoded sRGB, others in Oklab.
        legacy: bool,
    },
}

impl Color {
    const TRANSPARENT: Self = Self::Value {
        color: DynamicColor {
            cs: Space::Srgb,
            flags: color::Flags::from_missing(color::Missing::EMPTY),
            components: [0.0; 4],
        },
        legacy: true,
    };

    pub(super) fn parse(dom: &Dom, id: NodeId, text: &str) -> Option<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("currentcolor") {
            return Some(Self::Current);
        }
        if text.eq_ignore_ascii_case("transparent") {
            return Some(Self::TRANSPARENT);
        }
        // CSS Color 4 #css-system-colors and CSS Color 5 #light-dark compute
        // to the element's color scheme before interpolation.
        let resolved = super::super::color_scheme::resolve(text, dom.color_scheme(id));
        let text = resolved.as_deref().unwrap_or(text);
        if text.eq_ignore_ascii_case("transparent") {
            return Some(Self::TRANSPARENT);
        }
        let mixed = crate::relative_color::resolve(text).is_some();
        let color = crate::relative_color::parse(text)?;
        if !color.components.iter().all(|value| value.is_finite()) {
            return None;
        }
        // `color(srgb …)` and relative/mixed colors are not legacy syntax.
        let legacy = !mixed
            && color.flags.named()
            && matches!(color.cs, Space::Srgb | Space::Hsl | Space::Hwb);
        Some(Self::Value { color, legacy })
    }

    fn css(self) -> String {
        match self {
            Self::Current => "currentcolor".into(),
            Self::Value { color, legacy } => serialize_color(color, legacy),
        }
    }

    /// CSS Color 4 #interpolation: premultiplied, in sRGB when both colors
    /// are legacy and in Oklab otherwise.
    fn mix(self, end: Self, t: f32, current: &dyn Fn() -> Option<Self>) -> Option<Self> {
        if self == Self::Current && end == Self::Current {
            return Some(Self::Current);
        }
        let resolve = |color: Self| match color {
            Self::Current => current().filter(|color| *color != Self::Current),
            value => Some(value),
        };
        let (
            Self::Value {
                color: start,
                legacy: start_legacy,
            },
            Self::Value {
                color: end,
                legacy: end_legacy,
            },
        ) = (resolve(self)?, resolve(end)?)
        else {
            return None;
        };
        let legacy = start_legacy && end_legacy;
        let space = if legacy { Space::Srgb } else { Space::Oklab };
        let mut color = start.interpolate(end, space, HueDirection::Shorter).eval(t);
        // CSS Values 4 #combining-range: an overshooting easing still yields
        // an alpha in range.
        color.components[3] = color.components[3].clamp(0.0, 1.0);
        Some(Self::Value { color, legacy })
    }
}

/// CSS Color 4 #serializing-sRGB-values (`rgb()`/`rgba()` with 0–255
/// components) and #serializing-oklab-oklch.
fn serialize_color(color: DynamicColor, legacy: bool) -> String {
    let alpha = color.components[3].clamp(0.0, 1.0);
    let alpha_text = || {
        let text = format!("{:.4}", alpha);
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    if legacy {
        let srgb = color.convert(Space::Srgb).components;
        let channel = |value: f32| (value * 255.0).round().clamp(0.0, 255.0) as u8;
        let (r, g, b) = (channel(srgb[0]), channel(srgb[1]), channel(srgb[2]));
        if alpha >= 1.0 {
            format!("rgb({r}, {g}, {b})")
        } else {
            format!("rgba({r}, {g}, {b}, {})", alpha_text())
        }
    } else {
        let lab = color.convert(Space::Oklab).components;
        let number = |value: f32| {
            let text = format!("{:.5}", value);
            let text = text.trim_end_matches('0').trim_end_matches('.');
            if text == "-0" {
                "0".into()
            } else {
                text.to_string()
            }
        };
        if alpha >= 1.0 {
            format!(
                "oklab({} {} {})",
                number(lab[0]),
                number(lab[1]),
                number(lab[2])
            )
        } else {
            format!(
                "oklab({} {} {} / {})",
                number(lab[0]),
                number(lab[1]),
                number(lab[2]),
                alpha_text()
            )
        }
    }
}

/// One `<shadow>` with its lengths computed to CSS px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Shadow {
    color: Color,
    x: f32,
    y: f32,
    blur: f32,
    spread: f32,
    inset: bool,
}

impl Shadow {
    fn blank(inset: bool) -> Self {
        Self {
            color: Color::TRANSPARENT,
            x: 0.0,
            y: 0.0,
            blur: 0.0,
            spread: 0.0,
            inset,
        }
    }

    fn css(self, spread: bool) -> String {
        let mut out = format!(
            "{} {}px {}px {}px",
            self.color.css(),
            number(self.x),
            number(self.y),
            number(self.blur)
        );
        if spread {
            out.push_str(&format!(" {}px", number(self.spread)));
        }
        if self.inset {
            out.push_str(" inset");
        }
        out
    }
}

/// `none | <shadow>#`, with each shadow's color defaulting to `currentcolor`.
fn parse_shadows(dom: &Dom, id: NodeId, text: &str, spread: bool, vp: Vp) -> Option<Vec<Shadow>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let units = crate::layout2::Units::of(dom, id);
    split_top_level_commas(text)
        .into_iter()
        .map(|shadow| {
            let mut lengths = Vec::new();
            let mut color = None;
            let mut inset = false;
            for token in split_top_level_ws(shadow.trim()) {
                if spread && token.eq_ignore_ascii_case("inset") && !inset {
                    inset = true;
                } else if let Some(length) =
                    Len::parse(token, units, vp).and_then(|length| match length {
                        Len::Val(Length::Lin { k: 0.0, b }) => Some(b),
                        _ => None,
                    })
                {
                    lengths.push(length);
                } else if color.is_none() {
                    color = Some(Color::parse(dom, id, token)?);
                } else {
                    return None;
                }
            }
            let maximum = if spread { 4 } else { 3 };
            if !(2..=maximum).contains(&lengths.len()) || lengths.get(2).is_some_and(|b| *b < 0.0) {
                return None;
            }
            Some(Shadow {
                color: color.unwrap_or(Color::Current),
                x: lengths[0],
                y: lengths[1],
                blur: lengths.get(2).copied().unwrap_or(0.0),
                spread: lengths.get(3).copied().unwrap_or(0.0),
                inset,
            })
        })
        .collect()
}

/// A `<length-percentage>` as `px + pct·basis`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Linear {
    px: f32,
    pct: f32,
}

impl Linear {
    fn parse(dom: &Dom, id: NodeId, text: &str, vp: Vp) -> Option<Self> {
        let text = text.trim();
        if let Some(px) = text
            .strip_suffix("px")
            .unwrap_or(text)
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite() && (*value == 0.0 || text.ends_with("px")))
        {
            return Some(Self { px, pct: 0.0 });
        }
        if let Some(pct) = text
            .strip_suffix('%')
            .and_then(|text| text.parse::<f32>().ok())
            .filter(|value| value.is_finite())
        {
            return Some(Self {
                px: 0.0,
                pct: pct / 100.0,
            });
        }
        match Len::parse(text, crate::layout2::Units::of(dom, id), vp)? {
            Len::Val(Length::Lin { k, b }) => Some(Self { px: b, pct: k }),
            _ => None,
        }
    }

    fn mix(self, end: Self, t: f32) -> Self {
        Self {
            px: self.px + (end.px - self.px) * t,
            pct: self.pct + (end.pct - self.pct) * t,
        }
    }

    fn css(self) -> String {
        if self.pct == 0.0 {
            format!("{}px", number(self.px))
        } else if self.px == 0.0 {
            format!("{}%", number(self.pct * 100.0))
        } else {
            format!(
                "calc({}% + {}px)",
                number(self.pct * 100.0),
                number(self.px)
            )
        }
    }
}

/// One background position axis: `<length-percentage>`, an edge keyword, or
/// an edge keyword with an offset (CSS Backgrounds 4 #background-position-longhands).
fn parse_position(dom: &Dom, id: NodeId, text: &str, vertical: bool, vp: Vp) -> Option<Linear> {
    let tokens = split_top_level_ws(text.trim());
    let edge = |token: &str| -> Option<(f32, f32)> {
        Some(match token.to_ascii_lowercase().as_str() {
            "center" => (0.5, 1.0),
            "left" if !vertical => (0.0, 1.0),
            "right" if !vertical => (1.0, -1.0),
            "top" if vertical => (0.0, 1.0),
            "bottom" if vertical => (1.0, -1.0),
            _ => return None,
        })
    };
    match tokens.as_slice() {
        [single] => edge(single)
            .map(|(pct, _)| Linear { px: 0.0, pct })
            .or_else(|| Linear::parse(dom, id, single, vp)),
        [keyword, offset] => {
            let (pct, sign) = edge(keyword)?;
            let offset = Linear::parse(dom, id, offset, vp)?;
            Some(Linear {
                px: sign * offset.px,
                pct: pct + sign * offset.pct,
            })
        }
        _ => None,
    }
}

/// One `<filter-function>` (Filter Effects 1 #filter-functions), with
/// amounts as numbers, angles in degrees and lengths in CSS px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Filter {
    Blur(f32),
    Amount(&'static str, f32),
    HueRotate(f32),
    DropShadow(Shadow),
}

impl Filter {
    fn name(self) -> &'static str {
        match self {
            Self::Blur(_) => "blur",
            Self::Amount(name, _) => name,
            Self::HueRotate(_) => "hue-rotate",
            Self::DropShadow(_) => "drop-shadow",
        }
    }

    /// Filter Effects 1 #filter-functions: each function's initial value
    /// for interpolation (no effect), which can differ from the default for
    /// an omitted argument.
    fn neutral(self) -> Self {
        match self {
            Self::Blur(_) => Self::Blur(0.0),
            Self::Amount(name, _) => Self::Amount(
                name,
                if matches!(name, "grayscale" | "invert" | "sepia") {
                    0.0
                } else {
                    1.0
                },
            ),
            Self::HueRotate(_) => Self::HueRotate(0.0),
            Self::DropShadow(_) => Self::DropShadow(Shadow::blank(false)),
        }
    }

    fn css(self) -> String {
        match self {
            Self::Blur(px) => format!("blur({}px)", number(px)),
            Self::Amount(name, amount) => format!("{name}({})", number(amount)),
            Self::HueRotate(degrees) => format!("hue-rotate({}deg)", number(degrees)),
            Self::DropShadow(shadow) => format!("drop-shadow({})", shadow.css(false)),
        }
    }

    /// Filter Effects 1 #interpolation-of-filter-functions.
    fn mix(self, end: Self, t: f32, current: &dyn Fn() -> Option<Color>) -> Option<Self> {
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        Some(match (self, end) {
            (Self::Blur(a), Self::Blur(b)) => Self::Blur(lerp(a, b).max(0.0)),
            (Self::Amount(name, a), Self::Amount(other, b)) if name == other => {
                Self::Amount(name, lerp(a, b).max(0.0))
            }
            (Self::HueRotate(a), Self::HueRotate(b)) => Self::HueRotate(lerp(a, b)),
            (Self::DropShadow(a), Self::DropShadow(b)) => Self::DropShadow(Shadow {
                color: a.color.mix(b.color, t, current)?,
                x: lerp(a.x, b.x),
                y: lerp(a.y, b.y),
                blur: lerp(a.blur, b.blur).max(0.0),
                spread: 0.0,
                inset: false,
            }),
            _ => return None,
        })
    }
}

/// `none | <filter-value-list>`; a `url()` reference is not interpolable.
fn parse_filters(dom: &Dom, id: NodeId, text: &str, vp: Vp) -> Option<Vec<Filter>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let units = crate::layout2::Units::of(dom, id);
    split_top_level_ws(text)
        .into_iter()
        .map(|function| {
            let (name, rest) = function.split_once('(')?;
            let argument = rest.strip_suffix(')')?.trim();
            let name = name.trim().to_ascii_lowercase();
            let amount = |default: f32| -> Option<f32> {
                if argument.is_empty() {
                    return Some(default);
                }
                let value = match argument.strip_suffix('%') {
                    Some(percent) => percent.trim().parse::<f32>().ok()? / 100.0,
                    None => argument.parse::<f32>().ok()?,
                };
                (value.is_finite() && value >= 0.0).then_some(value)
            };
            Some(match name.as_str() {
                "blur" if argument.is_empty() => Filter::Blur(0.0),
                "blur" => match Len::parse(argument, units, vp)? {
                    Len::Val(Length::Lin { k, b }) if k == 0.0 && b >= 0.0 => Filter::Blur(b),
                    _ => return None,
                },
                "brightness" | "contrast" | "opacity" | "saturate" => {
                    Filter::Amount(amount_name(&name), amount(1.0)?)
                }
                "grayscale" | "invert" | "sepia" => {
                    Filter::Amount(amount_name(&name), amount(1.0)?)
                }
                "hue-rotate" => Filter::HueRotate(angle(argument)?),
                "drop-shadow" => {
                    let mut shadow = parse_shadows(dom, id, argument, false, vp)?;
                    if shadow.len() != 1 {
                        return None;
                    }
                    Filter::DropShadow(shadow.remove(0))
                }
                _ => return None,
            })
        })
        .collect()
}

fn amount_name(name: &str) -> &'static str {
    match name {
        "brightness" => "brightness",
        "contrast" => "contrast",
        "opacity" => "opacity",
        "saturate" => "saturate",
        "grayscale" => "grayscale",
        "invert" => "invert",
        _ => "sepia",
    }
}

/// An `<angle>` (or unitless zero) in degrees (CSS Values 4 #angles).
fn angle(text: &str) -> Option<f32> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Some(0.0);
    }
    let (number, scale) = if let Some(number) = text.strip_suffix("grad") {
        (number, 0.9)
    } else if let Some(number) = text.strip_suffix("deg") {
        (number, 1.0)
    } else if let Some(number) = text.strip_suffix("rad") {
        (number, 180.0 / std::f32::consts::PI)
    } else if let Some(number) = text.strip_suffix("turn") {
        (number, 360.0)
    } else if text.parse::<f32>().ok() == Some(0.0) {
        (text.as_str(), 0.0)
    } else {
        return None;
    };
    number
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value * scale)
}

/// Filter Effects 1 #interpolation-of-filters: matching function lists
/// interpolate pairwise, padding the shorter (or `none`) list with the
/// longer list's functions at their initial values for interpolation.
fn mix_filters(
    start: &[Filter],
    end: &[Filter],
    t: f32,
    current: &dyn Fn() -> Option<Color>,
) -> Option<String> {
    if start.iter().zip(end).any(|(a, b)| a.name() != b.name()) {
        return None;
    }
    let length = start.len().max(end.len());
    if length == 0 {
        return Some("none".into());
    }
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let (a, b) = match (start.get(index), end.get(index)) {
            (Some(a), Some(b)) => (*a, *b),
            (Some(a), None) => (*a, a.neutral()),
            (None, Some(b)) => (b.neutral(), *b),
            (None, None) => unreachable!("index below the longer list"),
        };
        out.push(a.mix(b, t, current)?.css());
    }
    Some(out.join(" "))
}

/// A computed keyframe or underlying value, parsed for interpolation.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    Color(Color),
    Shadows(Vec<Shadow>),
    Filters(Vec<Filter>),
    Length(Linear),
    Positions(Vec<Linear>),
    Number(f32),
    /// Kept as specified (after substitution); animates discretely.
    Other(String),
}

impl Value {
    /// Parse a computed value of `kind`. Anything that is not interpolable
    /// keeps its text and animates discretely.
    pub(super) fn parse(dom: &Dom, id: NodeId, kind: Kind, text: &str, vp: Vp) -> Self {
        let trimmed = text.trim();
        let parsed = match kind {
            Kind::Color | Kind::Paint => Color::parse(dom, id, trimmed).map(Self::Color),
            Kind::Shadow { spread } => {
                parse_shadows(dom, id, trimmed, spread, vp).map(Self::Shadows)
            }
            Kind::Length { .. } => Linear::parse(dom, id, trimmed, vp).map(Self::Length),
            Kind::Spacing if trimmed.eq_ignore_ascii_case("normal") => {
                Some(Self::Length(Linear { px: 0.0, pct: 0.0 }))
            }
            Kind::Spacing => Linear::parse(dom, id, trimmed, vp).map(Self::Length),
            Kind::Position { vertical } => split_top_level_commas(trimmed)
                .into_iter()
                .map(|layer| parse_position(dom, id, layer, vertical, vp))
                .collect::<Option<Vec<_>>>()
                .map(Self::Positions),
            Kind::Number { .. } => trimmed
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Self::Number),
            Kind::FontWeight => match trimmed.to_ascii_lowercase().as_str() {
                "normal" => Some(Self::Number(400.0)),
                "bold" => Some(Self::Number(700.0)),
                _ => trimmed
                    .parse::<f32>()
                    .ok()
                    .filter(|value| (1.0..=1000.0).contains(value))
                    .map(Self::Number),
            },
            Kind::Filter => parse_filters(dom, id, trimmed, vp).map(Self::Filters),
            Kind::Visibility | Kind::Discrete => None,
        };
        parsed.unwrap_or_else(|| Self::Other(trimmed.to_string()))
    }

    pub(super) fn css(&self, kind: Kind) -> String {
        match self {
            Self::Color(color) => color.css(),
            Self::Shadows(shadows) if shadows.is_empty() => "none".into(),
            Self::Shadows(shadows) => {
                let spread = matches!(kind, Kind::Shadow { spread: true });
                shadows
                    .iter()
                    .map(|shadow| shadow.css(spread))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            Self::Filters(filters) if filters.is_empty() => "none".into(),
            Self::Filters(filters) => filters
                .iter()
                .map(|filter| filter.css())
                .collect::<Vec<_>>()
                .join(" "),
            Self::Length(length) => length.css(),
            Self::Positions(layers) => layers
                .iter()
                .map(|layer| layer.css())
                .collect::<Vec<_>>()
                .join(", "),
            Self::Number(value) => number(*value),
            Self::Other(text) => text.clone(),
        }
    }

    /// Whether this value is a color with zero alpha (or no color). The
    /// terminal compositor decides at layout time whether a background
    /// paints at all.
    pub(super) fn transparent(&self) -> bool {
        match self {
            Self::Color(Color::Value { color, .. }) => color.components[3] <= 0.0,
            Self::Color(Color::Current) => false,
            _ => true,
        }
    }
}

/// CSS Values 4 #interpolation: the value at `t` between two computed
/// values. Values that cannot be combined animate discretely, switching at
/// the midpoint (Web Animations #discrete). `current` supplies
/// `currentcolor`'s used value.
pub(super) fn interpolate(
    kind: Kind,
    start: &Value,
    end: &Value,
    t: f32,
    current: &dyn Fn() -> Option<Color>,
) -> String {
    interpolated(kind, start, end, t, current).unwrap_or_else(|| discrete(kind, start, end, t))
}

fn discrete(kind: Kind, start: &Value, end: &Value, t: f32) -> String {
    if kind == Kind::Visibility {
        // CSS Display 4 #visibility: interpolating with `visible` maps every
        // p strictly between 0 and 1 to `visible`.
        let visible = |value: &Value| matches!(value, Value::Other(text) if text.eq_ignore_ascii_case("visible"));
        if (visible(start) || visible(end)) && t > 0.0 && t < 1.0 {
            return "visible".into();
        }
    }
    if t < 0.5 { start } else { end }.css(kind)
}

fn interpolated(
    kind: Kind,
    start: &Value,
    end: &Value,
    t: f32,
    current: &dyn Fn() -> Option<Color>,
) -> Option<String> {
    Some(match (start, end) {
        (Value::Color(a), Value::Color(b)) => a.mix(*b, t, current)?.css(),
        (Value::Shadows(a), Value::Shadows(b)) => {
            let spread = matches!(kind, Kind::Shadow { spread: true });
            mix_shadows(a, b, t, spread, current)?
        }
        (Value::Filters(a), Value::Filters(b)) => mix_filters(a, b, t, current)?,
        (Value::Length(a), Value::Length(b)) => {
            let mut value = a.mix(*b, t);
            // CSS Values 4 #combining-range: clamp to the property's range.
            if matches!(kind, Kind::Length { negative: false }) && value.pct == 0.0 {
                value.px = value.px.max(0.0);
            }
            value.css()
        }
        (Value::Positions(a), Value::Positions(b)) => {
            // Web Animations #repeatable-list: lists repeat to their least
            // common multiple length.
            let length = lcm(a.len(), b.len());
            if length == 0 || length > 64 {
                return None;
            }
            (0..length)
                .map(|index| a[index % a.len()].mix(b[index % b.len()], t).css())
                .collect::<Vec<_>>()
                .join(", ")
        }
        (Value::Number(a), Value::Number(b)) => {
            let mut value = a + (b - a) * t;
            match kind {
                Kind::Number { unit: true, .. } => value = value.clamp(0.0, 1.0),
                // CSS Values 4 #combine-integers: round to the nearest
                // integer, towards positive infinity at .5.
                Kind::Number { integer: true, .. } => value = (value + 0.5).floor(),
                Kind::FontWeight => value = value.clamp(1.0, 1000.0),
                _ => value = value.max(0.0),
            }
            number(value)
        }
        _ => return None,
    })
}

/// CSS Backgrounds 3 #box-shadow, Web Animations #animating-shadow-lists:
/// pairwise interpolation, the shorter list padded with transparent zero
/// shadows whose `inset` matches; mismatched `inset` keywords are discrete.
fn mix_shadows(
    start: &[Shadow],
    end: &[Shadow],
    t: f32,
    spread: bool,
    current: &dyn Fn() -> Option<Color>,
) -> Option<String> {
    let length = start.len().max(end.len());
    if length == 0 {
        return Some("none".into());
    }
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let (a, b) = match (start.get(index), end.get(index)) {
            (Some(a), Some(b)) => (*a, *b),
            (Some(a), None) => (*a, Shadow::blank(a.inset)),
            (None, Some(b)) => (Shadow::blank(b.inset), *b),
            (None, None) => unreachable!("index below the longer list"),
        };
        if a.inset != b.inset {
            return None;
        }
        let color = a.color.mix(b.color, t, current)?;
        let lerp = |x: f32, y: f32| x + (y - x) * t;
        out.push(Shadow {
            color,
            x: lerp(a.x, b.x),
            y: lerp(a.y, b.y),
            blur: lerp(a.blur, b.blur).max(0.0),
            spread: lerp(a.spread, b.spread),
            inset: a.inset,
        });
    }
    Some(
        out.into_iter()
            .map(|shadow| shadow.css(spread))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn lcm(a: usize, b: usize) -> usize {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    if a == 0 || b == 0 {
        0
    } else {
        a / gcd(a, b) * b
    }
}

/// A shortest round-trip-ish CSS number: at most four decimals, no `-0`.
fn number(value: f32) -> String {
    let text = format!("{:.4}", value);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" || text.is_empty() {
        "0".into()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> Vp {
        Vp { w: 800.0, h: 600.0 }
    }

    fn value(dom: &Dom, kind: Kind, text: &str) -> Value {
        Value::parse(dom, dom.get_by_id("x").unwrap(), kind, text, vp())
    }

    fn mix(dom: &Dom, kind: Kind, a: &str, b: &str, t: f32) -> String {
        let id = dom.get_by_id("x").unwrap();
        interpolate(kind, &value(dom, kind, a), &value(dom, kind, b), t, &|| {
            Color::parse(dom, id, "rgb(0, 0, 255)")
        })
    }

    fn dom() -> Dom {
        Dom::parse_document("<p id=x>x</p>")
    }

    #[test]
    fn legacy_colors_interpolate_premultiplied_in_srgb() {
        let dom = dom();
        // CSS Color 4 #interpolation-space: legacy syntax mixes in sRGB.
        assert_eq!(
            mix(&dom, Kind::Color, "red", "#0000ff", 0.5),
            "rgb(128, 0, 128)"
        );
        assert_eq!(
            mix(&dom, Kind::Color, "rgb(255 0 0)", "hsl(120 100% 50%)", 0.25),
            "rgb(191, 64, 0)"
        );
        // #interpolation-alpha: premultiplied, so transparent black does not
        // darken the color while it fades.
        assert_eq!(
            mix(&dom, Kind::Color, "transparent", "rgb(200, 100, 0)", 0.5),
            "rgba(200, 100, 0, 0.5)"
        );
        assert_eq!(mix(&dom, Kind::Color, "red", "blue", 0.0), "rgb(255, 0, 0)");
    }

    #[test]
    fn non_legacy_colors_interpolate_in_oklab() {
        let dom = dom();
        // Any non-legacy endpoint selects Oklab (#interpolation-space).
        let mixed = mix(&dom, Kind::Color, "color(srgb 1 0 0)", "blue", 0.5);
        assert!(mixed.starts_with("oklab("), "{mixed}");
        let expected = color::parse_color("color(srgb 1 0 0)")
            .unwrap()
            .interpolate(
                color::parse_color("blue").unwrap(),
                Space::Oklab,
                HueDirection::Shorter,
            )
            .eval(0.5)
            .convert(Space::Oklab);
        let parsed = color::parse_color(&mixed).unwrap();
        for i in 0..3 {
            assert!((parsed.components[i] - expected.components[i]).abs() < 1e-4);
        }
    }

    #[test]
    fn currentcolor_keeps_the_keyword_or_uses_the_used_color() {
        let dom = dom();
        assert_eq!(
            mix(&dom, Kind::Color, "currentcolor", "currentColor", 0.3),
            "currentcolor"
        );
        // The used value of `currentcolor` (blue here) mixes with red.
        assert_eq!(
            mix(&dom, Kind::Color, "currentcolor", "red", 0.5),
            "rgb(128, 0, 128)"
        );
    }

    #[test]
    fn shadow_lists_pad_with_transparent_shadows_and_flip_on_inset_mismatch() {
        let dom = dom();
        let spread = Kind::Shadow { spread: true };
        let text = Kind::Shadow { spread: false };
        assert_eq!(
            mix(
                &dom,
                text,
                "0 0 10px #fff",
                "2px 4px 20px #fff, 0 0 30px red",
                0.5
            ),
            "rgb(255, 255, 255) 1px 2px 15px, rgba(255, 0, 0, 0.5) 0px 0px 15px"
        );
        assert_eq!(
            mix(&dom, text, "none", "0 0 8px red", 0.5),
            "rgba(255, 0, 0, 0.5) 0px 0px 4px"
        );
        // Matching inset keywords interpolate; padding keeps `inset`.
        assert_eq!(
            mix(&dom, spread, "none", "inset 0 0 4px 2px red", 0.5),
            "rgba(255, 0, 0, 0.5) 0px 0px 2px 1px inset"
        );
        // A mismatched inset keyword is not interpolable: discrete at 50%.
        assert_eq!(
            mix(&dom, spread, "0 0 4px red", "inset 0 0 4px red", 0.4),
            "rgb(255, 0, 0) 0px 0px 4px 0px"
        );
        assert_eq!(
            mix(&dom, spread, "0 0 4px red", "inset 0 0 4px red", 0.6),
            "rgb(255, 0, 0) 0px 0px 4px 0px inset"
        );
        // An omitted color is currentcolor, kept symbolic for paint.
        assert_eq!(
            mix(&dom, text, "0 0 2px", "0 0 6px", 0.5),
            "currentcolor 0px 0px 4px"
        );
    }

    #[test]
    fn lengths_numbers_and_discrete_values() {
        let dom = dom();
        let length = Kind::Length { negative: false };
        assert_eq!(mix(&dom, length, "10px", "30px", 0.25), "15px");
        assert_eq!(mix(&dom, length, "10px", "50%", 0.5), "calc(25% + 5px)");
        // Keywords do not interpolate: discrete at the midpoint.
        assert_eq!(mix(&dom, length, "auto", "30px", 0.49), "auto");
        assert_eq!(mix(&dom, length, "auto", "30px", 0.5), "30px");
        assert_eq!(mix(&dom, length, "10px", "20px", -2.0), "0px");
        assert_eq!(mix(&dom, Kind::Spacing, "normal", "4px", 0.5), "2px");
        assert_eq!(
            mix(
                &dom,
                Kind::Number {
                    unit: false,
                    integer: true
                },
                "1",
                "2",
                0.5
            ),
            "2"
        );
        assert_eq!(mix(&dom, Kind::FontWeight, "normal", "bold", 0.5), "550");
        assert_eq!(
            mix(
                &dom,
                Kind::Position { vertical: false },
                "left",
                "right 10px",
                0.5
            ),
            "calc(50% + -5px)"
        );
        assert_eq!(mix(&dom, Kind::Discrete, "solid", "dashed", 0.6), "dashed");
        // Filter Effects 1 #interpolation-of-filters.
        assert_eq!(
            mix(
                &dom,
                Kind::Filter,
                "hue-rotate(0deg)",
                "hue-rotate(1turn)",
                0.25
            ),
            "hue-rotate(90deg)"
        );
        assert_eq!(
            mix(&dom, Kind::Filter, "none", "blur(4px) invert(100%)", 0.5),
            "blur(2px) invert(0.5)"
        );
        assert_eq!(
            mix(
                &dom,
                Kind::Filter,
                "brightness(2)",
                "brightness(1) sepia(1)",
                0.5
            ),
            "brightness(1.5) sepia(0.5)"
        );
        assert_eq!(
            mix(
                &dom,
                Kind::Filter,
                "drop-shadow(0 0 4px red)",
                "drop-shadow(2px 2px 8px red)",
                0.5
            ),
            "drop-shadow(rgb(255, 0, 0) 1px 1px 6px)"
        );
        // Mismatched function lists animate discretely.
        assert_eq!(
            mix(&dom, Kind::Filter, "blur(2px)", "sepia(1)", 0.4),
            "blur(2px)"
        );
        // CSS Display 4 #visibility.
        assert_eq!(
            mix(&dom, Kind::Visibility, "hidden", "visible", 0.1),
            "visible"
        );
        assert_eq!(
            mix(&dom, Kind::Visibility, "hidden", "visible", 0.0),
            "hidden"
        );
        assert_eq!(
            mix(&dom, Kind::Visibility, "hidden", "collapse", 0.4),
            "hidden"
        );
    }
}
