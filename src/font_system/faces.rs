//! `@font-face` descriptors of downloadable and locally referenced faces.
//!
//! CSS Fonts 4 #font-prop-desc (csswg-drafts 81c27f686901): the
//! font-width, font-weight and font-style descriptors replace the style
//! the font data implies for matching and synthesis; fontique matches with
//! them. #font-rend-desc: the font-feature-settings and
//! font-variation-settings descriptors "set features and variation values
//! on the font object", applied per run by text shaping. CSS Fonts 5
//! #size-adjust-desc scales a face's glyphs and metrics; #font-metrics-
//! override-desc replaces its ascent, descent and line gap, which this
//! module writes into the face's `hhea` and `OS/2` tables, as
//! `restrict_cmap` writes `unicode-range` into its `cmap`. #local-font-
//! fallback: `local()` names one installed face by its full or PostScript
//! name.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parley::fontique::{Descriptor, FaceDescriptors as MatchDescriptors, StyleRange};

/// #font-display-desc.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum FontDisplay {
    #[default]
    Auto,
    Block,
    Swap,
    Fallback,
    Optional,
}

impl FontDisplay {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "block" => Self::Block,
            "swap" => Self::Swap,
            "fallback" => Self::Fallback,
            "optional" => Self::Optional,
            _ => return None,
        })
    }

    /// #font-display-timeline: the block and swap periods (`None`:
    /// infinite). `auto` is "user-agent-defined"; like many browsers TRust
    /// gives it `block`'s policy. 3s and 100ms are the recommended values;
    /// `optional` gets a 100ms window to load from a cache.
    pub(crate) fn periods(self) -> (std::time::Duration, Option<std::time::Duration>) {
        use std::time::Duration;
        match self {
            Self::Auto | Self::Block => (Duration::from_secs(3), None),
            Self::Swap => (Duration::from_millis(100), None),
            Self::Fallback => (Duration::from_millis(100), Some(Duration::from_secs(3))),
            // "A user agent may choose to slightly delay rendering an element
            // using an optional font to give it time to load from a possibly-
            // slow local cache": 100ms, after which it is never used.
            Self::Optional => (Duration::from_millis(100), Some(Duration::ZERO)),
        }
    }
}

/// One `@font-face` rule's descriptors other than font-family, src and
/// unicode-range.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FaceDescriptors {
    /// Width ratios (1.0 is normal); `Auto` when absent.
    pub width: Descriptor<[f32; 2]>,
    pub weight: Descriptor<[f32; 2]>,
    pub style: Descriptor<StyleRange>,
    pub display: FontDisplay,
    /// CSS Fonts 5 #size-adjust-desc as a factor.
    pub size_adjust: f32,
    /// #font-metrics-override-desc as fractions of the used font size.
    pub ascent_override: Option<f32>,
    pub descent_override: Option<f32>,
    pub line_gap_override: Option<f32>,
    /// #font-rend-desc, deduplicated and sorted by tag.
    pub features: Vec<([u8; 4], u32)>,
    pub variations: Vec<([u8; 4], f32)>,
    /// #font-named-instance: the named instance to apply (`auto`: `None`).
    pub named_instance: Option<String>,
}

impl Default for FaceDescriptors {
    fn default() -> Self {
        Self {
            width: Descriptor::Auto,
            weight: Descriptor::Auto,
            style: Descriptor::Auto,
            display: FontDisplay::Auto,
            size_adjust: 1.0,
            ascent_override: None,
            descent_override: None,
            line_gap_override: None,
            features: Vec::new(),
            variations: Vec::new(),
            named_instance: None,
        }
    }
}

impl FaceDescriptors {
    /// What font matching and synthesis read.
    pub(crate) fn matching(&self) -> MatchDescriptors {
        MatchDescriptors {
            width: self.width,
            weight: self.weight,
            style: self.style,
        }
    }

    /// What shaping applies per run of this face; `None` when nothing.
    pub(crate) fn rendering(&self) -> Option<FaceRendering> {
        (self.size_adjust != 1.0 || !self.features.is_empty() || !self.variations.is_empty()).then(
            || FaceRendering {
                size_adjust: self.size_adjust,
                features: self.features.clone(),
                variations: self.variations.clone(),
            },
        )
    }
}

impl FaceDescriptors {
    /// Apply one `@font-face` descriptor declaration; `false` when it is
    /// not one of these descriptors or its value is invalid, which leaves
    /// the descriptor as it was (CSS Syntax 3: an invalid declaration is
    /// ignored).
    pub(crate) fn apply(&mut self, name: &str, value: &str) -> bool {
        use crate::text::typography::{parse_feature_settings, parse_variation_settings};
        let value = value.trim();
        let lower = value.to_ascii_lowercase();
        match name.trim().to_ascii_lowercase().as_str() {
            "font-weight" => parse_range(&lower, weight_value).map(|w| self.weight = w),
            "font-width" | "font-stretch" => {
                parse_range(&lower, width_value).map(|w| self.width = w)
            }
            "font-style" => style_descriptor(&lower).map(|s| self.style = s),
            "font-display" => FontDisplay::parse(&lower).map(|d| self.display = d),
            "size-adjust" => percentage(&lower)
                .filter(|p| *p >= 0.0)
                .map(|p| self.size_adjust = p / 100.0),
            "ascent-override" => metric_override(&lower).map(|m| self.ascent_override = m),
            "descent-override" => metric_override(&lower).map(|m| self.descent_override = m),
            "line-gap-override" => metric_override(&lower).map(|m| self.line_gap_override = m),
            "font-feature-settings" => parse_feature_settings(value).map(|f| self.features = f),
            "font-variation-settings" => {
                parse_variation_settings(value).map(|v| self.variations = v)
            }
            "font-named-instance" => named_instance(value).map(|n| self.named_instance = n),
            _ => None,
        }
        .is_some()
    }
}

/// `auto | <string>`.
fn named_instance(value: &str) -> Option<Option<String>> {
    if value.eq_ignore_ascii_case("auto") {
        return Some(None);
    }
    let mut parser = cssparser::Parser::new(value);
    let name = parser.expect_string().ok()?.to_string();
    parser.is_exhausted().then_some(Some(name))
}

/// CSS Fonts 4 #feature-variation-precedence step 5: the user coordinates
/// of the named instance of `font` with a localized name equal to `name`
/// (#localized-name-matching), `None` when it has none.
pub(crate) fn named_instance_coords(font: &[u8], name: &str) -> Option<Vec<([u8; 4], f32)>> {
    use skrifa::MetadataProvider as _;
    let face = skrifa::FontRef::new(font).ok()?;
    let wanted = super::fold(name);
    let axes: Vec<[u8; 4]> = face
        .axes()
        .iter()
        .map(|axis| axis.tag().to_be_bytes())
        .collect();
    face.named_instances().iter().find_map(|instance| {
        face.localized_strings(instance.subfamily_name_id())
            .any(|text| super::fold(&text.chars().collect::<String>()) == wanted)
            .then(|| axes.iter().copied().zip(instance.user_coords()).collect())
    })
}

/// `auto | <value>{1,2}`: "Where a single value is specified, it has the
/// same meaning as a range with identical startpoint and endpoint. User
/// agents must swap the computed value of the startpoint and endpoint of
/// the range in order to forbid decreasing ranges."
fn parse_range(value: &str, one: fn(&str) -> Option<f32>) -> Option<Descriptor<[f32; 2]>> {
    if value == "auto" {
        return Some(Descriptor::Auto);
    }
    let parts: Vec<f32> = value
        .split_ascii_whitespace()
        .map(one)
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [single] => Some(Descriptor::Value([*single, *single])),
        [a, b] => Some(Descriptor::Value([a.min(*b), a.max(*b)])),
        _ => None,
    }
}

/// `<font-weight-absolute>`: `normal | bold | <number [1,1000]>`.
fn weight_value(word: &str) -> Option<f32> {
    match word {
        "normal" => Some(400.0),
        "bold" => Some(700.0),
        _ => word
            .parse::<f32>()
            .ok()
            .filter(|n| (1.0..=1000.0).contains(n)),
    }
}

/// `<'font-width'>` as a ratio.
fn width_value(word: &str) -> Option<f32> {
    crate::text::typography::parse_width(word)
}

/// `auto | normal | italic | left | right | oblique [ <angle [-90deg,
/// 90deg]>{1,2} ]?`. TRust does not tell left- from right-slanted italic
/// faces apart, so `left` and `right` describe italic faces.
fn style_descriptor(value: &str) -> Option<Descriptor<StyleRange>> {
    let upright = StyleRange {
        italic: None,
        oblique: Some([0.0, 0.0]),
    };
    let italic = StyleRange {
        italic: Some([1.0, 1.0]),
        oblique: None,
    };
    Some(match value {
        "auto" => Descriptor::Auto,
        "normal" => Descriptor::Value(upright),
        "italic" | "left" | "right" => Descriptor::Value(italic),
        _ => {
            let angles = value.strip_prefix("oblique")?;
            let angles: Vec<f32> = if angles.trim().is_empty() {
                vec![14.0]
            } else {
                if !angles.starts_with(char::is_whitespace) {
                    return None;
                }
                angles
                    .split_ascii_whitespace()
                    .map(|angle| {
                        (!angle.ends_with(|c: char| c.is_ascii_digit()))
                            .then(|| crate::text::typography::parse_angle_deg(angle))
                            .flatten()
                            .filter(|deg| (-90.0..=90.0).contains(deg))
                    })
                    .collect::<Option<_>>()?
            };
            let (lo, hi) = match angles.as_slice() {
                [a] => (*a, *a),
                [a, b] => (a.min(*b), a.max(*b)),
                _ => return None,
            };
            Descriptor::Value(StyleRange {
                italic: None,
                oblique: Some([lo, hi]),
            })
        }
    })
}

fn percentage(value: &str) -> Option<f32> {
    let number: f32 = value.strip_suffix('%')?.trim().parse().ok()?;
    number.is_finite().then_some(number)
}

/// `[ normal | <percentage [0,∞]> ]{1,2}` (CSS Fonts 5; the second value,
/// for upright typesetting in vertical modes, is not used by horizontal
/// text): the first value as a fraction, `Some(None)` for `normal`.
fn metric_override(value: &str) -> Option<Option<f32>> {
    let mut values = value.split_ascii_whitespace().map(|word| match word {
        "normal" => Some(None),
        _ => percentage(word)
            .filter(|p| *p >= 0.0)
            .map(|p| Some(p / 100.0)),
    });
    let first = values.next()??;
    if let Some(second) = values.next() {
        second?;
    }
    values.next().is_none().then_some(first)
}

/// The descriptors text shaping applies to the runs of one face.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FaceRendering {
    pub size_adjust: f32,
    pub features: Vec<([u8; 4], u32)>,
    pub variations: Vec<([u8; 4], f32)>,
}

/// The table records of an SFNT font: (tag, record offset).
fn table_records(font: &[u8]) -> Option<Vec<([u8; 4], usize)>> {
    let count = usize::from(u16::from_be_bytes(font.get(4..6)?.try_into().ok()?));
    (0..count)
        .map(|index| {
            let at = 12 + 16 * index;
            Some((font.get(at..at + 4)?.try_into().ok()?, at))
        })
        .collect()
}

fn be32(font: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(font.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// Rewrite a face's `hhea` and `OS/2` vertical metrics (and neutralize the
/// `MVAR` deltas for them) with the given ascent, descent and line gap,
/// fractions of the em (#font-metrics-override-desc: "the given percentage
/// multiplied by the used font size"). Each is written into both the
/// `hhea` and the typographic (and Windows) fields, so whichever source a
/// shaper reads, an overridden metric is the override and the others are
/// the font's own. `None` when `font` is not a single SFNT face.
pub(crate) fn override_metrics(
    font: &[u8],
    ascent: Option<f32>,
    descent: Option<f32>,
    line_gap: Option<f32>,
) -> Option<Vec<u8>> {
    if font.get(..4)? == b"ttcf" {
        return None;
    }
    let records = table_records(font)?;
    let find = |tag: &[u8; 4]| {
        records
            .iter()
            .find(|(t, _)| t == tag)
            .and_then(|&(_, at)| Some((be32(font, at + 8)?, be32(font, at + 12)?, at)))
    };
    let (head, head_len, _) = find(b"head")?;
    if head_len < 54 {
        return None;
    }
    let upem = f32::from(u16::from_be_bytes(
        font.get(head + 18..head + 20)?.try_into().ok()?,
    ));
    if upem <= 0.0 {
        return None;
    }
    let units = |fraction: f32| (fraction * upem).round().clamp(0.0, f32::from(i16::MAX)) as i16;
    let mut out = font.to_vec();
    let mut touched: Vec<usize> = Vec::new();
    let put16 = |out: &mut Vec<u8>, at: usize, value: i16| {
        if let Some(slot) = out.get_mut(at..at + 2) {
            slot.copy_from_slice(&value.to_be_bytes());
        }
    };
    if let Some((hhea, len, record)) = find(b"hhea")
        && len >= 36
    {
        if let Some(a) = ascent {
            put16(&mut out, hhea + 4, units(a));
        }
        if let Some(d) = descent {
            put16(&mut out, hhea + 6, -units(d));
        }
        if let Some(g) = line_gap {
            put16(&mut out, hhea + 8, units(g));
        }
        touched.push(record);
    }
    if let Some((os2, len, record)) = find(b"OS/2")
        && len >= 78
    {
        if let Some(a) = ascent {
            put16(&mut out, os2 + 68, units(a));
            out[os2 + 74..os2 + 76].copy_from_slice(&(units(a) as u16).to_be_bytes());
        }
        if let Some(d) = descent {
            put16(&mut out, os2 + 70, -units(d));
            out[os2 + 76..os2 + 78].copy_from_slice(&(units(d) as u16).to_be_bytes());
        }
        if let Some(g) = line_gap {
            put16(&mut out, os2 + 72, units(g));
        }
        touched.push(record);
    }
    // MVAR value records: (tag, deltaSetOuterIndex, deltaSetInnerIndex).
    // An index pair outside the variation store contributes no delta.
    if let Some((mvar, len, record)) = find(b"MVAR")
        && len >= 12
    {
        let size = usize::from(u16::from_be_bytes(
            out.get(mvar + 6..mvar + 8)?.try_into().ok()?,
        ));
        let count = usize::from(u16::from_be_bytes(
            out.get(mvar + 8..mvar + 10)?.try_into().ok()?,
        ));
        let overridden = |tag: &[u8]| {
            (tag == b"hasc" || tag == b"hcla") && ascent.is_some()
                || (tag == b"hdsc" || tag == b"hcld") && descent.is_some()
                || tag == b"hlgp" && line_gap.is_some()
        };
        if size >= 8 {
            for index in 0..count {
                let at = mvar + 12 + size * index;
                if at + 8 > mvar + len {
                    break;
                }
                if overridden(&out[at..at + 4]) {
                    out[at + 4..at + 8].copy_from_slice(&[0xff; 4]);
                }
            }
            touched.push(record);
        }
    }
    for record in touched {
        let offset = be32(&out, record + 8)?;
        let length = be32(&out, record + 12)?;
        let checksum = super::sfnt_checksum(out.get(offset..offset + length)?);
        out[record + 4..record + 8].copy_from_slice(&checksum.to_be_bytes());
    }
    Some(out)
}

/// A standalone SFNT font of face `index` of a font collection (`ttcf`),
/// whose faces share one file: `@font-face` describes one face.
pub(crate) fn collection_face(font: &[u8], index: u32) -> Option<Vec<u8>> {
    if font.get(..4)? != b"ttcf" {
        return (index == 0).then(|| font.to_vec());
    }
    let count = be32(font, 8)?;
    if index as usize >= count {
        return None;
    }
    let offset = be32(font, 12 + 4 * index as usize)?;
    let face = font.get(offset..)?;
    let tables = usize::from(u16::from_be_bytes(face.get(4..6)?.try_into().ok()?));
    let mut out = face.get(..12 + 16 * tables)?.to_vec();
    for table in 0..tables {
        let at = 12 + 16 * table;
        let data_offset = be32(face, at + 8)?;
        let length = be32(face, at + 12)?;
        // Collection table offsets count from the start of the file.
        let data = font.get(data_offset..data_offset + length)?;
        let new_offset = out.len() as u32;
        out[at + 8..at + 12].copy_from_slice(&new_offset.to_be_bytes());
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    Some(out)
}

/// The full (nameID 4) and PostScript (nameID 6) names of a face: the US
/// English record, else the first (#local-font-fallback).
fn face_names(name_table: &[u8]) -> Vec<String> {
    let Some(table) = ttf_parser::name::Table::parse(name_table) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for id in [4u16, 6] {
        let records: Vec<_> = table
            .names
            .into_iter()
            .filter(|name| name.name_id == id)
            .collect();
        // The US English record (Windows language 0x409, Macintosh 0, as
        // Mac Roman, here its ASCII subset), else the first decodable one.
        let decode = |name: &ttf_parser::name::Name<'_>| {
            name.to_string().or_else(|| {
                (name.platform_id == ttf_parser::PlatformId::Macintosh
                    && name.encoding_id == 0
                    && name.name.is_ascii())
                .then(|| String::from_utf8_lossy(name.name).into_owned())
            })
        };
        let windows = records.iter().find(|name| {
            name.platform_id == ttf_parser::PlatformId::Windows && name.language_id == 0x409
        });
        let mac = records.iter().find(|name| {
            name.platform_id == ttf_parser::PlatformId::Macintosh && name.language_id == 0
        });
        if let Some(name) = windows
            .and_then(decode)
            .or_else(|| mac.and_then(decode))
            .or_else(|| records.iter().find_map(decode))
        {
            out.push(name);
        }
    }
    out
}

/// Bounded reads of one font file's face names, without loading the file.
fn file_face_names(path: &Path) -> Vec<(u32, Vec<String>)> {
    const MAX_NAME_TABLE: usize = 1 << 20;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut read_at = |offset: u64, length: usize| -> Option<Vec<u8>> {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buffer = vec![0; length];
        file.read_exact(&mut buffer).ok()?;
        Some(buffer)
    };
    let Some(header) = read_at(0, 12) else {
        return Vec::new();
    };
    let offsets: Vec<u64> = if &header[..4] == b"ttcf" {
        let count = u32::from_be_bytes(header[8..12].try_into().unwrap_or_default()).min(4096);
        read_at(12, 4 * count as usize).map_or_else(Vec::new, |table| {
            let (offsets, _) = table.as_chunks::<4>();
            offsets
                .iter()
                .map(|offset| u64::from(u32::from_be_bytes(*offset)))
                .collect()
        })
    } else {
        vec![0]
    };
    let mut out = Vec::new();
    for (index, offset) in offsets.into_iter().enumerate() {
        let Some(directory) = read_at(offset, 12) else {
            continue;
        };
        let tables = usize::from(u16::from_be_bytes([directory[4], directory[5]])).min(1024);
        let Some(records) = read_at(offset + 12, 16 * tables) else {
            continue;
        };
        let (records, _) = records.as_chunks::<16>();
        let Some(name) = records.iter().find(|record| &record[..4] == b"name") else {
            continue;
        };
        let at = u64::from(u32::from_be_bytes([name[8], name[9], name[10], name[11]]));
        let length = u32::from_be_bytes([name[12], name[13], name[14], name[15]]) as usize;
        if length > MAX_NAME_TABLE {
            continue;
        }
        if let Some(table) = read_at(at, length) {
            out.push((index as u32, face_names(&table)));
        }
    }
    out
}

/// Installed faces by case-folded full and PostScript name, built on the
/// first `local()` reference from the catalog's font files.
fn local_faces(paths: &[PathBuf]) -> &'static HashMap<String, (PathBuf, u32)> {
    static FACES: OnceLock<HashMap<String, (PathBuf, u32)>> = OnceLock::new();
    FACES.get_or_init(|| {
        let mut faces = HashMap::new();
        for path in paths {
            for (index, names) in file_face_names(path) {
                for name in names {
                    faces
                        .entry(super::fold(&name))
                        .or_insert_with(|| (path.clone(), index));
                }
            }
        }
        faces
    })
}

/// #local-font-fallback: the installed face whose full name or PostScript
/// name is `name`, as standalone SFNT bytes. "Platform substitutions for a
/// given font name must not be used."
pub(crate) fn local_face(name: &str) -> Option<Vec<u8>> {
    let (path, index) = local_faces(&super::catalog().paths).get(&super::fold(name))?;
    let length = std::fs::metadata(path).ok()?.len();
    if length > super::MAX_FONT_FILE_BYTES {
        return None;
    }
    collection_face(&std::fs::read(path).ok()?, *index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dejavu() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/dejavu/DejaVuSans.ttf"
        ))
        .unwrap()
    }

    #[test]
    fn metric_overrides_replace_hhea_and_typographic_metrics() {
        let font = dejavu();
        let out = override_metrics(&font, Some(0.5), Some(0.25), Some(0.0)).unwrap();
        let face = skrifa::FontRef::new(&out).unwrap();
        use skrifa::MetadataProvider as _;
        let metrics = face.metrics(
            skrifa::instance::Size::new(100.0),
            skrifa::instance::LocationRef::default(),
        );
        assert!((metrics.ascent - 50.0).abs() < 0.1, "{metrics:?}");
        assert!((metrics.descent + 25.0).abs() < 0.1, "{metrics:?}");
        assert_eq!(metrics.leading, 0.0);
        // A metric without an override keeps the font's own value.
        let only = override_metrics(&font, None, Some(0.25), None).unwrap();
        let original = skrifa::FontRef::new(&font).unwrap().metrics(
            skrifa::instance::Size::new(100.0),
            skrifa::instance::LocationRef::default(),
        );
        let changed = skrifa::FontRef::new(&only).unwrap().metrics(
            skrifa::instance::Size::new(100.0),
            skrifa::instance::LocationRef::default(),
        );
        assert_eq!(changed.ascent, original.ascent);
        assert_eq!(changed.leading, original.leading);
    }

    #[test]
    fn local_names_are_full_and_postscript_names() {
        let font = dejavu();
        let face = ttf_parser::Face::parse(&font, 0).unwrap();
        let name = face
            .raw_face()
            .table(ttf_parser::Tag::from_bytes(b"name"))
            .unwrap();
        let names = face_names(name);
        assert_eq!(names, ["DejaVu Sans", "DejaVuSans"]);
    }

    #[test]
    fn a_collection_face_becomes_a_standalone_font() {
        let font = dejavu();
        assert_eq!(collection_face(&font, 0).as_deref(), Some(&font[..]));
        assert!(collection_face(&font, 1).is_none());
    }

    #[test]
    fn descriptors_parse_ranges_and_reject_invalid_values() {
        let mut d = FaceDescriptors::default();
        assert!(d.apply("font-weight", "700 300"));
        assert_eq!(d.weight, Descriptor::Value([300.0, 700.0]));
        assert!(!d.apply("font-weight", "bolder"));
        assert!(!d.apply("font-weight", "0"));
        assert_eq!(d.weight, Descriptor::Value([300.0, 700.0]), "kept");
        assert!(d.apply("font-stretch", "condensed 125%"));
        assert_eq!(d.width, Descriptor::Value([0.75, 1.25]));
        assert!(d.apply("font-style", "oblique 20deg 10deg"));
        assert_eq!(
            d.style,
            Descriptor::Value(StyleRange {
                italic: None,
                oblique: Some([10.0, 20.0])
            })
        );
        assert!(!d.apply("font-style", "oblique 100deg"));
        assert!(d.apply("size-adjust", "90%"));
        assert_eq!(d.size_adjust, 0.9);
        assert!(!d.apply("size-adjust", "-1%"));
        assert!(d.apply("ascent-override", "80% normal"));
        assert_eq!(d.ascent_override, Some(0.8));
        assert!(d.apply("line-gap-override", "normal"));
        assert_eq!(d.line_gap_override, None);
        assert!(d.apply("font-feature-settings", "\"liga\" 0"));
        assert_eq!(d.features, vec![(*b"liga", 0)]);
        assert!(d.apply("font-display", "swap"));
        assert_eq!(d.display, FontDisplay::Swap);
    }

    #[test]
    fn display_periods_follow_the_recommendations() {
        use std::time::Duration;
        assert_eq!(FontDisplay::Block.periods(), (Duration::from_secs(3), None));
        assert_eq!(
            FontDisplay::Fallback.periods(),
            (Duration::from_millis(100), Some(Duration::from_secs(3)))
        );
        assert_eq!(FontDisplay::parse("OPTIONAL"), Some(FontDisplay::Optional));
        assert_eq!(FontDisplay::parse("late"), None);
    }
}
