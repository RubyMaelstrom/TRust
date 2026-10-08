//! Canvas text uses the browser's font catalog, shaper and glyph renderer.
//! HTML #canvastextdrawingstyles, #text-preparation-algorithm and
//! #drawing-text-to-the-bitmap; CSS Fonts 4 #font-kerning-prop,
//! #font-stretch-prop and #font-variant-caps-prop; CSS Text 3 #spacing;
//! CSS Inline 3 #baseline-tables and appendix A (local September 6, 2026
//! WHATWG e5071a20 and CSSWG 81c27f68 snapshots).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::text::{CanvasTypography, ShapedText, TextStyle};
use unicode_segmentation::UnicodeSegmentation as _;
use vello_common::paint::{Image, ImageSource, PaintType, Tint};
use vello_cpu::color::{AlphaColor, Srgb, palette::css::BLACK};
use vello_cpu::kurbo::{Affine, BezPath, Rect, Shape};
use vello_cpu::peniko::BlendMode;

/// CanvasFontKerning, CanvasFontStretch (with CSS Fonts 4 width ratios),
/// CanvasFontVariantCaps and CanvasTextRendering enumeration values.
const KERNING: [&str; 3] = ["auto", "normal", "none"];
const STRETCH: [(&str, f32); 9] = [
    ("ultra-condensed", 0.5),
    ("extra-condensed", 0.625),
    ("condensed", 0.75),
    ("semi-condensed", 0.875),
    ("normal", 1.),
    ("semi-expanded", 1.125),
    ("expanded", 1.25),
    ("extra-expanded", 1.5),
    ("ultra-expanded", 2.),
];
const CAPS: [&str; 7] = [
    "normal",
    "small-caps",
    "all-small-caps",
    "petite-caps",
    "all-petite-caps",
    "unicase",
    "titling-caps",
];
const RENDERING: [&str; 4] = [
    "auto",
    "optimizeSpeed",
    "optimizeLegibility",
    "geometricPrecision",
];
/// Synthesized small capitals: uppercase glyphs scaled to this fraction of
/// the font size (CSS leaves the factor to the UA; this is Chromium's).
const SMALL_CAPS_SCALE: f32 = 0.7;

/// The CanvasTextDrawingStyles attributes the host exposes by index.
pub(crate) const FONT: u8 = 0;
pub(crate) const TEXT_ALIGN: u8 = 1;
pub(crate) const TEXT_BASELINE: u8 = 2;
pub(crate) const DIRECTION: u8 = 3;
pub(crate) const LETTER_SPACING: u8 = 4;
pub(crate) const WORD_SPACING: u8 = 5;
pub(crate) const FONT_KERNING: u8 = 6;
pub(crate) const FONT_STRETCH: u8 = 7;
pub(crate) const FONT_VARIANT_CAPS: u8 = 8;
pub(crate) const TEXT_RENDERING: u8 = 9;
pub(crate) const LANG: u8 = 10;

/// What the font style source object contributes to text preparation:
/// "inherit" direction and language, and the environment of relative units.
#[derive(Clone, Debug)]
pub(crate) struct TextEnvironment {
    /// The canvas element's directionality, or the OffscreenCanvas's
    /// inherited direction.
    pub rtl: bool,
    /// The canvas element's language, or the OffscreenCanvas's inherited
    /// language; None is "explicitly unknown".
    pub language: Option<String>,
    pub root_font_size: f32,
    pub viewport: (f32, f32),
}

impl Default for TextEnvironment {
    fn default() -> Self {
        Self {
            rtl: false,
            language: None,
            root_font_size: 16.,
            viewport: (0., 0.),
        }
    }
}

/// A letter or word spacing: a CSS `<length>` kept as specified, because
/// font-relative units resolve against the font in effect when text is
/// prepared (the hypothetical inline box's 'font').
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Spacing {
    value: f32,
    unit: &'static str,
}

const LENGTH_UNITS: [&str; 39] = [
    "px", "cm", "mm", "q", "in", "pt", "pc", "em", "rem", "ex", "rex", "ch", "rch", "cap", "rcap",
    "ic", "ric", "lh", "rlh", "vw", "vh", "vi", "vb", "vmin", "vmax", "svw", "svh", "svmin",
    "svmax", "lvw", "lvh", "lvmin", "lvmax", "dvw", "dvh", "dvmin", "dvmax", "svi", "svb",
];

impl Spacing {
    const ZERO: Self = Self {
        value: 0.,
        unit: "px",
    };

    /// HTML #dom-context-2d-letterspacing setter step 1: parse a CSS
    /// `<length>` (a dimension with a length unit, or unitless zero).
    pub fn parse(text: &str) -> Option<Self> {
        use cssparser::{Parser, Token};
        let mut parser = Parser::new(text);
        let spacing = match parser.next().ok()?.clone() {
            Token::Number { value: 0., .. } => Self::ZERO,
            Token::Dimension { value, unit, .. } => {
                let lower = unit.to_ascii_lowercase();
                let unit = LENGTH_UNITS.iter().find(|known| **known == lower)?;
                Self { value, unit }
            }
            _ => return None,
        };
        parser.expect_exhausted().ok()?;
        spacing.value.is_finite().then_some(spacing)
    }

    /// CSSOM #serialize-a-css-component-value for a `<length>`.
    pub fn serialize(&self) -> String {
        let value = if self.value == 0. { 0. } else { self.value };
        format!("{value}{}", self.unit)
    }

    fn resolve(&self, units: &FontUnits) -> f32 {
        let n = self.value;
        let (vw, vh) = units.viewport;
        match self.unit {
            "px" => n,
            "cm" => n * 96. / 2.54,
            "mm" => n * 96. / 25.4,
            "q" => n * 96. / 101.6,
            "in" => n * 96.,
            "pt" => n * 4. / 3.,
            "pc" => n * 16.,
            "em" => n * units.size,
            "rem" => n * units.root,
            "ex" => n * units.x_height,
            "rex" => n * units.root * 0.5,
            "ch" => n * units.zero,
            "rch" => n * units.root * 0.5,
            "cap" => n * units.cap_height,
            "rcap" => n * units.root * units.cap_height / units.size.max(f32::MIN_POSITIVE),
            "ic" => n * units.ideograph,
            "ric" => n * units.root,
            "lh" => n * units.line_height,
            "rlh" => n * units.root * 1.2,
            "vw" | "vi" | "svw" | "lvw" | "dvw" | "svi" => n * vw / 100.,
            "vh" | "vb" | "svh" | "lvh" | "dvh" | "svb" => n * vh / 100.,
            "vmin" | "svmin" | "lvmin" | "dvmin" => n * vw.min(vh) / 100.,
            "vmax" | "svmax" | "lvmax" | "dvmax" => n * vw.max(vh) / 100.,
            _ => 0.,
        }
    }
}

/// The resolved font-relative unit bases of the first available font.
struct FontUnits {
    size: f32,
    root: f32,
    x_height: f32,
    cap_height: f32,
    zero: f32,
    ideograph: f32,
    line_height: f32,
    viewport: (f32, f32),
}

#[derive(Clone)]
pub(crate) struct TextState {
    /// family, size, weight and style of the current font.
    pub style: TextStyle,
    /// The font shorthand's font-style keyword (`italic`, `oblique`).
    pub font_style: Option<String>,
    pub align: String,
    pub baseline: String,
    pub direction: String,
    pub letter_spacing: Spacing,
    pub word_spacing: Spacing,
    pub kerning: &'static str,
    pub stretch: &'static str,
    pub variant_caps: &'static str,
    pub rendering: &'static str,
    /// HTML #concept-CanvasTextDrawingStyles-language.
    pub lang: String,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            style: TextStyle {
                size: 10.,
                ..TextStyle::default()
            },
            font_style: None,
            align: "start".into(),
            baseline: "alphabetic".into(),
            direction: "inherit".into(),
            letter_spacing: Spacing::ZERO,
            word_spacing: Spacing::ZERO,
            kerning: "auto",
            stretch: "normal",
            variant_caps: "normal",
            rendering: "auto",
            lang: "inherit".into(),
        }
    }
}

impl TextState {
    pub fn retained_bytes(&self) -> usize {
        self.style.family.capacity()
            + self.style.language.as_ref().map_or(0, String::capacity)
            + self.font_style.as_ref().map_or(0, String::capacity)
            + self.align.capacity()
            + self.baseline.capacity()
            + self.direction.capacity()
            + self.lang.capacity()
    }

    /// HTML #dom-context-2d-font getter: the serialized current font, without
    /// a line-height. Its font-variant and font-stretch components are the
    /// fontVariantCaps and fontStretch states the font is drawn with; only
    /// `small-caps` is expressible in the shorthand.
    pub fn font(&self) -> String {
        let mut parts = Vec::new();
        if let Some(style) = &self.font_style {
            parts.push(style.clone());
        }
        if self.variant_caps == "small-caps" {
            parts.push("small-caps".into());
        }
        let weight = self.style.weight;
        if weight != 400. {
            parts.push(if weight == 700. {
                "bold".into()
            } else {
                weight.to_string()
            });
        }
        if self.stretch != "normal" {
            parts.push(self.stretch.into());
        }
        parts.push(format!("{}px", self.style.size));
        parts.push(self.style.family.clone());
        parts.join(" ")
    }

    /// The value of an attribute other than `font`.
    pub fn attribute(&self, index: u8) -> String {
        match index {
            TEXT_ALIGN => self.align.clone(),
            TEXT_BASELINE => self.baseline.clone(),
            DIRECTION => self.direction.clone(),
            LETTER_SPACING => self.letter_spacing.serialize(),
            WORD_SPACING => self.word_spacing.serialize(),
            FONT_KERNING => self.kerning.into(),
            FONT_STRETCH => self.stretch.into(),
            FONT_VARIANT_CAPS => self.variant_caps.into(),
            TEXT_RENDERING => self.rendering.into(),
            LANG => self.lang.clone(),
            _ => self.font(),
        }
    }

    /// Set an attribute other than `font`. Enumerated attributes ignore
    /// values outside their enumeration (Web IDL); letterSpacing and
    /// wordSpacing ignore values that do not parse as a CSS `<length>`.
    pub fn set_attribute(&mut self, index: u8, value: &str) {
        let keyword = |table: &[&'static str]| table.iter().copied().find(|k| *k == value);
        match index {
            TEXT_ALIGN if matches!(value, "start" | "end" | "left" | "right" | "center") => {
                self.align = value.into()
            }
            TEXT_BASELINE
                if matches!(
                    value,
                    "top" | "hanging" | "middle" | "alphabetic" | "ideographic" | "bottom"
                ) =>
            {
                self.baseline = value.into()
            }
            DIRECTION if matches!(value, "inherit" | "ltr" | "rtl") => {
                self.direction = value.into()
            }
            LETTER_SPACING => {
                if let Some(spacing) = Spacing::parse(value) {
                    self.letter_spacing = spacing;
                }
            }
            WORD_SPACING => {
                if let Some(spacing) = Spacing::parse(value) {
                    self.word_spacing = spacing;
                }
            }
            FONT_KERNING => {
                if let Some(value) = keyword(&KERNING) {
                    self.kerning = value;
                }
            }
            FONT_STRETCH => {
                if let Some((value, _)) = STRETCH.iter().find(|(k, _)| *k == value) {
                    self.stretch = value;
                }
            }
            FONT_VARIANT_CAPS => {
                if let Some(value) = keyword(&CAPS) {
                    self.variant_caps = value;
                }
            }
            TEXT_RENDERING => {
                if let Some(value) = keyword(&RENDERING) {
                    self.rendering = value;
                }
            }
            // "a BCP 47 language tag, the empty string, or inherit": any
            // DOMString is stored and used as the content language.
            LANG => self.lang = value.into(),
            _ => {}
        }
    }

    /// HTML #dom-context-2d-font setter. The 'font' shorthand also sets its
    /// font-stretch and font-variant-caps longhands and resets font-kerning
    /// (CSS Fonts 4 #font-prop), which these attributes reflect.
    pub fn set_font(&mut self, value: &str, units: crate::layout2::Units, weight: f32) {
        // Use the existing SVG/CSS shorthand parser and common CSS length
        // conversion, with the canvas element's computed font environment.
        let Ok(font) = svgtypes::FontShorthand::from_str(value) else {
            return;
        };
        // SVG's legacy length grammar accepts nonzero unitless sizes; CSS
        // font-size is a length-percentage and permits only unitless zero.
        if font.font_size.parse::<f32>().is_ok_and(|n| n != 0.) {
            return;
        }
        let Ok(families) = svgtypes::parse_font_families(font.font_family) else {
            return;
        };
        if families.is_empty() {
            return;
        }
        let size = match font.font_size {
            keyword if crate::dom::absolute_size_px(keyword).is_some() => {
                crate::dom::absolute_size_px(keyword)
            }
            "larger" => Some(units.fs * 1.2),
            "smaller" => Some(units.fs / 1.2),
            size if size.ends_with('%') => size[..size.len() - 1]
                .parse::<f32>()
                .ok()
                .map(|n| n * units.fs / 100.),
            size => crate::layout2::css_length_px(size, units),
        };
        let Some(size) = size.filter(|n| n.is_finite() && *n >= 0.) else {
            return;
        };
        let weight = match font.font_weight.unwrap_or("normal") {
            "bolder" => {
                if weight < 350. {
                    400.
                } else if weight < 550. {
                    700.
                } else if weight < 900. {
                    900.
                } else {
                    weight
                }
            }
            "lighter" => {
                if weight < 100. {
                    weight
                } else if weight < 550. {
                    100.
                } else if weight < 750. {
                    400.
                } else {
                    700.
                }
            }
            value => crate::layout2::css_font_weight(value).unwrap_or(400.),
        };
        let family = families
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.font_style = font.font_style.map(str::to_string);
        self.variant_caps = if font.font_variant == Some("small-caps") {
            "small-caps"
        } else {
            "normal"
        };
        self.stretch = font
            .font_stretch
            .and_then(|stretch| STRETCH.iter().find(|(k, _)| *k == stretch))
            .map_or("normal", |(k, _)| k);
        self.kerning = "auto";
        self.style = TextStyle {
            family,
            size,
            weight,
            italic: font.font_style.is_some(),
            ..TextStyle::default()
        };
    }

    /// HTML #text-preparation-algorithm (without maxWidth, which the painters
    /// apply as a horizontal scale): the shaped glyphs of the hypothetical
    /// inline box, its anchor point for textAlign/textBaseline and the
    /// metrics of its first available font.
    pub fn prepare(&self, text: &str, environment: &TextEnvironment) -> PreparedText {
        // Step 2: ASCII whitespace becomes U+0020.
        let text: String = text
            .chars()
            .map(|c| {
                if matches!(c, '\t' | '\n' | '\r' | '\x0c') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        // Steps 4-5: the language; "" is explicitly unknown.
        let language = match self.lang.as_str() {
            "inherit" => environment.language.clone(),
            language => Some(language.to_string()),
        }
        .filter(|language| !language.is_empty());
        // Step 6: the direction.
        let rtl = match self.direction.as_str() {
            "rtl" => true,
            "ltr" => false,
            _ => environment.rtl,
        };
        let mut style = self.style.clone();
        style.language = language.clone();
        let width = STRETCH
            .iter()
            .find(|(k, _)| *k == self.stretch)
            .map_or(1., |(_, ratio)| *ratio);
        let plain = CanvasTypography {
            width,
            ..CanvasTypography::default()
        };
        // Space selects the first available font for stable font-box/em metrics,
        // independently of later fallback glyphs in the measured string.
        let primary = crate::text::shape_canvas_with(" ", &style, &plain);
        let face = primary.runs.first().map(|run| face_tables(&run.font));
        let size = style.size;
        let units = FontUnits {
            size,
            root: environment.root_font_size,
            x_height: face
                .as_ref()
                .and_then(|face| face.x_height)
                .map_or(size * 0.5, |ratio| ratio * size),
            cap_height: face
                .as_ref()
                .and_then(|face| face.cap_height)
                .map_or(primary.ascent.max(0.), |ratio| ratio * size),
            zero: crate::text::shape_canvas_with("0", &style, &plain)
                .advance
                .max(size * 0.25),
            ideograph: {
                let shaped = crate::text::shape_canvas_with("\u{6C34}", &style, &plain);
                if shaped.advance > 0. {
                    shaped.advance
                } else {
                    size
                }
            },
            line_height: primary.ascent + primary.descent + primary.leading,
            viewport: environment.viewport,
        };
        // Step 7's hypothetical inline box: 'letter-spacing', 'word-spacing'.
        style.letter_spacing = self.letter_spacing.resolve(&units);
        style.word_spacing = self.word_spacing.resolve(&units);
        // 'font-kerning' (CSS Fonts 4: "auto" is at the UA's discretion; SVG
        // 'text-rendering: optimizeSpeed' trades kerning and optional
        // ligatures for speed, other values keep them).
        let speed = self.rendering == "optimizeSpeed";
        let kern = match self.kerning {
            "normal" => 1,
            "none" => 0,
            _ => u16::from(!speed),
        };
        let mut features = vec![(*b"kern", kern)];
        if speed {
            features.push((*b"liga", 0));
            features.push((*b"clig", 0));
        }
        // 'font-variant-caps', synthesizing small capitals when the first
        // available font lacks them.
        let supports = |tag: &[u8; 4]| {
            face.as_ref()
                .is_some_and(|face| face.features.contains(tag))
        };
        let synthesize = match self.variant_caps {
            "small-caps" if supports(b"smcp") => {
                features.push((*b"smcp", 1));
                None
            }
            "small-caps" => Some(Caps::Lower),
            "all-small-caps" if supports(b"smcp") && supports(b"c2sc") => {
                features.extend([(*b"smcp", 1), (*b"c2sc", 1)]);
                None
            }
            "all-small-caps" => Some(Caps::All),
            "petite-caps" if supports(b"pcap") => {
                features.push((*b"pcap", 1));
                None
            }
            // "behaves as if small-caps ... had been specified".
            "petite-caps" if supports(b"smcp") => {
                features.push((*b"smcp", 1));
                None
            }
            "petite-caps" => Some(Caps::Lower),
            "all-petite-caps" if supports(b"pcap") && supports(b"c2pc") => {
                features.extend([(*b"pcap", 1), (*b"c2pc", 1)]);
                None
            }
            "all-petite-caps" if supports(b"smcp") && supports(b"c2sc") => {
                features.extend([(*b"smcp", 1), (*b"c2sc", 1)]);
                None
            }
            "all-petite-caps" => Some(Caps::All),
            "unicase" if supports(b"unic") => {
                features.push((*b"unic", 1));
                None
            }
            // "as if small-caps was applied only to lowercased uppercase letters".
            "unicase" => Some(Caps::Upper),
            "titling-caps" => {
                // Without the feature the property has no visible effect.
                if supports(b"titl") {
                    features.push((*b"titl", 1));
                }
                None
            }
            _ => None,
        };
        let turkic = language.as_deref().is_some_and(|tag| {
            let primary = tag.split(['-', '_']).next().unwrap_or("");
            primary.eq_ignore_ascii_case("tr") || primary.eq_ignore_ascii_case("az")
        });
        let (mut shaped_text, mut sized) = match synthesize {
            Some(caps) => synthesize_caps(&text, caps, size * SMALL_CAPS_SCALE, turkic),
            None => (text.clone(), Vec::new()),
        };
        // 'direction' sets the paragraph's base direction: a strong mark
        // decides UAX #9 rule P2 without adding a glyph or advance.
        shaped_text.insert(0, if rtl { '\u{200F}' } else { '\u{200E}' });
        let mark = if rtl { '\u{200F}' } else { '\u{200E}' }.len_utf8();
        for (range, _) in &mut sized {
            *range = range.start + mark..range.end + mark;
        }
        let shaped = crate::text::shape_canvas_with(
            &shaped_text,
            &style,
            &CanvasTypography {
                width,
                features,
                sized,
                unspaced_prefix: mark,
            },
        );
        let ascent = f64::from(primary.ascent);
        let descent = f64::from(primary.descent);
        let em = f64::from(size);
        let base = face
            .as_ref()
            .map(|face| face.baselines(&text))
            .unwrap_or_default();
        let scale = |value: Option<f32>| value.map(|ratio| f64::from(ratio) * em);
        let (hang, ideo, idtp, romn) = (
            scale(base.hanging),
            scale(base.ideographic_under),
            scale(base.ideographic_over),
            scale(base.alphabetic),
        );
        // CSS Inline 3 #baseline-synthesis-em: from the central baseline
        // when any ideographic baseline is defined, otherwise the ascent and
        // descent normalized to 1em. #baseline-synthesis-fonts: the other
        // ideographic baseline lies 1em away.
        let central = match (idtp, ideo) {
            (Some(over), Some(under)) => Some((over + under) * 0.5),
            (Some(over), None) => Some(over - em * 0.5),
            (None, Some(under)) => Some(under + em * 0.5),
            (None, None) => None,
        };
        let (em_over, em_under) = match central {
            Some(central) => (central + em * 0.5, central - em * 0.5),
            None if ascent + descent > 0. => {
                let over = ascent / (ascent + descent) * em;
                (over, over - em)
            }
            None => (0., -em),
        };
        // Synthesized when the font defines none: the hanging baseline at
        // 80% of the ascent, the ideographic-under baseline at the descent.
        let hanging = hang.unwrap_or(ascent * 0.8);
        let ideographic = ideo.unwrap_or(-descent);
        let alphabetic = romn.unwrap_or(0.);
        // Heights above the glyph origin of each alignment baseline.
        let baseline_y = match self.baseline.as_str() {
            "top" => em_over,
            "hanging" => hanging,
            "middle" => (em_over + em_under) * 0.5,
            "ideographic" => ideographic,
            "bottom" => em_under,
            _ => alphabetic,
        };
        let anchor_x = match self.align.as_str() {
            "center" => f64::from(shaped.advance) * 0.5,
            "right" => f64::from(shaped.advance),
            "start" if rtl => f64::from(shaped.advance),
            "end" if !rtl => f64::from(shaped.advance),
            _ => 0.,
        };
        PreparedText {
            shaped,
            anchor_x,
            baseline_y,
            ascent,
            descent,
            em_ascent: em_over,
            em_descent: -em_under,
            hanging,
            alphabetic,
            ideographic,
        }
    }
}

#[derive(Clone, Copy)]
enum Caps {
    /// Lowercase letters become small capitals.
    Lower,
    /// Lowercase and uppercase letters become small capitals.
    All,
    /// Uppercase letters become small capitals.
    Upper,
}

/// CSS Fonts 4 #font-variant-caps-prop simulation: the letters selected by
/// `caps` become their uppercase forms (the text-transform casing, full case
/// mappings, Turkic dotted I) shaped at `size`. Caseless characters keep
/// their glyphs. A grapheme cluster is converted as a unit, so combining
/// marks stay with their base.
fn synthesize_caps(
    text: &str,
    caps: Caps,
    size: f32,
    turkic: bool,
) -> (String, Vec<(Range<usize>, f32)>) {
    let mut out = String::with_capacity(text.len());
    let mut sized: Vec<(Range<usize>, f32)> = Vec::new();
    for cluster in text.graphemes(true) {
        let first = cluster.chars().next().unwrap_or(' ');
        let lower = first.is_lowercase() && first.to_uppercase().next() != Some(first);
        let upper = first.is_uppercase();
        let small = match caps {
            Caps::Lower => lower,
            Caps::All => lower || upper,
            Caps::Upper => upper,
        };
        if !small {
            out.push_str(cluster);
            continue;
        }
        let start = out.len();
        for c in cluster.chars() {
            if turkic && c == 'i' {
                out.push('\u{130}');
            } else {
                out.extend(c.to_uppercase());
            }
        }
        match sized.last_mut() {
            Some((range, _)) if range.end == start => range.end = out.len(),
            _ => sized.push((start..out.len(), size)),
        }
    }
    (out, sized)
}

/// What text preparation reads from a face beyond its shaping: the GSUB
/// features it defines, OS/2 heights and BASE baselines, in ems.
#[derive(Default)]
struct FaceTables {
    features: Vec<[u8; 4]>,
    x_height: Option<f32>,
    cap_height: Option<f32>,
    /// BASE horizontal axis: per script tag, the baseline tag/coordinate
    /// pairs of its BaseValues, in ems above the glyph origin.
    scripts: Vec<([u8; 4], BaseValues)>,
}

/// One script's BASE values: baseline tags and coordinates in ems.
type BaseValues = Vec<([u8; 4], f32)>;

#[derive(Default)]
struct Baselines {
    hanging: Option<f32>,
    ideographic_under: Option<f32>,
    ideographic_over: Option<f32>,
    alphabetic: Option<f32>,
}

impl FaceTables {
    fn read(face: &crate::text::FontFace) -> Self {
        use skrifa::raw::TableProvider as _;
        let data = face.data();
        let Ok(font) = skrifa::FontRef::from_index(data.data.as_ref(), data.index) else {
            return Self::default();
        };
        let upem = font
            .head()
            .map(|head| f32::from(head.units_per_em()))
            .unwrap_or(1000.)
            .max(1.);
        let mut tables = Self::default();
        if let Ok(gsub) = font.gsub()
            && let Ok(list) = gsub.feature_list()
        {
            for record in list.feature_records() {
                let tag = record.feature_tag().to_be_bytes();
                if !tables.features.contains(&tag) {
                    tables.features.push(tag);
                }
            }
        }
        if let Ok(os2) = font.os2() {
            tables.x_height = os2
                .sx_height()
                .filter(|h| *h > 0)
                .map(|h| f32::from(h) / upem);
            tables.cap_height = os2
                .s_cap_height()
                .filter(|h| *h > 0)
                .map(|h| f32::from(h) / upem);
        }
        // OpenType BASE: the horizontal axis's baseline tags index each
        // script's BaseValues coordinates (design units, y up).
        if let Ok(base) = font.base()
            && let Some(Ok(axis)) = base.horiz_axis()
            && let Some(Ok(tag_list)) = axis.base_tag_list()
            && let Ok(scripts) = axis.base_script_list()
        {
            let tags: Vec<[u8; 4]> = tag_list
                .baseline_tags()
                .iter()
                .map(|tag| tag.get().to_be_bytes())
                .collect();
            for record in scripts.base_script_records() {
                let Ok(script) = record.base_script(scripts.offset_data()) else {
                    continue;
                };
                let Some(Ok(values)) = script.base_values() else {
                    continue;
                };
                let coords: Vec<([u8; 4], f32)> = values
                    .base_coords()
                    .iter()
                    .zip(&tags)
                    .filter_map(|(coord, tag)| {
                        Some((*tag, f32::from(coord.ok()?.coordinate()) / upem))
                    })
                    .collect();
                tables
                    .scripts
                    .push((record.base_script_tag().to_be_bytes(), coords));
            }
        }
        tables
    }

    /// CSS Inline 3 #baseline-tables: the baseline set of the first available
    /// font for the text's script (its first script with BASE values), else
    /// the default script.
    fn baselines(&self, text: &str) -> Baselines {
        let script = text.chars().map(script_tags).find(|tags| !tags.is_empty());
        let values = script
            .and_then(|tags| {
                tags.iter()
                    .find_map(|tag| self.scripts.iter().find(|(s, _)| s == *tag))
            })
            .or_else(|| self.scripts.iter().find(|(s, _)| s == b"DFLT"))
            .or_else(|| self.scripts.iter().find(|(s, _)| s == b"latn"))
            .or_else(|| self.scripts.first());
        let Some((_, values)) = values else {
            return Baselines::default();
        };
        let get = |tag: &[u8; 4]| values.iter().find(|(t, _)| t == tag).map(|(_, v)| *v);
        Baselines {
            hanging: get(b"hang"),
            ideographic_under: get(b"ideo"),
            ideographic_over: get(b"idtp"),
            alphabetic: get(b"romn"),
        }
    }
}

/// The OpenType script tags (BASE may use either Indic tag version) of a
/// strong character in common scripts.
fn script_tags(c: char) -> &'static [&'static [u8; 4]] {
    match u32::from(c) {
        0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F | 0x1E00..=0x1EFF => &[b"latn"],
        0x370..=0x3FF | 0x1F00..=0x1FFF => &[b"grek"],
        0x400..=0x52F => &[b"cyrl"],
        0x590..=0x5FF => &[b"hebr"],
        0x600..=0x6FF | 0x750..=0x77F => &[b"arab"],
        0x900..=0x97F => &[b"dev2", b"deva"],
        0x980..=0x9FF => &[b"bng2", b"beng"],
        0xA00..=0xA7F => &[b"gur2", b"guru"],
        0xF00..=0xFFF => &[b"tibt"],
        0x3040..=0x30FF => &[b"kana", b"hani"],
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => &[b"hani"],
        0xAC00..=0xD7AF | 0x1100..=0x11FF => &[b"hang"],
        _ => &[],
    }
}

/// Face tables by font resource; small and bounded.
fn face_tables(face: &crate::text::FontFace) -> Arc<FaceTables> {
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<(u64, u32), Arc<FaceTables>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let key = (face.resource_id(), face.collection_index());
    CACHE.with_borrow_mut(|cache| {
        if let Some(tables) = cache.get(&key) {
            return tables.clone();
        }
        if cache.len() >= 64 {
            cache.clear();
        }
        let tables = Arc::new(FaceTables::read(face));
        cache.insert(key, tables.clone());
        tables
    })
}

pub(crate) struct PreparedText {
    pub shaped: ShapedText,
    pub anchor_x: f64,
    /// Height of the textBaseline alignment baseline above the glyph origin.
    pub baseline_y: f64,
    pub ascent: f64,
    pub descent: f64,
    /// Heights of the em-over baseline above, and the em-under baseline
    /// below, the glyph origin.
    pub em_ascent: f64,
    pub em_descent: f64,
    pub hanging: f64,
    pub alphabetic: f64,
    pub ideographic: f64,
}

impl PreparedText {
    /// HTML #dom-context-2d-measuretext: the TextMetrics members, each
    /// measured from the textBaseline line.
    pub fn metrics(&self) -> [f64; 12] {
        let (left, right, top, bottom) = self.ink_bounds().map_or((0., 0., 0., 0.), |r| {
            (
                self.anchor_x - r.x0,
                r.x1 - self.anchor_x,
                -r.y0 - self.baseline_y,
                r.y1 + self.baseline_y,
            )
        });
        [
            f64::from(self.shaped.advance),
            left,
            right,
            self.ascent - self.baseline_y,
            self.descent + self.baseline_y,
            top,
            bottom,
            self.em_ascent - self.baseline_y,
            self.em_descent + self.baseline_y,
            self.hanging - self.baseline_y,
            self.alphabetic - self.baseline_y,
            self.ideographic - self.baseline_y,
        ]
    }

    /// The union of the glyphs' ink in the coordinates the painters place
    /// glyphs in, before the text transform (TextMetrics' actual bounds).
    pub fn ink_bounds(&self) -> Option<Rect> {
        let mut bounds = GlyphBounds::default();
        let mut cache = glifo::GlyphPrepCache::default();
        for run in &self.shaped.runs {
            let backend = GlyphBackend {
                renderer: &mut bounds,
                cache: &mut cache,
            };
            let mut builder = glifo::GlyphRunBuilder::new(
                run.font.data().clone(),
                Affine::IDENTITY,
                Affine::IDENTITY,
                backend,
            )
            .font_size(run.font_size)
            .normalized_coords(&run.normalized_coords)
            .hint(false);
            if run.synth_bold {
                let amount = f64::from(run.font_size) * 0.025;
                builder = builder.font_embolden(glifo::FontEmbolden::new(
                    vello_cpu::kurbo::Diagonal2::new(amount, amount),
                ));
            }
            if let Some(skew) = run.synthetic_oblique_skew() {
                builder = builder.glyph_transform(Affine::skew(skew, 0.));
            }
            // Unrenderable glyphs are skipped; the rest of the run paints.
            let _ = builder.fill_glyphs(run.glyphs.iter().map(|glyph| glifo::Glyph {
                id: glyph.id,
                x: glyph.x,
                y: glyph.y - self.shaped.baseline,
            }));
        }
        bounds.bounds
    }
}

/// Measure the SAME glyph geometry consumed by the painters, including variable,
/// synthetic, bitmap and COLR glyphs. Atlas caching is disabled so cache padding
/// never becomes a TextMetrics ink bound. No raster bitmap is allocated here.
#[derive(Default)]
struct GlyphBounds {
    transform: Affine,
    bounds: Option<Rect>,
    clip: Option<Rect>,
    clips: Vec<Option<Rect>>,
}

impl GlyphBounds {
    fn add(&mut self, rect: Rect) {
        let rect = self.clip.map_or(rect, |clip| rect.intersect(clip));
        if rect.width() > 0. && rect.height() > 0. {
            self.bounds = Some(self.bounds.map_or(rect, |old| old.union(rect)));
        }
    }
}

impl glifo::DrawSink for GlyphBounds {
    fn set_transform(&mut self, transform: Affine) {
        self.transform = transform;
    }
    fn set_paint(&mut self, _paint: glifo::AtlasPaint) {}
    fn set_paint_transform(&mut self, _transform: Affine) {}
    fn fill_path(&mut self, path: &BezPath) {
        self.add((self.transform * path).bounding_box());
    }
    fn fill_rect(&mut self, rect: &Rect) {
        self.add(self.transform.transform_rect_bbox(*rect));
    }
    fn push_clip_layer(&mut self, path: &BezPath) {
        self.clips.push(self.clip);
        let clip = (self.transform * path).bounding_box();
        self.clip = Some(self.clip.map_or(clip, |old| old.intersect(clip)));
    }
    fn push_blend_layer(&mut self, _blend: BlendMode) {
        self.clips.push(self.clip);
    }
    fn pop_layer(&mut self) {
        self.clip = self.clips.pop().flatten();
    }
    fn width(&self) -> u16 {
        u16::MAX
    }
    fn height(&self) -> u16 {
        u16::MAX
    }
}

impl glifo::GlyphRenderer for GlyphBounds {
    type SavedState = Affine;
    fn save_state(&mut self) -> Affine {
        self.transform
    }
    fn restore_state(&mut self, state: Affine) {
        self.transform = state;
    }
    fn stroke_path(&mut self, path: &BezPath) {
        glifo::DrawSink::fill_path(self, path);
    }
    fn set_paint_image(&mut self, _image: Image) {}
    fn set_tint(&mut self, _tint: Option<Tint>) {}
    fn get_context_color(&self) -> AlphaColor<Srgb> {
        BLACK
    }
    fn current_paint(&self) -> &PaintType {
        static PAINT: PaintType = PaintType::Solid(BLACK);
        &PAINT
    }
    fn atlas_image_source(&self, _slot: &glifo::AtlasSlot) -> ImageSource {
        unreachable!("measurement disables atlas sampling")
    }
    fn atlas_paint_transform(&self, _slot: &glifo::AtlasSlot) -> Affine {
        unreachable!("measurement disables atlas sampling")
    }
}

struct GlyphBackend<'a, R> {
    renderer: &'a mut R,
    cache: &'a mut glifo::GlyphPrepCache,
}
impl<'a, R: glifo::GlyphRenderer> glifo::GlyphRunBackend<'a> for GlyphBackend<'a, R> {
    fn atlas_cache(self, _enabled: bool) -> Self {
        self
    }
    fn fill_glyphs<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        run: glifo::GlyphRun<'a>,
        glyphs: G,
    ) -> Result<(), glifo::GlyphRenderError> {
        run.build(glyphs, self.cache.as_mut(), glifo::AtlasCacher::Disabled)
            .fill_glyphs(self.renderer)
    }
    fn stroke_glyphs<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        run: glifo::GlyphRun<'a>,
        glyphs: G,
    ) -> Result<(), glifo::GlyphRenderError> {
        run.build(glyphs, self.cache.as_mut(), glifo::AtlasCacher::Disabled)
            .stroke_glyphs(self.renderer)
    }
    fn render_decoration<G: Iterator<Item = glifo::Glyph> + Clone>(
        self,
        _run: glifo::GlyphRun<'a>,
        _glyphs: G,
        _range: std::ops::RangeInclusive<f32>,
        _baseline: f32,
        _offset: f32,
        _size: f32,
        _buffer: f32,
    ) {
        unreachable!("canvas text does not request decorations")
    }
}

/// Glyph outlines are scaled to CSS pixels BEFORE applying Canvas line styles.
/// Otherwise font-unit scaling and maxWidth also scale the line width/dashes.
pub(crate) fn stroke_run(
    context: &mut vello_cpu::RenderContext,
    cache: &mut glifo::GlyphPrepCache,
    run: &crate::text::ShapedRun,
    baseline: f32,
    canvas_transform: Affine,
) {
    let transform = *context.transform();
    let paint_transform = *context.paint_transform();
    let mut renderer = StrokeRenderer {
        context,
        canvas_transform,
    };
    let backend = GlyphBackend {
        renderer: &mut renderer,
        cache,
    };
    let mut builder =
        glifo::GlyphRunBuilder::new(run.font.data().clone(), transform, paint_transform, backend)
            .font_size(run.font_size)
            .normalized_coords(&run.normalized_coords)
            .hint(false);
    if run.synth_bold {
        let amount = f64::from(run.font_size) * 0.025;
        builder = builder.font_embolden(glifo::FontEmbolden::new(
            vello_cpu::kurbo::Diagonal2::new(amount, amount),
        ));
    }
    if let Some(skew) = run.synthetic_oblique_skew() {
        builder = builder.glyph_transform(Affine::skew(skew, 0.));
    }
    // Unrenderable glyphs are skipped; the rest of the run paints.
    let _ = builder.stroke_glyphs(run.glyphs.iter().map(|glyph| glifo::Glyph {
        id: glyph.id,
        x: glyph.x,
        y: glyph.y - baseline,
    }));
}

struct StrokeRenderer<'a> {
    context: &'a mut vello_cpu::RenderContext,
    canvas_transform: Affine,
}

impl glifo::DrawSink for StrokeRenderer<'_> {
    fn set_transform(&mut self, transform: Affine) {
        self.context.set_transform(transform);
    }
    fn set_paint(&mut self, paint: glifo::AtlasPaint) {
        self.context.set_paint(paint);
    }
    fn set_paint_transform(&mut self, transform: Affine) {
        self.context.set_paint_transform(transform);
    }
    fn fill_path(&mut self, path: &BezPath) {
        self.context.fill_path(path);
    }
    fn fill_rect(&mut self, rect: &Rect) {
        self.context.fill_rect(rect);
    }
    fn push_clip_layer(&mut self, path: &BezPath) {
        self.context.push_clip_layer(path);
    }
    fn push_clip_path(&mut self, path: &BezPath) {
        self.context.push_clip_path(path);
    }
    fn push_blend_layer(&mut self, blend: BlendMode) {
        self.context.push_blend_layer(blend);
    }
    fn pop_layer(&mut self) {
        self.context.pop_layer();
    }
    fn pop_clip_path(&mut self) {
        self.context.pop_clip();
    }
    fn width(&self) -> u16 {
        self.context.width()
    }
    fn height(&self) -> u16 {
        self.context.height()
    }
}

impl glifo::GlyphRenderer for StrokeRenderer<'_> {
    type SavedState = vello_common::render_state::RenderState;
    fn save_state(&mut self) -> Self::SavedState {
        self.context.save_current_state()
    }
    fn restore_state(&mut self, state: Self::SavedState) {
        self.context.restore_state(state);
    }
    fn stroke_path(&mut self, path: &BezPath) {
        let local = self.canvas_transform.inverse() * *self.context.transform();
        let paint = local * *self.context.paint_transform();
        self.context.set_transform(self.canvas_transform);
        self.context.set_paint_transform(paint);
        self.context.stroke_path(&(local * path));
    }
    fn set_paint_image(&mut self, image: Image) {
        self.context.set_paint(image);
    }
    fn set_tint(&mut self, tint: Option<Tint>) {
        self.context.set_tint(tint);
    }
    fn get_context_color(&self) -> AlphaColor<Srgb> {
        glifo::GlyphRenderer::get_context_color(self.context)
    }
    fn current_paint(&self) -> &PaintType {
        self.context.paint()
    }
    fn atlas_image_source(&self, _slot: &glifo::AtlasSlot) -> ImageSource {
        unreachable!("canvas stroke disables atlas sampling")
    }
    fn atlas_paint_transform(&self, _slot: &glifo::AtlasSlot) -> Affine {
        unreachable!("canvas stroke disables atlas sampling")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_lengths_parse_serialize_and_resolve_when_prepared() {
        // HTML #dom-context-2d-letterspacing: a CSS <length>, serialized.
        for (text, serialized) in [
            ("3px", "3px"),
            ("-2PX", "-2px"),
            ("0", "0px"),
            ("-0px", "0px"),
            ("1.5em", "1.5em"),
            ("2rem", "2rem"),
            ("1in", "1in"),
            (" 4px ", "4px"),
        ] {
            assert_eq!(
                Spacing::parse(text).map(|s| s.serialize()).as_deref(),
                Some(serialized),
                "{text}"
            );
        }
        for invalid in [
            "",
            "1",
            "10%",
            "inherit",
            "1px 2px",
            "calc(1px)",
            "1zz",
            "nan px",
        ] {
            assert!(Spacing::parse(invalid).is_none(), "{invalid}");
        }
        let units = FontUnits {
            size: 20.,
            root: 16.,
            x_height: 9.,
            cap_height: 14.,
            zero: 11.,
            ideograph: 20.,
            line_height: 23.,
            viewport: (800., 600.),
        };
        let px = |text: &str| Spacing::parse(text).unwrap().resolve(&units);
        assert_eq!(px("1em"), 20.);
        assert_eq!(px("2rem"), 32.);
        assert_eq!(px("1ex"), 9.);
        assert_eq!(px("1ch"), 11.);
        assert_eq!(px("1cap"), 14.);
        assert_eq!(px("1lh"), 23.);
        assert_eq!(px("10vw"), 80.);
        assert_eq!(px("10vmin"), 60.);
        assert_eq!(px("3pt"), 4.);
    }

    #[test]
    fn small_capital_synthesis_follows_text_transform_casing() {
        // CSS Fonts 4 #font-variant-caps-prop: lowercase letters become
        // uppercase glyphs at a smaller size; capitals and caseless
        // characters keep theirs; casing matches 'text-transform'.
        let (text, sized) = synthesize_caps("aB1ß", Caps::Lower, 7., false);
        assert_eq!(text, "AB1SS");
        assert_eq!(sized, vec![(0..1, 7.), (3..5, 7.)]);
        let (text, sized) = synthesize_caps("aB", Caps::All, 7., false);
        assert_eq!((text.as_str(), sized), ("AB", vec![(0..2, 7.)]));
        let (text, sized) = synthesize_caps("aB", Caps::Upper, 7., false);
        assert_eq!((text.as_str(), sized), ("aB", vec![(1..2, 7.)]));
        // Turkic dotted capital I, and a combining mark stays with its base.
        let (text, _) = synthesize_caps("i", Caps::Lower, 7., true);
        assert_eq!(text, "\u{130}");
        let (text, sized) = synthesize_caps("e\u{301}x", Caps::Lower, 7., false);
        assert_eq!(text, "E\u{301}X");
        assert_eq!(sized, vec![(0..text.len(), 7.)]);
    }

    #[test]
    fn the_font_shorthand_sets_and_serializes_its_longhands() {
        let mut state = TextState::default();
        assert_eq!(state.font(), "10px sans-serif");
        state.set_font(
            "italic small-caps bold condensed 12px serif",
            crate::layout2::Units::default(),
            400.,
        );
        assert_eq!(state.font(), "italic small-caps bold condensed 12px serif");
        assert_eq!(
            (state.variant_caps, state.stretch, state.kerning),
            ("small-caps", "condensed", "auto")
        );
        state.set_attribute(FONT_STRETCH, "ultra-expanded");
        state.set_attribute(FONT_VARIANT_CAPS, "all-small-caps");
        state.set_attribute(FONT_KERNING, "none");
        assert_eq!(state.font(), "italic bold ultra-expanded 12px serif");
        state.set_font("12px serif", crate::layout2::Units::default(), 400.);
        assert_eq!(
            (state.variant_caps, state.stretch, state.kerning),
            ("normal", "normal", "auto")
        );
        // Enumerations compare exactly; other values are ignored.
        for (index, value) in [
            (FONT_KERNING, "NONE"),
            (FONT_STRETCH, "50%"),
            (FONT_VARIANT_CAPS, "Small-Caps"),
            (TEXT_RENDERING, "optimizespeed"),
            (DIRECTION, "RTL"),
            (TEXT_ALIGN, "middle"),
        ] {
            let before = state.attribute(index);
            state.set_attribute(index, value);
            assert_eq!(state.attribute(index), before, "{value}");
        }
        state.set_attribute(LANG, "");
        assert_eq!(state.attribute(LANG), "");
    }

    #[test]
    fn base_table_baselines_follow_the_text_script() {
        // CSS Inline 3 #baseline-tables: the first available font's baseline
        // set for the script, else its default script.
        let tables = FaceTables {
            scripts: vec![
                (*b"DFLT", vec![(*b"romn", 0.), (*b"ideo", -0.12)]),
                (*b"deva", vec![(*b"hang", 0.6), (*b"romn", 0.)]),
                (*b"grek", vec![(*b"romn", 0.01)]),
                (*b"hani", vec![(*b"ideo", -0.12), (*b"idtp", 0.88)]),
            ],
            ..FaceTables::default()
        };
        let latin = tables.baselines("Hello");
        assert_eq!(
            (latin.alphabetic, latin.ideographic_under, latin.hanging),
            (Some(0.), Some(-0.12), None)
        );
        let han = tables.baselines("  \u{6C34}");
        assert_eq!(
            (han.ideographic_over, han.ideographic_under),
            (Some(0.88), Some(-0.12))
        );
        assert!(FaceTables::default().baselines("x").hanging.is_none());
        assert_eq!(tables.baselines("\u{915}").hanging, Some(0.6));
        assert_eq!(tables.baselines("1 \u{3B1}").alphabetic, Some(0.01));
        let devanagari = FaceTables {
            scripts: vec![(*b"dev2", vec![(*b"hang", 0.62)])],
            ..FaceTables::default()
        };
        assert_eq!(devanagari.baselines("\u{915}").hanging, Some(0.62));
    }

    #[test]
    fn prepared_text_metrics_hold_the_baseline_relations() {
        let mut state = TextState::default();
        state.set_font("20px sans-serif", crate::layout2::Units::default(), 400.);
        let environment = TextEnvironment::default();
        let plain = state.prepare("Hg", &environment);
        // The directional mark contributes no advance.
        let direct = crate::text::shape_canvas("Hg", &state.style);
        assert!((plain.shaped.advance - direct.advance).abs() < 0.01);
        let m = plain.metrics();
        assert_eq!(m[10], 0., "alphabetic baseline");
        assert!((m[7] + m[8] - 20.).abs() < 1e-9, "em box");
        state.set_attribute(LETTER_SPACING, "5px");
        let spaced = state.prepare("Hg", &environment);
        assert!((spaced.shaped.advance - plain.shaped.advance - 10.).abs() < 0.01);
        state.set_attribute(TEXT_BASELINE, "top");
        let top = state.prepare("Hg", &environment).metrics();
        assert!(top[7].abs() < 1e-9 && (top[10] + plain.em_ascent).abs() < 1e-9);
    }
}
