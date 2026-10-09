// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Model for a font.

use crate::{CharmapIndex, Script, ScriptExt as _};

use super::source::{SourceInfo, SourceKind};
use super::{Blob, source_cache::SourceCache};
use crate::{FontStyle, FontWeight, FontWidth};
use core::fmt;
use read_fonts::{FontRef, TableProvider as _, types::Tag};
use smallvec::SmallVec;

type AxisVec = SmallVec<[AxisInfo; 1]>;
type ScriptCoverageVec = SmallVec<[(Script, u8); 16]>;

/// Representation of a single font in a family.
#[derive(Clone, Debug)]
pub struct FontInfo {
    source: SourceInfo,
    index: u32,
    width: FontWidth,
    style: FontStyle,
    weight: FontWeight,
    axes: AxisVec,
    attr_axes: u8,
    charmap_index: CharmapIndex,
    monospaced: bool,
    script_coverage: ScriptCoverageVec,
    /// TRust: CSS `@font-face` descriptors replacing the attributes implied
    /// by the font data (`None`: the font data's own).
    descriptors: Option<FaceDescriptors>,
}

/// TRust: the italic values and oblique angles a face supports (CSS Fonts 4
/// #font-style-matching). An upright face supports oblique 0deg, an italic
/// face the italic value 1, an `ital` axis its italic range and a `slnt`
/// axis the oblique angles of its range negated (OpenType slants are
/// counter-clockwise, CSS angles clockwise).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct StyleRange {
    /// Supported italic values (0 upright, 1 italic).
    pub italic: Option<[f32; 2]>,
    /// Supported oblique angles in degrees, positive clockwise.
    pub oblique: Option<[f32; 2]>,
}

/// TRust: one `@font-face` font-width, font-weight or font-style descriptor
/// (CSS Fonts 4 #font-prop-desc).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum Descriptor<T> {
    /// No descriptor: the attributes the font data implies.
    #[default]
    FromFont,
    /// `auto`: "For font selection purposes, the font is selected as if the
    /// appropriate normal value ... is chosen. For variation axis clamping,
    /// clamping does not occur."
    Auto,
    /// An inclusive range (a single value is a range with identical
    /// endpoints); variation values applied to the face clamp to it.
    Value(T),
}

/// TRust: a face's `@font-face` descriptors.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FaceDescriptors {
    /// Width ratios (1.0 is normal).
    pub width: Descriptor<[f32; 2]>,
    pub weight: Descriptor<[f32; 2]>,
    pub style: Descriptor<StyleRange>,
}

impl FontInfo {
    /// Creates a new font object from the given source and index.
    pub fn from_source(source: SourceInfo, index: u32) -> Option<Self> {
        match &source.kind {
            #[cfg(feature = "std")]
            SourceKind::Path(path) => {
                let file = std::fs::File::open(&**path).ok()?;
                let mapped = unsafe { memmap2::Mmap::map(&file).ok()? };
                let font = FontRef::from_index(&mapped, index).ok()?;
                Self::from_font_ref(&font, source.clone(), index)
            }
            SourceKind::Memory(memory) => {
                let font = FontRef::from_index(memory.as_ref(), index).ok()?;
                Self::from_font_ref(&font, source.clone(), index)
            }
        }
    }

    /// Returns an object describing how to locate the data containing this
    /// font.
    pub fn source(&self) -> &SourceInfo {
        &self.source
    }

    /// Returns the index of the font in a collection.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Attempts to load the font, optionally from a source cache.
    pub fn load(&self, source_cache: Option<&mut SourceCache>) -> Option<Blob<u8>> {
        if let Some(source_cache) = source_cache {
            source_cache.get(&self.source)
        } else {
            match &self.source.kind {
                SourceKind::Memory(blob) => Some(blob.clone()),
                #[cfg(feature = "std")]
                SourceKind::Path(path) => super::source_cache::load_blob(path),
            }
        }
    }

    /// Returns the visual width of the font-- a relative change from the normal
    /// aspect ratio, typically in the range `0.5` to `2.0`.
    pub fn width(&self) -> FontWidth {
        self.width
    }

    /// Returns the visual style or 'slope' of the font.
    pub fn style(&self) -> FontStyle {
        self.style
    }

    /// Returns the visual weight class of the font, typically on a scale
    /// from `1.0` to `1000.0`.
    pub fn weight(&self) -> FontWeight {
        self.weight
    }

    /// Returns synthesis suggestions for this font with the given attributes.
    ///
    /// TRust: CSS Fonts 4 #feature-variation-precedence step 2 applies the
    /// values font matching settled on through the `wdth`, `wght`, `ital`
    /// and `slnt` axes, clamped to an `@font-face` face's descriptor ranges
    /// (the shaper clamps them to the axes). Synthetic bold and oblique are
    /// suggested only where the descriptors, or the font data without them,
    /// imply the face lacks the requested style (#font-prop-desc).
    pub fn synthesis(&self, width: FontWidth, style: FontStyle, weight: FontWeight) -> Synthesis {
        let mut synth = Synthesis::default();
        let mut len = 0_usize;
        let descriptors = self.descriptors.unwrap_or_default();
        let mut push = |synth: &mut Synthesis, tag: &[u8; 4], value: f32| {
            synth.vars[len] = (Tag::new(tag), value);
            len += 1;
        };
        if let Some(axis) = self.axis(b"wdth") {
            let mut target = width.percentage();
            if let Descriptor::Value([lo, hi]) = descriptors.width {
                target = target.clamp(lo * 100.0, hi * 100.0);
            }
            if target != axis.default {
                push(&mut synth, b"wdth", target);
            }
        }
        if let Some(axis) = self.axis(b"wght") {
            let mut target = weight.value();
            if let Descriptor::Value([lo, hi]) = descriptors.weight {
                target = target.clamp(lo, hi);
            }
            if target != axis.default {
                push(&mut synth, b"wght", target);
            }
        } else if weight.value() >= 600.0 && self.weight_range()[1] < 600.0 {
            // TRust: CSS Fonts 4 #font-weight-prop synthesizes bold faces
            // "for families that lack actual bold faces". Font matching
            // has already picked the nearest face, so only a bold request
            // (600 and up, as in Blink and Gecko) emboldens a non-bold
            // one; a medium (500) request draws the regular face.
            synth.embolden = true;
        }
        let range = self.style_range();
        let clamp_style = matches!(descriptors.style, Descriptor::Value(_));
        let has_italic = |r: StyleRange| r.italic.is_some_and(|[_, hi]| hi > 0.0);
        let has_oblique = |r: StyleRange| r.oblique.is_some_and(|[lo, hi]| lo != 0.0 || hi != 0.0);
        match style {
            FontStyle::Normal => {}
            FontStyle::Italic => {
                if let Some(axis) = self.axis(b"ital") {
                    let mut target = 1.0_f32;
                    if let (true, Some([lo, hi])) = (clamp_style, range.italic) {
                        target = target.clamp(lo, hi);
                    }
                    if target != axis.default {
                        push(&mut synth, b"ital", target);
                    }
                } else if range.italic.is_some_and(|[lo, hi]| lo <= 1.0 && 1.0 <= hi) {
                    // An italic face.
                } else if let (Some(axis), Some(oblique)) = (self.axis(b"slnt"), range.oblique) {
                    // #font-style-matching, italic step 4: oblique angles at
                    // or above 11deg ascending, then below 11deg descending.
                    let angle = italic_oblique_angle(oblique);
                    if -angle != axis.default {
                        push(&mut synth, b"slnt", -angle);
                    }
                } else if !has_italic(range) && !has_oblique(range) {
                    synth.skew = crate::matching::DEFAULT_OBLIQUE_ANGLE;
                }
            }
            FontStyle::Oblique(angle) => {
                let angle = angle.unwrap_or(crate::matching::DEFAULT_OBLIQUE_ANGLE);
                if let Some(axis) = self.axis(b"slnt") {
                    let mut target = angle;
                    if let (true, Some([lo, hi])) = (clamp_style, range.oblique) {
                        target = target.clamp(lo, hi);
                    }
                    // OpenType `slnt` is counter-clockwise; CSS clockwise.
                    if -target != axis.default {
                        push(&mut synth, b"slnt", -target);
                    }
                } else if range.oblique.is_some_and(|[lo, hi]| lo <= angle && angle <= hi)
                    || has_oblique(range)
                {
                    // A face of the requested (or the nearest) angle.
                } else if range.oblique.is_some() {
                    // An upright face: "a fallback match is produced by
                    // geometric shearing to the specified oblique value". The
                    // `ital` axis is not used to satisfy an oblique request.
                    if angle != 0.0 {
                        synth.skew = angle;
                    }
                } else if let Some(axis) = self.axis(b"ital") {
                    // Only italic faces remain (#font-style-matching step 4).
                    if axis.default != 1.0 {
                        push(&mut synth, b"ital", 1.0);
                    }
                }
            }
        }
        synth.len = len as u8;
        synth
    }

    /// TRust: the width ratios this face supports for font matching.
    pub fn width_range(&self) -> [f32; 2] {
        match self.descriptors.map(|d| d.width) {
            Some(Descriptor::Value(range)) => range,
            Some(Descriptor::Auto) => [1.0, 1.0],
            _ => match self.axis(b"wdth") {
                Some(axis) => [axis.min / 100.0, axis.max / 100.0],
                None => [self.width.ratio(); 2],
            },
        }
    }

    /// TRust: the weights this face supports for font matching.
    pub fn weight_range(&self) -> [f32; 2] {
        match self.descriptors.map(|d| d.weight) {
            Some(Descriptor::Value(range)) => range,
            Some(Descriptor::Auto) => [400.0, 400.0],
            _ => match self.axis(b"wght") {
                Some(axis) => [axis.min, axis.max],
                None => [self.weight.value(); 2],
            },
        }
    }

    /// TRust: the italic values and oblique angles this face supports for
    /// font matching.
    pub fn style_range(&self) -> StyleRange {
        match self.descriptors.map(|d| d.style) {
            Some(Descriptor::Value(range)) => return range,
            Some(Descriptor::Auto) => {
                return StyleRange {
                    italic: None,
                    oblique: Some([0.0, 0.0]),
                };
            }
            _ => {}
        }
        let mut range = match self.style {
            FontStyle::Normal => StyleRange {
                italic: None,
                oblique: Some([0.0, 0.0]),
            },
            FontStyle::Italic => StyleRange {
                italic: Some([1.0, 1.0]),
                oblique: None,
            },
            FontStyle::Oblique(angle) => {
                let angle = angle.unwrap_or(crate::matching::DEFAULT_OBLIQUE_ANGLE);
                StyleRange {
                    italic: None,
                    oblique: Some([angle, angle]),
                }
            }
        };
        if let Some(axis) = self.axis(b"ital") {
            range.italic = Some([axis.min, axis.max]);
            // The axis's upright instances are normal faces (oblique 0deg).
            if axis.min <= 0.0 && range.oblique.is_none() {
                range.oblique = Some([0.0, 0.0]);
            }
        }
        if let Some(axis) = self.axis(b"slnt") {
            range.oblique = Some([-axis.max, -axis.min]);
        }
        range
    }

    fn axis(&self, tag: &[u8; 4]) -> Option<&AxisInfo> {
        let bit = match tag {
            b"wght" => WEIGHT_AXIS,
            b"wdth" => WIDTH_AXIS,
            b"slnt" => SLANT_AXIS,
            b"ital" => ITALIC_AXIS,
            b"opsz" => OPTICAL_SIZE_AXIS,
            _ => 0,
        };
        if bit != 0 && self.attr_axes & bit == 0 {
            return None;
        }
        self.axes.iter().find(|axis| axis.tag == Tag::new(tag))
    }

    /// Returns the variation [axes] for the font.
    ///
    /// [axes]: crate::AxisInfo
    pub fn axes(&self) -> &[AxisInfo] {
        &self.axes
    }

    /// Returns `true` if the font has a `wght` [axis].
    ///
    /// This is a quicker check than scanning the [axes].
    ///
    /// [axes]: Self::axes
    /// [axis]: crate::AxisInfo
    pub fn has_weight_axis(&self) -> bool {
        self.attr_axes & WEIGHT_AXIS != 0
    }

    /// Returns `true` if the font has a `wdth` [axis].
    ///
    /// This is a quicker check than scanning the [axes].
    ///
    /// [axes]: Self::axes
    /// [axis]: crate::AxisInfo
    pub fn has_width_axis(&self) -> bool {
        self.attr_axes & WIDTH_AXIS != 0
    }

    /// Returns `true` if the font has a `slnt` [axis].
    ///
    /// This is a quicker check than scanning the [axes].
    ///
    /// [axes]: Self::axes
    /// [axis]: crate::AxisInfo
    pub fn has_slant_axis(&self) -> bool {
        self.attr_axes & SLANT_AXIS != 0
    }

    /// Returns `true` if the font has an `ital` [axis].
    ///
    /// This is a quicker check than scanning the [axes].
    ///
    /// [axes]: Self::axes
    /// [axis]: crate::AxisInfo
    pub fn has_italic_axis(&self) -> bool {
        self.attr_axes & ITALIC_AXIS != 0
    }

    /// Returns `true` if the font as an `opsz` [axis].
    ///
    /// This is a quicker check than scanning the [axes].
    ///
    /// [axes]: Self::axes
    /// [axis]: crate::AxisInfo
    pub fn has_optical_size_axis(&self) -> bool {
        self.attr_axes & OPTICAL_SIZE_AXIS != 0
    }

    /// Returns the index used for constructing a [Charmap] for this font.
    ///
    /// [Charmap]: crate::Charmap
    pub fn charmap_index(&self) -> CharmapIndex {
        self.charmap_index
    }

    /// Returns whether the font declares a fixed-pitch design.
    pub fn is_monospaced(&self) -> bool {
        self.monospaced
    }

    /// Returns how many characters from Fontique's representative sample for
    /// `script` are present in this font.
    ///
    /// This compact metadata is collected while the font is already mapped
    /// for discovery. Consumers can build accurate fallback lists without
    /// reopening every font file and retaining or copying its bytes.
    pub fn script_coverage(&self, script: Script) -> u8 {
        self.script_coverage
            .iter()
            .find_map(|(candidate, score)| (*candidate == script).then_some(*score))
            .unwrap_or(0)
    }
}

impl FontInfo {
    pub(crate) fn from_font_ref(
        font: &FontRef<'_>,
        source: SourceInfo,
        index: u32,
    ) -> Option<Self> {
        // It's probably not useful to retain fonts that don't have
        // a valid cmap so just bail here if we fail.
        let charmap_index = CharmapIndex::new(font)?;
        let charmap = charmap_index.charmap(font.data().as_bytes())?;
        let mut script_coverage = ScriptCoverageVec::new();
        for (script, sample) in Script::all_samples() {
            let score = sample
                .chars()
                .filter(|ch| charmap.map(*ch).is_some())
                .count()
                .min(u8::MAX as usize) as u8;
            if score != 0 {
                script_coverage.push((*script, score));
            }
        }
        let monospaced = font.post().is_ok_and(|post| post.is_fixed_pitch() != 0);
        let (width, style, weight) = read_attributes(font);
        let (axes, attr_axes) = if let Ok(fvar_axes) = font.fvar().and_then(|fvar| fvar.axes()) {
            let mut axes = SmallVec::<[AxisInfo; 1]>::with_capacity(fvar_axes.len());
            let mut attrs_axes = 0_u8;
            for fvar_axis in fvar_axes {
                let axis = AxisInfo {
                    tag: fvar_axis.axis_tag(),
                    min: fvar_axis.min_value().to_f32(),
                    max: fvar_axis.max_value().to_f32(),
                    default: fvar_axis.default_value().to_f32(),
                };
                axes.push(axis);
                match &axis.tag.to_be_bytes() {
                    b"wght" => attrs_axes |= WEIGHT_AXIS,
                    b"wdth" => attrs_axes |= WIDTH_AXIS,
                    b"slnt" => attrs_axes |= SLANT_AXIS,
                    b"ital" => attrs_axes |= ITALIC_AXIS,
                    b"opsz" => attrs_axes |= OPTICAL_SIZE_AXIS,
                    _ => {}
                }
            }
            (axes, attrs_axes)
        } else {
            (SmallVec::default(), 0)
        };
        Some(Self {
            source,
            index,
            width,
            style,
            weight,
            axes,
            attr_axes,
            charmap_index,
            monospaced,
            script_coverage,
            descriptors: None,
        })
    }

    #[allow(unused)]
    pub(crate) fn maybe_override_attributes(
        &mut self,
        width: FontWidth,
        style: FontStyle,
        weight: FontWeight,
    ) {
        if self.width == FontWidth::default() {
            self.width = width;
        }
        if self.style == FontStyle::default() {
            self.style = style;
        }
        if self.weight == FontWeight::default() {
            self.weight = weight;
        }
    }

    pub(crate) fn apply_override(&mut self, info_override: &FontInfoOverride<'_>) {
        if info_override.descriptors != FaceDescriptors::default() {
            self.descriptors = Some(info_override.descriptors);
        }
        if let Some(width) = info_override.width {
            self.width = width;
        }
        if let Some(style) = info_override.style {
            self.style = style;
        }
        if let Some(weight) = info_override.weight {
            self.weight = weight;
        }
        if let Some(axes) = info_override.axes {
            // This is O(n^2) but no font should have enough axes for that to
            // matter (read-fonts does the same thing)
            for (tag, value) in axes {
                if let Some(axis) = self.axes.iter_mut().find(|axis| axis.tag == *tag) {
                    axis.default = *value;
                }
            }
        }
    }
}

/// TRust: the oblique angle an `italic` request applies to a face with a
/// `slnt` axis supporting `range` (CSS Fonts 4 #font-style-matching, italic
/// step 4): the smallest angle at or above 11deg, else the largest positive
/// one, else the largest non-positive one.
fn italic_oblique_angle([lo, hi]: [f32; 2]) -> f32 {
    if hi >= 11.0 {
        lo.max(11.0)
    } else if hi > 0.0 {
        hi
    } else {
        hi.min(0.0)
    }
}

const WEIGHT_AXIS: u8 = 0x01;
const WIDTH_AXIS: u8 = 0x02;
const SLANT_AXIS: u8 = 0x04;
const ITALIC_AXIS: u8 = 0x08;
const OPTICAL_SIZE_AXIS: u8 = 0x10;

/// An axis of variation for a variable font.
///
/// Instances of this can be obtained from [`FontInfo::axes`].
///
/// These give the [`Tag`] and range of valid values for a given font
/// variation. In `parley`, these values are used to create a
/// `FontVariation`.
///
/// OpenType defines some common axes:
///
/// * [Italic](https://fonts.google.com/knowledge/glossary/italic_axis) or `ital`
/// * [Optical Size](https://fonts.google.com/knowledge/glossary/optical_size_axis) or `opsz`
/// * [Slant](https://fonts.google.com/knowledge/glossary/slant_axis) or `slnt`
/// * [FontWeight](https://fonts.google.com/knowledge/glossary/weight_axis) or `wght`
/// * [Width](https://fonts.google.com/knowledge/glossary/width_axis) or `wdth`
///
/// For a broader explanation of this, see
/// [Axis in Variable Fonts](https://fonts.google.com/knowledge/glossary/axis_in_variable_fonts)
/// from Google Fonts.
#[derive(Copy, Clone, Default, Debug)]
pub struct AxisInfo {
    /// The tag that identifies the axis.
    pub tag: Tag,
    /// The inclusive minimum value of the axis.
    pub min: f32,
    /// The inclusive maximum value of the axis.
    pub max: f32,
    /// The default value of the axis.
    pub default: f32,
}

/// Suggestions for synthesizing a set of font attributes for a given
/// font.
///
/// Instances of this can be obtained from [`FontInfo::synthesis`]
/// as well as [`QueryFont::synthesis`].
///
/// [`QueryFont::synthesis`]: crate::QueryFont::synthesis
#[derive(Copy, Clone, Default, PartialEq)]
pub struct Synthesis {
    vars: [(Tag, f32); 3],
    len: u8,
    embolden: bool,
    /// TRust: degrees, fractional as CSS oblique angles may be.
    skew: f32,
}

impl Synthesis {
    /// Returns `true` if any synthesis suggestions are available.
    pub fn any(&self) -> bool {
        self.len != 0 || self.embolden || self.skew != 0.0
    }

    /// Returns the variation settings that should be applied to match the
    /// requested attributes.
    ///
    /// When using `parley`, these can be used to create `FontVariation`
    /// settings.
    pub fn variation_settings(&self) -> &[(Tag, f32)] {
        &self.vars[..self.len as usize]
    }

    /// Returns `true` if the scaler should apply a faux bold.
    pub fn embolden(&self) -> bool {
        self.embolden
    }

    /// Returns a skew angle for faux italic/oblique, if requested.
    pub fn skew(&self) -> Option<f32> {
        if self.skew != 0.0 {
            Some(self.skew)
        } else {
            None
        }
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for Synthesis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Synthesis")
            .field("vars", &self.variation_settings())
            .field("embolden", &self.embolden)
            .field("skew", &self.skew)
            .finish()
    }
}

fn read_attributes(font: &FontRef<'_>) -> (FontWidth, FontStyle, FontWeight) {
    use read_fonts::{
        TableProvider,
        tables::{
            head::{Head, MacStyle},
            os2::{Os2, SelectionFlags},
            post::Post,
        },
    };

    fn width_from_width_class(width_class: u16) -> FontWidth {
        FontWidth::from_ratio(match width_class {
            0..=1 => 0.5,
            2 => 0.625,
            3 => 0.75,
            4 => 0.875,
            5 => 1.0,
            6 => 1.125,
            7 => 1.25,
            8 => 1.5,
            _ => 2.0,
        })
    }

    fn from_os2_post(os2: Os2<'_>, post: Option<Post<'_>>) -> (FontWidth, FontStyle, FontWeight) {
        let width = width_from_width_class(os2.us_width_class());
        // Bits 1 and 9 of the fsSelection field signify italic and
        // oblique, respectively.
        // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/os2#fsselection>
        let fs_selection = os2.fs_selection();
        let style = if fs_selection.contains(SelectionFlags::ITALIC) {
            FontStyle::Italic
        } else if fs_selection.contains(SelectionFlags::OBLIQUE) {
            // TRust: `post.italicAngle` counts counter-clockwise degrees
            // (negative for a forward lean); CSS oblique angles count
            // clockwise (CSS Fonts 4 #font-style-prop).
            let angle = post
                .map(|post| -(post.italic_angle().to_f64() as f32))
                .filter(|angle| *angle != 0.0);
            FontStyle::Oblique(angle)
        } else {
            FontStyle::Normal
        };
        // The usFontWeightClass field is specified with a 1-1000 range, but
        // we don't clamp here because variable fonts could potentially
        // have a value outside of that range.
        // See <https://learn.microsoft.com/en-us/typography/opentype/spec/os2#usweightclass>
        let weight = FontWeight::new(os2.us_weight_class() as f32);
        (width, style, weight)
    }

    fn from_head(head: Head<'_>) -> (FontWidth, FontStyle, FontWeight) {
        let mac_style = head.mac_style();
        let style = if mac_style.contains(MacStyle::ITALIC) {
            FontStyle::Italic
        } else {
            FontStyle::default()
        };
        let weight = if mac_style.contains(MacStyle::BOLD) {
            FontWeight::BOLD
        } else {
            FontWeight::default()
        };
        (FontWidth::default(), style, weight)
    }

    if let Ok(os2) = font.os2() {
        // Prefer values from the OS/2 table if it exists. We also use
        // the post table to extract the angle for oblique styles.
        from_os2_post(os2, font.post().ok())
    } else if let Ok(head) = font.head() {
        // Otherwise, fall back to the macStyle field of the head table.
        from_head(head)
    } else {
        (
            FontWidth::default(),
            FontStyle::Normal,
            FontWeight::default(),
        )
    }
}

/// Helper for specifying aspects of a font's metadata to be overridden when the
/// font is registered. Helpful when implementing a `@font-face`-like API, which
/// allows those defining the fonts to specify certain font properties manually.
#[derive(Copy, Clone, Debug, Default)]
pub struct FontInfoOverride<'a> {
    /// Font family name to be used instead of the one specified in the font
    /// itself.
    pub family_name: Option<&'a str>,
    /// Font width to be used instead of the one specified in the font itself.
    pub width: Option<FontWidth>,
    /// Font's visual style / "slope" to be used instead of the one specified in
    /// the font itself.
    pub style: Option<FontStyle>,
    /// Font weight to be used instead of the one specified in the font itself.
    pub weight: Option<FontWeight>,
    /// Default values for the font's variation axes. Axes not included within
    /// the font will be ignored.
    pub axes: Option<&'a [(Tag, f32)]>,
    /// TRust: CSS `@font-face` font-width, font-weight and font-style
    /// descriptors, used for matching and synthesis in place of the font's
    /// own attributes.
    pub descriptors: FaceDescriptors,
}
