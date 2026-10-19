//! CSS Text 4 #text-transform-property / #text-transform-mapping /
//! #text-transform-order (CSSWG 81c27f686901), MathML Core
//! #math-auto-transform (f69212ad1f21), and Unicode 17 §3.13, Table 3-17.
//! Casing reads the original inline context, including across inline boxes.
//! Length-changing mappings retain their source cluster's UTF-16 range.

use std::{borrow::Cow, ops::Range};

use icu_properties::{CodePointMapData, CodePointSetData, props};
use unicode_segmentation::UnicodeSegmentation as _;

#[path = "transform_data.rs"]
mod data;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Case {
    #[default]
    None,
    Upper,
    Lower,
    Capitalize,
    Math,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextTransform {
    case: Case,
    width: bool,
    kana: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Mapping {
    pub bytes: Range<usize>,
    pub units: Range<u32>,
}

pub(crate) struct Transformed<'a> {
    pub text: Cow<'a, str>,
    /// Empty when each output character still uses the same UTF-16 units.
    pub mappings: Vec<Mapping>,
}

impl Transformed<'_> {
    /// Per-character shaping clusters in original UTF-8 coordinates. SVG
    /// positioning and text APIs index the source before transformation.
    pub(crate) fn source_byte_ranges(&self, source: &str) -> Vec<(Range<usize>, Range<usize>)> {
        let mut units = 0;
        let mut boundaries = Vec::new();
        for (byte, c) in source.char_indices() {
            boundaries.push((units, byte));
            units += c.len_utf16() as u32;
        }
        boundaries.push((units, source.len()));
        let byte = |unit| {
            boundaries[boundaries
                .partition_point(|(at, _)| *at < unit)
                .min(boundaries.len() - 1)]
            .1
        };
        let identity = [Mapping {
            bytes: 0..self.text.len(),
            units: 0..units,
        }];
        let mappings = if self.mappings.is_empty() {
            &identity[..]
        } else {
            &self.mappings
        };
        let mut result = Vec::new();
        for mapping in mappings {
            let text = &self.text[mapping.bytes.clone()];
            let exact =
                text.encode_utf16().count() as u32 == mapping.units.end - mapping.units.start;
            let mut unit = mapping.units.start;
            if text.is_empty() {
                result.push((
                    mapping.bytes.clone(),
                    byte(mapping.units.start)..byte(mapping.units.end),
                ));
            }
            for (offset, c) in text.char_indices() {
                let source = if exact {
                    let next = unit + c.len_utf16() as u32;
                    let range = byte(unit)..byte(next);
                    unit = next;
                    range
                } else {
                    byte(mapping.units.start)..byte(mapping.units.end)
                };
                result.push((
                    mapping.bytes.start + offset..mapping.bytes.start + offset + c.len_utf8(),
                    source,
                ));
            }
        }
        result
    }
}

pub(crate) fn svg_transform(
    spans: &[resvg::usvg::TextTransformInput<'_>],
) -> Vec<resvg::usvg::TextTransformOutput> {
    let mut original = String::new();
    let mut offsets = Vec::with_capacity(spans.len());
    for span in spans {
        offsets.push(original.len());
        original.push_str(span.text);
    }
    let context = Context::new(&original);
    spans
        .iter()
        .zip(offsets)
        .map(|(span, offset)| {
            let transformed = TextTransform::from_css(span.transform)
                .unwrap_or_default()
                .transform(
                    span.text,
                    span.language,
                    false,
                    span.preserve_spaces,
                    span.single_text_node,
                    Some((&context, offset)),
                );
            let mappings = transformed.source_byte_ranges(span.text);
            resvg::usvg::TextTransformOutput {
                text: transformed.text.into_owned(),
                mappings,
            }
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Conditions {
    before_cased: bool,
    after_cased: bool,
    after_i: bool,
    soft_dotted: bool,
    before_dot: bool,
    more_above: bool,
    title: bool,
    dutch_j: bool,
}

/// Precomputed context makes even long combining-mark sequences linear.
/// The caller supplies all in-flow text of an inline formatting context;
/// out-of-flow boxes contribute nothing, atomic boxes delimit words.
pub(crate) struct Context {
    chars: Vec<(usize, char, Conditions)>,
}

impl Context {
    pub(crate) fn new(text: &str) -> Self {
        let cased = CodePointSetData::new::<props::Cased>();
        let ignorable = CodePointSetData::new::<props::CaseIgnorable>();
        let dotted = CodePointSetData::new::<props::SoftDotted>();
        let ccc = CodePointMapData::<props::CanonicalCombiningClass>::new();
        let mut chars: Vec<_> = text
            .char_indices()
            .map(|(i, c)| (i, c, Conditions::default()))
            .collect();
        let (mut before_cased, mut after_i, mut soft_dotted) = (false, false, false);
        for (_, c, state) in &mut chars {
            state.before_cased = before_cased;
            state.after_i = after_i;
            state.soft_dotted = soft_dotted;
            if !ignorable.contains(*c) {
                before_cased = cased.contains(*c);
            }
            let class = ccc.get(*c);
            if class == props::CanonicalCombiningClass::NotReordered
                || class == props::CanonicalCombiningClass::Above
            {
                after_i = *c == 'I';
                soft_dotted = dotted.contains(*c);
            }
        }
        let (mut after_cased, mut before_dot, mut more_above) = (false, false, false);
        for (_, c, state) in chars.iter_mut().rev() {
            state.after_cased = after_cased;
            state.before_dot = before_dot;
            state.more_above = more_above;
            if !ignorable.contains(*c) {
                after_cased = cased.contains(*c);
            }
            let class = ccc.get(*c);
            if class == props::CanonicalCombiningClass::NotReordered
                || class == props::CanonicalCombiningClass::Above
            {
                before_dot = *c == '\u{307}';
                more_above = class == props::CanonicalCombiningClass::Above;
            }
        }
        let category = CodePointMapData::<props::GeneralCategory>::new();
        for (word_start, word) in text.split_word_bound_indices() {
            let Some((start, unit)) = word.grapheme_indices(true).find(|(_, unit)| {
                unit.chars().next().is_some_and(|c| {
                    let gc = category.get(c);
                    props::GeneralCategoryGroup::Letter.contains(gc)
                        || props::GeneralCategoryGroup::Number.contains(gc)
                })
            }) else {
                continue;
            };
            let start = word_start + start;
            let end = start + unit.len();
            let first = chars.partition_point(|(at, _, _)| *at < start);
            if unit.chars().next().is_some_and(char::is_lowercase) {
                for (at, _, state) in &mut chars[first..] {
                    if *at >= end {
                        break;
                    }
                    state.title = true;
                }
            }
            // Dutch treats initial IJ as one titlecased letter unit.
            if matches!(unit, "i" | "I") && matches!(text[end..].chars().next(), Some('j' | 'J')) {
                let next = chars.partition_point(|(at, _, _)| *at < end);
                chars[next].2.dutch_j = true;
            }
        }
        Self { chars }
    }

    fn at(&self, byte: usize) -> Conditions {
        self.chars
            .binary_search_by_key(&byte, |(at, _, _)| *at)
            .ok()
            .map_or_else(Conditions::default, |i| self.chars[i].2)
    }
}

impl TextTransform {
    pub(crate) const NONE: Self = Self {
        case: Case::None,
        width: false,
        kana: false,
    };

    pub(crate) fn from_css(value: &str) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        if lower == "none" {
            return Some(Self::NONE);
        }
        if lower == "math-auto" {
            return Some(Self {
                case: Case::Math,
                ..Self::NONE
            });
        }
        let mut result = Self::NONE;
        for word in lower.split_ascii_whitespace() {
            match word {
                "uppercase" | "lowercase" | "capitalize" if result.case == Case::None => {
                    result.case = match word {
                        "uppercase" => Case::Upper,
                        "lowercase" => Case::Lower,
                        _ => Case::Capitalize,
                    };
                }
                "full-width" if !result.width => result.width = true,
                "full-size-kana" if !result.kana => result.kana = true,
                _ => return None,
            }
        }
        (result != Self::NONE).then_some(result)
    }

    pub(crate) fn needs_context(self) -> bool {
        matches!(self.case, Case::Upper | Case::Lower | Case::Capitalize)
    }

    pub(crate) fn is_math(self) -> bool {
        self.case == Case::Math
    }

    pub(crate) fn apply<'a>(self, text: &'a str, language: Option<&str>) -> Cow<'a, str> {
        self.transform(text, language, false, true, true, None).text
    }

    /// Normalize segment breaks before transformation. Collapsible U+0020
    /// stays collapsible; only preserved spaces receive full-width mapping.
    pub(crate) fn transform<'a>(
        self,
        text: &'a str,
        language: Option<&str>,
        spaces: bool,
        preserve: bool,
        single_node: bool,
        context: Option<(&Context, usize)>,
    ) -> Transformed<'a> {
        if self == Self::NONE && !text.contains('\r') && !(spaces && text.contains(['\n', '\t'])) {
            return Transformed {
                text: Cow::Borrowed(text),
                mappings: Vec::new(),
            };
        }
        let local;
        let context = if let Some(context) = context {
            context
        } else {
            local = Context::new(text);
            (&local, 0)
        };
        let language = language
            .unwrap_or("")
            .split(['-', '_'])
            .next()
            .unwrap_or("");
        let turkic = language.eq_ignore_ascii_case("tr") || language.eq_ignore_ascii_case("az");
        let lithuanian = language.eq_ignore_ascii_case("lt");
        let dutch = language.eq_ignore_ascii_case("nl");
        let math = self.case == Case::Math && single_node && text.chars().count() == 1;
        let mut out = String::with_capacity(text.len());
        let mut cased = String::new();
        let mut mappings: Vec<Mapping> = Vec::new();
        let (mut units, mut changed_units, mut last_exact) = (0, false, false);
        for (at, cluster) in text.grapheme_indices(true) {
            let start = out.len();
            cased.clear();
            if cluster == "\r\n" {
                cased.push(if spaces { ' ' } else { '\n' });
            } else {
                for (relative, c) in cluster.char_indices() {
                    let state = context.0.at(context.1 + at + relative);
                    let case = match self.case {
                        Case::Capitalize if state.title || (dutch && state.dutch_j) => {
                            Case::Capitalize
                        }
                        Case::Capitalize => Case::None,
                        case => case,
                    };
                    if c == '\r' || (spaces && matches!(c, '\n' | '\t')) {
                        cased.push(if spaces { ' ' } else { '\n' });
                    } else if math {
                        cased.push(map(data::MATH, c));
                    } else {
                        case_char(&mut cased, c, case, state, turkic, lithuanian);
                    }
                }
            }
            for c in cased.chars() {
                let c = if self.width && (c != ' ' || preserve) {
                    map(data::WIDTH, c)
                } else {
                    c
                };
                out.push(if self.kana { map(data::KANA, c) } else { c });
            }
            let end_units = units + crate::render::selection::utf16_len(cluster);
            let exact = crate::render::selection::utf16_len(&out[start..]) == end_units - units;
            changed_units |= !exact;
            if exact && last_exact {
                let previous = mappings.last_mut().unwrap();
                previous.bytes.end = out.len();
                previous.units.end = end_units;
            } else {
                mappings.push(Mapping {
                    bytes: start..out.len(),
                    units: units..end_units,
                });
            }
            units = end_units;
            last_exact = exact;
        }
        if !changed_units {
            mappings.clear();
        }
        Transformed {
            text: if out == text {
                Cow::Borrowed(text)
            } else {
                Cow::Owned(out)
            },
            mappings,
        }
    }
}

fn map(table: &[(char, char)], c: char) -> char {
    table
        .binary_search_by_key(&c, |(from, _)| *from)
        .ok()
        .map_or(c, |i| table[i].1)
}

fn case_char(
    out: &mut String,
    c: char,
    case: Case,
    state: Conditions,
    turkic: bool,
    lithuanian: bool,
) {
    match case {
        Case::Lower => {
            // SpecialCasing.txt: context always refers to original text.
            if turkic {
                match c {
                    '\u{130}' => {
                        out.push('i');
                        return;
                    }
                    'I' if !state.before_dot => {
                        out.push('\u{131}');
                        return;
                    }
                    '\u{307}' if state.after_i => return,
                    _ => {}
                }
            }
            if lithuanian {
                match c {
                    'I' | 'J' | '\u{12e}' if state.more_above => {
                        out.extend(c.to_lowercase());
                        out.push('\u{307}');
                        return;
                    }
                    '\u{cc}' => {
                        out.push_str("i\u{307}\u{300}");
                        return;
                    }
                    '\u{cd}' => {
                        out.push_str("i\u{307}\u{301}");
                        return;
                    }
                    '\u{128}' => {
                        out.push_str("i\u{307}\u{303}");
                        return;
                    }
                    _ => {}
                }
            }
            if c == '\u{3a3}' && state.before_cased && !state.after_cased {
                out.push('\u{3c2}');
            } else {
                out.extend(c.to_lowercase());
            }
        }
        Case::Upper | Case::Capitalize => {
            if turkic && c == 'i' {
                out.push('\u{130}');
                return;
            }
            if lithuanian && c == '\u{307}' && state.soft_dotted {
                return;
            }
            if case == Case::Capitalize
                && let Ok(i) = data::TITLE.binary_search_by_key(&c, |(from, _)| *from)
            {
                out.push_str(data::TITLE[i].1);
            } else {
                out.extend(c.to_uppercase());
            }
        }
        _ => out.push(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(css: &str, text: &str, language: Option<&str>) -> String {
        TextTransform::from_css(css)
            .unwrap()
            .apply(text, language)
            .into_owned()
    }

    #[test]
    fn text_transform_grammar_accepts_independent_effects_once() {
        for value in [
            "none",
            "math-auto",
            "uppercase full-width full-size-kana",
            "full-size-kana capitalize",
            "full-width",
        ] {
            assert!(TextTransform::from_css(value).is_some(), "{value}");
        }
        for value in [
            "",
            "none full-width",
            "uppercase lowercase",
            "capitalize capitalize",
            "math-auto full-width",
            "full-width full-width",
        ] {
            assert!(TextTransform::from_css(value).is_none(), "{value}");
        }
    }

    #[test]
    fn text_transform_capitalizes_letter_units_with_full_titlecase() {
        assert_eq!(
            convert("capitalize", "'hello-world' 123abc ǳUNGLE ßtraße ﬃSH", None),
            "'Hello-World' 123abc ǲUNGLE Sstraße FfiSH"
        );
        assert_eq!(
            convert("capitalize", "ijssel IJssel", Some("nl-NL")),
            "IJssel IJssel"
        );
        assert_eq!(
            convert("capitalize", "istanbul izmir", Some("tr")),
            "İstanbul İzmir"
        );
    }

    #[test]
    fn text_transform_uses_original_context_for_conditional_casing() {
        assert_eq!(
            convert("lowercase", "ΟΣ ΟΣΑ Ο\u{345}Σ", None),
            "ος οσα ο\u{345}ς"
        );
        assert_eq!(
            convert("lowercase", "I İ I\u{307} I\u{301}", Some("tr-TR")),
            "ı i i ı\u{301}"
        );
        assert_eq!(
            convert("lowercase", "I\u{301} J\u{300} Į\u{303}", Some("lt")),
            "i\u{307}\u{301} j\u{307}\u{300} į\u{307}\u{303}"
        );
        assert_eq!(
            convert("uppercase", "i\u{307}\u{301}", Some("lt")),
            "I\u{301}"
        );
        let context = Context::new("aBC ΟΣΑ");
        let transform = TextTransform::from_css("capitalize").unwrap();
        assert_eq!(
            transform
                .transform("BC", None, false, true, true, Some((&context, 1)))
                .text,
            "BC"
        );
        let transform = TextTransform::from_css("lowercase").unwrap();
        assert_eq!(
            transform
                .transform("Σ", None, false, true, true, Some((&context, 6)))
                .text,
            "σ"
        );
    }

    #[test]
    fn text_transform_width_kana_and_math_follow_the_official_maps() {
        assert_eq!(
            convert("uppercase full-width full-size-kana", "a ｡ｧｶﾞ ぁㇰ", None),
            "Ａ　。アカ\u{3099}　あク"
        );
        assert_eq!(
            convert("full-size-kana", "ぁァｧㇰ\u{1b132}\u{1b167}", None),
            "あアｱクこン"
        );
        assert_eq!(convert("math-auto", "h", None), "ℎ");
        assert_eq!(convert("math-auto", "α", None), "𝛼");
        assert_eq!(convert("math-auto", "ab", None), "ab");
        let width = TextTransform::from_css("full-width").unwrap();
        assert_eq!(
            width.transform("a  b", None, false, false, true, None).text,
            "ａ  ｂ"
        );
        assert_eq!(
            width
                .transform("a\t\r\nb", None, true, true, true, None)
                .text,
            "ａ　　ｂ"
        );
    }

    #[test]
    fn text_transform_retains_offsets_around_expanded_and_contracted_clusters() {
        let t = TextTransform::from_css("uppercase")
            .unwrap()
            .transform("aßb", None, false, true, true, None);
        assert_eq!(t.text, "ASSB");
        let ranges: Vec<_> = t
            .mappings
            .iter()
            .map(|m| (m.bytes.clone(), m.units.clone()))
            .collect();
        assert_eq!(ranges, [(0..1, 0..1), (1..3, 1..2), (3..4, 2..3)]);
        let t = TextTransform::NONE.transform("a\r\nb", None, false, true, true, None);
        let ranges: Vec<_> = t
            .mappings
            .iter()
            .map(|m| (m.bytes.clone(), m.units.clone()))
            .collect();
        assert_eq!(t.text, "a\nb");
        assert_eq!(ranges, [(0..1, 0..1), (1..2, 1..3), (2..3, 3..4)]);
        let t = TextTransform::from_css("math-auto")
            .unwrap()
            .transform("x", None, false, true, true, None);
        assert_eq!(t.mappings[0].units, 0..1);
        assert_eq!(t.mappings[0].bytes, 0..4);
    }
}
