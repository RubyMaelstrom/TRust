//! The Color state of the `input` element (local whatwg/html e5071a20,
//! 2026-09-04, #color-state-(type=color)): its value sanitization algorithm
//! runs "update a color well control color", which parses the value as a CSS
//! `<color>` (CSS Color 4 #parse-a-css-color-value, local csswg-drafts
//! 81c27f68), falls back to opaque black, and stores "serialize a color well
//! control color" of the result. The `alpha` boolean attribute keeps the
//! alpha component; the `colorspace` enumerated attribute selects Limited
//! sRGB (the missing and invalid value default) or Display P3.

use color::{ColorSpaceTag, DynamicColor};

/// HTML #attr-input-colorspace: the attribute's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ColorSpace {
    LimitedSrgb,
    DisplayP3,
}

impl ColorSpace {
    /// The keywords are ASCII case-insensitive; the missing value default
    /// and the invalid value default are both Limited sRGB.
    pub(super) fn from_attribute(value: Option<&str>) -> Self {
        match value {
            Some(value) if value.eq_ignore_ascii_case("display-p3") => Self::DisplayP3,
            _ => Self::LimitedSrgb,
        }
    }
}

/// HTML #update-a-color-well-control-color steps 3-5, given the value step 2
/// chose (the `value` content attribute while the dirty value flag is false,
/// else the element's value).
pub(super) fn sanitize(value: &str, alpha: bool, space: ColorSpace) -> String {
    let color = parse(value).unwrap_or_else(opaque_black);
    serialize(color, alpha, space)
}

/// CSS Color 4 #opaque-black.
fn opaque_black() -> DynamicColor {
    DynamicColor::from_alpha_color(color::AlphaColor::<color::Srgb>::new([0., 0., 0., 1.]))
}

/// CSS Color 4 #parse-a-css-color-value without a context element: parse
/// `input` as a `<color>` (relative colors and `color-mix()` included), and
/// resolve it to a used color with the initial values of the properties the
/// resolution needs: `currentcolor` is the initial `color`, CanvasText, and
/// system colors and `light-dark()` take the initial `color-scheme`, whose
/// used scheme is light.
fn parse(input: &str) -> Option<DynamicColor> {
    // CSS Syntax 3 #parse-grammar: surrounding whitespace is not part of
    // the `<color>`.
    let text = input.trim_matches(['\t', '\n', '\x0c', '\r', ' ']);
    if text.is_empty() {
        return None;
    }
    let text = replace_current_color(text);
    let text = super::color_scheme::resolve(&text, super::color_scheme::ColorScheme::Light)
        .unwrap_or(text);
    crate::relative_color::parse(&text)
}

/// Replace each `currentcolor` keyword with the initial value of `color`.
fn replace_current_color(text: &str) -> String {
    const KEYWORD: &str = "currentcolor";
    let ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_';
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let end = i + KEYWORD.len();
        if end <= bytes.len()
            && bytes[i..end].eq_ignore_ascii_case(KEYWORD.as_bytes())
            && (i == 0 || !ident(bytes[i - 1]))
            && bytes.get(end).is_none_or(|&b| !ident(b))
        {
            out.push_str("canvastext");
            i = end;
        } else {
            let ch = text[i..].chars().next().expect("in bounds");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// HTML #serialize-a-color-well-control-color.
fn serialize(color: DynamicColor, alpha: bool, space: ColorSpace) -> String {
    let missing = color.flags.missing();
    let mut components = color.components;
    // CSS Color 4 #serializing-color-values: a form that converts missing
    // components treats them as zero.
    for (index, component) in components.iter_mut().enumerate() {
        if missing.contains(index) {
            *component = 0.;
        }
    }
    let color = DynamicColor {
        components,
        ..color
    };
    let alpha_value = if alpha {
        f64::from(color.components[3]).clamp(0., 1.)
    } else {
        // Step 3: without `alpha`, the color is fully opaque.
        1.
    };
    let alpha_suffix = |value: f64| {
        if value >= 1. {
            String::new()
        } else {
            format!(" / {}", number(value))
        }
    };
    match space {
        ColorSpace::LimitedSrgb => {
            // Step 4.1: convert to 'srgb'; step 4.2: round each component
            // to the range 0..=255, rounding towards +infinity
            // (CSS Values 4 #combine-integers).
            let srgb = color.convert(ColorSpaceTag::Srgb).components;
            let [r, g, b] = [srgb[0], srgb[1], srgb[2]].map(|component| {
                let scaled = (f64::from(component) * 255.).clamp(0., 255.);
                (scaled + 0.5).floor() as u8
            });
            if !alpha {
                // Steps 4.3 and 6: HTML-compatible serialization requested,
                // CSS Color 4 #HTML-compatible-serialization-of-srgb.
                return format!("#{r:02x}{g:02x}{b:02x}");
            }
            // Step 4.4: the 'color()' function (CSS Color 4
            // #serializing-color-function-values).
            let channel = |value: u8| number(f64::from(value) / 255.);
            format!(
                "color(srgb {} {} {}{})",
                channel(r),
                channel(g),
                channel(b),
                alpha_suffix(alpha_value)
            )
        }
        ColorSpace::DisplayP3 => {
            let p3 = color.convert(ColorSpaceTag::DisplayP3).components;
            format!(
                "color(display-p3 {} {} {}{})",
                number(f64::from(p3[0])),
                number(f64::from(p3[1])),
                number(f64::from(p3[2])),
                alpha_suffix(alpha_value)
            )
        }
    }
}

/// CSS Color 4 #serializing-color-function-values: a base-10 `<number>`
/// without trailing fractional zeroes, at the single-precision round-trip
/// precision TRust's computed colors use.
fn number(value: f64) -> String {
    let value = value as f32;
    if value == 0. || !value.is_finite() {
        return String::from("0");
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb(value: &str) -> String {
        sanitize(value, false, ColorSpace::LimitedSrgb)
    }

    #[test]
    fn values_sanitize_to_lowercase_hex_or_opaque_black() {
        // HTML #update-a-color-well-control-color: failure is opaque black,
        // and Limited sRGB without `alpha` requests the HTML-compatible
        // serialization (CSS Color 4 #HTML-compatible-serialization-of-srgb).
        for (input, expected) in [
            ("", "#000000"),
            ("#FFAABB", "#ffaabb"),
            ("  #abc\n", "#aabbcc"),
            ("red", "#ff0000"),
            ("RED", "#ff0000"),
            ("rgb(0 128 255 / 0.25)", "#0080ff"),
            ("transparent", "#000000"),
            ("currentColor", "#000000"),
            ("CanvasText", "#000000"),
            ("Canvas", "#ffffff"),
            ("light-dark(#010203, white)", "#010203"),
            ("color-mix(in srgb, red, blue)", "#800080"),
            ("rgb(from red r g 255)", "#ff00ff"),
            ("hsl(120deg 100% 50%)", "#00ff00"),
            ("color(display-p3 1 0 0)", "#ff0000"),
            ("#12345", "#000000"),
            ("bogus", "#000000"),
            ("red blue", "#000000"),
        ] {
            assert_eq!(srgb(input), expected, "{input:?}");
        }
    }

    #[test]
    fn components_round_towards_positive_infinity() {
        // 127.5 rounds up (CSS Values 4 #combine-integers).
        assert_eq!(srgb("color(srgb 0.5 0 0)"), "#800000");
        assert_eq!(srgb("rgb(127.4 0 0)"), "#7f0000");
    }

    #[test]
    fn alpha_and_colorspace_select_the_color_function() {
        // Step 4.4: with `alpha`, Limited sRGB serializes through 'color()';
        // an opaque alpha is omitted (#serializing-alpha-values).
        assert_eq!(
            sanitize("rgb(0 255 0 / 0.5)", true, ColorSpace::LimitedSrgb),
            "color(srgb 0 1 0 / 0.5)"
        );
        assert_eq!(
            sanitize("#ff0000", true, ColorSpace::LimitedSrgb),
            "color(srgb 1 0 0)"
        );
        assert_eq!(
            sanitize("#ff000080", false, ColorSpace::LimitedSrgb),
            "#ff0000"
        );
        assert_eq!(
            sanitize("bogus", true, ColorSpace::LimitedSrgb),
            "color(srgb 0 0 0)"
        );
        // Step 5: Display P3 converts without 8-bit rounding.
        assert_eq!(
            sanitize(
                "color(display-p3 1 0.5 0 / 0.25)",
                true,
                ColorSpace::DisplayP3
            ),
            "color(display-p3 1 0.5 0 / 0.25)"
        );
        assert_eq!(
            sanitize(
                "color(display-p3 1 0.5 0 / 0.25)",
                false,
                ColorSpace::DisplayP3
            ),
            "color(display-p3 1 0.5 0)"
        );
        assert_eq!(
            ColorSpace::from_attribute(Some("DISPLAY-P3")),
            ColorSpace::DisplayP3
        );
        assert_eq!(
            ColorSpace::from_attribute(Some("srgb")),
            ColorSpace::LimitedSrgb
        );
    }
}
