//! CSS Fonts 4 typographic settings of one element, resolved into the
//! OpenType features and variations the shaper applies.
//!
//! The cascade (`dom`) owns the grammar and computed values of
//! `font-width`, the oblique angle of `font-style`, `font-kerning`,
//! `font-variant-*`, `font-feature-settings`, `font-variation-settings`,
//! `font-optical-sizing`, `font-synthesis-*` and `font-size-adjust`.
//! Layout reads their computed values once per element into a
//! [`FontTypography`]; text shaping turns it into feature and variation
//! lists in the order of CSS Fonts 4 #feature-variation-precedence
//! (csswg-drafts 81c27f686901, CSS Fonts 5 for `font-size-adjust`), and
//! synthesizes small capitals and superscripts/subscripts where the font
//! lacks them (#font-variant-caps-prop, #font-variant-position-prop,
//! #font-synthesis).

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;

/// An OpenType tag.
pub type Tag = [u8; 4];

/// The CSS Fonts 4 properties a [`super::TextStyle`] carries beyond family,
/// size, weight and the italic flag. `Default` is every property's initial
/// value; a style at its initial values carries no `FontTypography` at all.
#[derive(Clone, Debug, PartialEq)]
pub struct FontTypography {
    /// #font-width-prop as a ratio of the normal width.
    pub width: f32,
    /// #font-style-prop `oblique <angle>`, in degrees (positive clockwise).
    /// `None` with `TextStyle::italic` is `italic`.
    pub oblique: Option<f32>,
    pub kerning: Kerning,
    /// #font-optical-sizing-def: `auto`.
    pub optical_sizing: bool,
    pub ligatures: Ligatures,
    pub caps: Caps,
    pub numeric: Numeric,
    pub east_asian: EastAsian,
    pub position: Position,
    /// #font-variant-alternates-prop: the features its values select.
    pub alternates: Vec<(Tag, u16)>,
    pub emoji: Emoji,
    /// #font-feature-settings-prop: deduplicated, sorted by tag.
    pub feature_settings: Vec<(Tag, u32)>,
    /// #font-variation-settings-def: deduplicated, sorted by tag.
    pub variation_settings: Vec<(Tag, f32)>,
    pub synthesis: Synthesis,
    /// CSS Fonts 5 #font-size-adjust-prop: the metric to normalize and its
    /// aspect value (`from-font` already computed to a number).
    pub size_adjust: Option<(SizeAdjustMetric, f32)>,
}

/// Shaping caches key on typography; no value is NaN (the cascade rejects
/// non-finite numbers), so float equality is an equivalence.
impl Eq for FontTypography {}

impl std::hash::Hash for FontTypography {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.width.to_bits().hash(state);
        self.oblique.map(f32::to_bits).hash(state);
        self.kerning.hash(state);
        self.optical_sizing.hash(state);
        self.ligatures.hash(state);
        self.caps.hash(state);
        self.numeric.hash(state);
        self.east_asian.hash(state);
        self.position.hash(state);
        self.alternates.hash(state);
        self.emoji.hash(state);
        self.feature_settings.hash(state);
        for (tag, value) in &self.variation_settings {
            (tag, value.to_bits()).hash(state);
        }
        self.synthesis.hash(state);
        self.size_adjust
            .map(|(metric, value)| (metric, value.to_bits()))
            .hash(state);
    }
}

impl Default for FontTypography {
    fn default() -> Self {
        Self {
            width: 1.0,
            oblique: None,
            kerning: Kerning::Auto,
            optical_sizing: true,
            ligatures: Ligatures::default(),
            caps: Caps::Normal,
            numeric: Numeric::default(),
            east_asian: EastAsian::default(),
            position: Position::Normal,
            alternates: Vec::new(),
            emoji: Emoji::Normal,
            feature_settings: Vec::new(),
            variation_settings: Vec::new(),
            synthesis: Synthesis::default(),
            size_adjust: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kerning {
    #[default]
    Auto,
    Normal,
    None,
}

/// #font-variant-ligatures-prop. `None` fields keep the font's default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ligatures {
    /// The `none` keyword.
    pub none: bool,
    pub common: Option<bool>,
    pub discretionary: Option<bool>,
    pub historical: Option<bool>,
    pub contextual: Option<bool>,
}

/// #font-variant-caps-prop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Caps {
    #[default]
    Normal,
    SmallCaps,
    AllSmallCaps,
    PetiteCaps,
    AllPetiteCaps,
    Unicase,
    TitlingCaps,
}

/// #font-variant-numeric-prop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Numeric {
    /// `lining-nums` (`true`) or `oldstyle-nums`.
    pub lining: Option<bool>,
    /// `tabular-nums` (`true`) or `proportional-nums`.
    pub tabular: Option<bool>,
    /// `stacked-fractions` (`true`) or `diagonal-fractions`.
    pub stacked: Option<bool>,
    pub ordinal: bool,
    pub slashed_zero: bool,
}

/// #font-variant-east-asian-prop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EastAsian {
    /// The `<east-asian-variant-values>` feature (`jp78` … `trad`).
    pub variant: Option<Tag>,
    /// `full-width` (`true`) or `proportional-width`.
    pub full_width: Option<bool>,
    pub ruby: bool,
}

/// #font-variant-position-prop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Position {
    #[default]
    Normal,
    Sub,
    Super,
}

/// #font-variant-emoji-prop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Emoji {
    #[default]
    Normal,
    Text,
    Emoji,
    Unicode,
}

/// #font-synthesis: what the user agent may synthesize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Synthesis {
    pub weight: bool,
    pub style: SynthesisStyle,
    pub small_caps: bool,
    pub position: bool,
}

impl Default for Synthesis {
    fn default() -> Self {
        Self {
            weight: true,
            style: SynthesisStyle::Auto,
            small_caps: true,
            position: true,
        }
    }
}

/// #font-synthesis-style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SynthesisStyle {
    #[default]
    Auto,
    None,
    /// Oblique synthesis is allowed, but not as a fallback for `italic`.
    ObliqueOnly,
}

/// CSS Fonts 5 #font-size-adjust-prop metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SizeAdjustMetric {
    #[default]
    ExHeight,
    CapHeight,
    ChWidth,
    IcWidth,
    IcHeight,
}

impl SizeAdjustMetric {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::ExHeight => "ex-height",
            Self::CapHeight => "cap-height",
            Self::ChWidth => "ch-width",
            Self::IcWidth => "ic-width",
            Self::IcHeight => "ic-height",
        }
    }

    pub fn parse(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "ex-height" => Self::ExHeight,
            "cap-height" => Self::CapHeight,
            "ch-width" => Self::ChWidth,
            "ic-width" => Self::IcWidth,
            "ic-height" => Self::IcHeight,
            _ => return None,
        })
    }
}

/// #font-width-prop keyword percentages.
pub const WIDTH_KEYWORDS: [(&str, f32); 9] = [
    ("ultra-condensed", 50.),
    ("extra-condensed", 62.5),
    ("condensed", 75.),
    ("semi-condensed", 87.5),
    ("normal", 100.),
    ("semi-expanded", 112.5),
    ("expanded", 125.),
    ("extra-expanded", 150.),
    ("ultra-expanded", 200.),
];

impl FontTypography {
    /// The typography of an element from the computed values `computed`
    /// reads (`None` for a property at its initial value), or `None` when
    /// every property has its initial value. Values are the canonical
    /// serializations the cascade computes.
    pub fn from_computed(computed: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let mut t = Self::default();
        let mut any = false;
        let read = |name: &str| {
            let value = computed(name)?;
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        if let Some(v) = read("font-width")
            && let Some(ratio) = parse_width(&v)
            && ratio != 1.0
        {
            t.width = ratio;
            any = true;
        }
        if let Some(v) = read("font-style")
            && let Some(rest) = v.strip_prefix("oblique")
        {
            t.oblique = Some(parse_angle_deg(rest.trim()).unwrap_or(14.0));
            any = true;
        }
        if let Some(v) = read("font-kerning") {
            t.kerning = match v.as_str() {
                "normal" => Kerning::Normal,
                "none" => Kerning::None,
                _ => Kerning::Auto,
            };
            any |= t.kerning != Kerning::Auto;
        }
        if read("font-optical-sizing").as_deref() == Some("none") {
            t.optical_sizing = false;
            any = true;
        }
        if let Some(v) = read("font-variant-ligatures") {
            t.ligatures = parse_ligatures(&v).unwrap_or_default();
            any |= t.ligatures != Ligatures::default();
        }
        if let Some(v) = read("font-variant-caps") {
            t.caps = parse_caps(&v).unwrap_or_default();
            any |= t.caps != Caps::Normal;
        }
        if let Some(v) = read("font-variant-numeric") {
            t.numeric = parse_numeric(&v).unwrap_or_default();
            any |= t.numeric != Numeric::default();
        }
        if let Some(v) = read("font-variant-east-asian") {
            t.east_asian = parse_east_asian(&v).unwrap_or_default();
            any |= t.east_asian != EastAsian::default();
        }
        if let Some(v) = read("font-variant-position") {
            t.position = match v.as_str() {
                "sub" => Position::Sub,
                "super" => Position::Super,
                _ => Position::Normal,
            };
            any |= t.position != Position::Normal;
        }
        if let Some(v) = read("font-variant-alternates")
            && v.split_ascii_whitespace()
                .any(|word| word == "historical-forms")
        {
            // The functional values name `@font-feature-values` entries,
            // which the caller resolves (`alternates`).
            t.alternates.push((*b"hist", 1));
            any = true;
        }
        if let Some(v) = read("font-variant-emoji") {
            t.emoji = match v.as_str() {
                "text" => Emoji::Text,
                "emoji" => Emoji::Emoji,
                "unicode" => Emoji::Unicode,
                _ => Emoji::Normal,
            };
            any |= t.emoji != Emoji::Normal;
        }
        if let Some(v) = read("font-feature-settings") {
            t.feature_settings = parse_feature_settings(&v).unwrap_or_default();
            any |= !t.feature_settings.is_empty();
        }
        if let Some(v) = read("font-variation-settings") {
            t.variation_settings = parse_variation_settings(&v).unwrap_or_default();
            any |= !t.variation_settings.is_empty();
        }
        if read("font-synthesis-weight").as_deref() == Some("none") {
            t.synthesis.weight = false;
            any = true;
        }
        match read("font-synthesis-style").as_deref() {
            Some("none") => t.synthesis.style = SynthesisStyle::None,
            Some("oblique-only") => t.synthesis.style = SynthesisStyle::ObliqueOnly,
            _ => {}
        }
        any |= t.synthesis.style != SynthesisStyle::Auto;
        if read("font-synthesis-small-caps").as_deref() == Some("none") {
            t.synthesis.small_caps = false;
            any = true;
        }
        if read("font-synthesis-position").as_deref() == Some("none") {
            t.synthesis.position = false;
            any = true;
        }
        if let Some(v) = read("font-size-adjust") {
            t.size_adjust = parse_size_adjust(&v);
            any |= t.size_adjust.is_some();
        }
        any.then_some(t)
    }

    /// CSS Fonts 4 #feature-variation-precedence steps 10, 11 and 13: the
    /// features of `font-variant` and its longhands and of `font-kerning`;
    /// then those other properties determine (`letter-spacing` disables the
    /// optional ligatures, CSS Text 3 #letter-spacing-property); then
    /// `font-feature-settings`. A later setting of the same tag wins.
    /// `caps` are the capitalization features the font supports (see
    /// [`caps_plan`]); `position` whether the position feature applies.
    pub fn features(
        &self,
        letter_spacing: f32,
        caps: &[(Tag, u16)],
        position: bool,
    ) -> Vec<(Tag, u16)> {
        let mut out: Vec<(Tag, u16)> = Vec::new();
        let l = self.ligatures;
        if l.none {
            out.extend([*b"liga", *b"clig", *b"dlig", *b"hlig", *b"calt"].map(|tag| (tag, 0)));
        } else {
            if let Some(on) = l.common {
                out.extend([(*b"liga", u16::from(on)), (*b"clig", u16::from(on))]);
            }
            if let Some(on) = l.discretionary {
                out.push((*b"dlig", u16::from(on)));
            }
            if let Some(on) = l.historical {
                out.push((*b"hlig", u16::from(on)));
            }
            if let Some(on) = l.contextual {
                out.push((*b"calt", u16::from(on)));
            }
        }
        out.extend_from_slice(caps);
        let n = self.numeric;
        if let Some(lining) = n.lining {
            out.push((if lining { *b"lnum" } else { *b"onum" }, 1));
        }
        if let Some(tabular) = n.tabular {
            out.push((if tabular { *b"tnum" } else { *b"pnum" }, 1));
        }
        if let Some(stacked) = n.stacked {
            out.push((if stacked { *b"afrc" } else { *b"frac" }, 1));
        }
        if n.ordinal {
            out.push((*b"ordn", 1));
        }
        if n.slashed_zero {
            out.push((*b"zero", 1));
        }
        out.extend_from_slice(&self.alternates);
        let e = self.east_asian;
        if let Some(tag) = e.variant {
            out.push((tag, 1));
        }
        if let Some(full) = e.full_width {
            out.push((if full { *b"fwid" } else { *b"pwid" }, 1));
        }
        if e.ruby {
            out.push((*b"ruby", 1));
        }
        if position {
            match self.position {
                Position::Sub => out.push((*b"subs", 1)),
                Position::Super => out.push((*b"sups", 1)),
                Position::Normal => {}
            }
        }
        // #font-kerning-prop: `auto` leaves the font's default kerning.
        match self.kerning {
            Kerning::Normal => out.push((*b"kern", 1)),
            Kerning::None => out.push((*b"kern", 0)),
            Kerning::Auto => {}
        }
        if letter_spacing != 0.0 {
            out.extend([*b"liga", *b"clig", *b"dlig", *b"hlig"].map(|tag| (tag, 0)));
        }
        out.extend(
            self.feature_settings
                .iter()
                .map(|&(tag, value)| (tag, u16::try_from(value).unwrap_or(u16::MAX))),
        );
        out
    }

    /// CSS Fonts 4 #feature-variation-precedence steps 9 and 12: the
    /// optical size for the used font size `size` (#font-optical-sizing-def,
    /// CSS pixels, which the shaper clamps to the font's `opsz` range), then
    /// `font-variation-settings`.
    pub fn variations(&self, size: f32) -> Vec<(Tag, f32)> {
        let mut out = Vec::with_capacity(self.variation_settings.len() + 1);
        if self.optical_sizing {
            out.push((*b"opsz", size));
        }
        out.extend_from_slice(&self.variation_settings);
        out
    }

    /// Whether shaping needs the font's capitalization features or a
    /// position test before it can build the feature list.
    pub(crate) fn needs_face_features(&self) -> bool {
        self.caps != Caps::Normal || self.position != Position::Normal
    }
}

/// `font-width` computed value (a percentage) as a ratio.
pub fn parse_width(value: &str) -> Option<f32> {
    if let Some((_, percent)) = WIDTH_KEYWORDS.iter().find(|(k, _)| *k == value) {
        return Some(percent / 100.0);
    }
    let percent: f32 = value.strip_suffix('%')?.trim().parse().ok()?;
    (percent.is_finite() && percent >= 0.0).then_some(percent / 100.0)
}

/// An `<angle>` in degrees.
pub fn parse_angle_deg(value: &str) -> Option<f32> {
    let value = value.trim();
    let split = value
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f32 = number.trim().parse().ok()?;
    let degrees = match unit.to_ascii_lowercase().as_str() {
        "deg" => number,
        "grad" => number * 0.9,
        "rad" => number.to_degrees(),
        "turn" => number * 360.0,
        "" if number == 0.0 => 0.0,
        _ => return None,
    };
    degrees.is_finite().then_some(degrees)
}

pub fn parse_ligatures(value: &str) -> Option<Ligatures> {
    let mut l = Ligatures::default();
    match value {
        "normal" => return Some(l),
        "none" => {
            l.none = true;
            return Some(l);
        }
        _ => {}
    }
    for word in value.split_ascii_whitespace() {
        let (slot, on) = match word {
            "common-ligatures" => (&mut l.common, true),
            "no-common-ligatures" => (&mut l.common, false),
            "discretionary-ligatures" => (&mut l.discretionary, true),
            "no-discretionary-ligatures" => (&mut l.discretionary, false),
            "historical-ligatures" => (&mut l.historical, true),
            "no-historical-ligatures" => (&mut l.historical, false),
            "contextual" => (&mut l.contextual, true),
            "no-contextual" => (&mut l.contextual, false),
            _ => return None,
        };
        if slot.replace(on).is_some() {
            return None;
        }
    }
    Some(l)
}

pub fn parse_caps(value: &str) -> Option<Caps> {
    Some(match value {
        "normal" => Caps::Normal,
        "small-caps" => Caps::SmallCaps,
        "all-small-caps" => Caps::AllSmallCaps,
        "petite-caps" => Caps::PetiteCaps,
        "all-petite-caps" => Caps::AllPetiteCaps,
        "unicase" => Caps::Unicase,
        "titling-caps" => Caps::TitlingCaps,
        _ => return None,
    })
}

pub fn parse_numeric(value: &str) -> Option<Numeric> {
    let mut n = Numeric::default();
    if value == "normal" {
        return Some(n);
    }
    for word in value.split_ascii_whitespace() {
        let fresh = match word {
            "lining-nums" => n.lining.replace(true).is_none(),
            "oldstyle-nums" => n.lining.replace(false).is_none(),
            "tabular-nums" => n.tabular.replace(true).is_none(),
            "proportional-nums" => n.tabular.replace(false).is_none(),
            "stacked-fractions" => n.stacked.replace(true).is_none(),
            "diagonal-fractions" => n.stacked.replace(false).is_none(),
            "ordinal" => !std::mem::replace(&mut n.ordinal, true),
            "slashed-zero" => !std::mem::replace(&mut n.slashed_zero, true),
            _ => false,
        };
        if !fresh {
            return None;
        }
    }
    Some(n)
}

/// The `<east-asian-variant-values>` keywords and their features.
pub const EAST_ASIAN_VARIANTS: [(&str, Tag); 6] = [
    ("jis78", *b"jp78"),
    ("jis83", *b"jp83"),
    ("jis90", *b"jp90"),
    ("jis04", *b"jp04"),
    ("simplified", *b"smpl"),
    ("traditional", *b"trad"),
];

pub fn parse_east_asian(value: &str) -> Option<EastAsian> {
    let mut e = EastAsian::default();
    if value == "normal" {
        return Some(e);
    }
    for word in value.split_ascii_whitespace() {
        let fresh = if let Some((_, tag)) = EAST_ASIAN_VARIANTS.iter().find(|(k, _)| *k == word) {
            e.variant.replace(*tag).is_none()
        } else {
            match word {
                "full-width" => e.full_width.replace(true).is_none(),
                "proportional-width" => e.full_width.replace(false).is_none(),
                "ruby" => !std::mem::replace(&mut e.ruby, true),
                _ => false,
            }
        };
        if !fresh {
            return None;
        }
    }
    Some(e)
}

/// One `<opentype-tag>`: a string of exactly four characters in
/// U+20–U+7E (#font-feature-settings-prop).
pub fn opentype_tag(text: &str) -> Option<Tag> {
    let bytes = text.as_bytes();
    (bytes.len() == 4 && bytes.iter().all(|b| (0x20..=0x7e).contains(b)))
        .then(|| [bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// `font-feature-settings`: `normal | <feature-tag-value>#`, as a map
/// sorted by tag ("the computed value of font-feature-settings is a map").
pub fn parse_feature_settings(value: &str) -> Option<Vec<(Tag, u32)>> {
    use cssparser::{ParseError, Parser, Token};
    let mut parser = Parser::new(value);
    if parser
        .try_parse(|p| p.expect_ident_matching("normal"))
        .is_ok()
    {
        return parser.is_exhausted().then(Vec::new);
    }
    let items = parser
        .parse_comma_separated(|p| {
            let tag = opentype_tag(&p.expect_string()?.clone())
                .ok_or_else(ParseError::<()>::unexpected_token)?;
            let value = match p.next() {
                Err(_) => 1,
                Ok(Token::Ident(word)) if word.eq_ignore_ascii_case("on") => 1,
                Ok(Token::Ident(word)) if word.eq_ignore_ascii_case("off") => 0,
                Ok(Token::Number {
                    int_value: Some(n), ..
                }) if *n >= 0 => *n as u32,
                Ok(_) => return Err(ParseError::unexpected_token()),
            };
            Ok::<_, ParseError<()>>((tag, value))
        })
        .ok()?;
    parser.is_exhausted().then(|| sorted_map(items))
}

/// `font-variation-settings`: `normal | [ <opentype-tag> <number> ]#`.
pub fn parse_variation_settings(value: &str) -> Option<Vec<(Tag, f32)>> {
    use cssparser::{ParseError, Parser};
    let mut parser = Parser::new(value);
    if parser
        .try_parse(|p| p.expect_ident_matching("normal"))
        .is_ok()
    {
        return parser.is_exhausted().then(Vec::new);
    }
    let items = parser
        .parse_comma_separated(|p| {
            let tag = opentype_tag(&p.expect_string()?.clone())
                .ok_or_else(ParseError::<()>::unexpected_token)?;
            let value = p.expect_number()?;
            Ok::<_, ParseError<()>>((tag, value))
        })
        .ok()?;
    (parser.is_exhausted() && items.iter().all(|(_, v)| v.is_finite())).then(|| sorted_map(items))
}

/// Deduplicate (the last value wins) and sort by tag, as the computed
/// values of the feature and variation settings are.
fn sorted_map<T: Copy>(items: Vec<(Tag, T)>) -> Vec<(Tag, T)> {
    let mut out: Vec<(Tag, T)> = Vec::with_capacity(items.len());
    for (tag, value) in items {
        match out.iter_mut().find(|(t, _)| *t == tag) {
            Some(existing) => existing.1 = value,
            None => out.push((tag, value)),
        }
    }
    out.sort_by_key(|(tag, _)| *tag);
    out
}

/// `font-size-adjust` computed value: `none`, or a metric and a number.
pub fn parse_size_adjust(value: &str) -> Option<(SizeAdjustMetric, f32)> {
    let mut words = value.split_ascii_whitespace();
    let first = words.next()?;
    let (metric, number) = match SizeAdjustMetric::parse(first) {
        Some(metric) => (metric, words.next()?),
        None => (SizeAdjustMetric::ExHeight, first),
    };
    if words.next().is_some() {
        return None;
    }
    let number: f32 = number.parse().ok()?;
    (number.is_finite() && number >= 0.0).then_some((metric, number))
}

/// The OpenType features a face defines in its GSUB and GPOS tables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FaceFeatures(pub Vec<Tag>);

impl FaceFeatures {
    pub fn read(font: &[u8], index: u32) -> Self {
        use skrifa::raw::{FontRef, TableProvider as _};
        let Ok(face) = FontRef::from_index(font, index) else {
            return Self::default();
        };
        let mut tags: Vec<Tag> = Vec::new();
        if let Ok(gsub) = face.gsub()
            && let Ok(list) = gsub.feature_list()
        {
            tags.extend(
                list.feature_records()
                    .iter()
                    .map(|r| r.feature_tag().to_be_bytes()),
            );
        }
        if let Ok(gpos) = face.gpos()
            && let Ok(list) = gpos.feature_list()
        {
            tags.extend(
                list.feature_records()
                    .iter()
                    .map(|r| r.feature_tag().to_be_bytes()),
            );
        }
        tags.sort_unstable();
        tags.dedup();
        Self(tags)
    }

    pub fn contains(&self, tag: &Tag) -> bool {
        self.0.binary_search(tag).is_ok()
    }
}

/// Which letters synthesized small capitals replace (#font-variant-caps-prop).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntheticCaps {
    /// Lowercase letters become small capitals.
    Lower,
    /// Lowercase and uppercase letters become small capitals.
    All,
    /// Uppercase letters become small capitals.
    Upper,
}

/// Synthesized small capitals: uppercase glyphs scaled to this fraction of
/// the font size (CSS leaves the factor to the UA; this is Chromium's).
pub const SMALL_CAPS_SCALE: f32 = 0.7;

/// #font-variant-caps-prop for a font defining `supports` features: the
/// capitalization features to enable, or the letters to synthesize small
/// capitals for. "If petite-caps or all-petite-caps is specified for a font
/// that doesn't support these features, the property behaves as if
/// small-caps or all-small-caps, respectively, had been specified. If
/// unicase is specified for a font that doesn't support that feature, the
/// property behaves as if small-caps was applied only to lowercased
/// uppercase letters. If titling-caps is specified with a font that does not
/// support this feature, this property has no visible effect."
pub fn caps_plan(
    caps: Caps,
    supports: impl Fn(&Tag) -> bool,
) -> (Vec<(Tag, u16)>, Option<SyntheticCaps>) {
    let on = |tags: &[&Tag]| (tags.iter().map(|tag| (**tag, 1)).collect(), None);
    match caps {
        Caps::Normal => (Vec::new(), None),
        Caps::SmallCaps if supports(b"smcp") => on(&[b"smcp"]),
        Caps::SmallCaps => (Vec::new(), Some(SyntheticCaps::Lower)),
        Caps::AllSmallCaps if supports(b"smcp") && supports(b"c2sc") => on(&[b"smcp", b"c2sc"]),
        Caps::AllSmallCaps => (Vec::new(), Some(SyntheticCaps::All)),
        Caps::PetiteCaps if supports(b"pcap") => on(&[b"pcap"]),
        Caps::PetiteCaps if supports(b"smcp") => on(&[b"smcp"]),
        Caps::PetiteCaps => (Vec::new(), Some(SyntheticCaps::Lower)),
        Caps::AllPetiteCaps if supports(b"pcap") && supports(b"c2pc") => on(&[b"pcap", b"c2pc"]),
        Caps::AllPetiteCaps if supports(b"smcp") && supports(b"c2sc") => on(&[b"smcp", b"c2sc"]),
        Caps::AllPetiteCaps => (Vec::new(), Some(SyntheticCaps::All)),
        Caps::Unicase if supports(b"unic") => on(&[b"unic"]),
        Caps::Unicase => (Vec::new(), Some(SyntheticCaps::Upper)),
        Caps::TitlingCaps if supports(b"titl") => on(&[b"titl"]),
        Caps::TitlingCaps => (Vec::new(), None),
    }
}

/// Whether `language` (a BCP 47 tag) uses the Turkic casing of i.
pub fn turkic(language: Option<&str>) -> bool {
    language.is_some_and(|tag| {
        let primary = tag.split(['-', '_']).next().unwrap_or("");
        primary.eq_ignore_ascii_case("tr") || primary.eq_ignore_ascii_case("az")
    })
}

/// #font-variant-caps-prop simulation: the letters selected by `caps`
/// become their uppercase forms (the text-transform casing, full case
/// mappings, Turkic dotted I) shaped at `size`. Caseless characters keep
/// their glyphs. A grapheme cluster is converted as a unit, so combining
/// marks stay with their base. Returns the text to shape, the byte ranges
/// to shape at `size`, and the source range of each converted cluster with
/// its range in the returned text.
pub fn synthesize_caps(
    text: &str,
    caps: SyntheticCaps,
    size: f32,
    turkic: bool,
) -> SynthesizedCaps {
    let mut out = String::with_capacity(text.len());
    let mut sized: Vec<(Range<usize>, f32)> = Vec::new();
    let mut converted = Vec::new();
    for (start, cluster) in text.grapheme_indices(true) {
        let first = cluster.chars().next().unwrap_or(' ');
        let lower = first.is_lowercase() && first.to_uppercase().next() != Some(first);
        let upper = first.is_uppercase();
        let small = match caps {
            SyntheticCaps::Lower => lower,
            SyntheticCaps::All => lower || upper,
            SyntheticCaps::Upper => upper,
        };
        if !small {
            out.push_str(cluster);
            continue;
        }
        let out_start = out.len();
        for c in cluster.chars() {
            if turkic && c == 'i' {
                out.push('\u{130}');
            } else {
                out.extend(c.to_uppercase());
            }
        }
        converted.push((start..start + cluster.len(), out_start..out.len()));
        match sized.last_mut() {
            Some((range, _)) if range.end == out_start => range.end = out.len(),
            _ => sized.push((out_start..out.len(), size)),
        }
    }
    (out, sized, converted)
}

/// [`synthesize_caps`]'s text, sized ranges and converted clusters.
pub type SynthesizedCaps = (
    String,
    Vec<(Range<usize>, f32)>,
    Vec<(Range<usize>, Range<usize>)>,
);

/// Maps byte offsets of shaped text whose clusters were converted
/// (synthesized small capitals) back to the source text: a converted
/// cluster maps as a unit, other bytes shift by the converted clusters'
/// accumulated change in length.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OffsetMap {
    /// (source range, shaped range) of each converted cluster, ascending.
    segments: Vec<(Range<usize>, Range<usize>)>,
}

impl OffsetMap {
    pub(crate) fn new(segments: Vec<(Range<usize>, Range<usize>)>) -> Self {
        Self { segments }
    }

    pub(crate) fn is_identity(&self) -> bool {
        self.segments.iter().all(|(a, b)| a == b)
    }

    /// The source offset of shaped offset `at`; an offset inside a
    /// converted cluster maps to its start (`end`: to its end).
    pub(crate) fn to_source(&self, at: usize, end: bool) -> usize {
        let index = self
            .segments
            .partition_point(|(_, shaped)| shaped.start <= at);
        let Some((source, shaped)) = index.checked_sub(1).map(|i| &self.segments[i]) else {
            return at;
        };
        if at >= shaped.end {
            return at - shaped.end + source.end;
        }
        if at == shaped.start || !end {
            source.start
        } else {
            source.end
        }
    }

    /// The shaped offset of source offset `at`.
    pub(crate) fn to_shaped(&self, at: usize, end: bool) -> usize {
        let index = self
            .segments
            .partition_point(|(source, _)| source.start <= at);
        let Some((source, shaped)) = index.checked_sub(1).map(|i| &self.segments[i]) else {
            return at;
        };
        if at >= source.end {
            return at - source.end + shaped.end;
        }
        if at == source.start || !end {
            shaped.start
        } else {
            shaped.end
        }
    }

    pub(crate) fn range_to_source(&self, range: Range<usize>) -> Range<usize> {
        self.to_source(range.start, false)..self.to_source(range.end, true)
    }

    pub(crate) fn range_to_shaped(&self, range: Range<usize>) -> Range<usize> {
        self.to_shaped(range.start, false)..self.to_shaped(range.end, true)
    }
}

/// Synthesized superscripts and subscripts (#font-variant-position-prop):
/// the size and baseline offset, as fractions of the font size, of the
/// "reduced forms of the glyphs that would be used without this feature".
/// "For larger text, the font size is often reduced by a third but for
/// smaller text sizes, the reduction can be much less": 0.82 at 20px and
/// below, two thirds at 45px and above, linear between (Gecko's ratios), and
/// a raise of 0.34em or drop of 0.2em.
pub fn position_metrics(position: Position, size: f32) -> Option<(f32, f32)> {
    let scale = if size <= 20.0 {
        0.82
    } else if size >= 45.0 {
        2.0 / 3.0
    } else {
        0.82 + (size - 20.0) / 25.0 * (2.0 / 3.0 - 0.82)
    };
    match position {
        Position::Normal => None,
        Position::Super => Some((scale, 0.34)),
        Position::Sub => Some((scale, -0.2)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_list_follows_the_precedence_order() {
        // #feature-variation-precedence: font-variant (10), then features
        // other properties determine (11: letter-spacing disables optional
        // ligatures), then font-feature-settings (13); the last value of a
        // tag wins.
        let t = FontTypography {
            ligatures: parse_ligatures("discretionary-ligatures no-contextual").unwrap(),
            numeric: parse_numeric("oldstyle-nums tabular-nums").unwrap(),
            kerning: Kerning::None,
            feature_settings: parse_feature_settings("\"dlig\" 0, \"liga\"").unwrap(),
            ..FontTypography::default()
        };
        let features = t.features(0.0, &[], false);
        assert_eq!(
            features,
            vec![
                (*b"dlig", 1),
                (*b"calt", 0),
                (*b"onum", 1),
                (*b"tnum", 1),
                (*b"kern", 0),
                (*b"dlig", 0),
                (*b"liga", 1),
            ]
        );
        let spaced = t.features(2.0, &[], false);
        let liga = spaced.iter().rposition(|(tag, _)| tag == b"liga").unwrap();
        assert_eq!(
            spaced[liga],
            (*b"liga", 1),
            "font-feature-settings re-enables it"
        );
        assert!(spaced.contains(&(*b"hlig", 0)));
        assert!(
            !spaced.contains(&(*b"rlig", 0)),
            "required ligatures are never disabled"
        );
    }

    #[test]
    fn none_disables_every_ligature_and_contextual_feature() {
        let t = FontTypography {
            ligatures: parse_ligatures("none").unwrap(),
            ..FontTypography::default()
        };
        let tags = t.features(0.0, &[], false);
        for tag in [b"liga", b"clig", b"dlig", b"hlig", b"calt"] {
            assert!(tags.contains(&(*tag, 0)));
        }
        assert!(parse_ligatures("common-ligatures no-common-ligatures").is_none());
    }

    #[test]
    fn settings_compute_to_sorted_maps() {
        assert_eq!(
            parse_feature_settings("\"smcp\", \"liga\" off, \"smcp\" 2").unwrap(),
            vec![(*b"liga", 0), (*b"smcp", 2)]
        );
        assert!(parse_feature_settings("\"silly\" off").is_none());
        assert!(parse_feature_settings("sinf").is_none());
        assert!(parse_feature_settings("\"sinf\" \"twid\"").is_none());
        assert!(parse_feature_settings("\"sinf\" -1").is_none());
        assert_eq!(
            parse_variation_settings("\"wght\" 700, \"XHGT\" 0.7, \"wght\" 500").unwrap(),
            vec![(*b"XHGT", 0.7), (*b"wght", 500.)]
        );
        assert!(parse_variation_settings("\"wght\"").is_none());
    }

    #[test]
    fn caps_fall_back_through_related_features_then_synthesis() {
        let only_smcp = |tag: &Tag| tag == b"smcp";
        assert_eq!(
            caps_plan(Caps::PetiteCaps, only_smcp).0,
            vec![(*b"smcp", 1)]
        );
        assert_eq!(
            caps_plan(Caps::AllPetiteCaps, only_smcp),
            (Vec::new(), Some(SyntheticCaps::All))
        );
        assert_eq!(
            caps_plan(Caps::Unicase, |_: &Tag| false),
            (Vec::new(), Some(SyntheticCaps::Upper))
        );
        assert_eq!(
            caps_plan(Caps::TitlingCaps, |_: &Tag| false),
            (Vec::new(), None)
        );
    }

    #[test]
    fn small_capital_synthesis_follows_text_transform_casing() {
        // #font-variant-caps-prop: lowercase letters become uppercase glyphs
        // at a smaller size; capitals and caseless characters keep theirs;
        // casing matches 'text-transform'.
        let synth = |text, caps, turkic| {
            let (text, sized, _) = synthesize_caps(text, caps, 7., turkic);
            (text, sized)
        };
        let (text, sized) = synth("aB1ß", SyntheticCaps::Lower, false);
        assert_eq!(text, "AB1SS");
        assert_eq!(sized, vec![(0..1, 7.), (3..5, 7.)]);
        let (text, sized) = synth("aB", SyntheticCaps::All, false);
        assert_eq!((text.as_str(), sized), ("AB", vec![(0..2, 7.)]));
        let (text, sized) = synth("aB", SyntheticCaps::Upper, false);
        assert_eq!((text.as_str(), sized), ("aB", vec![(1..2, 7.)]));
        // Turkic dotted capital I, and a combining mark stays with its base.
        let (text, _) = synth("i", SyntheticCaps::Lower, true);
        assert_eq!(text, "\u{130}");
        let (text, sized) = synth("e\u{301}x", SyntheticCaps::Lower, false);
        assert_eq!(text, "E\u{301}X");
        assert_eq!(sized, vec![(0..text.len(), 7.)]);
    }

    #[test]
    fn synthesized_caps_map_offsets_back_to_the_source() {
        let (text, sized, converted) =
            synthesize_caps("Straße é", SyntheticCaps::Lower, 7.0, false);
        assert_eq!(text, "STRASSE É");
        assert_eq!(sized, vec![(1..7, 7.0), (8..text.len(), 7.0)]);
        let map = OffsetMap::new(converted);
        // "ß" (2 bytes at 4) became "SS" (bytes 4..6).
        assert_eq!(map.to_source(4, false), 4);
        assert_eq!(map.to_source(5, false), 4);
        assert_eq!(map.to_source(5, true), 6);
        assert_eq!(map.to_source(6, false), 6);
        assert_eq!(map.to_source(text.len(), true), "Straße é".len());
        assert_eq!(map.to_shaped(6, false), 6);
        assert_eq!(map.range_to_shaped(0.."Straße".len()), 0..7);
    }

    #[test]
    fn typography_reads_canonical_computed_values() {
        let values = [
            ("font-width", "75%"),
            ("font-style", "oblique 10deg"),
            ("font-variant-caps", "small-caps"),
            ("font-variant-position", "super"),
            ("font-synthesis-style", "oblique-only"),
            ("font-size-adjust", "cap-height 0.7"),
            ("font-optical-sizing", "none"),
        ];
        let t = FontTypography::from_computed(|name| {
            values
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        })
        .unwrap();
        assert_eq!(t.width, 0.75);
        assert_eq!(t.oblique, Some(10.0));
        assert_eq!(t.caps, Caps::SmallCaps);
        assert_eq!(t.position, Position::Super);
        assert_eq!(t.synthesis.style, SynthesisStyle::ObliqueOnly);
        assert_eq!(t.size_adjust, Some((SizeAdjustMetric::CapHeight, 0.7)));
        assert!(!t.optical_sizing);
        assert!(FontTypography::from_computed(|_| None).is_none());
        assert!(
            FontTypography::from_computed(|name| (name == "font-width").then(|| "100%".into()))
                .is_none()
        );
    }
}
