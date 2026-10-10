//! TRust-owned font discovery, shaping, and retained glyph-run boundary.
//!
//! Parley performs Unicode bidi resolution, script and cluster analysis, font
//! matching/fallback, OpenType shaping, and typographic metric extraction.
//! CSS layout consumes the small types in this module rather than Parley's
//! evolving layout objects; render backends receive retained glyph ids and
//! positions and therefore never reshape a line during paint.
//!
//! CSS Fonts 4 §2.1/§5 requires ordered family matching followed by
//! character/cluster fallback. CSS Inline 3 §3.2 defines ascent, descent, and
//! leading as font metrics used by line layout. TRust's process-wide pure-Rust
//! catalog and Parley's shaped runs implement those two pieces of machinery.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;

use parley::{
    FontContext, FontFamily, FontStyle, FontWeight, Language, Layout, LayoutContext, LineHeight,
    OverflowWrap, PositionedLayoutItem, StyleProperty, TextWrapMode, WordBreak,
    editing::PlainEditor,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::core::{ImeAction, Key, KeyInput, KeyState};

mod cells;
mod layout_cache;
mod outline;
#[cfg(test)]
pub(crate) mod test_fonts;
pub mod typography;
pub use crate::font_system::FontSet;
pub use cells::CellMetrics;
pub(crate) use outline::{append_glyph_path, append_text_path, x_height};
pub use typography::FontTypography;

/// CSS-facing text style. No Parley, Glifo, or renderer type escapes this
/// boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub family: String,
    pub font_set: Option<Arc<FontSet>>,
    /// Inherited BCP 47 language from HTML `lang`/`xml:lang`. Font fallback
    /// uses this to distinguish locale-specific glyph conventions.
    pub language: Option<String>,
    pub size: f32,
    pub weight: f32,
    pub italic: bool,
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub line_height: CssLineHeight,
    pub underline: bool,
    pub strikethrough: bool,
    /// The element's other CSS Fonts 4 properties; `None` when all have
    /// their initial values.
    pub typography: Option<Arc<FontTypography>>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            family: String::from("sans-serif"),
            font_set: None,
            language: None,
            size: 16.0,
            weight: 400.0,
            italic: false,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            line_height: CssLineHeight::Normal,
            underline: false,
            strikethrough: false,
            typography: None,
        }
    }
}

impl TextStyle {
    /// #font-style-prop as Parley's style: the oblique angle, else italic.
    fn font_style(&self) -> FontStyle {
        match self.typography.as_ref().and_then(|t| t.oblique) {
            Some(angle) => FontStyle::Oblique(Some(angle)),
            None if self.italic => FontStyle::Italic,
            None => FontStyle::Normal,
        }
    }

    /// #font-width-prop as a ratio of the normal width.
    fn font_width(&self) -> f32 {
        self.typography.as_ref().map_or(1.0, |t| t.width)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum CssLineHeight {
    #[default]
    Normal,
    Number(f32),
    Length(f32),
}

/// CSS line-breaking controls, kept independent of Parley's public enums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextBreakStyle {
    pub wrap: bool,
    pub word_break: TextWordBreak,
    pub overflow_wrap: TextOverflowWrap,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextWordBreak {
    #[default]
    Normal,
    BreakAll,
    KeepAll,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextOverflowWrap {
    #[default]
    Normal,
    Anywhere,
    BreakWord,
}

/// Renderer-neutral reference to immutable font bytes. The inner resource is
/// intentionally private; the Vello adapter can borrow it inside the crate,
/// while browser/layout public APIs do not expose Parley or Vello types.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFace(parley::FontData);

impl FontFace {
    #[cfg(test)]
    pub(crate) fn from_test_data(data: parley::FontData) -> Self {
        Self(data)
    }

    pub(crate) fn data(&self) -> &parley::FontData {
        &self.0
    }

    pub fn collection_index(&self) -> u32 {
        self.0.index
    }

    pub fn resource_id(&self) -> u64 {
        self.0.data.id()
    }
}

/// One positioned glyph in CSS pixels, relative to its text piece's origin.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShapedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
}

/// A run sharing one font face, size, variation location, direction, and
/// synthetic-style requirements.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    pub font: FontFace,
    pub font_size: f32,
    /// Optional desktop text color; glyph geometry and searchable text stay intact.
    pub color: Option<[u8; 3]>,
    pub normalized_coords: Vec<i16>,
    pub glyphs: Vec<ShapedGlyph>,
    pub text_range: Range<usize>,
    pub rtl: bool,
    pub synth_bold: bool,
    pub synth_skew_degrees: Option<f32>,
}

impl ShapedRun {
    /// The x-skew factor of a synthetic oblique in a renderer's Y-down glyph
    /// space (Glifo applies the glyph transform after flipping font units):
    /// negative, so the tops of glyphs lean forward.
    pub fn synthetic_oblique_skew(&self) -> Option<f64> {
        self.synth_skew_degrees
            .map(|degrees| -f64::from(degrees).to_radians().tan())
    }
}

/// Logical byte/cluster mapping retained for selection and hit testing.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    pub text_range: Range<usize>,
    pub x: f32,
    pub advance: f32,
    pub rtl: bool,
}

/// A shaped single-line text piece. `baseline` is measured from its top.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShapedText {
    pub text: String,
    pub advance: f32,
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
    pub line_height: f32,
    pub baseline: f32,
    pub underline: bool,
    pub strikethrough: bool,
    pub runs: Vec<ShapedRun>,
    pub clusters: Vec<Cluster>,
}

impl ShapedText {
    /// CSS Overflow 3 #text-overflow: the visual position, from this text's
    /// origin, where content hidden to make room for an ellipsis begins.
    /// Characters (grapheme clusters, the property's unit) that end after
    /// `limit` are hidden; because clusters tile the run in visual order,
    /// they form a visual suffix starting at the returned position. With
    /// `keep_first`, the visually first character survives regardless: the
    /// first character on a line "must be clipped rather than ellipsed".
    /// `None` when no character remains before the cut.
    pub(crate) fn visual_elision_cut(&self, limit: f32, keep_first: bool) -> Option<f32> {
        const EPSILON: f32 = 0.01;
        let mut order: Vec<&Cluster> = self.clusters.iter().collect();
        order.sort_by(|a, b| a.x.total_cmp(&b.x));
        let first = order.first()?;
        let cut = order
            .iter()
            .find(|cluster| cluster.x + cluster.advance > limit + EPSILON)
            .map_or(self.advance.max(limit), |cluster| cluster.x);
        let cut = if keep_first {
            cut.max(first.x + first.advance)
        } else {
            cut
        };
        order
            .iter()
            .any(|cluster| cluster.x < cut - EPSILON)
            .then_some(cut)
    }

    /// The mirror of [`ShapedText::visual_elision_cut`] for an end edge at
    /// the left (CSS Overflow 3 #text-overflow in a right-to-left line): the
    /// visual position from which every cluster to its right ends no
    /// further left than `limit`, or `None` when no cluster would remain.
    /// `keep_last` keeps the rightmost cluster (the line's first character
    /// is clipped rather than ellipsed).
    pub(crate) fn visual_elision_cut_start(&self, limit: f32, keep_last: bool) -> Option<f32> {
        const EPSILON: f32 = 0.01;
        let mut order: Vec<&Cluster> = self.clusters.iter().collect();
        order.sort_by(|a, b| (b.x + b.advance).total_cmp(&(a.x + a.advance)));
        let last = order.first()?;
        let cut = order
            .iter()
            .find(|cluster| cluster.x < limit - EPSILON)
            .map_or(limit.min(0.0), |cluster| cluster.x + cluster.advance);
        let cut = if keep_last { cut.min(last.x) } else { cut };
        order
            .iter()
            .any(|cluster| cluster.x + cluster.advance > cut + EPSILON)
            .then_some(cut)
    }

    /// The part of this text painted before `cut` (from
    /// [`ShapedText::visual_elision_cut`]): its clusters and glyphs at their
    /// original visual positions, so bidi reordering and shaping are
    /// unchanged. The text and cluster ranges keep only the surviving
    /// characters, in logical order.
    pub(crate) fn visual_prefix(&self, cut: f32) -> ShapedText {
        const EPSILON: f32 = 0.01;
        self.visual_part(|x| x < cut - EPSILON, |center| center < cut - EPSILON, 0.0)
    }

    /// The part of this text painted from `cut` on (from
    /// [`ShapedText::visual_elision_cut_start`]), moved so that `cut` is its
    /// origin; otherwise as [`ShapedText::visual_prefix`].
    pub(crate) fn visual_suffix(&self, cut: f32) -> ShapedText {
        const EPSILON: f32 = 0.01;
        self.visual_part(
            |x| x >= cut - EPSILON,
            |center| center >= cut - EPSILON,
            cut,
        )
    }

    /// The whole clusters painted in `[from, to)` from this text's origin,
    /// moved so that `from` is the part's origin: a piece of a run that a
    /// selection's highlight redraws in its own color (CSS Pseudo 4
    /// #highlight-text), with decorations spanning only that piece.
    pub(crate) fn visual_range(&self, from: f32, to: f32) -> ShapedText {
        const EPSILON: f32 = 0.01;
        self.visual_part(
            |x| x >= from - EPSILON && x < to - EPSILON,
            |center| center >= from - EPSILON && center < to - EPSILON,
            from,
        )
    }

    fn visual_part(
        &self,
        keep_cluster: impl Fn(f32) -> bool,
        keep_glyph: impl Fn(f32) -> bool,
        origin: f32,
    ) -> ShapedText {
        let mut kept: Vec<&Cluster> = self
            .clusters
            .iter()
            .filter(|cluster| keep_cluster(cluster.x))
            .collect();
        kept.sort_by_key(|cluster| cluster.text_range.start);
        let mut text = String::new();
        // (old start, old end, new start) for each surviving cluster.
        let mut spans = Vec::with_capacity(kept.len());
        let mut clusters = Vec::with_capacity(kept.len());
        for cluster in &kept {
            let start = text.len();
            text.push_str(&self.text[cluster.text_range.clone()]);
            spans.push((cluster.text_range.start, cluster.text_range.end, start));
            clusters.push(Cluster {
                text_range: start..text.len(),
                x: cluster.x - origin,
                ..(*cluster).clone()
            });
        }
        // A logical offset maps to the surviving text that precedes it.
        let map = |offset: usize| {
            spans
                .iter()
                .filter(|(start, _, _)| *start < offset)
                .map(|(start, end, new)| new + (offset.min(*end) - start))
                .max()
                .unwrap_or(0)
        };
        let runs = self
            .runs
            .iter()
            .filter_map(|run| {
                let glyphs: Vec<ShapedGlyph> = run
                    .glyphs
                    .iter()
                    .filter(|glyph| keep_glyph(glyph.x + glyph.advance.max(0.0) * 0.5))
                    .map(|glyph| ShapedGlyph {
                        x: glyph.x - origin,
                        ..*glyph
                    })
                    .collect();
                (!glyphs.is_empty()).then(|| ShapedRun {
                    font: run.font.clone(),
                    font_size: run.font_size,
                    color: run.color,
                    normalized_coords: run.normalized_coords.clone(),
                    glyphs,
                    text_range: map(run.text_range.start)..map(run.text_range.end),
                    rtl: run.rtl,
                    synth_bold: run.synth_bold,
                    synth_skew_degrees: run.synth_skew_degrees,
                })
            })
            .collect();
        let advance = clusters
            .iter()
            .map(|cluster| cluster.x + cluster.advance)
            .fold(0.0f32, f32::max);
        ShapedText {
            text,
            advance,
            ascent: self.ascent,
            descent: self.descent,
            leading: self.leading,
            line_height: self.line_height,
            baseline: self.baseline,
            underline: self.underline,
            strikethrough: self.strikethrough,
            runs,
            clusters,
        }
    }

    /// This cell-font text (logical clusters, no glyph runs) laid out right
    /// to left: each cluster's position mirrored within the advance, listed
    /// in visual order.
    fn mirrored_cells(mut self) -> ShapedText {
        let advance = self.advance;
        for cluster in &mut self.clusters {
            cluster.x = advance - cluster.x - cluster.advance;
            cluster.rtl = true;
        }
        self.clusters.reverse();
        self
    }

    /// This text, shaped behind a leading `len`-byte directional control,
    /// without that control: `text` with its clusters and runs. The control
    /// has no advance; its zero-width glyph is dropped with it.
    fn without_leading_control(mut self, len: usize, text: &str) -> ShapedText {
        self.clusters
            .retain(|cluster| cluster.text_range.start >= len);
        for cluster in &mut self.clusters {
            cluster.text_range = cluster.text_range.start - len..cluster.text_range.end - len;
        }
        self.runs.retain(|run| run.text_range.end > len);
        for run in &mut self.runs {
            if run.text_range.start < len {
                // The control's glyph is the run's logically first one: the
                // visually last of a right-to-left run.
                let control = if run.rtl {
                    run.glyphs.len().checked_sub(1)
                } else {
                    (!run.glyphs.is_empty()).then_some(0)
                };
                if let Some(index) = control.filter(|&index| run.glyphs[index].advance == 0.0) {
                    run.glyphs.remove(index);
                }
            }
            run.text_range = run.text_range.start.saturating_sub(len)..run.text_range.end - len;
        }
        self.text = text.to_string();
        self
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let ShapedText {
            text,
            advance,
            ascent,
            descent,
            leading,
            line_height,
            baseline,
            underline,
            strikethrough,
            runs,
            clusters,
        } = self;
        let _ = (
            advance,
            ascent,
            descent,
            leading,
            line_height,
            baseline,
            underline,
            strikethrough,
        );
        let mut bytes = text
            .capacity()
            .saturating_add(runs.capacity() * std::mem::size_of::<ShapedRun>())
            .saturating_add(clusters.capacity() * std::mem::size_of::<Cluster>());
        for run in runs {
            // FontFace data belongs to the process font catalog and is shared across pages.
            let _ = &run.font;
            bytes = bytes
                .saturating_add(run.normalized_coords.capacity() * std::mem::size_of::<i16>())
                .saturating_add(run.glyphs.capacity() * std::mem::size_of::<ShapedGlyph>());
        }
        bytes
    }
}

/// Geometry exposed by the frontend-neutral editor in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EditorRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Glyphs and editing geometry from the same single-line Parley layout.
/// Native input fields must paint these glyphs, rather than reshape their
/// value with an independently chosen font or whitespace policy.
#[derive(Clone, Debug)]
pub struct EditorLine {
    pub shaped: ShapedText,
    pub selection: Vec<EditorRect>,
    pub caret: Option<EditorRect>,
    pub ime: EditorRect,
    pub underlines: Vec<EditorRect>,
}

/// A TRust-owned Unicode text editor backed by Parley's cluster, bidi and
/// line-layout machinery. Native frontends never manipulate byte indices or
/// guess grapheme boundaries themselves. This is used by browser chrome and
/// HTML text controls; password masking is a paint concern, while the logical
/// value remains available for form submission.
#[derive(Clone)]
pub struct TextEditor {
    editor: PlainEditor<()>,
    font_set: Option<Arc<FontSet>>,
    multiline: bool,
    line_cache: Option<(parley::editing::Generation, bool, Arc<EditorLine>)>,
}

impl std::fmt::Debug for TextEditor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextEditor")
            .field("text", &self.text())
            .field("selection", &self.selection())
            .field("multiline", &self.multiline)
            .finish()
    }
}

impl TextEditor {
    pub fn new(text: &str, style: &TextStyle, width: f32, multiline: bool) -> Self {
        let mut editor = PlainEditor::new(style.size.max(1.0));
        editor.set_text(text);
        editor.set_width(multiline.then_some(width.max(1.0)));
        editor.set_quantize(false);
        let styles = editor.edit_styles();
        let family = font_family_source(&style.family).into_owned();
        styles.insert(StyleProperty::FontFamily(FontFamily::Source(family.into())));
        styles.insert(StyleProperty::Locale(text_language(style)));
        styles.insert(StyleProperty::FontSize(style.size.max(1.0)));
        styles.insert(StyleProperty::FontWeight(FontWeight::new(
            style.weight.clamp(1.0, 1000.0),
        )));
        styles.insert(StyleProperty::FontStyle(style.font_style()));
        if style.font_width() != 1.0 {
            styles.insert(StyleProperty::FontWidth(parley::FontWidth::from_ratio(
                style.font_width(),
            )));
        }
        // CSS Fonts 4 #feature-variation-precedence for editable text, which
        // is shaped as typed: no synthesized capitals or positions.
        let typography = style.typography.as_deref().cloned().unwrap_or_default();
        let features = typography.features(style.letter_spacing, &[], true);
        if !features.is_empty() {
            styles.insert(parley_features(&features));
        }
        let variations = typography.variations(style.size.max(1.0));
        if !variations.is_empty() {
            styles.insert(parley_variations(&variations));
        }
        styles.insert(StyleProperty::LetterSpacing(style.letter_spacing));
        styles.insert(StyleProperty::WordSpacing(style.word_spacing));
        styles.insert(StyleProperty::LineHeight(match style.line_height {
            CssLineHeight::Normal => LineHeight::default(),
            CssLineHeight::Number(number) => LineHeight::FontSizeRelative(number.max(0.0)),
            CssLineHeight::Length(px) => LineHeight::Absolute(px.max(0.0)),
        }));
        let mut this = Self {
            editor,
            font_set: style.font_set.clone(),
            multiline,
            line_cache: None,
        };
        this.move_to_end(false, false);
        this
    }

    pub fn text(&self) -> String {
        self.editor.text().into_iter().collect()
    }

    pub fn raw_text(&self) -> &str {
        self.editor.raw_text()
    }

    pub fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
        self.move_to_end(false, false);
    }

    pub fn set_width(&mut self, width: f32) {
        // Single-line fields scroll their presentation; setting an unused
        // width on every frame would needlessly invalidate Parley's layout.
        if self.multiline {
            self.editor.set_width(Some(width.max(1.0)));
        }
    }

    pub fn is_composing(&self) -> bool {
        self.editor.is_composing()
    }

    pub fn selection(&self) -> std::ops::Range<usize> {
        self.editor.raw_selection().text_range()
    }

    pub fn control_selection(&self) -> crate::doc::ControlSelection {
        let selection = self.editor.raw_selection();
        let anchor = selection.anchor().index();
        let focus = selection.focus().index();
        let units = |byte| self.editor.raw_text()[..byte].encode_utf16().count() as u32;
        crate::doc::ControlSelection {
            start: units(anchor.min(focus)),
            end: units(anchor.max(focus)),
            direction: if anchor > focus {
                -1
            } else if anchor < focus {
                1
            } else {
                0
            },
        }
    }

    pub fn set_control_selection(&mut self, selection: crate::doc::ControlSelection) {
        let byte = |units: u32| {
            let mut offset = 0;
            for (byte, ch) in self.editor.raw_text().char_indices() {
                if offset >= units {
                    return byte;
                }
                offset += ch.len_utf16() as u32;
            }
            self.editor.raw_text().len()
        };
        let (start, end) = (byte(selection.start), byte(selection.end));
        if selection.direction < 0 {
            self.select_byte_range(end, start);
        } else {
            self.select_byte_range(start, end);
        }
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.editor.selected_text()
    }

    pub fn select_all(&mut self) {
        self.drive(|driver| driver.select_all());
    }

    pub fn select_byte_range(&mut self, start: usize, end: usize) {
        self.drive(|driver| driver.select_byte_range(start, end));
    }

    pub fn replace_selection(&mut self, text: &str) {
        let filtered;
        let text = if self.multiline {
            text
        } else {
            filtered = text
                .chars()
                .filter(|character| !matches!(character, '\r' | '\n'))
                .collect::<String>();
            &filtered
        };
        self.drive(|driver| driver.insert_or_replace_selection(text));
    }

    pub fn delete_selection(&mut self) {
        self.drive(|driver| driver.delete_selection());
    }

    pub fn move_to_point(&mut self, x: f32, y: f32, extend: bool) {
        self.drive(|driver| {
            if extend {
                driver.extend_selection_to_point(x, y);
            } else {
                driver.move_to_point(x, y);
            }
        });
    }

    pub fn handle_ime(&mut self, action: &ImeAction) -> bool {
        match action {
            ImeAction::Preedit { text, cursor } if !text.is_empty() => {
                self.drive(|driver| driver.set_compose(text, *cursor));
                true
            }
            ImeAction::Preedit { .. } => {
                self.drive(|driver| driver.clear_compose());
                true
            }
            ImeAction::Commit(text) => {
                if self.editor.is_composing() {
                    // UI Events #event-type-compositionend: the committed
                    // string is the final result, which can differ from the
                    // last preedit (or be empty when composition is canceled).
                    self.drive(|driver| driver.clear_compose());
                }
                self.replace_selection(text);
                true
            }
            ImeAction::Disabled => {
                if self.editor.is_composing() {
                    self.drive(|driver| driver.clear_compose());
                    true
                } else {
                    false
                }
            }
            ImeAction::Enabled => false,
        }
    }

    /// Apply a platform-neutral editing key. Returns true when consumed.
    pub fn handle_key(&mut self, input: &KeyInput) -> bool {
        if input.state != KeyState::Pressed || input.composing {
            return false;
        }
        let extend = input.modifiers.shift;
        let word = input.modifiers.control || input.modifiers.alt;
        match &input.key {
            Key::Backspace => {
                if word {
                    self.drive(|driver| driver.backdelete_word());
                } else {
                    self.delete_grapheme_before();
                }
            }
            Key::Delete => {
                if word {
                    self.drive(|driver| driver.delete_word());
                } else {
                    self.delete_grapheme_after();
                }
            }
            Key::ArrowLeft => self.drive(|driver| match (extend, word) {
                (true, true) => driver.select_word_left(),
                (true, false) => driver.select_left(),
                (false, true) => driver.move_word_left(),
                (false, false) => driver.move_left(),
            }),
            Key::ArrowRight => self.drive(|driver| match (extend, word) {
                (true, true) => driver.select_word_right(),
                (true, false) => driver.select_right(),
                (false, true) => driver.move_word_right(),
                (false, false) => driver.move_right(),
            }),
            Key::ArrowUp if self.multiline => self.drive(|driver| {
                if extend {
                    driver.select_up();
                } else {
                    driver.move_up();
                }
            }),
            Key::ArrowDown if self.multiline => self.drive(|driver| {
                if extend {
                    driver.select_down();
                } else {
                    driver.move_down();
                }
            }),
            Key::Home => self.move_to_start(extend, word || input.modifiers.meta),
            Key::End => self.move_to_end(extend, word || input.modifiers.meta),
            Key::Enter if self.multiline => self.replace_selection("\n"),
            Key::Character(character)
                if (input.modifiers.control || input.modifiers.meta)
                    && character.eq_ignore_ascii_case("a") =>
            {
                self.select_all();
            }
            _ => return false,
        }
        true
    }

    pub fn geometry(&mut self) -> (Vec<EditorRect>, Option<EditorRect>, EditorRect) {
        TEXT.with_borrow_mut(|system| {
            system.select_fonts(self.font_set.as_ref());
            let mut driver = self.editor.driver(&mut system.fonts, &mut system.layouts);
            driver.refresh_layout();
            let selection = driver
                .editor
                .selection_geometry()
                .into_iter()
                .map(|(rect, _)| editor_rect(rect))
                .collect();
            let caret = driver.editor.cursor_geometry(1.5).map(editor_rect);
            let ime = editor_rect(driver.editor.ime_cursor_area());
            (selection, caret, ime)
        })
    }

    /// Retain the actual editor layout, including preedit and trailing spaces.
    /// Masked fields use one bullet per extended grapheme (UAX #29 §3); their
    /// selection and caret are measured against those bullets, never secrets.
    pub fn line_layout(&mut self, masked: bool) -> Arc<EditorLine> {
        debug_assert!(!self.multiline);
        self.drive(|driver| driver.refresh_layout());
        let generation = self.editor.generation();
        if let Some((cached_generation, cached_masked, line)) = &self.line_cache
            && *cached_generation == generation
            && *cached_masked == masked
        {
            return line.clone();
        }
        let line = if masked {
            let mut display = self.masked_editor();
            display.retain_editor_line()
        } else {
            self.retain_editor_line()
        };
        let line = Arc::new(line);
        self.line_cache = Some((generation, masked, line.clone()));
        line
    }

    /// Hit test the same display used by `line_layout`, including masking.
    pub fn move_to_line_point(&mut self, x: f32, y: f32, extend: bool, masked: bool) {
        if !masked {
            self.move_to_point(x, y, extend);
            return;
        }
        let mut display = self.masked_editor();
        display.move_to_point(x, y, false);
        let index = display.editor.raw_selection().focus().index() / '•'.len_utf8();
        let byte = self
            .raw_text()
            .grapheme_indices(true)
            .nth(index)
            .map_or(self.raw_text().len(), |(byte, _)| byte);
        self.drive(|driver| {
            if extend {
                driver.extend_selection_to_byte(byte);
            } else {
                driver.move_to_byte(byte);
            }
        });
    }

    fn masked_editor(&self) -> Self {
        let mut display = Self {
            editor: self.editor.clone(),
            font_set: self.font_set.clone(),
            multiline: false,
            line_cache: None,
        };
        let selection = self.editor.raw_selection();
        let mask_index = |index| {
            self.raw_text()
                .grapheme_indices(true)
                .take_while(|(byte, _)| *byte < index)
                .count()
                * '•'.len_utf8()
        };
        let anchor = mask_index(selection.anchor().index());
        let focus = mask_index(selection.focus().index());
        display
            .editor
            .set_text(&"•".repeat(self.raw_text().graphemes(true).count()));
        display.select_byte_range(anchor, focus);
        display
    }

    fn retain_editor_line(&mut self) -> EditorLine {
        TEXT.with_borrow_mut(|system| {
            system.select_fonts(self.font_set.as_ref());
            self.editor
                .refresh_layout(&mut system.fonts, &mut system.layouts);
            let layout = self.editor.try_layout().expect("refreshed editor layout");
            let mut underlines = Vec::new();
            if let Some(line) = layout.lines().next() {
                for item in line.items() {
                    let PositionedLayoutItem::GlyphRun(run) = item else {
                        continue;
                    };
                    if let Some(underline) = &run.style().underline {
                        let metrics = run.run().metrics();
                        underlines.push(EditorRect {
                            x: run.offset(),
                            y: line.metrics().baseline
                                - underline.offset.unwrap_or(metrics.underline_offset),
                            width: run.advance(),
                            height: underline.size.unwrap_or(metrics.underline_size).max(1.0),
                        });
                    }
                }
            }
            // Parley supplies an empty editor with a synthetic metrics run;
            // its byte range does not refer to the zero-length input string.
            let shaped = if self.editor.raw_text().is_empty() {
                let metrics = layout.lines().next().map(|line| *line.metrics());
                metrics.map_or_else(ShapedText::default, |metrics| ShapedText {
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    leading: metrics.leading,
                    line_height: metrics.line_height,
                    baseline: css_baseline(&metrics),
                    ..Default::default()
                })
            } else {
                retain_first_line(self.editor.raw_text(), layout)
            };
            EditorLine {
                shaped,
                selection: self
                    .editor
                    .selection_geometry()
                    .into_iter()
                    .map(|(rect, _)| editor_rect(rect))
                    .collect(),
                caret: self.editor.cursor_geometry(1.5).map(editor_rect),
                ime: editor_rect(self.editor.ime_cursor_area()),
                underlines,
            }
        })
    }

    fn move_to_start(&mut self, extend: bool, document: bool) {
        self.drive(|driver| match (extend, document) {
            (true, true) => driver.select_to_text_start(),
            (true, false) => driver.select_to_line_start(),
            (false, true) => driver.move_to_text_start(),
            (false, false) => driver.move_to_line_start(),
        });
    }

    fn delete_grapheme_before(&mut self) {
        let selection = self.selection();
        if !selection.is_empty() {
            self.delete_selection();
            return;
        }
        let cursor = selection.start.min(self.editor.raw_text().len());
        let Some((start, _)) = self.editor.raw_text()[..cursor]
            .grapheme_indices(true)
            .next_back()
        else {
            return;
        };
        if let Some(bytes) = NonZeroUsize::new(cursor - start) {
            self.drive(|driver| driver.delete_bytes_before_selection(bytes));
        }
    }

    fn delete_grapheme_after(&mut self) {
        let selection = self.selection();
        if !selection.is_empty() {
            self.delete_selection();
            return;
        }
        let cursor = selection.end.min(self.editor.raw_text().len());
        let Some(grapheme) = self.editor.raw_text()[cursor..].graphemes(true).next() else {
            return;
        };
        if let Some(bytes) = NonZeroUsize::new(grapheme.len()) {
            self.drive(|driver| driver.delete_bytes_after_selection(bytes));
        }
    }

    fn move_to_end(&mut self, extend: bool, document: bool) {
        self.drive(|driver| match (extend, document) {
            (true, true) => driver.select_to_text_end(),
            (true, false) => driver.select_to_line_end(),
            (false, true) => driver.move_to_text_end(),
            (false, false) => driver.move_to_line_end(),
        });
    }

    fn drive(&mut self, operation: impl FnOnce(&mut parley::editing::PlainEditorDriver<'_, ()>)) {
        TEXT.with_borrow_mut(|system| {
            system.select_fonts(self.font_set.as_ref());
            let mut driver = self.editor.driver(&mut system.fonts, &mut system.layouts);
            operation(&mut driver);
        });
    }
}

fn editor_rect(rect: parley::BoundingBox) -> EditorRect {
    EditorRect {
        x: rect.x0 as f32,
        y: rect.y0 as f32,
        width: (rect.x1 - rect.x0).max(0.0) as f32,
        height: (rect.y1 - rect.y0).max(0.0) as f32,
    }
}

struct TextSystem {
    fonts: FontContext,
    layouts: LayoutContext<()>,
    color_layouts: LayoutContext<Option<[u8; 3]>>,
    page_font_epoch: u64,
    active_font_set: u64,
    /// Contexts of recently active font sets other than `active_font_set`.
    /// Text from different tree scopes (a page and its frame's Document, or a
    /// shadow tree with its own `@font-face` rules) alternates between font
    /// sets; reactivating a parked context avoids cloning a whole collection
    /// on every switch. A set's collection never changes, and the installed
    /// one changes only with the page font epoch, which clears these.
    parked_fonts: Vec<(u64, FontContext)>,
    /// Shaping is pure for a fixed system-font collection and CSS text style.
    /// Inline layout asks for the same spaces, words, labels, and intrinsic
    /// probes many times; retaining those results avoids repeating font
    /// fallback, cmap lookup, bidi analysis, and OpenType shaping. FIFO is
    /// intentional here: it gives O(1) hits/inserts and a strict ownership
    /// bound without turning a stream of unique page text into LRU bookkeeping
    /// work. The cache dies with its thread-local page/layout worker.
    shape_cache: HashMap<ShapeKey, CachedShape>,
    shape_order: VecDeque<ShapeKey>,
    shape_cache_bytes: usize,
    layout_cache: layout_cache::Cache,
    /// Emoji-presentation family lists by (font set, CSS family list).
    emoji_families: HashMap<(u64, String), Arc<str>>,
    /// `font-size-adjust` metrics by (font resource, index, coordinates).
    face_metrics: HashMap<(u64, u32, Vec<i16>), FaceMetrics>,
    /// The terminal frontend's cell font, when this thread lays out for it
    /// (see `cells`). `None` keeps proportional shaping.
    cells: Option<CellMetrics>,
    #[cfg(test)]
    shaped_input_bytes: usize,
}

// A gallery-sized DOM can contain several thousand distinct words/runs. Keep
// that normal working set resident so a JS geometry pass followed by a native
// relayout does not turn FIFO eviction into a full sequential cache miss. The
// independent byte ceiling remains the authoritative memory bound.
const MAX_PARKED_FONT_SETS: usize = 4;
const MAX_SHAPE_CACHE_ENTRIES: usize = 8_192;
const MAX_SHAPE_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    style: TextStyleKey,
    quantize: bool,
    typography: Option<TypographyKey>,
}

/// Typographic settings Canvas text preparation adds to a [`TextStyle`]
/// (HTML #text-preparation-algorithm: 'font-stretch', 'font-kerning',
/// 'font-variant-caps' and SVG 'text-rendering' of the hypothetical inline
/// box). Layout text never uses them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CanvasTypography {
    /// CSS Fonts 4 #font-stretch-prop as a ratio of the normal width.
    pub width: f32,
    /// OpenType features to enable (1) or disable (0).
    pub features: Vec<([u8; 4], u16)>,
    /// Byte ranges shaped at another font size: synthesized small capitals
    /// (CSS Fonts 4 #font-variant-caps-prop).
    pub sized: Vec<(Range<usize>, f32)>,
    /// Leading bytes (a directional mark) that take no letter spacing.
    pub unspaced_prefix: usize,
}

impl Default for CanvasTypography {
    fn default() -> Self {
        Self {
            width: 1.,
            features: Vec::new(),
            sized: Vec::new(),
            unspaced_prefix: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TypographyKey {
    width: u32,
    features: Vec<([u8; 4], u16)>,
    sized: Vec<(usize, usize, u32)>,
    unspaced_prefix: usize,
}

impl From<&CanvasTypography> for TypographyKey {
    fn from(typography: &CanvasTypography) -> Self {
        Self {
            width: typography.width.to_bits(),
            features: typography.features.clone(),
            sized: typography
                .sized
                .iter()
                .map(|(range, size)| (range.start, range.end, size.to_bits()))
                .collect(),
            unspaced_prefix: typography.unspaced_prefix,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TextStyleKey {
    family: String,
    font_set: u64,
    language: Option<String>,
    size: u32,
    weight: u32,
    italic: bool,
    letter_spacing: u32,
    word_spacing: u32,
    line_height: (u8, u32),
    underline: bool,
    strikethrough: bool,
    typography: Option<Arc<FontTypography>>,
}

impl From<&TextStyle> for TextStyleKey {
    fn from(style: &TextStyle) -> Self {
        let line_height = match style.line_height {
            CssLineHeight::Normal => (0, 0),
            CssLineHeight::Number(value) => (1, value.to_bits()),
            CssLineHeight::Length(value) => (2, value.to_bits()),
        };
        Self {
            family: style.family.clone(),
            font_set: style.font_set.as_ref().map_or(0, |fonts| fonts.id()),
            language: style.language.clone(),
            size: style.size.to_bits(),
            weight: style.weight.to_bits(),
            italic: style.italic,
            letter_spacing: style.letter_spacing.to_bits(),
            word_spacing: style.word_spacing.to_bits(),
            line_height,
            underline: style.underline,
            strikethrough: style.strikethrough,
            typography: style.typography.clone(),
        }
    }
}

struct CachedShape {
    shaped: ShapedText,
    bytes: usize,
}

/// CSS Fonts 4 §2.1 treats the value as a prioritized list and recommends a
/// generic family as the final alternative when named faces are unavailable.
fn font_family_source(family: &str) -> Cow<'_, str> {
    crate::font_system::css_family_source(family)
}

fn text_language(style: &TextStyle) -> Option<Language> {
    style.language.as_deref()?.parse().ok()
}

impl TextSystem {
    fn new() -> Self {
        Self {
            fonts: crate::font_system::font_context(),
            layouts: LayoutContext::new(),
            color_layouts: LayoutContext::new(),
            page_font_epoch: crate::font_system::page_font_epoch(),
            active_font_set: 0,
            parked_fonts: Vec::new(),
            shape_cache: HashMap::new(),
            shape_order: VecDeque::new(),
            shape_cache_bytes: 0,
            layout_cache: Default::default(),
            emoji_families: HashMap::new(),
            face_metrics: HashMap::new(),
            cells: None,
            #[cfg(test)]
            shaped_input_bytes: 0,
        }
    }

    fn shape(&mut self, text: &str, style: &TextStyle) -> ShapedText {
        self.shape_with_quantization(text, style, true)
    }

    fn shape_with_quantization(
        &mut self,
        text: &str,
        style: &TextStyle,
        quantize: bool,
    ) -> ShapedText {
        self.shape_with_typography(text, style, quantize, None)
    }

    fn shape_with_typography(
        &mut self,
        text: &str,
        style: &TextStyle,
        quantize: bool,
        typography: Option<&CanvasTypography>,
    ) -> ShapedText {
        self.select_fonts(style.font_set.as_ref());
        let key = ShapeKey {
            text: text.to_string(),
            style: style.into(),
            quantize,
            typography: typography.map(Into::into),
        };
        if let Some(hit) = self.shape_cache.get(&key) {
            return hit.shaped.clone();
        }
        let shaped = self.shape_uncached(text, style, quantize, typography);
        let bytes = shape_cost(&key, &shaped);
        if bytes <= MAX_SHAPE_CACHE_BYTES / 4 {
            self.shape_cache_bytes = self.shape_cache_bytes.saturating_add(bytes);
            self.shape_order.push_back(key.clone());
            self.shape_cache.insert(
                key,
                CachedShape {
                    shaped: shaped.clone(),
                    bytes,
                },
            );
            while self.shape_cache.len() > MAX_SHAPE_CACHE_ENTRIES
                || self.shape_cache_bytes > MAX_SHAPE_CACHE_BYTES
            {
                let Some(oldest) = self.shape_order.pop_front() else {
                    break;
                };
                if let Some(old) = self.shape_cache.remove(&oldest) {
                    self.shape_cache_bytes = self.shape_cache_bytes.saturating_sub(old.bytes);
                }
            }
        }
        shaped
    }

    fn shape_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        quantize: bool,
        typography: Option<&CanvasTypography>,
    ) -> ShapedText {
        if text.is_empty() || style.size <= 0.0 {
            return zero_size_shape(text, style);
        }
        let prepared = self.prepare(text, style, typography);
        let mut layout = self.prepared_layout::<()>(&prepared, style, quantize, None, &[]);
        layout.break_all_lines(None);
        let mut retained = match layout.lines().next() {
            Some(line) => retain_line(&prepared.text, &line, &prepared.synthesis),
            None => ShapedText::default(),
        };
        self.finish_line(
            &mut retained,
            &prepared,
            text,
            0..prepared.text.len(),
            style,
        );
        // The whole piece, as for unwrapped text.
        retained.text = text.to_string();
        retained.underline = style.underline;
        retained.strikethrough = style.strikethrough;
        retained
    }

    /// CSS Fonts 4 #feature-variation-precedence for `text` in `style`, or
    /// Canvas text preparation's settings: the features and variations to
    /// shape with, and any synthesized small capitals (#font-variant-caps-prop)
    /// or superscripts/subscripts (#font-variant-position-prop).
    fn prepare<'t>(
        &mut self,
        text: &'t str,
        style: &TextStyle,
        canvas: Option<&CanvasTypography>,
    ) -> Prepared<'t> {
        let mut prepared = Prepared {
            text: Cow::Borrowed(text),
            map: None,
            width: style.font_width(),
            features: Vec::new(),
            optical_sizing: true,
            variation_settings: Vec::new(),
            sized: Vec::new(),
            unspaced_prefix: 0,
            position: None,
            size_adjust: None,
            synthesis: SynthesisPolicy::default(),
            emoji: typography::Emoji::Normal,
            strut: false,
        };
        if let Some(canvas) = canvas {
            prepared.width = canvas.width;
            prepared.features.clone_from(&canvas.features);
            prepared.sized.clone_from(&canvas.sized);
            prepared.unspaced_prefix = canvas.unspaced_prefix;
            return prepared;
        }
        let Some(t) = style.typography.clone() else {
            prepared.features =
                FontTypography::default().features(style.letter_spacing, &[], false);
            return prepared;
        };
        prepared.synthesis = SynthesisPolicy {
            weight: t.synthesis.weight,
            style: match t.synthesis.style {
                typography::SynthesisStyle::Auto => true,
                typography::SynthesisStyle::None => false,
                // "must not be used as fallback if italic is specified".
                typography::SynthesisStyle::ObliqueOnly => t.oblique.is_some(),
            },
        };
        prepared.emoji = t.emoji;
        let face = if t.needs_face_features() {
            self.primary_face_features(style)
        } else {
            Arc::default()
        };
        let (caps, synthetic_caps) = typography::caps_plan(t.caps, |tag| face.contains(tag));
        let synthetic_caps = synthetic_caps.filter(|_| t.synthesis.small_caps);
        let position = match t.position {
            typography::Position::Normal => false,
            _ if !t.synthesis.position => true,
            position => {
                let base = t.features(style.letter_spacing, &caps, false);
                let mut with = base.clone();
                with.push((
                    if position == typography::Position::Super {
                        *b"sups"
                    } else {
                        *b"subs"
                    },
                    1,
                ));
                let available = self.substitutes_every_glyph(text, style, &base, &with);
                if !available {
                    prepared.position = typography::position_metrics(position, style.size);
                }
                available
            }
        };
        prepared.features = t.features(style.letter_spacing, &caps, position);
        prepared.optical_sizing = t.optical_sizing;
        prepared
            .variation_settings
            .clone_from(&t.variation_settings);
        prepared.size_adjust = t.size_adjust;
        if let Some(caps) = synthetic_caps {
            let (converted, sized, segments) = typography::synthesize_caps(
                text,
                caps,
                style.size * typography::SMALL_CAPS_SCALE,
                typography::turkic(style.language.as_deref()),
            );
            if !sized.is_empty() {
                let map = typography::OffsetMap::new(segments);
                if !map.is_identity() {
                    prepared.map = Some(map);
                }
                prepared.text = Cow::Owned(converted);
                prepared.sized = sized;
            }
        }
        prepared.strut = !prepared.sized.is_empty() || prepared.position.is_some();
        prepared
    }

    /// Whether shaping `text` with `with` replaces the glyphs of every
    /// non-space cluster shaped with `base` (#font-variant-position-prop:
    /// "if a variant glyph is not available for all the characters in the
    /// run, simulated glyphs should be synthesized for all characters").
    fn substitutes_every_glyph(
        &mut self,
        text: &str,
        style: &TextStyle,
        base: &[(typography::Tag, u16)],
        with: &[(typography::Tag, u16)],
    ) -> bool {
        let mut glyphs = |features: &[(typography::Tag, u16)]| {
            let prepared = Prepared {
                features: features.to_vec(),
                ..self.prepare(
                    text,
                    &TextStyle {
                        typography: None,
                        ..style.clone()
                    },
                    None,
                )
            };
            let mut layout = self.prepared_layout::<()>(&prepared, style, false, None, &[]);
            layout.break_all_lines(None);
            let mut clusters: Vec<(Range<usize>, Vec<u32>)> = Vec::new();
            for line in layout.lines() {
                for run in line.runs() {
                    for cluster in run.visual_clusters() {
                        clusters.push((
                            cluster.text_range(),
                            cluster.glyphs().map(|glyph| glyph.id).collect(),
                        ));
                    }
                }
            }
            clusters
        };
        let plain = glyphs(base);
        let substituted = glyphs(with);
        plain.iter().all(|(range, ids)| {
            text[range.clone()].chars().all(char::is_whitespace)
                || substituted
                    .iter()
                    .find(|(other, _)| other == range)
                    .is_some_and(|(_, other)| other != ids)
        })
    }

    /// The OpenType features of the first available font of `style`
    /// (CSS Fonts 4 #first-available-font): "The availability of these
    /// glyphs is based on whether a given feature is defined or not in the
    /// feature list of the font".
    fn primary_face_features(&mut self, style: &TextStyle) -> Arc<typography::FaceFeatures> {
        let plain = TextStyle {
            typography: None,
            ..style.clone()
        };
        let primary = self.shape(" ", &plain);
        let Some(run) = primary.runs.first() else {
            return Arc::default();
        };
        face_features(&run.font)
    }

    /// Build the layout of prepared text, then shape again any run whose
    /// font needs its own size, features or variations: an `@font-face`
    /// face's `size-adjust`, `font-feature-settings` and
    /// `font-variation-settings` descriptors (CSS Fonts 5 #size-adjust-desc,
    /// CSS Fonts 4 #font-rend-desc) and `font-size-adjust` (CSS Fonts 5
    /// #font-size-adjust-prop) apply per font, not per element. `brushes`
    /// color byte ranges of the prepared text.
    fn prepared_layout<B: parley::Brush>(
        &mut self,
        prepared: &Prepared<'_>,
        style: &TextStyle,
        quantize: bool,
        breaks: Option<TextBreakStyle>,
        brushes: &[(Range<usize>, B)],
    ) -> Layout<B>
    where
        Self: LayoutsFor<B>,
    {
        #[cfg(test)]
        {
            self.shaped_input_bytes += prepared.text.len();
        }
        let emoji = self.emoji_presentation(&prepared.text, style, prepared.emoji);
        let build = |system: &mut Self, adjust: &[RunAdjust]| {
            let (layouts, fonts) = system.contexts();
            let mut builder = layouts.ranged_builder(fonts, &prepared.text, 1.0, quantize);
            configure_prepared(&mut builder, prepared, style, breaks, &emoji, adjust);
            for (range, brush) in brushes {
                builder.push(StyleProperty::Brush(brush.clone()), range.clone());
            }
            builder.build(&prepared.text)
        };
        let mut layout = build(self, &[]);
        if !self.per_font_candidates(prepared, style) {
            return layout;
        }
        // The fonts of each run are known once it is shaped; breaking the
        // probe into lines exposes them. Callers break the result again.
        layout.break_all_lines(None);
        let adjust = self.per_font_adjustments(&layout, prepared, style);
        if adjust.is_empty() {
            layout
        } else {
            build(self, &adjust)
        }
    }

    /// Whether any run may need per-font shaping (see `prepared_layout`).
    fn per_font_candidates(&self, prepared: &Prepared<'_>, style: &TextStyle) -> bool {
        prepared.size_adjust.is_some()
            || crate::font_system::has_face_renderings(style.font_set.as_ref())
    }

    fn per_font_adjustments<B: parley::Brush>(
        &mut self,
        layout: &Layout<B>,
        prepared: &Prepared<'_>,
        style: &TextStyle,
    ) -> Vec<RunAdjust> {
        let mut adjust: Vec<RunAdjust> = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let font = run.font();
                let rendering =
                    crate::font_system::face_rendering(style.font_set.as_ref(), font.data.id());
                let base = run.font_size();
                let mut size = base;
                if let Some((metric, aspect)) = prepared.size_adjust {
                    // "u = (m / m') s": the font's own metric, before any
                    // `size-adjust` (#font-metrics-override-desc: font-size-adjust
                    // "is applied after the size-adjust descriptor").
                    let coords = run.normalized_coords().to_vec();
                    if let Some(ratio) = self.face_metric(font, &coords, metric)
                        && ratio > 0.0
                    {
                        size = base * aspect / ratio;
                    }
                } else if let Some(rendering) = &rendering {
                    size = base * rendering.size_adjust;
                }
                let features = rendering
                    .as_ref()
                    .filter(|r| !r.features.is_empty())
                    .map(|r| {
                        let mut list: Vec<(typography::Tag, u16)> = r
                            .features
                            .iter()
                            .map(|&(tag, value)| (tag, u16::try_from(value).unwrap_or(u16::MAX)))
                            .collect();
                        list.extend_from_slice(&prepared.features);
                        list
                    });
                let descriptor_variations = rendering
                    .as_ref()
                    .map_or(&[][..], |r| r.variations.as_slice());
                // Step 6 (the descriptor), then 9 (the optical size of the
                // adjusted size) and 12 (the property).
                let variations = (size != base || !descriptor_variations.is_empty()).then(|| {
                    let mut list = descriptor_variations.to_vec();
                    list.extend(prepared.variations(size));
                    list
                });
                if size == base && features.is_none() && variations.is_none() {
                    continue;
                }
                adjust.push(RunAdjust {
                    range: run.text_range(),
                    size: (size != base).then_some(size),
                    features,
                    variations,
                });
            }
        }
        adjust
    }

    /// CSS Fonts 5 #font-size-adjust-prop: font `font`'s `metric` as a
    /// fraction of its em, or `None` when it "cannot be derived from a font".
    fn face_metric(
        &mut self,
        font: &parley::FontData,
        coords: &[i16],
        metric: typography::SizeAdjustMetric,
    ) -> Option<f32> {
        let key = (font.data.id(), font.index, coords.to_vec());
        if !self.face_metrics.contains_key(&key) {
            if self.face_metrics.len() >= 256 {
                self.face_metrics.clear();
            }
            let metrics = FaceMetrics::read(font.data.as_ref(), font.index, coords);
            self.face_metrics.insert(key.clone(), metrics);
        }
        let metrics = self.face_metrics.get(&key)?;
        match metric {
            typography::SizeAdjustMetric::ExHeight => metrics.x_height,
            typography::SizeAdjustMetric::CapHeight => metrics.cap_height,
            typography::SizeAdjustMetric::ChWidth => metrics.zero_advance,
            typography::SizeAdjustMetric::IcWidth => metrics.ideograph_advance,
            typography::SizeAdjustMetric::IcHeight => metrics.ideograph_vertical_advance,
        }
    }

    /// Restore source offsets, synthesized-position baselines and the
    /// strut of one retained line of prepared text. `range` is the line's
    /// byte range in the prepared text; `source` the source text.
    fn finish_line(
        &mut self,
        shaped: &mut ShapedText,
        prepared: &Prepared<'_>,
        source: &str,
        range: Range<usize>,
        style: &TextStyle,
    ) {
        if let Some(map) = &prepared.map {
            let start = map.to_source(range.start, false);
            let end = map.to_source(range.end, true);
            shaped.text = source[start..end].to_string();
            let to_source = |r: &Range<usize>| {
                let mapped = map.range_to_source(r.start + range.start..r.end + range.start);
                mapped.start - start..mapped.end - start
            };
            for cluster in &mut shaped.clusters {
                cluster.text_range = to_source(&cluster.text_range);
            }
            for run in &mut shaped.runs {
                run.text_range = to_source(&run.text_range);
            }
        }
        if prepared.strut {
            // Synthesized glyphs are reduced forms: the line keeps the
            // metrics of the element's font at its own size, and
            // synthesized superscripts/subscripts move off its baseline
            // "without affecting the line height".
            let plain = TextStyle {
                typography: None,
                ..style.clone()
            };
            let strut = self.shape(" ", &plain);
            merge_strut(shaped, &strut);
            if let Some((_, raise)) = prepared.position {
                let dy = raise * style.size;
                for glyph in shaped.runs.iter_mut().flat_map(|run| run.glyphs.iter_mut()) {
                    glyph.y -= dy;
                }
            }
        }
    }

    /// The emoji-presentation clusters of `text` and the family list they
    /// select fonts from.
    fn emoji_presentation(
        &mut self,
        text: &str,
        style: &TextStyle,
        variant: typography::Emoji,
    ) -> Option<(Vec<Range<usize>>, Arc<str>)> {
        let runs = emoji_presentation_runs(text, variant);
        if runs.is_empty() {
            return None;
        }
        let key = (self.active_font_set, style.family.clone());
        let family = match self.emoji_families.get(&key) {
            Some(family) => family.clone(),
            None => {
                let family: Arc<str> =
                    crate::font_system::emoji_family_source(&style.family, &mut self.fonts).into();
                if self.emoji_families.len() >= 256 {
                    self.emoji_families.clear();
                }
                self.emoji_families.insert(key, family.clone());
                family
            }
        };
        Some((runs, family))
    }

    fn first_line_end_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        width: f32,
        breaks: TextBreakStyle,
    ) -> usize {
        self.select_fonts(style.font_set.as_ref());
        if text.is_empty() || width <= 0.0 || style.size <= 0.0 {
            return 0;
        }
        let prepared = self.prepare(text, style, None);
        let mut layout = self.prepared_layout::<()>(&prepared, style, true, Some(breaks), &[]);
        let mut breaker = layout.break_lines();
        breaker.state_mut().set_layout_max_advance(width.max(0.01));
        breaker.state_mut().set_line_max_advance(width.max(0.01));
        breaker.break_next();
        breaker.finish();
        let end = layout
            .lines()
            .next()
            .map(|line| line.text_range().end.min(prepared.text.len()))
            .unwrap_or(0);
        prepared
            .map
            .as_ref()
            .map_or(end, |map| map.to_source(end, true))
            .min(text.len())
    }

    fn wrapped_lines_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        first_width: f32,
        width: f32,
        breaks: TextBreakStyle,
    ) -> Vec<ShapedText> {
        self.select_fonts(style.font_set.as_ref());
        if text.is_empty() || style.size <= 0.0 {
            return Vec::new();
        }
        // Parley keeps a cluster's byte offset within its run in 16 bits, so
        // in a run over 64 KiB the offsets wrap and a line's text range goes
        // invalid. Break such a paragraph through windows well below that:
        // lay a window, keep its complete lines, and lay again from the
        // start of its last line, which may continue past the window. The
        // greedy breaker settles each kept line within the window, so these
        // are the paragraph's own lines. A window without a break opportunity
        // (an unbreakable run of that length) becomes one line.
        // Synthesized small capitals can triple a cluster's bytes.
        let expands = style
            .typography
            .as_ref()
            .is_some_and(|t| t.caps != typography::Caps::Normal);
        let (limit, window) = if expands {
            (usize::from(u16::MAX) / 3, 1 << 13)
        } else {
            (usize::from(u16::MAX), 1 << 15)
        };
        if text.len() <= limit {
            return self
                .wrapped_window(text, style, first_width, width, breaks)
                .0;
        }
        let mut lines = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + window).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end < text.len()
                && let Some(space) = text[start..end].rfind(char::is_whitespace)
                && space > 0
            {
                end = start
                    + space
                    + text[start + space..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
            }
            let line_width = if lines.is_empty() { first_width } else { width };
            let (mut window, last_start) =
                self.wrapped_window(&text[start..end], style, line_width, width, breaks);
            if end == text.len() || window.len() < 2 {
                lines.append(&mut window);
                start = end;
                continue;
            }
            window.pop();
            lines.append(&mut window);
            start += last_start;
        }
        lines
    }

    /// Break one paragraph (at most 64 KiB once prepared) into lines; also
    /// returns the byte offset at which its last line starts.
    fn wrapped_window(
        &mut self,
        text: &str,
        style: &TextStyle,
        first_width: f32,
        width: f32,
        breaks: TextBreakStyle,
    ) -> (Vec<ShapedText>, usize) {
        // Shape one paragraph, then advance a single greedy breaker. Rebuilding
        // the shrinking tail on every line makes long preserved text quadratic.
        // CSS Text 3 #word-break-shaping also requires joining forms to survive
        // emergency line breaks; retain glyphs from the whole shaped paragraph.
        let prepared = self.prepare(text, style, None);
        let mut layout = self.prepared_layout::<()>(&prepared, style, true, Some(breaks), &[]);
        let mut breaker = layout.break_lines();
        breaker.state_mut().set_layout_max_advance(f32::INFINITY);
        breaker
            .state_mut()
            .set_line_max_advance(first_width.max(0.01));
        while breaker.break_next().is_some() {
            breaker.state_mut().set_line_max_advance(width.max(0.01));
        }
        breaker.finish();
        let last_start = layout
            .lines()
            .last()
            .map_or(0, |line| line.text_range().start);
        let last_start = prepared
            .map
            .as_ref()
            .map_or(last_start, |map| map.to_source(last_start, false));
        let retained: Vec<(ShapedText, Range<usize>)> = layout
            .lines()
            .map(|line| {
                (
                    retain_line(&prepared.text, &line, &prepared.synthesis),
                    line.text_range(),
                )
            })
            .collect();
        let lines = retained
            .into_iter()
            .map(|(mut shaped, range)| {
                self.finish_line(&mut shaped, &prepared, text, range, style);
                shaped.underline = style.underline;
                shaped.strikethrough = style.strikethrough;
                shaped
            })
            .collect();
        (lines, last_start)
    }

    fn content_widths_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        breaks: TextBreakStyle,
    ) -> (f32, f32) {
        self.select_fonts(style.font_set.as_ref());
        if text.is_empty() || style.size <= 0.0 {
            return (0.0, 0.0);
        }
        let prepared = self.prepare(text, style, None);
        // Content widths ignore `text-wrap-mode`: min-content is the widest
        // unbreakable piece whatever the element's own wrapping.
        let breaks = TextBreakStyle {
            wrap: true,
            ..breaks
        };
        let layout = self.prepared_layout::<()>(&prepared, style, true, Some(breaks), &[]);
        let widths = layout.calculate_content_widths();
        (widths.min, widths.max)
    }

    fn refresh_page_fonts(&mut self) {
        let epoch = crate::font_system::page_font_epoch();
        if self.page_font_epoch == epoch {
            return;
        }
        self.fonts = crate::font_system::font_context();
        self.layouts = LayoutContext::new();
        self.color_layouts = LayoutContext::new();
        self.shape_cache.clear();
        self.shape_order.clear();
        self.shape_cache_bytes = 0;
        self.layout_cache = Default::default();
        self.emoji_families.clear();
        self.face_metrics.clear();
        self.page_font_epoch = epoch;
        self.active_font_set = 0;
        self.parked_fonts.clear();
    }

    fn select_fonts(&mut self, fonts: Option<&Arc<FontSet>>) {
        self.refresh_page_fonts();
        let id = fonts.map_or(0, |fonts| fonts.id());
        if self.active_font_set != id {
            let next = match self.parked_fonts.iter().position(|(set, _)| *set == id) {
                Some(index) => self.parked_fonts.remove(index).1,
                None => fonts.map_or_else(crate::font_system::font_context, |f| f.context()),
            };
            let previous = std::mem::replace(&mut self.fonts, next);
            if self.parked_fonts.len() >= MAX_PARKED_FONT_SETS {
                self.parked_fonts.remove(0);
            }
            self.parked_fonts.push((self.active_font_set, previous));
            self.active_font_set = id;
        }
    }
}

/// Text prepared for shaping: CSS Fonts 4 #feature-variation-precedence
/// resolved for one element's text, or Canvas text preparation's settings.
struct Prepared<'t> {
    /// The text to shape: the source, or with synthesized small capitals.
    text: Cow<'t, str>,
    /// Offsets of `text` back to the source, when they differ.
    map: Option<typography::OffsetMap>,
    /// #font-width-prop as a ratio.
    width: f32,
    /// Steps 10, 11 and 13: the features to apply, the last of a tag winning.
    features: Vec<(typography::Tag, u16)>,
    /// Step 9: `font-optical-sizing: auto`.
    optical_sizing: bool,
    /// Step 12: `font-variation-settings`.
    variation_settings: Vec<(typography::Tag, f32)>,
    /// Byte ranges of `text` shaped at another font size.
    sized: Vec<(Range<usize>, f32)>,
    /// Leading bytes (a directional mark) that take no letter spacing.
    unspaced_prefix: usize,
    /// Synthesized superscripts/subscripts: size scale and raise in ems.
    position: Option<(f32, f32)>,
    /// CSS Fonts 5 #font-size-adjust-prop.
    size_adjust: Option<(typography::SizeAdjustMetric, f32)>,
    synthesis: SynthesisPolicy,
    emoji: typography::Emoji,
    /// Whether synthesized reduced glyphs keep the element font's strut.
    strut: bool,
}

impl Prepared<'_> {
    /// Steps 9 and 12 for glyphs shaped at `size`.
    fn variations(&self, size: f32) -> Vec<(typography::Tag, f32)> {
        let mut out = Vec::with_capacity(self.variation_settings.len() + 1);
        if self.optical_sizing {
            out.push((*b"opsz", size));
        }
        out.extend_from_slice(&self.variation_settings);
        out
    }
}

/// CSS Fonts 4 #font-synthesis: whether a run may draw the synthetic bold
/// and oblique its font match suggests.
#[derive(Clone, Copy, Debug)]
struct SynthesisPolicy {
    weight: bool,
    style: bool,
}

impl Default for SynthesisPolicy {
    fn default() -> Self {
        Self {
            weight: true,
            style: true,
        }
    }
}

/// A run shaped again with its own font's settings (`prepared_layout`).
struct RunAdjust {
    range: Range<usize>,
    size: Option<f32>,
    features: Option<Vec<(typography::Tag, u16)>>,
    variations: Option<Vec<(typography::Tag, f32)>>,
}

/// The layout context of a brush type, with the font context.
trait LayoutsFor<B: parley::Brush> {
    fn contexts(&mut self) -> (&mut LayoutContext<B>, &mut FontContext);
}

impl LayoutsFor<()> for TextSystem {
    fn contexts(&mut self) -> (&mut LayoutContext<()>, &mut FontContext) {
        (&mut self.layouts, &mut self.fonts)
    }
}

impl LayoutsFor<Option<[u8; 3]>> for TextSystem {
    fn contexts(&mut self) -> (&mut LayoutContext<Option<[u8; 3]>>, &mut FontContext) {
        (&mut self.color_layouts, &mut self.fonts)
    }
}

fn parley_features<B: parley::Brush>(
    features: &[(typography::Tag, u16)],
) -> StyleProperty<'static, B> {
    StyleProperty::FontFeatures(parley::FontFeatures::List(Cow::Owned(
        features
            .iter()
            .map(|(tag, value)| parley::FontFeature::new(parley::setting::Tag::new(tag), *value))
            .collect(),
    )))
}

fn parley_variations<B: parley::Brush>(
    variations: &[(typography::Tag, f32)],
) -> StyleProperty<'static, B> {
    StyleProperty::FontVariations(parley::FontVariations::List(Cow::Owned(
        variations
            .iter()
            .map(|(tag, value)| parley::FontVariation::new(parley::setting::Tag::new(tag), *value))
            .collect(),
    )))
}

fn configure_prepared<B: parley::Brush>(
    builder: &mut parley::RangedBuilder<'_, B>,
    prepared: &Prepared<'_>,
    style: &TextStyle,
    breaks: Option<TextBreakStyle>,
    emoji: &Option<(Vec<Range<usize>>, Arc<str>)>,
    adjust: &[RunAdjust],
) {
    configure_layout(builder, style, breaks);
    let scale = prepared.position.map_or(1.0, |(scale, _)| scale);
    let size = style.size.max(0.01) * scale;
    if scale != 1.0 {
        builder.push_default(StyleProperty::FontSize(size));
    }
    // CSS Inline 3: a number line-height multiplies the element's computed
    // font size, not that of reduced or adjusted runs.
    if (scale != 1.0 || !prepared.sized.is_empty() || !adjust.is_empty())
        && let CssLineHeight::Number(number) = style.line_height
    {
        builder.push_default(StyleProperty::LineHeight(LineHeight::Absolute(
            (number * style.size).max(0.0),
        )));
    }
    if prepared.width != 1.0 {
        builder.push_default(StyleProperty::FontWidth(parley::FontWidth::from_ratio(
            prepared.width,
        )));
    }
    if !prepared.features.is_empty() {
        builder.push_default(parley_features(&prepared.features));
    }
    let variations = prepared.variations(size);
    if !variations.is_empty() {
        builder.push_default(parley_variations(&variations));
    }
    for (range, sized) in &prepared.sized {
        if range.start < range.end && range.end <= prepared.text.len() {
            let sized = sized.max(0.01) * scale;
            builder.push(StyleProperty::FontSize(sized), range.clone());
            if prepared.optical_sizing {
                builder.push(
                    parley_variations(&prepared.variations(sized)),
                    range.clone(),
                );
            }
        }
    }
    if prepared.unspaced_prefix > 0 && prepared.unspaced_prefix <= prepared.text.len() {
        builder.push(
            StyleProperty::LetterSpacing(0.),
            0..prepared.unspaced_prefix,
        );
    }
    push_emoji_presentation(builder, emoji);
    for run in adjust {
        if let Some(size) = run.size {
            builder.push(StyleProperty::FontSize(size.max(0.01)), run.range.clone());
        }
        if let Some(features) = &run.features {
            builder.push(parley_features(features), run.range.clone());
        }
        if let Some(variations) = &run.variations {
            builder.push(parley_variations(variations), run.range.clone());
        }
    }
}

/// Give `shaped`, a line of reduced (synthesized) glyphs, at least the
/// line metrics of `strut`, the element's font at its own size, keeping
/// its glyphs on the baseline.
fn merge_strut(shaped: &mut ShapedText, strut: &ShapedText) {
    let above = shaped.baseline.max(strut.baseline);
    let below = (shaped.line_height - shaped.baseline).max(strut.line_height - strut.baseline);
    let dy = above - shaped.baseline;
    if dy != 0.0 {
        for glyph in shaped.runs.iter_mut().flat_map(|run| run.glyphs.iter_mut()) {
            glyph.y += dy;
        }
    }
    shaped.baseline = above;
    shaped.line_height = above + below;
    shaped.ascent = shaped.ascent.max(strut.ascent);
    shaped.descent = shaped.descent.max(strut.descent);
    shaped.leading = shaped.line_height - shaped.ascent - shaped.descent;
}

/// What CSS Fonts 5 #font-size-adjust-prop normalizes, as fractions of a
/// face's em at a variation location.
#[derive(Clone, Copy, Debug, Default)]
struct FaceMetrics {
    x_height: Option<f32>,
    cap_height: Option<f32>,
    zero_advance: Option<f32>,
    ideograph_advance: Option<f32>,
    ideograph_vertical_advance: Option<f32>,
}

impl FaceMetrics {
    fn read(font: &[u8], index: u32, coords: &[i16]) -> Self {
        use skrifa::MetadataProvider as _;
        use skrifa::instance::{LocationRef, NormalizedCoord, Size};
        use skrifa::raw::TableProvider as _;
        let Ok(face) = skrifa::FontRef::from_index(font, index) else {
            return Self::default();
        };
        let coords: Vec<NormalizedCoord> = coords
            .iter()
            .copied()
            .map(NormalizedCoord::from_bits)
            .collect();
        let location = LocationRef::new(&coords);
        let em = Size::new(1.0);
        let metrics = face.metrics(em, location);
        let glyphs = face.glyph_metrics(em, location);
        let charmap = face.charmap();
        let positive = |value: f32| (value.is_finite() && value > 0.0).then_some(value);
        let advance = |c: char| {
            charmap
                .map(c)
                .and_then(|glyph| glyphs.advance_width(glyph))
                .and_then(positive)
        };
        // "If the required font metric cannot be derived from a font, then
        // that font's size is not adjusted."
        let vertical = charmap.map('\u{6C34}').and_then(|glyph| {
            let vmtx = face.vmtx().ok()?;
            let upem = f32::from(face.head().ok()?.units_per_em());
            let advance = vmtx.advance(glyph)?;
            positive(f32::from(advance) / upem)
        });
        // CSS Values 4 #ex: a font without an x-height metric still has
        // one, the height of its "x" (and of "H" for the cap height).
        let top = |c: char| {
            charmap
                .map(c)
                .and_then(|glyph| glyphs.bounds(glyph))
                .and_then(|bounds| positive(bounds.y_max))
        };
        Self {
            x_height: metrics.x_height.and_then(positive).or_else(|| top('x')),
            cap_height: metrics.cap_height.and_then(positive).or_else(|| top('H')),
            zero_advance: advance('0'),
            ideograph_advance: advance('\u{6C34}'),
            ideograph_vertical_advance: vertical,
        }
    }
}

thread_local! {
    /// The OpenType features of recently used faces; small and bounded.
    static FACE_FEATURES: RefCell<HashMap<(u64, u32), Arc<typography::FaceFeatures>>> =
        RefCell::new(HashMap::new());
}

/// The GSUB and GPOS features `face` defines.
pub(crate) fn face_features(face: &FontFace) -> Arc<typography::FaceFeatures> {
    let key = (face.resource_id(), face.collection_index());
    FACE_FEATURES.with_borrow_mut(|cache| {
        if let Some(features) = cache.get(&key) {
            return features.clone();
        }
        if cache.len() >= 64 {
            cache.clear();
        }
        let features = Arc::new(typography::FaceFeatures::read(
            face.data().data.as_ref(),
            face.collection_index(),
        ));
        cache.insert(key, features.clone());
        features
    })
}

fn configure_layout<B: parley::Brush>(
    builder: &mut parley::RangedBuilder<'_, B>,
    style: &TextStyle,
    breaks: Option<TextBreakStyle>,
) {
    let family = font_family_source(&style.family);
    builder.push_default(StyleProperty::FontFamily(FontFamily::Source(family)));
    builder.push_default(StyleProperty::Locale(text_language(style)));
    builder.push_default(StyleProperty::FontSize(style.size.max(0.01)));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(
        style.weight.clamp(1.0, 1000.0),
    )));
    builder.push_default(StyleProperty::FontStyle(style.font_style()));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    builder.push_default(StyleProperty::WordSpacing(style.word_spacing));
    builder.push_default(StyleProperty::LineHeight(match style.line_height {
        CssLineHeight::Normal => LineHeight::default(),
        CssLineHeight::Number(number) => LineHeight::FontSizeRelative(number.max(0.0)),
        CssLineHeight::Length(px) => LineHeight::Absolute(px.max(0.0)),
    }));
    builder.push_default(StyleProperty::Underline(style.underline));
    builder.push_default(StyleProperty::Strikethrough(style.strikethrough));
    if let Some(breaks) = breaks {
        builder.push_default(StyleProperty::WordBreak(match breaks.word_break {
            TextWordBreak::Normal => WordBreak::Normal,
            TextWordBreak::BreakAll => WordBreak::BreakAll,
            TextWordBreak::KeepAll => WordBreak::KeepAll,
        }));
        builder.push_default(StyleProperty::OverflowWrap(match breaks.overflow_wrap {
            TextOverflowWrap::Normal => OverflowWrap::Normal,
            TextOverflowWrap::Anywhere => OverflowWrap::Anywhere,
            TextOverflowWrap::BreakWord => OverflowWrap::BreakWord,
        }));
        builder.push_default(StyleProperty::TextWrapMode(if breaks.wrap {
            TextWrapMode::Wrap
        } else {
            TextWrapMode::NoWrap
        }));
    }
}

fn push_emoji_presentation<B: parley::Brush>(
    builder: &mut parley::RangedBuilder<'_, B>,
    emoji: &Option<(Vec<Range<usize>>, Arc<str>)>,
) {
    let Some((runs, family)) = emoji else {
        return;
    };
    for range in runs {
        builder.push(
            StyleProperty::FontFamily(FontFamily::Source(Cow::Owned(family.to_string()))),
            range.clone(),
        );
    }
}

/// The byte ranges of clusters that request emoji presentation. UTS #51
/// #Emoji_Variation_Sequences: U+FE0F after an emoji character requests
/// emoji presentation and U+FE0E text presentation; without either, the
/// Emoji_Presentation property gives the default, which is how
/// `font-variant-emoji: normal` platforms choose (CSS Fonts 4
/// #font-variant-emoji-prop). A selector, keycap, skin-tone modifier, tag
/// or zero-width-joined sequence stays in its base's cluster.
/// `font-variant-emoji: text` and `emoji` render each emoji "as if U+FE0E
/// (U+FE0F) was appended"; an explicit selector still overrides them. ASCII
/// digits, `#` and `*` keep text presentation, since alone they are no
/// emoji (UTS #51 keycap sequences carry U+20E3).
fn emoji_presentation_runs(text: &str, variant: typography::Emoji) -> Vec<Range<usize>> {
    use icu_properties::CodePointSetData;
    use icu_properties::props::{Emoji, EmojiPresentation};
    let mut runs: Vec<Range<usize>> = Vec::new();
    if text.is_ascii() {
        return runs;
    }
    let emoji = CodePointSetData::new::<Emoji>();
    let presentation = CodePointSetData::new::<EmojiPresentation>();
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if !emoji.contains(ch) {
            continue;
        }
        let selector = chars.peek().map(|&(_, next)| next);
        let wanted = match (selector, variant) {
            (Some('\u{FE0F}'), _) => true,
            (Some('\u{FE0E}'), _) | (_, typography::Emoji::Text) => false,
            (_, typography::Emoji::Emoji) => !ch.is_ascii(),
            _ => presentation.contains(ch),
        };
        let mut end = start + ch.len_utf8();
        let mut joined = false;
        while let Some(&(index, next)) = chars.peek() {
            let extends = matches!(
                next,
                '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}' | '\u{200D}'
                    | '\u{1F3FB}'..='\u{1F3FF}'
                    | '\u{E0020}'..='\u{E007F}'
            );
            if !extends && !joined {
                break;
            }
            joined = next == '\u{200D}';
            end = index + next.len_utf8();
            chars.next();
        }
        if !wanted {
            continue;
        }
        match runs.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => runs.push(start..end),
        }
    }
    runs
}

fn shape_cost(key: &ShapeKey, shaped: &ShapedText) -> usize {
    key.text
        .len()
        .saturating_add(key.style.family.len())
        .saturating_add(key.style.language.as_ref().map_or(0, String::len))
        .saturating_add(shaped.text.len())
        .saturating_add(
            shaped
                .runs
                .iter()
                .map(|run| {
                    run.glyphs
                        .len()
                        .saturating_mul(std::mem::size_of::<ShapedGlyph>())
                        .saturating_add(run.normalized_coords.len().saturating_mul(2))
                })
                .sum::<usize>(),
        )
        .saturating_add(
            shaped
                .clusters
                .len()
                .saturating_mul(std::mem::size_of::<Cluster>()),
        )
}

/// Empty text, or text in a zero-size font. CSS2 #leading / #strut: a
/// zero-size font has zero ascent and descent, but an absolute line-height
/// still contributes its full leading, split equally above and below the
/// baseline.
fn zero_size_shape(text: &str, style: &TextStyle) -> ShapedText {
    let line_height = if !text.is_empty() {
        match style.line_height {
            CssLineHeight::Length(px) => px.max(0.0),
            _ => 0.0,
        }
    } else {
        0.0
    };
    ShapedText {
        text: text.to_string(),
        line_height,
        leading: line_height,
        baseline: line_height / 2.0,
        ..ShapedText::default()
    }
}

thread_local! {
    // LayoutContext is reusable scratch storage. A thread-local owner avoids a
    // global lock across the terminal owner, desktop event loop, and live-page
    // actor while still reusing font discovery and allocations on each thread.
    static TEXT: RefCell<TextSystem> = RefCell::new(TextSystem::new());
}

/// Shape one unbroken text piece at CSS-pixel scale.
/// Move `shaped`'s glyphs so that its line box is vertically centered in a
/// line `line_height` tall, which then becomes its line height. A text
/// control centers its one line of text in its content box; text in another
/// font than the control's (its `::placeholder`) keeps that center.
pub(crate) fn center_in_line(shaped: &mut ShapedText, line_height: f32) {
    let dy = (line_height - shaped.line_height) / 2.0;
    if !dy.is_finite() || dy == 0.0 {
        return;
    }
    for glyph in shaped.runs.iter_mut().flat_map(|run| run.glyphs.iter_mut()) {
        glyph.y += dy;
    }
    shaped.baseline += dy;
    shaped.line_height = line_height;
}

pub fn shape(text: &str, style: &TextStyle) -> ShapedText {
    TEXT.with_borrow_mut(|system| match system.cells {
        Some(cells) => cells::shape(text, style, cells),
        None => system.shape(text, style),
    })
}

/// Shape `text`, all of whose characters the paragraph's Unicode
/// Bidirectional Algorithm resolved to one embedding level, in that level's
/// direction (`rtl` for an odd level): UAX #9 L2 reverses an odd-level run,
/// and L4 mirrors its Bidi_Mirrored characters.
///
/// Parley resolves a shaped run's bidi levels itself, from its first strong
/// character (P2/P3). That agrees with the paragraph's resolution for every
/// level-uniform run except one with no strong character of its level's
/// direction: neutrals alone, or text under a directional override. Those
/// are reshaped behind LRO/RLO (U+202D/U+202E), which forces every character
/// to the requested direction, and the control is removed from the result.
/// In the terminal's cell font clusters are laid in logical order; an odd
/// level mirrors their positions, and the cell adapter paints the text in
/// visual order (`visual_text`).
pub(crate) fn shape_directed(text: &str, style: &TextStyle, rtl: bool) -> ShapedText {
    let shaped = shape(text, style);
    if cell_metrics().is_some() {
        return if rtl { shaped.mirrored_cells() } else { shaped };
    }
    if text.is_empty() || shaped.runs.iter().all(|run| run.rtl == rtl) {
        return shaped;
    }
    let control = if rtl { '\u{202E}' } else { '\u{202D}' };
    let mut forced = String::with_capacity(text.len() + control.len_utf8());
    forced.push(control);
    forced.push_str(text);
    let forced = shape(&forced, style);
    forced.without_leading_control(control.len_utf8(), text)
}

/// UAX #9 L2/L4 for a character-cell painter: `text`, all at one odd
/// embedding level, in visual order. Grapheme clusters are reversed as
/// units (L3's combining marks stay after their bases) and characters with
/// a Bidi_Mirroring_Glyph are mirrored.
pub(crate) fn visual_text(text: &str) -> String {
    use icu_properties::CodePointMapData;
    use icu_properties::props::BidiMirroringGlyph;
    let mirror = CodePointMapData::<BidiMirroringGlyph>::new();
    let mut visual = String::with_capacity(text.len());
    for grapheme in text.graphemes(true).rev() {
        for character in grapheme.chars() {
            visual.push(mirror.get(character).mirroring_glyph.unwrap_or(character));
        }
    }
    visual
}

/// Lay this thread's text out in the terminal's cell font (`Some`), or in
/// proportional fonts (`None`, the default). Returns the previous setting.
/// Only the terminal frontend's layout threads enable it; canvas text
/// (`shape_canvas`) and text editing stay proportional regardless.
pub fn set_cell_metrics(cells: Option<CellMetrics>) -> Option<CellMetrics> {
    TEXT.with_borrow_mut(|system| std::mem::replace(&mut system.cells, cells))
}

/// This thread's terminal cell font, if it lays text out in cells.
pub fn cell_metrics() -> Option<CellMetrics> {
    TEXT.with_borrow(|system| system.cells)
}

/// Lay text out in the cell font until the returned guard drops, restoring
/// the thread's previous setting — for terminal layouts run on shared worker
/// threads.
#[must_use]
pub fn cell_metrics_scope(cells: Option<CellMetrics>) -> CellMetricsScope {
    CellMetricsScope {
        previous: set_cell_metrics(cells),
    }
}

pub struct CellMetricsScope {
    previous: Option<CellMetrics>,
}

impl Drop for CellMetricsScope {
    fn drop(&mut self) {
        set_cell_metrics(self.previous);
    }
}

/// Color-only ranges share a whole-paragraph shaping pass, preserving bidi,
/// joining, ligatures, wrapping, and the single searchable/copyable string.
/// CSS Text 3 #boundary-shaping / #word-break-shaping (81c27f686901).
pub(crate) fn colored_lines(
    text: &str,
    style: &TextStyle,
    width: Option<f32>,
    colors: impl Iterator<Item = (Range<usize>, [u8; 3])>,
) -> Vec<ShapedText> {
    TEXT.with_borrow_mut(|system| {
        system.select_fonts(style.font_set.as_ref());
        let prepared = system.prepare(text, style, None);
        let brushes: Vec<(Range<usize>, Option<[u8; 3]>)> = colors
            .map(|(range, color)| {
                let range = match &prepared.map {
                    Some(map) => map.range_to_shaped(range),
                    None => range,
                };
                (range, Some(color))
            })
            .collect();
        let mut layout = system.prepared_layout(
            &prepared,
            style,
            true,
            Some(TextBreakStyle {
                wrap: width.is_some(),
                overflow_wrap: TextOverflowWrap::Anywhere,
                ..TextBreakStyle::default()
            }),
            &brushes,
        );
        layout.break_all_lines(width.map(|w| w.max(0.01)));
        let lines: Vec<(ShapedText, Range<usize>)> = layout
            .lines()
            .take(if width.is_some() { usize::MAX } else { 1 })
            .map(|line| {
                let mut shaped = retain_line(&prepared.text, &line, &prepared.synthesis);
                for (run, item) in
                    shaped
                        .runs
                        .iter_mut()
                        .zip(line.items().filter_map(|item| match item {
                            PositionedLayoutItem::GlyphRun(run) => Some(run),
                            _ => None,
                        }))
                {
                    run.color = item.style().brush;
                }
                (shaped, line.text_range())
            })
            .collect();
        lines
            .into_iter()
            .map(|(mut shaped, range)| {
                system.finish_line(&mut shaped, &prepared, text, range, style);
                if width.is_none() {
                    // Match shape() for unwrapped text.
                    shaped.text = text.to_string();
                }
                shaped.underline = style.underline;
                shaped.strikethrough = style.strikethrough;
                shaped
            })
            .collect()
    })
}

/// Canvas metrics stay in fractional CSS pixels, regardless of bitmap/device
/// transforms. Share the bounded font/shape cache without changing layout's
/// existing quantization choice or aliasing its entries.
pub(crate) fn shape_canvas(text: &str, style: &TextStyle) -> ShapedText {
    TEXT.with_borrow_mut(|system| system.shape_with_quantization(text, style, false))
}

/// [`shape_canvas`] with Canvas text preparation's typographic settings.
pub(crate) fn shape_canvas_with(
    text: &str,
    style: &TextStyle,
    typography: &CanvasTypography,
) -> ShapedText {
    TEXT.with_borrow_mut(|system| {
        system.shape_with_typography(text, style, false, Some(typography))
    })
}

/// CSS Fonts 5 #font-size-adjust-prop `from-font`: `metric` of the first
/// available font of `style` (CSS Fonts 4 #first-available-font) as a
/// fraction of its em, if the font defines it.
pub(crate) fn first_available_metric(
    style: &TextStyle,
    metric: typography::SizeAdjustMetric,
) -> Option<f32> {
    TEXT.with_borrow_mut(|system| {
        let plain = TextStyle {
            typography: None,
            ..style.clone()
        };
        let primary = system.shape(" ", &plain);
        let run = primary.runs.first()?;
        let coords = run.normalized_coords.clone();
        system.face_metric(run.font.data(), &coords, metric)
    })
}

/// CSS `ch` basis: advance measure of U+0030 ZERO in the element's font.
pub fn zero_advance(style: &TextStyle) -> f32 {
    TEXT.with_borrow_mut(|system| match system.cells {
        Some(cells) => cells::zero_advance(style, cells),
        None => system.shape("0", style).advance.max(style.size * 0.25),
    })
}

/// Return the byte end of the first Unicode/CSS line that fits `width`.
/// Parley supplies UAX #14 opportunities, bidi-aware cluster boundaries, and
/// emergency wrapping. The caller remains responsible for CSS whitespace
/// processing and for composing differently styled inline boxes.
pub fn first_line_end(text: &str, style: &TextStyle, width: f32, breaks: TextBreakStyle) -> usize {
    TEXT.with_borrow_mut(|system| match system.cells {
        Some(cells) => cells::first_line_end(text, style, width, breaks, cells),
        None => system.first_line_end(text, style, width, breaks),
    })
}

/// Shape and wrap a preserved-whitespace run in a rectangular inline region.
/// The first line can have a different available width (indent/preceding boxes).
/// Callers with varying float exclusions retain the per-line composition path.
pub(crate) fn wrapped_lines(
    text: &str,
    style: &TextStyle,
    first_width: f32,
    width: f32,
    breaks: TextBreakStyle,
) -> Vec<ShapedText> {
    TEXT.with_borrow_mut(|system| match system.cells {
        Some(cells) => cells::wrapped_lines(text, style, first_width, width, breaks, cells),
        None => system.wrapped_lines(text, style, first_width, width, breaks),
    })
}

/// The first `max_lines` of [`wrapped_lines`], breaking only as much of a
/// long paragraph as they need: a prefix window that breaks into more
/// lines than that holds them exactly, as the greedy breaker settles a line
/// within the text before the next one starts. Windows double from 8 KiB
/// until one does, or the whole paragraph is broken.
pub(crate) fn wrapped_lines_limited(
    text: &str,
    style: &TextStyle,
    first_width: f32,
    width: f32,
    breaks: TextBreakStyle,
    max_lines: usize,
) -> Vec<ShapedText> {
    let mut window = 1 << 13;
    while window < text.len() {
        let mut end = window;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut lines = wrapped_lines(&text[..end], style, first_width, width, breaks);
        if lines.len() > max_lines {
            lines.truncate(max_lines);
            return lines;
        }
        window *= 2;
    }
    let mut lines = wrapped_lines(text, style, first_width, width, breaks);
    lines.truncate(max_lines);
    lines
}

/// CSS min-/max-content bounds for a single styled text run.
pub fn content_widths(text: &str, style: &TextStyle, breaks: TextBreakStyle) -> (f32, f32) {
    TEXT.with_borrow_mut(|system| match system.cells {
        Some(cells) => cells::content_widths(text, style, breaks, cells),
        None => system.content_widths(text, style, breaks),
    })
}

/// CSS 2.2 §10.8.1: the baseline sits the ascent plus one half-leading
/// below the top of the line-height box, and that leading is negative when
/// `line-height` is smaller than the content area. Parley keeps a negative
/// half-leading in `baseline` but clamps it out of `block_min_coord` (its
/// selection box); restore it, split as Parley splits it (the upper half
/// floored when its metrics are pixel-quantized, which leaves the clamped
/// content height integral).
fn css_baseline(metrics: &parley::LineMetrics) -> f32 {
    let clamped = metrics.baseline - metrics.block_min_coord;
    let content = metrics.block_max_coord - metrics.block_min_coord;
    let leading = metrics.line_height - content;
    if leading >= 0.0 {
        return clamped;
    }
    clamped
        + if content.fract() == 0.0 {
            (leading * 0.5).floor()
        } else {
            leading * 0.5
        }
}

fn retain_first_line(text: &str, layout: &Layout<()>) -> ShapedText {
    let Some(line) = layout.lines().next() else {
        return ShapedText {
            text: text.to_string(),
            ..ShapedText::default()
        };
    };
    let mut shaped = retain_line(text, &line, &SynthesisPolicy::default());
    shaped.text = text.to_string();
    shaped
}

fn retain_line<B: parley::Brush>(
    text: &str,
    line: &parley::Line<'_, B>,
    synthesis: &SynthesisPolicy,
) -> ShapedText {
    let range = line.text_range();
    let line_text = &text[range.clone()];
    let metrics = line.metrics();
    // Glyphs sit on the same CSS baseline the layout aligns, including a
    // negative half-leading that Parley's clamped line top omits.
    let baseline = css_baseline(metrics);
    let mut runs = Vec::new();
    let mut clusters: Vec<Cluster> = Vec::new();
    let graphemes: Vec<Range<usize>> = line_text
        .grapheme_indices(true)
        .map(|(start, grapheme)| range.start + start..range.start + start + grapheme.len())
        .collect();
    let mut cluster_x = metrics.offset;
    for run in line.runs() {
        for cluster in run.visual_clusters() {
            let shaped_range = cluster.text_range();
            // Visual clusters are not in logical order for bidi text, so a
            // forward cursor is insufficient. The grapheme ranges are sorted
            // by byte offset: binary-search the first range ending after this
            // cluster instead of rescanning every grapheme for every cluster.
            let grapheme = graphemes.partition_point(|range| range.end <= shaped_range.start);
            let logical_range = graphemes
                .get(grapheme)
                .filter(|range| range.start < shaped_range.end && shaped_range.start < range.end)
                .cloned()
                .unwrap_or_else(|| shaped_range.clone());
            let logical_range = logical_range.start - range.start..logical_range.end - range.start;
            if let Some(previous) = clusters.last_mut()
                && previous.text_range == logical_range
            {
                // A single extended grapheme can be split into several
                // shaping clusters when fallback fonts participate (emoji
                // ZWJ sequences are the common case). Selection/caret motion
                // must still expose one Unicode boundary.
                previous.x = previous.x.min(cluster_x);
                previous.advance += cluster.advance();
            } else {
                clusters.push(Cluster {
                    text_range: logical_range,
                    x: cluster_x,
                    advance: cluster.advance(),
                    rtl: cluster.is_rtl(),
                });
            }
            cluster_x += cluster.advance();
        }
    }
    for item in line.items() {
        let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
            continue;
        };
        let run = glyph_run.run();
        let (synthesis, policy) = (run.synthesis(), synthesis);
        runs.push(ShapedRun {
            font: FontFace(run.font().clone()),
            font_size: run.font_size(),
            color: None,
            normalized_coords: run.normalized_coords().to_vec(),
            glyphs: glyph_run
                .positioned_glyphs()
                .map(|glyph| ShapedGlyph {
                    id: glyph.id,
                    x: glyph.x,
                    y: glyph.y - metrics.baseline + baseline,
                    advance: glyph.advance,
                })
                .collect(),
            text_range: run.text_range().start - range.start..run.text_range().end - range.start,
            rtl: run.is_rtl(),
            // CSS Fonts 4 #font-synthesis-weight / #font-synthesis-style.
            synth_bold: synthesis.embolden() && policy.weight,
            synth_skew_degrees: synthesis.skew().filter(|_| policy.style),
        });
    }
    ShapedText {
        text: line_text.to_string(),
        // Whitespace processing already happened in the CSS inline formatter.
        // Retain the actual shaped advance here: preserved/trailing spaces and
        // tab-size bases must occupy geometry, while collapsed trailing space
        // is never handed to the shaper in the first place.
        advance: metrics.advance,
        ascent: metrics.ascent,
        descent: metrics.descent,
        leading: metrics.leading,
        line_height: metrics.line_height,
        baseline,
        underline: false,
        strikethrough: false,
        runs,
        clusters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CSS Overflow 3 #bidi-ellipsis: the characters hidden for an ellipsis
    /// are those visually at the end edge, which in a right-to-left run are
    /// its logically first characters.
    #[test]
    fn visual_elision_hides_the_visual_end_of_bidi_text() {
        // "ab" then the right-to-left run "\u{5d0}\u{5d1}", painted as
        // a b bet alef.
        let text = "ab\u{5d0}\u{5d1}".to_string();
        let cluster = |range: Range<usize>, x: f32, rtl| Cluster {
            text_range: range,
            x,
            advance: 1.0,
            rtl,
        };
        let shaped = ShapedText {
            text,
            advance: 4.0,
            ascent: 1.0,
            descent: 0.0,
            leading: 0.0,
            line_height: 1.0,
            baseline: 1.0,
            underline: false,
            strikethrough: false,
            runs: Vec::new(),
            clusters: vec![
                cluster(0..1, 0.0, false),
                cluster(1..2, 1.0, false),
                cluster(2..4, 3.0, true),
                cluster(4..6, 2.0, true),
            ],
        };
        let cut = shaped.visual_elision_cut(3.0, false).unwrap();
        assert_eq!(cut, 3.0);
        let prefix = shaped.visual_prefix(cut);
        // Alef (logically first in its run, visually last) is hidden.
        assert_eq!(prefix.text, "ab\u{5d1}");
        assert_eq!(prefix.advance, 3.0);
        assert_eq!(prefix.clusters[2].text_range, 2..4);
        assert_eq!(prefix.clusters[2].x, 2.0);
        // Nothing fits before the limit, yet the first character stays.
        assert_eq!(shaped.visual_elision_cut(0.5, false), None);
        assert_eq!(shaped.visual_elision_cut(0.5, true), Some(1.0));
        // Everything fits: no character is hidden.
        assert_eq!(shaped.visual_elision_cut(10.0, false), Some(10.0));
    }

    #[test]
    fn downloaded_regular_face_synthesizes_italic_and_bold() {
        // CSS Fonts 4 #font-synthesis-style/-weight (initial `weight style`):
        // a family with only an upright regular face is slanted for italic
        // and emboldened for bold.
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/dejavu/DejaVuSerif.ttf"
        ))
        .unwrap();
        let fonts = FontSet::new(
            vec![crate::font_system::PageFont {
                family: "Web".into(),
                bytes,
                ..Default::default()
            }],
            None,
            false,
        );
        let synthesis = |italic, weight| {
            let shaped = shape(
                "Hello",
                &TextStyle {
                    family: "Web".into(),
                    font_set: Some(fonts.clone()),
                    italic,
                    weight,
                    ..TextStyle::default()
                },
            );
            assert_eq!(shaped.runs.len(), 1);
            (shaped.runs[0].synth_skew_degrees, shaped.runs[0].synth_bold)
        };
        assert_eq!(synthesis(false, 400.), (None, false));
        assert!(synthesis(true, 400.).0.is_some_and(|skew| skew > 0.));
        assert!(synthesis(false, 700.).1);
        assert!(synthesis(false, 600.).1);
        // A medium request is not bold: the regular face draws it as is.
        assert_eq!(synthesis(false, 500.), (None, false));
    }

    #[test]
    fn glyphs_sit_on_the_baseline_with_negative_half_leading() {
        // CSS 2.2 §10.8.1: a line-height below the content area gives a
        // negative half-leading; the glyphs still sit on the baseline the
        // line aligns (a large initial with `line-height: 1` must not drop).
        for line_height in [
            CssLineHeight::Normal,
            CssLineHeight::Length(60.),
            CssLineHeight::Length(20.),
            CssLineHeight::Number(0.5),
        ] {
            let shaped = shape(
                "Oh",
                &TextStyle {
                    family: "serif".into(),
                    size: 40.,
                    line_height,
                    ..TextStyle::default()
                },
            );
            for glyph in shaped.runs.iter().flat_map(|run| &run.glyphs) {
                assert!(
                    (glyph.y - shaped.baseline).abs() < 0.01,
                    "{line_height:?}: glyph at {} on baseline {}",
                    glyph.y,
                    shaped.baseline
                );
            }
        }
    }

    #[test]
    fn zero_font_size_retains_absolute_line_height_and_half_leading() {
        for (line_height, expected) in [
            (CssLineHeight::Length(34.), 34.),
            (CssLineHeight::Length(0.), 0.),
            (CssLineHeight::Number(2.), 0.),
            (CssLineHeight::Normal, 0.),
        ] {
            let shaped = shape(
                " ",
                &TextStyle {
                    size: 0.,
                    line_height,
                    ..TextStyle::default()
                },
            );
            assert_eq!(shaped.line_height, expected);
            assert_eq!(shaped.baseline, expected / 2.);
            assert_eq!(shaped.leading, expected);
            assert_eq!(shaped.advance, 0.);
            assert!(shaped.runs.is_empty());
        }
    }

    #[test]
    fn retained_installed_fonts_survive_environment_switches() {
        let style = TextStyle::default();
        let mut system = TextSystem::new();
        let first = system.shape("retained", &style);
        let mut child_style = style.clone();
        child_style.font_set = Some(FontSet::new(Vec::new(), None, false));
        let child = system.shape("retained", &child_style);
        assert_eq!(first.runs[0].font, child.runs[0].font);
        let fresh = TextSystem::new().shape("retained", &style);
        assert_eq!(first.runs[0].font, fresh.runs[0].font);
    }

    #[test]
    fn alternating_font_sets_park_contexts_without_changing_shaping() {
        // Installing page fonts would legitimately discard parked contexts.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/dejavu/DejaVuSerif.ttf"
        ))
        .unwrap();
        let fonts = FontSet::new(
            vec![crate::font_system::PageFont {
                family: "Web".into(),
                bytes,
                ..Default::default()
            }],
            None,
            false,
        );
        let installed = TextStyle {
            family: "Web, serif".into(),
            ..TextStyle::default()
        };
        let scoped = TextStyle {
            font_set: Some(fonts.clone()),
            ..installed.clone()
        };
        let mut system = TextSystem::new();
        for round in 0..4 {
            for style in [&installed, &scoped] {
                // Distinct text misses the shape cache and is shaped by the
                // reactivated context, exactly as by a freshly cloned one.
                let text = format!("round {round}");
                assert_eq!(
                    system.shape(&text, style),
                    TextSystem::new().shape(&text, style)
                );
            }
            assert_eq!(system.active_font_set, fonts.id());
            let parked: Vec<_> = system.parked_fonts.iter().map(|(set, _)| *set).collect();
            assert_eq!(parked, [0]);
        }
    }

    #[test]
    fn control_selection_bridges_utf16_offsets_and_visual_direction() {
        let mut editor = TextEditor::new("A😀éB", &TextStyle::default(), 400.0, false);
        editor.select_byte_range(7, 1);
        let selected = crate::doc::ControlSelection {
            start: 1,
            end: 4,
            direction: -1,
        };
        assert_eq!(editor.control_selection(), selected);
        editor.select_all();
        editor.set_control_selection(selected);
        assert_eq!(editor.selected_text(), Some("😀é"));
        assert_eq!(editor.control_selection(), selected);
        editor.replace_selection("X");
        assert_eq!(editor.text(), "AXB");
        assert_eq!(
            editor.control_selection(),
            crate::doc::ControlSelection {
                start: 2,
                end: 2,
                direction: 0
            }
        );
    }

    #[test]
    fn wrapped_lines_break_paragraphs_longer_than_a_parley_run() {
        // Parley keeps a cluster's offset in its run in 16 bits. A preserved
        // paragraph over 64 KiB (a pasted log line in a textarea) must still
        // break into the lines the greedy breaker gives a shorter prefix,
        // partitioning the text, instead of panicking on wrapped offsets.
        let style = TextStyle {
            size: 13.0,
            ..TextStyle::default()
        };
        let breaks = TextBreakStyle {
            wrap: true,
            ..TextBreakStyle::default()
        };
        let text: String = (0..14_000).map(|word| format!("w{word} ")).collect();
        assert!(text.len() > usize::from(u16::MAX));
        let lines = wrapped_lines(&text, &style, 90.0, 200.0, breaks);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<String>(),
            text
        );
        assert!(
            lines
                .iter()
                .all(|line| shape(line.text.trim_end(), &style).advance <= 200.0 + 0.01)
        );
        let prefix = wrapped_lines(&text[..4_000], &style, 90.0, 200.0, breaks);
        for (whole, short) in lines.iter().zip(&prefix[..prefix.len() - 1]) {
            assert_eq!(whole.text, short.text);
        }
    }

    #[test]
    fn wrapped_paragraph_retains_line_local_glyphs_and_cluster_ranges() {
        let style = TextStyle {
            size: 13.0,
            line_height: CssLineHeight::Number(1.5),
            underline: true,
            ..TextStyle::default()
        };
        let breaks = TextBreakStyle {
            wrap: true,
            ..TextBreakStyle::default()
        };
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        let lines = wrapped_lines(&text, &style, 79.0, 237.0, breaks);
        assert!(lines.len() > 10);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<String>(),
            text
        );
        let mut rest = text.as_str();
        for (index, line) in lines.iter().enumerate() {
            let width = if index == 0 { 79.0 } else { 237.0 };
            let cut = first_line_end(rest, &style, width, breaks);
            assert_eq!(line.text, &rest[..cut]);
            rest = &rest[cut..];
            let independent = shape(&line.text, &style);
            assert!((line.advance - independent.advance).abs() < 0.02);
            assert!((line.baseline - independent.baseline).abs() < 0.02);
            assert!(line.underline);
            assert!(
                line.runs
                    .iter()
                    .all(|run| run.text_range.end <= line.text.len())
            );
            assert!(
                line.clusters
                    .iter()
                    .all(|cluster| cluster.text_range.end <= line.text.len())
            );
            assert!(
                line.runs
                    .iter()
                    .flat_map(|run| &run.glyphs)
                    .all(|glyph| glyph.y.abs() < style.size * 3.0)
            );
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn wrapped_paragraph_preserves_emergency_shaping_and_graphemes() {
        let style = TextStyle::default();
        let breaks = TextBreakStyle {
            wrap: true,
            overflow_wrap: TextOverflowWrap::Anywhere,
            ..TextBreakStyle::default()
        };
        for text in [
            "نوشتننوشتننوشتن",
            "a\u{301}b\u{301}c\u{301}",
            "你好世界こんにちは",
            "👩‍👩‍👧‍👦👩‍👩‍👧‍👦",
        ] {
            let lines = wrapped_lines(text, &style, 20.0, 20.0, breaks);
            assert!(lines.len() > 1, "{text}");
            assert_eq!(
                lines
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<String>(),
                text
            );
            let boundaries: Vec<_> = text
                .grapheme_indices(true)
                .map(|(start, _)| start)
                .chain(std::iter::once(text.len()))
                .collect();
            let mut end = 0;
            for line in &lines {
                end += line.text.len();
                assert!(
                    boundaries.contains(&end),
                    "split grapheme in {text} at {end}"
                );
            }
            let mut original: Vec<_> = shape(text, &style)
                .runs
                .into_iter()
                .flat_map(|run| run.glyphs.into_iter().map(|glyph| glyph.id))
                .collect();
            let mut wrapped: Vec<_> = lines
                .into_iter()
                .flat_map(|line| line.runs)
                .flat_map(|run| run.glyphs.into_iter().map(|glyph| glyph.id))
                .collect();
            original.sort_unstable();
            wrapped.sort_unstable();
            assert_eq!(
                wrapped, original,
                "joining forms must survive emergency wrapping: {text}"
            );
        }
    }

    #[test]
    fn wrapped_paragraph_shaping_work_is_linear_and_not_line_count_dependent() {
        let mut system = TextSystem::new();
        let style = TextStyle::default();
        let breaks = TextBreakStyle {
            wrap: true,
            ..TextBreakStyle::default()
        };
        for count in [250, 500] {
            let text = "A long paragraph must be shaped only once. ".repeat(count);
            for width in [80.0, 800.0] {
                let before = system.shaped_input_bytes;
                let lines = system.wrapped_lines(&text, &style, width, width, breaks);
                assert!(lines.len() > 10);
                assert_eq!(system.shaped_input_bytes - before, text.len());
                assert_eq!(
                    lines.iter().map(|line| line.text.len()).sum::<usize>(),
                    text.len()
                );
            }
        }
    }

    #[test]
    fn proportional_advances_are_not_character_counts() {
        let style = TextStyle::default();
        let narrow = shape("iiii", &style);
        let wide = shape("WWWW", &style);
        assert!(wide.advance > narrow.advance * 1.5, "{narrow:?} {wide:?}");
    }

    #[test]
    fn identical_text_and_style_reuse_one_retained_shape() {
        let mut system = TextSystem::new();
        let style = TextStyle::default();
        let first = system.shape("repeated gallery label", &style);
        assert_eq!(system.shape_cache.len(), 1);
        let second = system.shape("repeated gallery label", &style);
        assert_eq!(system.shape_cache.len(), 1);
        assert_eq!(first, second);

        let bold = TextStyle {
            weight: 700.0,
            ..style
        };
        system.shape("repeated gallery label", &bold);
        assert_eq!(system.shape_cache.len(), 2);
    }

    #[test]
    fn language_is_part_of_the_shape_cache_identity() {
        let en = TextStyle {
            language: Some("en".into()),
            ..TextStyle::default()
        };
        let ja = TextStyle {
            language: Some("ja".into()),
            ..en.clone()
        };
        assert_ne!(TextStyleKey::from(&en), TextStyleKey::from(&ja));
        assert_eq!(text_language(&ja), "ja".parse().ok());
    }

    #[test]
    fn thai_text_has_dictionary_line_break_opportunities() {
        // CSS Text 3 §5 requires soft-wrap opportunities to follow the
        // writing system's line-breaking rules; UAX #14 class SA delegates
        // scripts such as Thai to dictionary-based segmentation. A repeated
        // no-space Thai phrase must therefore wrap without overflow-wrap's
        // emergency breaking behavior.
        let style = TextStyle {
            language: Some(String::from("th")),
            ..TextStyle::default()
        };
        let text = "ภาษาไทยภาษาไทย";
        let end = first_line_end(
            text,
            &style,
            shape("ภาษาไทย", &style).advance + 1.0,
            TextBreakStyle {
                wrap: true,
                ..TextBreakStyle::default()
            },
        );
        assert!(end > 0 && end < text.len(), "unexpected first line: {end}");
        assert!(text.is_char_boundary(end));
    }

    #[test]
    fn mixed_script_and_bidi_keep_clusters_and_fallback_runs() {
        let shaped = shape("abc שלום 世界", &TextStyle::default());
        assert!(!shaped.runs.is_empty());
        assert!(shaped.runs.iter().any(|run| run.rtl));
        assert!(
            shaped
                .clusters
                .iter()
                .all(|cluster| !cluster.text_range.is_empty())
        );
        assert!(shaped.advance > 0.0);
    }

    #[test]
    fn emoji_presentation_clusters_follow_selectors_and_defaults() {
        // UTS #51: U+FE0F requests emoji presentation, U+FE0E text, and the
        // Emoji_Presentation property supplies the default. Modifiers,
        // keycaps and joined sequences stay with their base.
        let text = "a\u{26A0}\u{FE0F} \u{26A0} \u{2728} \u{2728}\u{FE0E} \u{2764}\u{FE0F}\u{200D}\u{1F525} 1\u{FE0F}\u{20E3} \u{1F44B}\u{1F3FD}\u{A9}";
        let runs: Vec<&str> = emoji_presentation_runs(text, typography::Emoji::Normal)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(
            runs,
            [
                "\u{26A0}\u{FE0F}",
                "\u{2728}",
                "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
                "1\u{FE0F}\u{20E3}",
                "\u{1F44B}\u{1F3FD}",
            ]
        );
        assert!(
            emoji_presentation_runs("plain ascii 123 #*", typography::Emoji::Normal).is_empty()
        );
    }

    #[test]
    fn emoji_presentation_sequences_prefer_color_faces() {
        // CSS Fonts 4 #font-variant-emoji-prop: an explicit U+FE0F selects a
        // color face even where the first text family maps the base and the
        // selector (DejaVu Sans maps both U+26A0 and U+FE0F).
        let color = |shaped: &ShapedText| {
            shaped.runs.iter().any(|run| {
                let data = run.font.data();
                skrifa::FontRef::from_index(data.data.as_ref(), data.index).is_ok_and(|face| {
                    [b"COLR", b"CBDT", b"SVG ", b"sbix"]
                        .iter()
                        .any(|tag| face.table_data(skrifa::Tag::new(tag)).is_some())
                })
            })
        };
        if !color(&shape("\u{1F600}", &TextStyle::default())) {
            return; // No color emoji face is installed.
        }
        assert!(color(&shape("\u{26A0}\u{FE0F}", &TextStyle::default())));
        assert!(color(&shape("x\u{2728}x", &TextStyle::default())));
        if !color(&shape("\u{26A0}", &TextStyle::default())) {
            assert!(!color(&shape("\u{26A0}\u{FE0E}", &TextStyle::default())));
        }
    }

    #[test]
    fn missing_css_family_falls_back_to_a_generic_system_face() {
        let shaped = shape(
            "fallback works",
            &TextStyle {
                family: String::from("\"TRust Definitely Missing\", sans-serif"),
                ..TextStyle::default()
            },
        );
        assert!(!shaped.runs.is_empty());
        assert!(shaped.runs.iter().all(|run| !run.glyphs.is_empty()));
        assert!(shaped.advance > 0.0);
    }

    #[test]
    fn custom_family_lists_get_a_generic_fallback_without_overriding_generics() {
        assert_eq!(
            font_family_source("faustina,faustina-fallback").as_ref(),
            "faustina,faustina-fallback, sans-serif"
        );
        assert_eq!(font_family_source("serif").as_ref(), "serif");
        assert_eq!(font_family_source("SANS-SERIF").as_ref(), "sans-serif");
        assert_eq!(font_family_source("").as_ref(), "sans-serif");
    }

    #[test]
    fn weight_and_style_reach_the_shaper() {
        let normal = shape("mixed", &TextStyle::default());
        let styled = shape(
            "mixed",
            &TextStyle {
                weight: 700.0,
                italic: true,
                ..TextStyle::default()
            },
        );
        assert!(!normal.runs.is_empty());
        assert!(!styled.runs.is_empty());
        assert!(styled.line_height > 0.0);
    }

    #[test]
    fn editor_backspace_respects_emoji_cluster_boundaries() {
        let mut editor = TextEditor::new("a👩‍👩‍👧‍👦", &TextStyle::default(), 300.0, false);
        assert!(editor.handle_key(&KeyInput {
            key: Key::Backspace,
            code: String::new(),
            location: 0,
            state: KeyState::Pressed,
            modifiers: Default::default(),
            repeat: false,
            composing: false,
        }));
        assert_eq!(editor.text(), "a");
    }

    #[test]
    fn editor_delete_respects_combining_grapheme_boundaries() {
        let mut editor = TextEditor::new("e\u{301}x", &TextStyle::default(), 300.0, false);
        editor.select_byte_range(0, 0);
        assert!(editor.handle_key(&KeyInput {
            key: Key::Delete,
            code: String::new(),
            location: 0,
            state: KeyState::Pressed,
            modifiers: Default::default(),
            repeat: false,
            composing: false,
        }));
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn editor_ime_keeps_preedit_out_of_committed_value_then_commits() {
        let mut editor = TextEditor::new("a", &TextStyle::default(), 300.0, false);
        editor.handle_ime(&ImeAction::Preedit {
            text: String::from("に"),
            cursor: Some((3, 3)),
        });
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.raw_text(), "aに");
        editor.handle_ime(&ImeAction::Commit(String::from("に")));
        assert_eq!(editor.text(), "aに");
    }

    #[test]
    fn editor_ime_commit_replaces_preedit_with_the_actual_final_text() {
        for (commit, expected) in [("日本", "@ 日本 tail"), ("", "@  tail")] {
            let mut editor = TextEditor::new("@  tail", &TextStyle::default(), 300.0, false);
            editor.select_byte_range(2, 2);
            editor.handle_ime(&ImeAction::Preedit {
                text: "にほん".into(),
                cursor: Some((9, 9)),
            });
            editor.handle_ime(&ImeAction::Commit(commit.into()));
            assert_eq!(editor.text(), expected);
            assert!(!editor.is_composing());
            assert_eq!(editor.selection(), 2 + commit.len()..2 + commit.len());
        }
    }

    #[test]
    fn editor_visual_motion_uses_bidi_shaped_clusters() {
        let mut editor = TextEditor::new("abc שלום", &TextStyle::default(), 300.0, false);
        editor.handle_key(&KeyInput {
            key: Key::ArrowLeft,
            code: String::new(),
            location: 0,
            state: KeyState::Pressed,
            modifiers: crate::core::Modifiers {
                shift: true,
                ..crate::core::Modifiers::default()
            },
            repeat: false,
            composing: false,
        });
        assert!(editor.selected_text().is_some_and(|text| !text.is_empty()));
    }

    // CSS Fonts 4 conformance: typography reaching the shaper.

    fn dejavu(file: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/assets/fonts/dejavu/{file}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn web_fonts(faces: Vec<(&str, Vec<u8>, crate::font_system::FaceDescriptors)>) -> Arc<FontSet> {
        FontSet::new(
            faces
                .into_iter()
                .map(|(family, bytes, descriptors)| {
                    crate::font_system::PageFont {
                        family: family.into(),
                        bytes,
                        ..Default::default()
                    }
                    .with_descriptors(descriptors)
                })
                .collect(),
            None,
            false,
        )
    }

    fn typed(apply: impl FnOnce(&mut FontTypography)) -> Option<Arc<FontTypography>> {
        let mut typography = FontTypography::default();
        apply(&mut typography);
        Some(Arc::new(typography))
    }

    fn glyph_count(shaped: &ShapedText) -> usize {
        shaped.runs.iter().map(|run| run.glyphs.len()).sum()
    }

    #[test]
    fn kerning_and_ligature_properties_select_opentype_features() {
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSans.ttf"), Default::default())]);
        let style = TextStyle {
            family: "Web".into(),
            font_set: Some(fonts),
            size: 40.,
            ..TextStyle::default()
        };
        // #font-kerning-prop: `none` disables the font's pair kerning.
        let kerned = shape("AV", &style).advance;
        let unkerned = shape(
            "AV",
            &TextStyle {
                typography: typed(|t| t.kerning = typography::Kerning::None),
                ..style.clone()
            },
        )
        .advance;
        assert!(unkerned > kerned, "{unkerned} > {kerned}");
        // #font-variant-ligatures-prop: common ligatures are on by default.
        assert_eq!(glyph_count(&shape("fi", &style)), 1);
        let none = TextStyle {
            typography: typed(|t| t.ligatures = typography::parse_ligatures("none").unwrap()),
            ..style.clone()
        };
        assert_eq!(glyph_count(&shape("fi", &none)), 2);
        // #feature-variation-precedence step 11: letter-spacing disables
        // optional ligatures; step 13: font-feature-settings wins.
        let spaced = TextStyle {
            letter_spacing: 2.,
            ..style.clone()
        };
        assert_eq!(glyph_count(&shape("fi", &spaced)), 2);
        let forced = TextStyle {
            typography: typed(|t| {
                t.feature_settings = typography::parse_feature_settings("\"liga\"").unwrap()
            }),
            ..spaced
        };
        assert_eq!(glyph_count(&shape("fi", &forced)), 1);
    }

    #[test]
    fn small_capitals_are_synthesized_with_source_offsets_and_the_element_strut() {
        // DejaVu Sans has no `smcp`: lowercase letters become reduced
        // capitals (#font-variant-caps-prop), yet clusters and the text keep
        // the source offsets and the line keeps the element font's metrics.
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSans.ttf"), Default::default())]);
        let style = TextStyle {
            family: "Web".into(),
            font_set: Some(fonts),
            size: 40.,
            ..TextStyle::default()
        };
        let small = TextStyle {
            typography: typed(|t| t.caps = typography::Caps::SmallCaps),
            ..style.clone()
        };
        let text = "Straße x";
        let plain = shape(text, &style);
        let shaped = shape(text, &small);
        assert_eq!(shaped.text, text);
        assert_eq!(shaped.clusters.last().unwrap().text_range.end, text.len());
        assert!(
            shaped
                .runs
                .iter()
                .any(|run| (run.font_size - 28.).abs() < 0.01)
        );
        assert!(
            shaped
                .runs
                .iter()
                .any(|run| (run.font_size - 40.).abs() < 0.01)
        );
        assert_eq!(shaped.ascent, plain.ascent);
        assert_eq!(shaped.line_height, plain.line_height);
        // An all-lowercase word keeps the strut too.
        assert_eq!(
            shape("abc", &small).line_height,
            shape("abc", &style).line_height
        );
        // Line breaking reports source offsets.
        let end = first_line_end(text, &small, 1000., TextBreakStyle::default());
        assert_eq!(end, text.len());
        // font-synthesis-small-caps: none forbids the simulation.
        let forbidden = TextStyle {
            typography: typed(|t| {
                t.caps = typography::Caps::SmallCaps;
                t.synthesis.small_caps = false;
            }),
            ..style
        };
        assert_eq!(shape(text, &forbidden).advance, plain.advance);
    }

    #[test]
    fn missing_superscripts_are_synthesized_off_the_baseline() {
        // #font-variant-position-prop: DejaVu Sans lacks `sups`, so reduced
        // glyphs are raised; the line metrics stay the element font's.
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSans.ttf"), Default::default())]);
        let style = TextStyle {
            family: "Web".into(),
            font_set: Some(fonts),
            size: 40.,
            ..TextStyle::default()
        };
        let plain = shape("2", &style);
        let sup = shape(
            "2",
            &TextStyle {
                typography: typed(|t| t.position = typography::Position::Super),
                ..style.clone()
            },
        );
        assert!(sup.runs[0].font_size < 40.);
        assert!(sup.runs[0].glyphs[0].y < plain.runs[0].glyphs[0].y);
        assert_eq!(sup.baseline, plain.baseline);
        assert_eq!(sup.line_height, plain.line_height);
        let sub = shape(
            "2",
            &TextStyle {
                typography: typed(|t| t.position = typography::Position::Sub),
                ..style.clone()
            },
        );
        assert!(sub.runs[0].glyphs[0].y > plain.runs[0].glyphs[0].y);
        // font-synthesis-position: none uses the (absent) feature only.
        let unsynthesized = shape(
            "2",
            &TextStyle {
                typography: typed(|t| {
                    t.position = typography::Position::Super;
                    t.synthesis.position = false;
                }),
                ..style
            },
        );
        assert_eq!(unsynthesized.runs[0].font_size, 40.);
    }

    #[test]
    fn oblique_angles_and_style_synthesis_policy_reach_rendering() {
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSerif.ttf"), Default::default())]);
        let skew = |italic: bool, apply: &dyn Fn(&mut FontTypography)| {
            let mut typography = FontTypography::default();
            apply(&mut typography);
            shape(
                "Hello",
                &TextStyle {
                    family: "Web".into(),
                    font_set: Some(fonts.clone()),
                    italic,
                    typography: Some(Arc::new(typography)),
                    ..TextStyle::default()
                },
            )
            .runs[0]
                .synth_skew_degrees
        };
        // #font-style-prop: `oblique 20deg` shears the upright face by 20deg.
        assert_eq!(skew(true, &|t| t.oblique = Some(20.)), Some(20.));
        assert_eq!(skew(true, &|t| t.oblique = Some(-8.5)), Some(-8.5));
        assert_eq!(skew(true, &|_| {}), Some(14.));
        // #font-synthesis-style.
        let none = |t: &mut FontTypography| t.synthesis.style = typography::SynthesisStyle::None;
        assert_eq!(skew(true, &none), None);
        let oblique_only =
            |t: &mut FontTypography| t.synthesis.style = typography::SynthesisStyle::ObliqueOnly;
        assert_eq!(skew(true, &oblique_only), None);
        assert_eq!(
            skew(true, &|t| {
                oblique_only(t);
                t.oblique = Some(10.);
            }),
            Some(10.)
        );
    }

    #[test]
    fn font_synthesis_weight_none_suppresses_synthetic_bold() {
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSerif.ttf"), Default::default())]);
        let bold = |typography| {
            shape(
                "Hello",
                &TextStyle {
                    family: "Web".into(),
                    font_set: Some(fonts.clone()),
                    weight: 700.,
                    typography,
                    ..TextStyle::default()
                },
            )
            .runs[0]
                .synth_bold
        };
        assert!(bold(None));
        assert!(!bold(typed(|t| t.synthesis.weight = false)));
    }

    fn descriptors(apply: &[(&str, &str)]) -> crate::font_system::FaceDescriptors {
        let mut descriptors = crate::font_system::FaceDescriptors::default();
        for (name, value) in apply {
            assert!(descriptors.apply(name, value), "{name}: {value}");
        }
        descriptors
    }

    #[test]
    fn face_descriptor_ranges_take_part_in_font_matching() {
        // CSS Fonts 4 #font-style-matching with #font-prop-desc ranges: the
        // descriptors, not the font data, place each face.
        let serif = dejavu("DejaVuSerif.ttf");
        let sans = dejavu("DejaVuSans.ttf");
        let fonts = web_fonts(vec![
            (
                "Ranged",
                serif.clone(),
                descriptors(&[("font-weight", "100 399")]),
            ),
            (
                "Ranged",
                sans.clone(),
                descriptors(&[("font-weight", "400 700")]),
            ),
        ]);
        let face = |weight: f32| {
            let shaped = shape(
                "a",
                &TextStyle {
                    family: "Ranged".into(),
                    font_set: Some(fonts.clone()),
                    weight,
                    ..TextStyle::default()
                },
            );
            let data = shaped.runs[0].font.data();
            (data.data.as_ref().len(), shaped.runs[0].synth_bold)
        };
        assert_eq!(face(300.).0, serif.len());
        assert_eq!(face(500.).0, sans.len());
        // 450: weights 450-500 ascending are in the 400-700 face.
        assert_eq!(face(450.).0, sans.len());
        // 800: above 700 nothing; then below 800 descending finds 700.
        assert_eq!(face(800.), (sans.len(), false));
        // A face described as bold is not emboldened; one described as
        // normal (`auto`) is, whatever the font file says.
        let fonts = web_fonts(vec![(
            "Described",
            dejavu("DejaVuSerif.ttf"),
            descriptors(&[("font-weight", "bold")]),
        )]);
        let shaped = shape(
            "a",
            &TextStyle {
                family: "Described".into(),
                font_set: Some(fonts),
                weight: 700.,
                ..TextStyle::default()
            },
        );
        assert!(!shaped.runs[0].synth_bold);
        // `font-style: italic` describes an italic face: no synthesis.
        let fonts = web_fonts(vec![(
            "Slanted",
            dejavu("DejaVuSerif.ttf"),
            descriptors(&[("font-style", "italic")]),
        )]);
        let shaped = shape(
            "a",
            &TextStyle {
                family: "Slanted".into(),
                font_set: Some(fonts),
                italic: true,
                ..TextStyle::default()
            },
        );
        assert_eq!(shaped.runs[0].synth_skew_degrees, None);
    }

    /// The value the shaper applied to axis `axis` of the test font.
    fn axis(style: &TextStyle, axis: usize) -> f32 {
        let text = test_fonts::AXIS_CHARS[axis].to_string();
        let shaped = shape(
            &text,
            &TextStyle {
                size: f32::from(test_fonts::UPEM),
                ..style.clone()
            },
        );
        test_fonts::axis_value(axis, shaped.advance)
    }

    #[test]
    fn variation_axes_follow_font_properties_in_precedence_order() {
        // #feature-variation-precedence: step 2 (font-weight, font-width,
        // font-style through wght/wdth/slnt/ital), step 9 (optical size),
        // step 12 (font-variation-settings), clamped to the font's axes.
        let fonts = web_fonts(vec![(
            "Axes",
            test_fonts::variable_font(),
            Default::default(),
        )]);
        let base = TextStyle {
            family: "Axes".into(),
            font_set: Some(fonts),
            ..TextStyle::default()
        };
        let close = |a: f32, b: f32| (a - b).abs() < 0.5;
        assert!(close(axis(&base, 0), 400.));
        assert!(close(
            axis(
                &TextStyle {
                    weight: 700.,
                    ..base.clone()
                },
                0
            ),
            700.
        ));
        let width = TextStyle {
            typography: typed(|t| t.width = 1.5),
            ..base.clone()
        };
        assert!(close(axis(&width, 1), 150.));
        // #font-optical-sizing-def: `auto` follows the used font size,
        // clamped to the axis; `none` leaves the default.
        assert!(close(axis(&base, 2), 72.), "{}", axis(&base, 2));
        let off = TextStyle {
            typography: typed(|t| t.optical_sizing = false),
            ..base.clone()
        };
        assert!(close(axis(&off, 2), 12.));
        // #font-style-prop: CSS angles are clockwise, `slnt` counter-clockwise.
        let oblique = TextStyle {
            italic: true,
            typography: typed(|t| t.oblique = Some(10.)),
            ..base.clone()
        };
        assert!(close(axis(&oblique, 3), -10.), "{}", axis(&oblique, 3));
        let italic = TextStyle {
            italic: true,
            ..base.clone()
        };
        assert!(close(axis(&italic, 4), 1.));
        // font-variation-settings overrides font-weight.
        let set = TextStyle {
            weight: 700.,
            typography: typed(|t| {
                t.variation_settings = typography::parse_variation_settings("\"wght\" 850").unwrap()
            }),
            ..base.clone()
        };
        assert!(close(axis(&set, 0), 850.), "{}", axis(&set, 0));
    }

    #[test]
    fn face_descriptors_clamp_and_set_variations() {
        // #font-prop-desc: variation values applied to an @font-face face
        // clamp to its descriptor ranges; #font-rend-desc variation settings
        // apply to that face (step 6), below the property's (step 12).
        let fonts = web_fonts(vec![(
            "Clamped",
            test_fonts::variable_font(),
            descriptors(&[
                ("font-weight", "300 500"),
                ("font-variation-settings", "\"wdth\" 150"),
            ]),
        )]);
        let style = TextStyle {
            family: "Clamped".into(),
            font_set: Some(fonts),
            weight: 700.,
            ..TextStyle::default()
        };
        let close = |a: f32, b: f32| (a - b).abs() < 0.5;
        assert!(close(axis(&style, 0), 500.), "{}", axis(&style, 0));
        assert!(close(axis(&style, 1), 150.), "{}", axis(&style, 1));
        let overridden = TextStyle {
            typography: typed(|t| {
                t.variation_settings = typography::parse_variation_settings("\"wdth\" 120").unwrap()
            }),
            ..style
        };
        assert!(close(axis(&overridden, 1), 120.));
    }

    #[test]
    fn named_instance_descriptor_applies_before_face_variation_settings() {
        // #font-named-instance, #feature-variation-precedence steps 5 and 6.
        let fonts = web_fonts(vec![
            (
                "Named",
                test_fonts::variable_font(),
                descriptors(&[("font-named-instance", "\"heavy wide\"")]),
            ),
            (
                "Overridden",
                test_fonts::variable_font(),
                descriptors(&[
                    ("font-named-instance", "\"Heavy Wide\""),
                    ("font-variation-settings", "\"wght\" 600"),
                ]),
            ),
        ]);
        let style = |family: &str| TextStyle {
            family: family.into(),
            font_set: Some(fonts.clone()),
            ..TextStyle::default()
        };
        let close = |a: f32, b: f32| (a - b).abs() < 0.5;
        assert!(close(axis(&style("Named"), 0), 900.));
        assert!(close(axis(&style("Named"), 1), 200.));
        assert!(close(axis(&style("Overridden"), 0), 600.));
        assert!(close(axis(&style("Overridden"), 1), 200.));
    }

    #[test]
    fn size_adjust_and_metric_overrides_apply_to_their_face() {
        // CSS Fonts 5 #size-adjust-desc scales the face's glyphs and metrics;
        // #font-metrics-override-desc replaces ascent, descent and line gap.
        let plain = web_fonts(vec![("Web", dejavu("DejaVuSans.ttf"), Default::default())]);
        let adjusted = web_fonts(vec![(
            "Web",
            dejavu("DejaVuSans.ttf"),
            descriptors(&[
                ("size-adjust", "150%"),
                ("ascent-override", "50%"),
                ("descent-override", "25%"),
                ("line-gap-override", "0%"),
            ]),
        )]);
        let style = |fonts: &Arc<FontSet>| TextStyle {
            family: "Web".into(),
            font_set: Some(fonts.clone()),
            size: 20.,
            ..TextStyle::default()
        };
        let before = shape("Hello", &style(&plain));
        let after = shape("Hello", &style(&adjusted));
        assert!((after.advance - before.advance * 1.5).abs() < 0.5);
        assert_eq!(after.runs[0].font_size, 30.);
        // The overrides are fractions of the used (adjusted) size.
        assert!((after.ascent - 15.).abs() < 0.1, "{}", after.ascent);
        assert!((after.descent - 7.5).abs() < 0.1, "{}", after.descent);
        // A number line-height still multiplies the computed font size.
        let numbered = shape(
            "Hello",
            &TextStyle {
                line_height: CssLineHeight::Number(2.),
                ..style(&adjusted)
            },
        );
        assert_eq!(numbered.line_height, 40.);
    }

    #[test]
    fn font_size_adjust_normalizes_each_font_metric() {
        // CSS Fonts 5 #font-size-adjust-prop: u = (m / m') s per font.
        let fonts = web_fonts(vec![("Web", dejavu("DejaVuSans.ttf"), Default::default())]);
        let style = TextStyle {
            family: "Web".into(),
            font_set: Some(fonts),
            size: 20.,
            ..TextStyle::default()
        };
        let aspect =
            first_available_metric(&style, typography::SizeAdjustMetric::ExHeight).unwrap();
        assert!(aspect > 0.4 && aspect < 0.7, "{aspect}");
        let adjusted = shape(
            "x",
            &TextStyle {
                typography: typed(|t| {
                    t.size_adjust = Some((typography::SizeAdjustMetric::ExHeight, 0.5))
                }),
                ..style.clone()
            },
        );
        assert!((adjusted.runs[0].font_size - 20. * 0.5 / aspect).abs() < 0.01);
        // The value equal to the font's own aspect leaves its size.
        let same = shape(
            "x",
            &TextStyle {
                typography: typed(|t| {
                    t.size_adjust = Some((typography::SizeAdjustMetric::ExHeight, aspect))
                }),
                ..style
            },
        );
        assert!((same.runs[0].font_size - 20.).abs() < 0.01);
    }

    #[test]
    fn face_feature_settings_descriptor_applies_to_its_face_only() {
        let fonts = web_fonts(vec![
            (
                "NoLiga",
                dejavu("DejaVuSans.ttf"),
                descriptors(&[("font-feature-settings", "\"liga\" 0")]),
            ),
            ("Liga", dejavu("DejaVuSans.ttf"), Default::default()),
        ]);
        let count = |family: &str| {
            glyph_count(&shape(
                "fi",
                &TextStyle {
                    family: family.into(),
                    font_set: Some(fonts.clone()),
                    ..TextStyle::default()
                },
            ))
        };
        assert_eq!(count("NoLiga"), 2);
        assert_eq!(count("Liga"), 1);
    }

    #[test]
    fn font_variant_emoji_sets_the_default_presentation() {
        let runs = |text, variant| emoji_presentation_runs(text, variant);
        // U+263A has text presentation by default, U+1F600 emoji.
        assert!(runs("\u{263A}", typography::Emoji::Normal).is_empty());
        assert_eq!(runs("\u{263A}", typography::Emoji::Emoji).len(), 1);
        assert!(runs("\u{1F600}", typography::Emoji::Text).is_empty());
        // An explicit selector still wins.
        assert_eq!(runs("\u{263A}\u{FE0F}", typography::Emoji::Text).len(), 1);
        // ASCII digits are not emoji alone.
        let digit = runs("1\u{263A}", typography::Emoji::Emoji);
        assert_eq!(digit.len(), 1);
        assert_eq!(digit[0], 1..4);
    }
}
